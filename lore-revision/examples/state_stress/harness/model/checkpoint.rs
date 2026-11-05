// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The checkpoint, at a quiescent point a run reaches: no operation in flight, every future it
//! spawned ended. Over every State of the run's context:
//! 1. the model held against the State ([`StateModel::compare`]);
//! 2. the orphans ([`StateModel::orphans`]): a served State's are violations, an unserved State's
//!    counted;
//! 3. the invariants ([`invariants::check`]) under the rules the model gives ([`rules`]), on every
//!    State or, as [`Checkpoint::every_state`] says, on each a record changed since its model
//!    started (`StateModel::changed`), the others held only to what a save, which writes no
//!    node's words, may leave wrong ([`invariants::after_save`]); and, on a served State walked
//!    whose context names the committed State it settles against (`Modelled::committed`), each
//!    clean directory's children against its committed directory's, outside the swaps' changed
//!    paths, where the model knows them ([`invariants::committed_children`], C11c);
//! 4. the round trip, on a State the run wrote that is unserved and takes no fresh blocks: saved,
//!    the revision it answers loaded into a new State and compared node by node ([`by_path`])
//!    with the walk of part 3, as a save writes no node's words (`State::serialize`).
//!
//! The invariants' walk loads blocks and name tables, and the save writes: both run under a
//! recording scheduler of their own ([`recorded`]), their records replayed, so the model follows
//! them, and the model is held against the State again. Where operations follow, the checkpoint
//! then evicts every clean block ([`Checkpoint::evict`]), and the model goes on from the log; a
//! State taking fresh blocks keeps them, as their registration holds them.

use std::collections::BTreeSet;
use std::sync::Arc;

use lore_base::types::Hash;
use lore_revision::node::Node;
use lore_revision::node::NodeID;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::state::State;
use lore_revision::state::StateError;

use super::Model;
use super::Modelled;
use super::StateModel;
use crate::harness::fixtures::InMemory;
use crate::harness::invariants;
use crate::harness::invariants::Report;
use crate::harness::invariants::Rules;
use crate::harness::log::recorded;

/// What a checkpoint found.
#[derive(Clone, Debug, Default)]
pub struct Checked {
    /// Each difference, violation and served orphan, named with its State.
    pub violations: Vec<String>,
    /// The unserved States' orphans.
    pub orphans: usize,
    /// The served States' orphans in a rename's window ([`super::Orphans`]).
    pub window: usize,
    /// The States saved and loaded again for the round trip.
    pub saved: usize,
}

/// The invariants that hold on a State as its model stands: on a served State dirty closure always
/// and names unique by hash; on a State never served no extension words; on an unserved one
/// closure unless excused whole, or left to the model ([`closure_left_to_model`]).
pub fn rules(state: &StateModel, dropped: bool) -> Rules {
    Rules {
        dirty_closure: !excused_whole(state, dropped) && !closure_left_to_model(state, dropped),
        unique_names: state.served,
        no_ext_flags: !state.served,
        no_builds: false,
    }
}

/// Whether closure on `state` is excused whole: unserved, where an operation of the run stopped
/// part way outside every call, `dropped` there or by a fault the context tells, or the State held
/// an unfinished change as the run began, which no record explains (C12). One a call of the run
/// left is narrowed to the breaches its records formed ([`StateModel::excused`]).
fn excused_whole(state: &StateModel, dropped: bool) -> bool {
    !state.served && (dropped || state.unfinished_at_start)
}

/// Whether the model checks closure on unserved `state` itself, all but the breaches a dropped or
/// failed call's records formed ([`StateModel::excused`]), where it holds some that stand, which
/// the invariants would find: a call's records tell what it left.
pub fn closure_left_to_model(state: &StateModel, dropped: bool) -> bool {
    !state.served && !excused_whole(state, dropped) && !state.excused.is_empty()
}

/// Where two walks of a State differ, node by node, as a load keeps the node ids: a node one
/// reaches and the other not, at another path, or whose flags, extension flags or content differ,
/// a link's `child` with them.
pub fn by_path(live: &Report, loaded: &Report) -> Vec<String> {
    let mut differences = Vec::new();
    for (id, path) in &live.paths {
        let (node, held) = (&live.nodes[id], loaded.nodes.get(id));
        match (loaded.paths.get(id), held) {
            (Some(at), Some(held)) => {
                if at != path {
                    differences.push(format!("{id} is at '{path}' live, at '{at}' loaded"));
                } else if !same_saved(node, held) {
                    differences.push(format!("'{path}' is {node:?} live, {held:?} loaded"));
                }
            }
            _ => differences.push(format!("'{path}' is live and not in the load")),
        }
    }
    for (id, path) in &loaded.paths {
        if !live.paths.contains_key(id) {
            differences.push(format!("'{path}' is in the load and not live"));
        }
    }
    differences
}

