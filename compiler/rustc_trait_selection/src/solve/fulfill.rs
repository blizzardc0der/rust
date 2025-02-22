use std::mem;
use std::ops::ControlFlow;

use rustc_infer::infer::InferCtxt;
use rustc_infer::traits::query::NoSolution;
use rustc_infer::traits::solve::inspect::ProbeKind;
use rustc_infer::traits::solve::{CandidateSource, GoalSource, MaybeCause};
use rustc_infer::traits::{
    self, FulfillmentError, FulfillmentErrorCode, MismatchedProjectionTypes, Obligation,
    ObligationCause, PredicateObligation, SelectionError, TraitEngine,
};
use rustc_middle::ty::error::{ExpectedFound, TypeError};
use rustc_middle::ty::{self, TyCtxt};

use super::eval_ctxt::GenerateProofTree;
use super::inspect::{ProofTreeInferCtxtExt, ProofTreeVisitor};
use super::{Certainty, InferCtxtEvalExt};

/// A trait engine using the new trait solver.
///
/// This is mostly identical to how `evaluate_all` works inside of the
/// solver, except that the requirements are slightly different.
///
/// Unlike `evaluate_all` it is possible to add new obligations later on
/// and we also have to track diagnostics information by using `Obligation`
/// instead of `Goal`.
///
/// It is also likely that we want to use slightly different datastructures
/// here as this will have to deal with far more root goals than `evaluate_all`.
pub struct FulfillmentCtxt<'tcx> {
    obligations: ObligationStorage<'tcx>,

    /// The snapshot in which this context was created. Using the context
    /// outside of this snapshot leads to subtle bugs if the snapshot
    /// gets rolled back. Because of this we explicitly check that we only
    /// use the context in exactly this snapshot.
    usable_in_snapshot: usize,
}

#[derive(Default)]
struct ObligationStorage<'tcx> {
    /// Obligations which resulted in an overflow in fulfillment itself.
    ///
    /// We cannot eagerly return these as error so we instead store them here
    /// to avoid recomputing them each time `select_where_possible` is called.
    /// This also allows us to return the correct `FulfillmentError` for them.
    overflowed: Vec<PredicateObligation<'tcx>>,
    pending: Vec<PredicateObligation<'tcx>>,
}

impl<'tcx> ObligationStorage<'tcx> {
    fn register(&mut self, obligation: PredicateObligation<'tcx>) {
        self.pending.push(obligation);
    }

    fn clone_pending(&self) -> Vec<PredicateObligation<'tcx>> {
        let mut obligations = self.pending.clone();
        obligations.extend(self.overflowed.iter().cloned());
        obligations
    }

    fn take_pending(&mut self) -> Vec<PredicateObligation<'tcx>> {
        let mut obligations = mem::take(&mut self.pending);
        obligations.append(&mut self.overflowed);
        obligations
    }

    fn unstalled_for_select(&mut self) -> impl Iterator<Item = PredicateObligation<'tcx>> {
        mem::take(&mut self.pending).into_iter()
    }

    fn on_fulfillment_overflow(&mut self, infcx: &InferCtxt<'tcx>) {
        infcx.probe(|_| {
            // IMPORTANT: we must not use solve any inference variables in the obligations
            // as this is all happening inside of a probe. We use a probe to make sure
            // we get all obligations involved in the overflow. We pretty much check: if
            // we were to do another step of `select_where_possible`, which goals would
            // change.
            self.overflowed.extend(self.pending.extract_if(|o| {
                let goal = o.clone().into();
                let result = infcx.evaluate_root_goal(goal, GenerateProofTree::Never).0;
                match result {
                    Ok((has_changed, _)) => has_changed,
                    _ => false,
                }
            }));
        })
    }
}

impl<'tcx> FulfillmentCtxt<'tcx> {
    pub fn new(infcx: &InferCtxt<'tcx>) -> FulfillmentCtxt<'tcx> {
        assert!(
            infcx.next_trait_solver(),
            "new trait solver fulfillment context created when \
            infcx is set up for old trait solver"
        );
        FulfillmentCtxt {
            obligations: Default::default(),
            usable_in_snapshot: infcx.num_open_snapshots(),
        }
    }

    fn inspect_evaluated_obligation(
        &self,
        infcx: &InferCtxt<'tcx>,
        obligation: &PredicateObligation<'tcx>,
        result: &Result<(bool, Certainty), NoSolution>,
    ) {
        if let Some(inspector) = infcx.obligation_inspector.get() {
            let result = match result {
                Ok((_, c)) => Ok(*c),
                Err(NoSolution) => Err(NoSolution),
            };
            (inspector)(infcx, &obligation, result);
        }
    }
}

impl<'tcx> TraitEngine<'tcx> for FulfillmentCtxt<'tcx> {
    #[instrument(level = "debug", skip(self, infcx))]
    fn register_predicate_obligation(
        &mut self,
        infcx: &InferCtxt<'tcx>,
        obligation: PredicateObligation<'tcx>,
    ) {
        assert_eq!(self.usable_in_snapshot, infcx.num_open_snapshots());
        self.obligations.register(obligation);
    }

