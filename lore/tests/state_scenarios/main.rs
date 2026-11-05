// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The deterministic scenarios of `State`'s node operations that run the C API's revision-tree
//! verbs, on a handle loaded over in-memory stores, through the harness lore-revision's scenarios
//! and its stress example share (`lore-revision/examples/state_stress/harness/`). They build
//! lore-revision with its scheduling points, so a plain `cargo test` leaves them out to test the
//! build that ships, and they run by hand: `cargo test --release -p lore --features state-stress
//! --test state_scenarios`.

mod capi;
mod h18;
mod h8;
#[allow(
    dead_code,
    unused_imports,
    reason = "shared with lore-revision's scenarios and its stress example, which use what these \
              scenarios do not"
)]
#[path = "../../../lore-revision/examples/state_stress/harness/mod.rs"]
mod harness;
mod hooks;
mod park;
mod slow_down;

use std::future::Future;
use std::sync::Arc;

use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;

use crate::harness::catalogue::modelled::Modelling;
use crate::harness::catalogue::modelled::modelled;
use crate::harness::invariants::Report;
use crate::harness::invariants::Rules;
use crate::harness::model::Modelled;
use crate::harness::scheduler::Covered;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Scenario;

/// `scenario` held to the model as `modelling` says and explored as `exploration` says: every
/// interleaving, or, with sleep sets off, every one within the preemption bound. One the wall
/// budget stopped fails, as a hang.
pub(crate) fn explored<C: Modelled + Send + Sync + 'static>(
    scenario: Scenario<C>,
    modelling: Modelling,
    exploration: Exploration,
) -> Covered {
    let name = scenario.name;
    let covered = modelled(scenario, modelling).explore(exploration);
    let bounded = !exploration.sleep_sets && covered.bound == Some(exploration.preemptions);
    assert!(
        covered.complete || (bounded && !covered.timed_out),
        "'{name}' explored {} runs without finishing",
        covered.runs
    );
    covered
}

/// `state`'s invariants under `rules`, or the test failed with the violations.
pub(crate) async fn holding(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    rules: Rules,
) -> Report {
    crate::harness::invariants::check(state, repository, rules)
        .await
        .unwrap_or_else(|violations| panic!("invariants: {violations:#?}"))
}

/// `future` in the execution context the State's work reads.
pub(crate) fn in_context<F: Future>(future: F) -> impl Future<Output = F::Output> {
    lore_base::runtime::LORE_CONTEXT.scope(crate::harness::fixtures::execution(), future)
}