/// Whether a save and a load keep what a node holds of its own.
fn same_saved(live: &Node, loaded: &Node) -> bool {
    live.flags == loaded.flags
        && live.ext_flags == loaded.ext_flags
        && live.address == loaded.address
        && live.size == loaded.size
        && live.mode == loaded.mode
        && (!live.is_link() || live.child == loaded.child)
}

/// How a checkpoint runs.
#[derive(Clone, Copy, Debug)]
pub struct Checkpoint {
    /// Whether a task of the run was dropped where it parked outside every call, which may excuse
    /// closure: one dropped in a call excuses only the breaches that call's records formed.
    pub dropped: bool,
    /// Whether each State the run wrote, unserved and taking no fresh blocks, is saved and its
    /// revision loaded again.
    pub round_trip: bool,
    /// Whether the clean blocks are evicted after, for the operations that follow: a run's end
    /// needs none, and a scenario's own check then finds the blocks the run left.
    pub evict: bool,
    /// Whether every State is walked for the invariants, or only those a record changed since
    /// their model started: a State none changed holds what an earlier checkpoint walked, but
    /// for its registrations and unused chain, which a save changes recording no word.
    pub every_state: bool,
}

impl Checkpoint {
    /// At a run's end, which no operation follows: everything but the eviction.
    pub fn ending(dropped: bool) -> Self {
        Self {
            dropped,
            round_trip: true,
            evict: false,
            every_state: true,
        }
    }
}

impl Default for Checkpoint {
    /// Between operations: every part, the eviction too.
    fn default() -> Self {
        Self {
            dropped: false,
            round_trip: true,
            evict: true,
            every_state: true,
        }
    }
}

/// What a State's walk under recording found, and its save where it took the round trip.
struct Walked {
    invariants: Result<Report, Vec<String>>,
    /// The save's signature, and what the save left for the next one that does not hold.
    saved: Option<(Result<Hash, StateError>, Vec<String>)>,
}

