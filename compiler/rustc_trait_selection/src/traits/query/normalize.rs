//! Code for the 'normalization' query. This consists of a wrapper
//! which folds deeply, invoking the underlying
//! `normalize_projection_ty` query when it encounters projections.

use crate::infer::at::At;
use crate::infer::canonical::OriginalQueryValues;
use crate::infer::{InferCtxt, InferOk};
use crate::traits::error_reporting::InferCtxtExt;
use crate::traits::project::{needs_normalization, BoundVarReplacer, PlaceholderReplacer};
use crate::traits::{Obligation, ObligationCause, PredicateObligation, Reveal};
use rustc_data_structures::sso::SsoHashMap;
use rustc_data_structures::stack::ensure_sufficient_stack;
use rustc_infer::traits::Normalized;
use rustc_middle::mir;
use rustc_middle::ty::fold::{FallibleTypeFolder, TypeFoldable, TypeSuperFoldable};
use rustc_middle::ty::visit::{TypeSuperVisitable, TypeVisitable};
use rustc_middle::ty::{self, Ty, TyCtxt, TypeVisitor};

use std::ops::ControlFlow;

use super::NoSolution;

pub use rustc_middle::traits::query::NormalizationResult;

pub trait AtExt<'tcx> {
    fn normalize<T>(&self, value: T) -> Result<Normalized<'tcx, T>, NoSolution>
    where
        T: TypeFoldable<'tcx>;
}

impl<'cx, 'tcx> AtExt<'tcx> for At<'cx, 'tcx> {
    /// Normalize `value` in the context of the inference context,
    /// yielding a resulting type, or an error if `value` cannot be
    /// normalized. If you don't care about regions, you should prefer
    /// `normalize_erasing_regions`, which is more efficient.
    ///
    /// If the normalization succeeds and is unambiguous, returns back
    /// the normalized value along with various outlives relations (in
    /// the form of obligations that must be discharged).
    ///
    /// N.B., this will *eventually* be the main means of
    /// normalizing, but for now should be used only when we actually
    /// know that normalization will succeed, since error reporting
    /// and other details are still "under development".
    fn normalize<T>(&self, value: T) -> Result<Normalized<'tcx, T>, NoSolution>
    where
        T: TypeFoldable<'tcx>,
    {
        debug!(
            "normalize::<{}>(value={:?}, param_env={:?}, cause={:?})",
            std::any::type_name::<T>(),
            value,
            self.param_env,
            self.cause,
        );
        if !needs_normalization(&value, self.param_env.reveal()) {
            return Ok(Normalized { value, obligations: vec![] });
        }

        let mut normalizer = QueryNormalizer {
            infcx: self.infcx,
            cause: self.cause,
            param_env: self.param_env,
            obligations: vec![],
            cache: SsoHashMap::new(),
            anon_depth: 0,
            universes: vec![],
        };

        // This is actually a consequence by the way `normalize_erasing_regions` works currently.
        // Because it needs to call the `normalize_generic_arg_after_erasing_regions`, it folds
        // through tys and consts in a `TypeFoldable`. Importantly, it skips binders, leaving us
        // with trying to normalize with escaping bound vars.
        //
        // Here, we just add the universes that we *would* have created had we passed through the binders.
        //
        // We *could* replace escaping bound vars eagerly here, but it doesn't seem really necessary.
        // The rest of the code is already set up to be lazy about replacing bound vars,
        // and only when we actually have to normalize.
        if value.has_escaping_bound_vars() {
            let mut max_visitor =
                MaxEscapingBoundVarVisitor { outer_index: ty::INNERMOST, escaping: 0 };
            value.visit_with(&mut max_visitor);
            if max_visitor.escaping > 0 {
                normalizer.universes.extend((0..max_visitor.escaping).map(|_| None));
            }
        }
        let result = value.try_fold_with(&mut normalizer);
        info!(
            "normalize::<{}>: result={:?} with {} obligations",
            std::any::type_name::<T>(),
            result,
            normalizer.obligations.len(),
        );
        debug!(
            "normalize::<{}>: obligations={:?}",
            std::any::type_name::<T>(),
            normalizer.obligations,
        );
        result.map(|value| Normalized { value, obligations: normalizer.obligations })
    }
}

// Visitor to find the maximum escaping bound var
struct MaxEscapingBoundVarVisitor {
    // The index which would count as escaping
    outer_index: ty::DebruijnIndex,
    escaping: usize,
}

