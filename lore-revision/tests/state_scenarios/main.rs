// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The deterministic scenarios of `State`'s node operations: each explores every interleaving of
//! its operations' steps, or every one within a preemption bound, and the thread mixes run on many
//! workers, all through the harness the stress example shares (`examples/state_stress/harness/`).
//! They build lore-revision with its scheduling points, so a plain `cargo test` leaves them out to
//! test the build that ships, and they run by hand: `cargo test --release -p lore-revision
//! --features state-stress --test state_scenarios`. The scenarios that run the C API's verbs are
//! the `lore` package's own.

#![allow(
    clippy::large_futures,
    reason = "lore-revision's future-size threshold is for the futures it ships; a scenario's are a \
              test's own, awaited once"
)]

mod calls;
mod catalogue;
#[cfg(target_os = "linux")]
mod fuse;
#[allow(
    dead_code,
    unused_imports,
    reason = "shared with the stress example, which uses what these scenarios do not"
)]
#[path = "../../examples/state_stress/harness/mod.rs"]
mod harness;
mod hazards;
#[allow(
    dead_code,
    unused_imports,
    reason = "shared with the unit tests, which use what these scenarios do not"
)]
#[path = "../helper.rs"]
mod helper;
mod hooks;
mod invariants;
mod log;
mod model;
mod probes;
mod retries;
mod scheduler;
mod served;
mod store;
mod sweep;
mod threads;
mod walk_errors;

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

/// A runtime on the calling thread, which a stall in a window blocks whole.
pub(crate) fn current_thread() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
}