impl Model {
    /// The checkpoint of every State of `context`, which stands at a quiescent point, its log so
    /// far replayed, run as `checkpoint` says; a task dropped outside every call excuses as a fault
    /// the context tells outside every call does, either of which may have stopped an operation
    /// part way where no call's records tell what it left.
    pub async fn checkpoint(&mut self, context: &impl Modelled, checkpoint: Checkpoint) -> Checked {
        let dropped = checkpoint.dropped || context.faulted();
        context.mend();
        let states = context.states();
        let committed = context.committed();
        let mut checked = Checked::default();
        // Each walked State's rules and whether it takes the round trip, read from its model
        // before the walks change anything; `None` for a State with no model or none walked.
        let mut plans: Vec<Option<(Rules, bool)>> = Vec::with_capacity(states.len());
        // Each walked served State's committed State, by its place in `committed`, and the nodes
        // along its swaps' changed paths, where its model knows them.
        let mut settled: Vec<Option<(usize, BTreeSet<NodeID>)>> = Vec::with_capacity(states.len());
        for (state, _) in &states {
            let id = state.hooks().id();
            settled.push(None);
            let Some(modelled) = self.state(id) else {
                checked.violations.push(format!("State {id} has no model"));
                plans.push(None);
                continue;
            };
            checked.violations.extend(modelled.compare(state));
            let orphans = modelled.orphans();
            checked.violations.extend(orphans.violations);
            if closure_left_to_model(modelled, dropped) {
                checked.violations.extend(
                    modelled
                        .closure_breaches()
                        .into_iter()
                        .map(|breach| format!("State {id}: {breach}")),
                );
            }
            checked.orphans += orphans.counted.len();
            checked.window += orphans.window.len();
            if !modelled.changed && !checkpoint.every_state {
                checked.violations.extend(
                    invariants::after_save(state)
                        .into_iter()
                        .map(|found| format!("State {id}: {found}")),
                );
                plans.push(None);
                continue;
            }
            let round_trip = checkpoint.round_trip
                && modelled.written
                && !state.is_served()
                && !state.attach_slot_resident();
            plans.push(Some((rules(modelled, dropped), round_trip)));
            if modelled.served && !modelled.swaps_unknown {
                let against = committed.iter().position(|entry| entry.served == id);
                *settled.last_mut().expect("pushed above") =
                    against.map(|at| (at, modelled.swapped.clone()));
            }
        }
        let ((walks, stored), records) = recorded(async {
            let mut walks = Vec::with_capacity(states.len());
            for ((state, repository), plan) in states.iter().zip(&plans) {
                let Some((rules, round_trip)) = *plan else {
                    walks.push(None);
                    continue;
                };
                let invariants = invariants::check(state, repository, rules).await;
                let saved = if round_trip {
                    let token = RepositoryWriteToken::in_memory(&InMemory);
                    let signature = state.serialize(repository.clone(), &token).await;
                    Some((signature, invariants::after_save(state)))
                } else {
                    None
                };
                walks.push(Some(Walked { invariants, saved }));
            }
            // Each committed State a served one settles against, walked once.
            let mut stored: Vec<Option<Result<Report, Vec<String>>>> = vec![None; committed.len()];
            for &(at, _) in settled.iter().flatten() {
                if stored[at].is_none() {
                    let entry = &committed[at];
                    let walk = invariants::check(&entry.state, &entry.repository, Rules::default());
                    stored[at] = Some(walk.await);
                }
            }
            (walks, stored)
        })
        .await;
        let before = self.violations.len();
        self.replay(&records);
        checked.violations.extend(
            self.violations[before..]
                .iter()
                .map(|violation| format!("the checkpoint's own work: {violation}")),
        );
        // The model against the State again, as the save's writes and the walks' loads left it: what
        // differs anew.
        for (state, _) in &states {
            if let Some(modelled) = self.state(state.hooks().id()) {
                let anew: Vec<String> = modelled
                    .compare(state)
                    .into_iter()
                    .filter(|difference| !checked.violations.contains(difference))
                    .map(|difference| format!("after the checkpoint's own work: {difference}"))
                    .collect();
                checked.violations.extend(anew);
            }
        }
        for (((state, repository), walked), settled) in states.iter().zip(walks).zip(&settled) {
            let id = state.hooks().id();
            let Some(Walked { invariants, saved }) = walked else {
                continue;
            };
            let walk = match invariants {
                Ok(walk) => Some(walk),
                Err(found) => {
                    checked.violations.extend(
                        found
                            .into_iter()
                            .map(|found| format!("State {id}: {found}")),
                    );
                    None
                }
            };
            if let (Some(walk), Some((at, swapped))) = (&walk, settled) {
                let found = match &stored[*at] {
                    Some(Ok(against)) => invariants::committed_children(
                        walk,
                        (against, &committed[*at].nodes),
                        swapped,
                    ),
                    Some(Err(found)) => found
                        .iter()
                        .map(|found| format!("its committed State: {found}"))
                        .collect(),
                    None => Vec::new(),
                };
                checked.violations.extend(
                    found
                        .into_iter()
                        .map(|found| format!("State {id}: {found}")),
                );
            }
            if let Some((signature, left)) = saved {
                checked.violations.extend(
                    left.into_iter()
                        .map(|found| format!("State {id} after its save: {found}")),
                );
                if let Some(walk) = walk {
                    checked.saved += 1;
                    checked
                        .violations
                        .extend(round_trip(id, repository, walk, signature).await);
                }
            }
        }
        for (state, _) in &states {
            if let Some(modelled) = self
                .states
                .get_mut(&state.hooks().id())
                .filter(|_| checkpoint.round_trip)
            {
                modelled.written = false;
            }
            if checkpoint.evict {
                state.evict_blocks_for_tests();
            }
        }
        checked
    }
}

/// Where the State `id`, walked as `walk` before its save, which writes no node's words, and the
/// load of the revision the save answered (`signature`) from `repository` differ.
pub async fn round_trip(
    id: u64,
    repository: &Arc<RepositoryContext>,
    walk: Report,
    signature: Result<Hash, StateError>,
) -> Vec<String> {
    let signature = match signature {
        Ok(signature) => signature,
        Err(error) => return vec![format!("State {id} does not save: {error}")],
    };
    let loaded = match State::deserialize(repository.clone(), signature).await {
        Ok(loaded) => loaded,
        Err(error) => return vec![format!("State {id}'s save does not load: {error}")],
    };
    match invariants::check(&loaded, repository, Rules::default()).await {
        Ok(load) => by_path(&walk, &load)
            .into_iter()
            .map(|difference| format!("State {id}'s round trip: {difference}"))
            .collect(),
        Err(found) => found
            .into_iter()
            .map(|found| format!("State {id}'s load: {found}"))
            .collect(),
    }
}