impl<'tcx> TypeVisitor<'tcx> for MaxEscapingBoundVarVisitor {
    fn visit_binder<T: TypeVisitable<'tcx>>(
        &mut self,
        t: &ty::Binder<'tcx, T>,
    ) -> ControlFlow<Self::BreakTy> {
        self.outer_index.shift_in(1);
        let result = t.super_visit_with(self);
        self.outer_index.shift_out(1);
        result
    }

    #[inline]
    fn visit_ty(&mut self, t: Ty<'tcx>) -> ControlFlow<Self::BreakTy> {
        if t.outer_exclusive_binder() > self.outer_index {
            self.escaping = self
                .escaping
                .max(t.outer_exclusive_binder().as_usize() - self.outer_index.as_usize());
        }
        ControlFlow::CONTINUE
    }

    #[inline]
    fn visit_region(&mut self, r: ty::Region<'tcx>) -> ControlFlow<Self::BreakTy> {
        match *r {
            ty::ReLateBound(debruijn, _) if debruijn > self.outer_index => {
                self.escaping =
                    self.escaping.max(debruijn.as_usize() - self.outer_index.as_usize());
            }
            _ => {}
        }
        ControlFlow::CONTINUE
    }

    fn visit_const(&mut self, ct: ty::Const<'tcx>) -> ControlFlow<Self::BreakTy> {
        match ct.kind() {
            ty::ConstKind::Bound(debruijn, _) if debruijn >= self.outer_index => {
                self.escaping =
                    self.escaping.max(debruijn.as_usize() - self.outer_index.as_usize());
                ControlFlow::CONTINUE
            }
            _ => ct.super_visit_with(self),
        }
    }
}

struct QueryNormalizer<'cx, 'tcx> {
    infcx: &'cx InferCtxt<'cx, 'tcx>,
    cause: &'cx ObligationCause<'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    obligations: Vec<PredicateObligation<'tcx>>,
    cache: SsoHashMap<Ty<'tcx>, Ty<'tcx>>,
    anon_depth: usize,
    universes: Vec<Option<ty::UniverseIndex>>,
}