    fn collect_remaining_errors(&mut self, infcx: &InferCtxt<'tcx>) -> Vec<FulfillmentError<'tcx>> {
        let mut errors: Vec<_> = self
            .obligations
            .pending
            .drain(..)
            .map(|obligation| fulfillment_error_for_stalled(infcx, obligation))
            .collect();

        errors.extend(self.obligations.overflowed.drain(..).map(|obligation| FulfillmentError {
            obligation: find_best_leaf_obligation(infcx, &obligation),
            code: FulfillmentErrorCode::Ambiguity { overflow: Some(true) },
            root_obligation: obligation,
        }));

        errors
    }

    fn select_where_possible(&mut self, infcx: &InferCtxt<'tcx>) -> Vec<FulfillmentError<'tcx>> {
        assert_eq!(self.usable_in_snapshot, infcx.num_open_snapshots());
        let mut errors = Vec::new();
        for i in 0.. {
            if !infcx.tcx.recursion_limit().value_within_limit(i) {
                self.obligations.on_fulfillment_overflow(infcx);
                // Only return true errors that we have accumulated while processing.
                return errors;
            }

            let mut has_changed = false;
            for obligation in self.obligations.unstalled_for_select() {
                let goal = obligation.clone().into();
                let result = infcx.evaluate_root_goal(goal, GenerateProofTree::IfEnabled).0;
                self.inspect_evaluated_obligation(infcx, &obligation, &result);
                let (changed, certainty) = match result {
                    Ok(result) => result,
                    Err(NoSolution) => {
                        errors.push(fulfillment_error_for_no_solution(infcx, obligation));
                        continue;
                    }
                };
                has_changed |= changed;
                match certainty {
                    Certainty::Yes => {}
                    Certainty::Maybe(_) => self.obligations.register(obligation),
                }
            }

            if !has_changed {
                break;
            }
        }

        errors
    }

    fn pending_obligations(&self) -> Vec<PredicateObligation<'tcx>> {
        self.obligations.clone_pending()
    }

    fn drain_unstalled_obligations(
        &mut self,
        _: &InferCtxt<'tcx>,
    ) -> Vec<PredicateObligation<'tcx>> {
        self.obligations.take_pending()
    }
}

fn fulfillment_error_for_no_solution<'tcx>(
    infcx: &InferCtxt<'tcx>,
    root_obligation: PredicateObligation<'tcx>,
) -> FulfillmentError<'tcx> {
    let obligation = find_best_leaf_obligation(infcx, &root_obligation);

    let code = match obligation.predicate.kind().skip_binder() {
        ty::PredicateKind::Clause(ty::ClauseKind::Projection(_)) => {
            FulfillmentErrorCode::ProjectionError(
                // FIXME: This could be a `Sorts` if the term is a type
                MismatchedProjectionTypes { err: TypeError::Mismatch },
            )
        }
        ty::PredicateKind::NormalizesTo(..) => {
            FulfillmentErrorCode::ProjectionError(MismatchedProjectionTypes {
                err: TypeError::Mismatch,
            })
        }
        ty::PredicateKind::AliasRelate(_, _, _) => {
            FulfillmentErrorCode::ProjectionError(MismatchedProjectionTypes {
                err: TypeError::Mismatch,
            })
        }
        ty::PredicateKind::Subtype(pred) => {
            let (a, b) = infcx.enter_forall_and_leak_universe(
                obligation.predicate.kind().rebind((pred.a, pred.b)),
            );
            let expected_found = ExpectedFound::new(true, a, b);
            FulfillmentErrorCode::SubtypeError(expected_found, TypeError::Sorts(expected_found))
        }
        ty::PredicateKind::Coerce(pred) => {
            let (a, b) = infcx.enter_forall_and_leak_universe(
                obligation.predicate.kind().rebind((pred.a, pred.b)),
            );
            let expected_found = ExpectedFound::new(false, a, b);
            FulfillmentErrorCode::SubtypeError(expected_found, TypeError::Sorts(expected_found))
        }
        ty::PredicateKind::Clause(_)
        | ty::PredicateKind::ObjectSafe(_)
        | ty::PredicateKind::Ambiguous => {
            FulfillmentErrorCode::SelectionError(SelectionError::Unimplemented)
        }
        ty::PredicateKind::ConstEquate(..) => {
            bug!("unexpected goal: {obligation:?}")
        }
    };

    FulfillmentError { obligation, code, root_obligation }
}

fn fulfillment_error_for_stalled<'tcx>(
    infcx: &InferCtxt<'tcx>,
    obligation: PredicateObligation<'tcx>,
) -> FulfillmentError<'tcx> {
    let code = infcx.probe(|_| {
        match infcx.evaluate_root_goal(obligation.clone().into(), GenerateProofTree::Never).0 {
            Ok((_, Certainty::Maybe(MaybeCause::Ambiguity))) => {
                FulfillmentErrorCode::Ambiguity { overflow: None }
            }
            Ok((_, Certainty::Maybe(MaybeCause::Overflow { suggest_increasing_limit }))) => {
                FulfillmentErrorCode::Ambiguity { overflow: Some(suggest_increasing_limit) }
            }
            Ok((_, Certainty::Yes)) => {
                bug!("did not expect successful goal when collecting ambiguity errors")
            }
            Err(_) => {
                bug!("did not expect selection error when collecting ambiguity errors")
            }
        }
    });

    FulfillmentError {
        obligation: find_best_leaf_obligation(infcx, &obligation),
        code,
        root_obligation: obligation,
    }
}