impl<'cx, 'tcx> FallibleTypeFolder<'tcx> for QueryNormalizer<'cx, 'tcx> {
    type Error = NoSolution;

    fn tcx<'c>(&'c self) -> TyCtxt<'tcx> {
        self.infcx.tcx
    }

    fn try_fold_binder<T: TypeFoldable<'tcx>>(
        &mut self,
        t: ty::Binder<'tcx, T>,
    ) -> Result<ty::Binder<'tcx, T>, Self::Error> {
        self.universes.push(None);
        let t = t.try_super_fold_with(self);
        self.universes.pop();
        t
    }

    #[instrument(level = "debug", skip(self))]
    fn try_fold_ty(&mut self, ty: Ty<'tcx>) -> Result<Ty<'tcx>, Self::Error> {
        if !needs_normalization(&ty, self.param_env.reveal()) {
            return Ok(ty);
        }

        if let Some(ty) = self.cache.get(&ty) {
            return Ok(*ty);
        }

        // See note in `rustc_trait_selection::traits::project` about why we
        // wait to fold the substs.

        // Wrap this in a closure so we don't accidentally return from the outer function
        let res = (|| match *ty.kind() {
            // This is really important. While we *can* handle this, this has
            // severe performance implications for large opaque types with
            // late-bound regions. See `issue-88862` benchmark.
            ty::Opaque(def_id, substs) => {
                // Only normalize `impl Trait` outside of type inference, usually in codegen.
                match self.param_env.reveal() {
                    Reveal::UserFacing => ty.try_super_fold_with(self),

                    Reveal::All => {
                        let substs = substs.try_fold_with(self)?;
                        let recursion_limit = self.tcx().recursion_limit();
                        if !recursion_limit.value_within_limit(self.anon_depth) {
                            let obligation = Obligation::with_depth(
                                self.cause.clone(),
                                recursion_limit.0,
                                self.param_env,
                                ty,
                            );
                            self.infcx.report_overflow_error(&obligation, true);
                        }

                        let generic_ty = self.tcx().bound_type_of(def_id);
                        let concrete_ty = generic_ty.subst(self.tcx(), substs);
                        self.anon_depth += 1;
                        if concrete_ty == ty {
                            bug!(
                                "infinite recursion generic_ty: {:#?}, substs: {:#?}, \
                                 concrete_ty: {:#?}, ty: {:#?}",
                                generic_ty,
                                substs,
                                concrete_ty,
                                ty
                            );
                        }
                        let folded_ty = ensure_sufficient_stack(|| self.try_fold_ty(concrete_ty));
                        self.anon_depth -= 1;
                        folded_ty
                    }
                }
            }

            ty::Projection(data) if !data.has_escaping_bound_vars() => {
                // This branch is just an optimization: when we don't have escaping bound vars,
                // we don't need to replace them with placeholders (see branch below).

                let tcx = self.infcx.tcx;
                let data = data.try_fold_with(self)?;

                let mut orig_values = OriginalQueryValues::default();
                // HACK(matthewjasper) `'static` is special-cased in selection,
                // so we cannot canonicalize it.
                let c_data = self
                    .infcx
                    .canonicalize_query_keep_static(self.param_env.and(data), &mut orig_values);
                debug!("QueryNormalizer: c_data = {:#?}", c_data);
                debug!("QueryNormalizer: orig_values = {:#?}", orig_values);
                let result = tcx.normalize_projection_ty(c_data)?;
                // We don't expect ambiguity.
                if result.is_ambiguous() {
                    bug!("unexpected ambiguity: {:?} {:?}", c_data, result);
                }
                let InferOk { value: result, obligations } =
                    self.infcx.instantiate_query_response_and_region_obligations(
                        self.cause,
                        self.param_env,
                        &orig_values,
                        result,
                    )?;
                debug!("QueryNormalizer: result = {:#?}", result);
                debug!("QueryNormalizer: obligations = {:#?}", obligations);
                self.obligations.extend(obligations);

                let res = result.normalized_ty;
                // `tcx.normalize_projection_ty` may normalize to a type that still has
                // unevaluated consts, so keep normalizing here if that's the case.
                if res != ty && res.has_type_flags(ty::TypeFlags::HAS_CT_PROJECTION) {
                    Ok(res.try_super_fold_with(self)?)
                } else {
                    Ok(res)
                }
            }

            ty::Projection(data) => {
                // See note in `rustc_trait_selection::traits::project`

                let tcx = self.infcx.tcx;
                let infcx = self.infcx;
                let (data, mapped_regions, mapped_types, mapped_consts) =
                    BoundVarReplacer::replace_bound_vars(infcx, &mut self.universes, data);
                let data = data.try_fold_with(self)?;

                let mut orig_values = OriginalQueryValues::default();
                // HACK(matthewjasper) `'static` is special-cased in selection,
                // so we cannot canonicalize it.
                let c_data = self
                    .infcx
                    .canonicalize_query_keep_static(self.param_env.and(data), &mut orig_values);
                debug!("QueryNormalizer: c_data = {:#?}", c_data);
                debug!("QueryNormalizer: orig_values = {:#?}", orig_values);
                let result = tcx.normalize_projection_ty(c_data)?;
                // We don't expect ambiguity.
                if result.is_ambiguous() {
                    bug!("unexpected ambiguity: {:?} {:?}", c_data, result);
                }
                let InferOk { value: result, obligations } =
                    self.infcx.instantiate_query_response_and_region_obligations(
                        self.cause,
                        self.param_env,
                        &orig_values,
                        result,
                    )?;
                debug!("QueryNormalizer: result = {:#?}", result);
                debug!("QueryNormalizer: obligations = {:#?}", obligations);
                self.obligations.extend(obligations);
                let res = PlaceholderReplacer::replace_placeholders(
                    infcx,
                    mapped_regions,
                    mapped_types,
                    mapped_consts,
                    &self.universes,
                    result.normalized_ty,
                );
                // `tcx.normalize_projection_ty` may normalize to a type that still has
                // unevaluated consts, so keep normalizing here if that's the case.
                if res != ty && res.has_type_flags(ty::TypeFlags::HAS_CT_PROJECTION) {
                    Ok(res.try_super_fold_with(self)?)
                } else {
                    Ok(res)
                }
            }

            _ => ty.try_super_fold_with(self),
        })()?;

        self.cache.insert(ty, res);
        Ok(res)
    }

    fn try_fold_const(
        &mut self,
        constant: ty::Const<'tcx>,
    ) -> Result<ty::Const<'tcx>, Self::Error> {
        let constant = constant.try_super_fold_with(self)?;
        debug!(?constant, ?self.param_env);
        Ok(crate::traits::project::with_replaced_escaping_bound_vars(
            self.infcx,
            &mut self.universes,
            constant,
            |constant| constant.eval(self.infcx.tcx, self.param_env),
        ))
    }

    fn try_fold_mir_const(
        &mut self,
        constant: mir::ConstantKind<'tcx>,
    ) -> Result<mir::ConstantKind<'tcx>, Self::Error> {
        Ok(match constant {
            mir::ConstantKind::Ty(c) => {
                let const_folded = c.try_super_fold_with(self)?;
                match const_folded.kind() {
                    ty::ConstKind::Value(valtree) => {
                        let tcx = self.infcx.tcx;
                        let ty = const_folded.ty();
                        let const_val = tcx.valtree_to_const_val((ty, valtree));
                        debug!(?ty, ?valtree, ?const_val);

                        mir::ConstantKind::Val(const_val, ty)
                    }
                    _ => mir::ConstantKind::Ty(const_folded),
                }
            }
            mir::ConstantKind::Val(_, _) | mir::ConstantKind::Unevaluated(..) => {
                constant.try_super_fold_with(self)?
            }
        })
    }

    #[inline]
    fn try_fold_predicate(
        &mut self,
        p: ty::Predicate<'tcx>,
    ) -> Result<ty::Predicate<'tcx>, Self::Error> {
        if p.allow_normalization() && needs_normalization(&p, self.param_env.reveal()) {
            p.try_super_fold_with(self)
        } else {
            Ok(p)
        }
    }
}