fn find_best_leaf_obligation<'tcx>(
    infcx: &InferCtxt<'tcx>,
    obligation: &PredicateObligation<'tcx>,
) -> PredicateObligation<'tcx> {
    let obligation = infcx.resolve_vars_if_possible(obligation.clone());
    infcx
        .visit_proof_tree(
            obligation.clone().into(),
            &mut BestObligation { obligation: obligation.clone() },
        )
        .break_value()
        .unwrap_or(obligation)
}

struct BestObligation<'tcx> {
    obligation: PredicateObligation<'tcx>,
}

impl<'tcx> BestObligation<'tcx> {
    fn with_derived_obligation(
        &mut self,
        derived_obligation: PredicateObligation<'tcx>,
        and_then: impl FnOnce(&mut Self) -> <Self as ProofTreeVisitor<'tcx>>::Result,
    ) -> <Self as ProofTreeVisitor<'tcx>>::Result {
        let old_obligation = std::mem::replace(&mut self.obligation, derived_obligation);
        let res = and_then(self);
        self.obligation = old_obligation;
        res
    }
}

impl<'tcx> ProofTreeVisitor<'tcx> for BestObligation<'tcx> {
    type Result = ControlFlow<PredicateObligation<'tcx>>;

    fn span(&self) -> rustc_span::Span {
        self.obligation.cause.span
    }

    fn visit_goal(&mut self, goal: &super::inspect::InspectGoal<'_, 'tcx>) -> Self::Result {
        // FIXME: Throw out candidates that have no failing WC and >0 failing misc goal.
        // This most likely means that the goal just didn't unify at all, e.g. a param
        // candidate with an alias in it.
        let candidates = goal.candidates();

        let [candidate] = candidates.as_slice() else {
            return ControlFlow::Break(self.obligation.clone());
        };

        // FIXME: Could we extract a trait ref from a projection here too?
        // FIXME: Also, what about considering >1 layer up the stack? May be necessary
        // for normalizes-to.
        let Some(parent_trait_pred) = goal.goal().predicate.to_opt_poly_trait_pred() else {
            return ControlFlow::Break(self.obligation.clone());
        };

        let tcx = goal.infcx().tcx;
        let mut impl_where_bound_count = 0;
        for nested_goal in candidate.instantiate_nested_goals(self.span()) {
            let obligation;
            match nested_goal.source() {
                GoalSource::Misc => {
                    continue;
                }
                GoalSource::ImplWhereBound => {
                    obligation = Obligation {
                        cause: derive_cause(
                            tcx,
                            candidate.kind(),
                            self.obligation.cause.clone(),
                            impl_where_bound_count,
                            parent_trait_pred,
                        ),
                        param_env: nested_goal.goal().param_env,
                        predicate: nested_goal.goal().predicate,
                        recursion_depth: self.obligation.recursion_depth + 1,
                    };
                    impl_where_bound_count += 1;
                }
                GoalSource::InstantiateHigherRanked => {
                    obligation = self.obligation.clone();
                }
            }

            // Skip nested goals that hold.
            //FIXME: We should change the max allowed certainty based on if we're
            // visiting an ambiguity or error obligation.
            if matches!(nested_goal.result(), Ok(Certainty::Yes)) {
                continue;
            }

            self.with_derived_obligation(obligation, |this| nested_goal.visit_with(this))?;
        }

        ControlFlow::Break(self.obligation.clone())
    }
}

fn derive_cause<'tcx>(
    tcx: TyCtxt<'tcx>,
    candidate_kind: ProbeKind<'tcx>,
    mut cause: ObligationCause<'tcx>,
    idx: usize,
    parent_trait_pred: ty::PolyTraitPredicate<'tcx>,
) -> ObligationCause<'tcx> {
    match candidate_kind {
        ProbeKind::TraitCandidate { source: CandidateSource::Impl(impl_def_id), result: _ } => {
            if let Some((_, span)) =
                tcx.predicates_of(impl_def_id).instantiate_identity(tcx).iter().nth(idx)
            {
                cause = cause.derived_cause(parent_trait_pred, |derived| {
                    traits::ImplDerivedObligation(Box::new(traits::ImplDerivedObligationCause {
                        derived,
                        impl_or_alias_def_id: impl_def_id,
                        impl_def_predicate_index: Some(idx),
                        span,
                    }))
                })
            }
        }
        ProbeKind::TraitCandidate { source: CandidateSource::BuiltinImpl(..), result: _ } => {
            cause = cause.derived_cause(parent_trait_pred, traits::BuiltinDerivedObligation);
        }
        _ => {}
    };
    cause
}
