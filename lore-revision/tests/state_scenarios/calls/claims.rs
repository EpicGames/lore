// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The stage's claims of a directory's entries in one call (`STAGE_CLAIM`), recorded, each run
//! replayed through the model, which holds each read by the call's own listing's mark within the
//! read's interval, by the mark as the listing's head read made it and the prepends the call
//! counted by then.
//! - Alone: a search since the mark for what the listing lacks; a node created since the listing
//!   found by it, and one renamed since found through the whole chain after a stale search, the
//!   mark's reattach read for each node found; two missing names sharing a hash, the second's
//!   search finding the first's add.
//! - Within the contract: claims in sibling directories; a parent's claims of a listed directory
//!   and of a missing one, each child's claims inline after its claim or add, as the stage
//!   descends with no permit free, beside a sibling's; a respelling of a claimed node in the
//!   window after its claim, beside a sibling's claims.
//! - Beyond it: the claims beside a detach of their directory, a rename of a listed child into an
//!   entry's name, a move out of the child behind the listing's block crossing, and a create of
//!   an entry's name; a rename and a create landing inside a search, at its own block crossing,
//!   which only the read's own interval admits.

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeID;
use lore_revision::state::AddedSince;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Record;
use lore_storage::hash::hash_string;

use super::Explore;
use super::Run;
use super::across;
use super::call_of;
use super::explore;
use super::explore_with;
use super::tree;
use crate::harness::catalogue;
use crate::harness::catalogue::call::Answer;
use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Lookup;
use crate::harness::catalogue::call::Outcome;
use crate::harness::catalogue::call::Refusal;
use crate::harness::catalogue::ops;
use crate::harness::fixtures::Tree;
use crate::harness::invariants::Rules;
use crate::harness::scheduler::Op;
use crate::harness::scheduler::Setup;

/// The stage claims `run` made.
fn stage_claims<'a>(run: &'a Run<'_>) -> impl Iterator<Item = &'a Call> {
    (run.calls.iter()).filter(|call| std::ptr::eq(call.kind, &catalogue::STAGE_CLAIM))
}

/// The one stage claim `run` made.
fn the_one_claim<'a>(run: &'a Run<'_>) -> &'a Call {
    let claims: Vec<&Call> = stage_claims(run).collect();
    let [claim] = claims.as_slice() else {
        panic!("one stage claim: {:#?}", run.calls);
    };
    claim
}

/// The one stage claim `run` made, which answered done.
fn the_done_claim<'a>(run: &'a Run<'_>) -> &'a Call {
    let claim = the_one_claim(run);
    assert_eq!(claim.outcome, Outcome::Done(Answer::None));
    claim
}

/// The nodes the records `claim` holds in `run` published, in order.
fn published(run: &Run<'_>, claim: &Call) -> Vec<NodeID> {
    (run.records.iter())
        .filter(|record| claim.holds(record, &run.spawns))
        .filter_map(|record| match record.effect {
            Effect::Publish { node, .. } => Some(node),
            _ => None,
        })
        .collect()
}

/// The node a call of `kind` in `run` answered.
fn answered_node(run: &Run<'_>, kind: &catalogue::Kind) -> NodeID {
    (run.calls.iter())
        .find_map(|call| match call.outcome {
            Outcome::Done(Answer::Node(node)) if std::ptr::eq(call.kind, kind) => Some(node),
            _ => None,
        })
        .unwrap_or_else(|| panic!("a call of '{}' answered a node", kind.name))
}

/// Claims of a listed entry and of two missing ones: the listed one claimed from the listing
/// alone, each missing one searched for since the mark, absent, the second with the first's add
/// counted in the mark.
#[test]
fn a_stage_claim_searches_since_its_mark_for_what_its_listing_lacks() {
    explore(
        tree(&["d/f", "d/g"]),
        vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let entries = [("f", true), ("n", true), ("m", false)];
                (ops::stage_claim(&tree, tree.id("d"), &entries).await).expect("the claims run");
            })
        })],
        vec![],
        |run| {
            let searches: Vec<(AddedSince, u32)> = (the_done_claim(run).lookups.iter())
                .map(|lookup| match *lookup {
                    Lookup::Since {
                        searched, prepends, ..
                    } => (searched, prepends),
                    other => panic!("a search since the mark: {other:?}"),
                })
                .collect();
            assert_eq!(searches, [(AddedSince::Absent, 0), (AddedSince::Absent, 1)]);
        },
    );
}

/// A claim of a name created in its window, after its listing: its search since the mark finds
/// the node, and its read of the mark's reattach answers none.
#[test]
fn a_stage_claim_finds_a_node_created_since_its_listing() {
    explore(
        tree(&["d/f"]),
        vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let d = tree.id("d");
                let created = async {
                    assert!(ops::checked_create(&tree, d, "n").await.is_some());
                };
                (ops::stage_claim_waiting(&tree, d, &[("n", true)], created).await)
                    .expect("the claims run");
            })
        })],
        vec![],
        |run| {
            let n = answered_node(run, &catalogue::CHECKED_CREATE);
            let lookups = &the_done_claim(run).lookups;
            assert!(
                matches!(
                    lookups.as_slice(),
                    [
                        Lookup::Since {
                            searched: AddedSince::Found(found),
                            prepends: 0,
                            ..
                        },
                        Lookup::Reattached {
                            reattached: false,
                            ..
                        },
                    ] if *found == n
                ),
                "{lookups:?}"
            );
        },
    );
}

/// A claim of a name its listing holds under another, the node renamed in place in its window:
/// its search since the mark stale, its find through the chain finding the node, and its read of
/// the mark's reattach answering one. The rename changes the hash in place, which no caller does
/// beside a stage, as the respellings keep it: it covers the mark, which counts such a rename as a
/// reattach.
#[test]
fn a_stage_claim_finds_a_node_renamed_since_its_listing_through_the_chain() {
    explore(
        tree(&["d/f", "d/g"]),
        vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let (d, g) = (tree.id("d"), tree.id("d/g"));
                let renamed = ops::rename_then_mark(&tree, g, "n");
                (ops::stage_claim_waiting(&tree, d, &[("n", true)], renamed).await)
                    .expect("the claims run");
            })
        })],
        vec![],
        |run| {
            let g = run.tree.id("d/g");
            let lookups = &the_done_claim(run).lookups;
            assert!(
                matches!(
                    lookups.as_slice(),
                    [
                        Lookup::Since {
                            searched: AddedSince::Stale,
                            ..
                        },
                        Lookup::Find {
                            found: Some(found),
                            ..
                        },
                        Lookup::Reattached {
                            reattached: true,
                            ..
                        },
                    ] if *found == g
                ),
                "{lookups:?}"
            );
        },
    );
}

/// Claims of two missing names sharing a hash, differing in case, as the file system can list
/// beside each other: the first's search answers none and the claims add the name; the second's,
/// that add counted in the mark, finds it, the mark's reattach read answering none, so both answer
/// the node added. The stage's default case mode stops there with an error; the op goes on.
#[test]
fn stage_claims_of_two_missing_names_sharing_a_hash_answer_the_first_ones_add_twice() {
    explore(
        tree(&["d/f"]),
        vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let entries = [("n", true), ("N", true)];
                let claimed = ops::stage_claim(&tree, tree.id("d"), &entries).await;
                let claimed = claimed.expect("the claims run");
                assert_eq!(claimed.nodes[0], claimed.nodes[1], "both answer the add");
                assert!(
                    !claimed.listed.contains(&claimed.nodes[0]),
                    "the claims added it"
                );
            })
        })],
        vec![],
        |run| {
            let claim = the_done_claim(run);
            let [added] = published(run, claim)[..] else {
                panic!("one add: {:?}", published(run, claim));
            };
            let lookups = &claim.lookups;
            assert!(
                matches!(
                    lookups.as_slice(),
                    [
                        Lookup::Since {
                            searched: AddedSince::Absent,
                            prepends: 0,
                            ..
                        },
                        Lookup::Since {
                            searched: AddedSince::Found(found),
                            prepends: 1,
                            ..
                        },
                        Lookup::Reattached {
                            reattached: false,
                            prepends: 1,
                            ..
                        },
                    ] if *found == added
                ),
                "{lookups:?}"
            );
        },
    );
}

/// Beyond the contract, claims of two missing entries beside an unserved detach of their
/// directory: refused, adding nothing, where the detach came before the first add; done where it
/// came after an add of the claims' own, the add standing, the directory noted taken where a later
/// add found it gone, or the node added noted taken where its mark found it gone; done with both
/// added and nothing taken where it came last. Each run replays clean with its calls.
#[test]
fn a_stage_claim_beside_a_detach_of_its_directory() {
    let counts: Rc<[Cell<usize>; 4]> = Rc::default();
    let counted = counts.clone();
    explore(
        tree(&["d/f"]),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let entries = [("m", true), ("o", true)];
                    let _ = ops::stage_claim(&tree, tree.id("d"), &entries).await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move { assert!(ops::unserved_detach(&tree, tree.id("d")).await) })
            }),
        ],
        vec![],
        move |run| {
            let (claim, d) = (the_one_claim(run), run.tree.id("d"));
            let added = published(run, claim);
            let taken: Vec<NodeID> = claim.taken.iter().map(|&(node, _)| node).collect();
            let class = match (&claim.outcome, added.len(), taken.as_slice()) {
                (Outcome::Refused(Refusal::NotFound), 0, []) => 0,
                (Outcome::Done(Answer::None), 1, &[node]) if node == d => 1,
                (Outcome::Done(Answer::None), 1 | 2, &[node]) if added.last() == Some(&node) => 2,
                (Outcome::Done(Answer::None), 2, []) => 3,
                (outcome, _, _) => {
                    panic!("the claims answered {outcome:?}, adding {added:?}, taking {taken:?}")
                }
            };
            counted[class].set(counted[class].get() + 1);
        },
    );
    let counts = counts.each_ref().map(|count| count.get());
    assert!(
        counts.iter().all(|&count| count > 0),
        "runs refused, the directory taken, the node taken, both added: {counts:?}"
    );
}

/// What a stage claim that answered done read: its directory, how many entries it searched for
/// since its mark, and how many children its listing yielded.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Reads {
    dir: NodeID,
    searched: usize,
    listed: usize,
}

impl Reads {
    fn of(call: &Call) -> Self {
        assert_eq!(call.outcome, Outcome::Done(Answer::None), "{call:#?}");
        Self {
            dir: call.target.expect("a claim's directory"),
            searched: (call.lookups.iter())
                .filter(|lookup| matches!(lookup, Lookup::Since { .. }))
                .count(),
            listed: call.walks.iter().map(|walk| walk.yielded.len()).sum(),
        }
    }
}

/// That `claimed` answered `listed` for its first entry and added its second.
fn claimed_then_added(claimed: &ops::Claimed, listed: NodeID) {
    assert_eq!(claimed.nodes.len(), 2, "{claimed:?}");
    assert_eq!(claimed.nodes[0], listed, "the listed entry claimed");
    assert!(
        !claimed.listed.contains(&claimed.nodes[1]),
        "the missing entry added: {claimed:?}"
    );
}

/// Within the contract, the stage's claims in sibling directories, each a task of its own: each
/// claims its listed entry and adds its missing one, every run replaying clean and leaving dirty
/// closure and unique names. Every schedule: fourteen preemptions a run reach them all.
#[test]
fn stage_claims_in_sibling_directories() {
    explore_with(
        Explore {
            rules: Some(Rules::CLOSED),
            preemptions: Some(14),
            ..Explore::default()
        },
        tree(&["d/f", "e/g"]),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let entries = [("f", true), ("n", true)];
                    let claimed = ops::stage_claim(&tree, tree.id("d"), &entries).await;
                    claimed_then_added(&claimed.expect("d's claims"), tree.id("d/f"));
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let entries = [("g", true), ("m", false)];
                    let claimed = ops::stage_claim(&tree, tree.id("e"), &entries).await;
                    claimed_then_added(&claimed.expect("e's claims"), tree.id("e/g"));
                })
            }),
        ],
        vec![],
        |run| {
            let (d, e) = (run.tree.id("d"), run.tree.id("e"));
            let mut reads: Vec<Reads> = stage_claims(run).map(Reads::of).collect();
            reads.sort_unstable();
            let mut expected = [d, e].map(|dir| Reads {
                dir,
                searched: 1,
                listed: 1,
            });
            expected.sort_unstable();
            assert_eq!(
                reads, expected,
                "each claim searched for its missing entry alone"
            );
        },
    );
}

/// Within the contract, the stage's claims of `F`, which `d/f` carries in another case, and of a
/// missing `n`, the claimed node respelled in the window after its claim, as `lore stage --case
/// rename` respells there, beside a sibling's claims, each a task of its own. The respelling
/// renames `d/f` in every run, and each claim claims its listed entry and adds its missing one.
/// Every schedule, fourteen preemptions a run reaching them all: some land the sibling's claims
/// inside the respelling; every run replays clean, leaving dirty closure and unique names.
#[test]
fn a_stage_respelling_in_its_claims_window_beside_a_siblings_claims() {
    let inside: Rc<Cell<usize>> = Rc::default();
    let counted = inside.clone();
    explore_with(
        Explore {
            rules: Some(Rules::CLOSED),
            preemptions: Some(14),
            ..Explore::default()
        },
        tree(&["d/f", "e/g"]),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let tree = &tree;
                    let respell = |index: usize, node: NodeID| async move {
                        if index == 0 {
                            ops::stage_respell(tree, node, "F").await;
                        }
                    };
                    let entries = [("F", true), ("n", true)];
                    let claimed =
                        ops::stage_claim_descending(tree, tree.id("d"), &entries, respell);
                    claimed_then_added(&claimed.await.expect("d's claims"), tree.id("d/f"));
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let entries = [("g", true), ("m", false)];
                    let claimed = ops::stage_claim(&tree, tree.id("e"), &entries).await;
                    claimed_then_added(&claimed.expect("e's claims"), tree.id("e/g"));
                })
            }),
        ],
        vec![],
        move |run| {
            let (f, e) = (run.tree.id("d/f"), run.tree.id("e"));
            let respelling = call_of(run, &catalogue::STAGE_RESPELL);
            assert_eq!(respelling.outcome, Outcome::Done(Answer::None));
            assert_eq!(respelling.target, Some(f));
            let held = |record: &&Record| respelling.holds(record, &run.spawns);
            assert!(
                (run.records.iter()).filter(held).any(
                    |record| matches!(record.effect, Effect::Rename { node, .. } if node == f)
                ),
                "the respelling renamed no 'd/f'"
            );
            let siblings = stage_claims(run).find(|claim| claim.target == Some(e));
            let sibling = siblings.expect("the sibling's claims");
            let within = |record: &Record| {
                sibling.holds(record, &run.spawns)
                    && (respelling.begin..respelling.end).contains(&record.seq)
            };
            if run.records.iter().any(within) {
                counted.set(counted.get() + 1);
            }
        },
    );
    assert!(
        inside.get() > 0,
        "no sibling's claim landed inside the respelling"
    );
}

/// Within the contract, a parent's claims of a listed directory and of a missing one, each child's
/// claims inline in the window after its claim or add, as the stage descends with no permit free,
/// the added directory staged and dirty added by then, then the parent's next entry by its own
/// mark, beside a sibling's claims: the listed child lists its file and adds its missing one, the
/// added child lists none and adds both its entries, the parent's mark counting only its own adds;
/// every run within four preemptions replays clean, leaving dirty closure and unique names.
#[test]
fn a_parents_add_then_its_childs_claims_beside_a_siblings() {
    explore_with(
        Explore {
            rules: Some(Rules::CLOSED),
            preemptions: Some(4),
            ..Explore::default()
        },
        tree(&["d/c/k", "d/f", "e/g"]),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let tree = &tree;
                    let descend = |index: usize, node: NodeID| async move {
                        if index == 0 {
                            let entries = [("k", true), ("v", true)];
                            (ops::stage_claim(tree, node, &entries).await).expect("c's claims");
                        }
                        if index == 1 {
                            let block = (tree.state.resident_block(NodeBlock::index(node)))
                                .expect("s's block is in memory");
                            let added = block.node(Node::index(node));
                            assert!(
                                added.is_staged_add() && added.is_dirty_add(),
                                "the stage descends into a directory staged and dirty added"
                            );
                            let entries = [("t", true), ("u", true)];
                            (ops::stage_claim(tree, node, &entries).await).expect("s's claims");
                        }
                    };
                    let entries = [("c", false), ("s", false), ("o", true)];
                    let claimed =
                        ops::stage_claim_descending(tree, tree.id("d"), &entries, descend);
                    claimed.await.expect("d's claims");
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let entries = [("m", true)];
                    (ops::stage_claim(&tree, tree.id("e"), &entries).await).expect("e's claims");
                })
            }),
        ],
        vec![],
        |run| {
            let (d, e, c) = (run.tree.id("d"), run.tree.id("e"), run.tree.id("d/c"));
            let reads: Vec<Reads> = stage_claims(run).map(Reads::of).collect();
            assert_eq!(
                reads.len(),
                4,
                "the parent's, the children's and the sibling's: {reads:?}"
            );
            let child = |dir: &dyn Fn(NodeID) -> bool| {
                let reads = (reads.iter()).find(|reads| dir(reads.dir));
                let reads = reads.expect("the child's claims");
                (reads.searched, reads.listed)
            };
            assert_eq!(
                child(&|dir| dir == c),
                (1, 1),
                "the listed child lists its file, searches for the other"
            );
            assert_eq!(
                child(&|dir| ![d, e, c].contains(&dir)),
                (2, 0),
                "the added child lists none, searches for both"
            );
            let prepends: Vec<u32> = stage_claims(run)
                .filter(|call| call.target == Some(d))
                .flat_map(|call| call.lookups.iter())
                .filter_map(|lookup| match *lookup {
                    Lookup::Since { prepends, .. } => Some(prepends),
                    _ => None,
                })
                .collect();
            assert_eq!(
                prepends,
                [0, 1],
                "the parent's mark counts its own add alone"
            );
        },
    );
}

/// Beyond the contract, the stage's claims of a listed entry and of a name beside a served rename
/// of the other listed child to that name: the listing holds the child under the name where the
/// rename came first; where after the listing and before the search, the search finds the child
/// ahead of the mark's head and the mark's reattach read claims it in the listing too; where after
/// the search, it answers none and the claims add the name, the rename refused, or landing first
/// beside the claims' add. Every schedule, within the thirteen preemptions that reach them all,
/// replays clean with its calls.
#[test]
fn a_stage_claim_beside_a_rename_into_an_entrys_name() {
    let counts: Rc<[Cell<usize>; 3]> = Rc::default();
    let counted = counts.clone();
    explore_with(
        Explore {
            preemptions: Some(13),
            ..Explore::default()
        },
        tree(&["d/f", "d/g"]),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let entries = [("g", true), ("n", true)];
                    let claimed = ops::stage_claim(&tree, tree.id("d"), &entries).await;
                    let claimed = claimed.expect("the claims run");
                    let f = tree.id("d/f");
                    if claimed.nodes[1] == f {
                        assert!(
                            !claimed.unclaimed.contains(&f),
                            "f is claimed in the listing"
                        );
                    }
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = ops::served_rename(&tree, tree.id("d/f"), tree.id("d"), "n").await;
                })
            }),
        ],
        vec![],
        move |run| {
            let f = run.tree.id("d/f");
            let lookups = &the_done_claim(run).lookups;
            let class = match lookups.as_slice() {
                [] => 0,
                [
                    Lookup::Since {
                        searched: AddedSince::Found(node),
                        prepends: 0,
                        ..
                    },
                    Lookup::Reattached {
                        reattached: true, ..
                    },
                ] if *node == f => 1,
                [
                    Lookup::Since {
                        searched: AddedSince::Absent,
                        ..
                    },
                ] => 2,
                other => panic!("the claim of 'n' read {other:?}"),
            };
            counted[class].set(counted[class].get() + 1);
        },
    );
    let counts = counts.each_ref().map(|count| count.get());
    assert!(
        counts.iter().all(|&count| count > 0),
        "runs listing f as 'n', finding it and reattached, answering none: {counts:?}"
    );
}

/// Beyond the contract, the stage's claim of a missing name in a directory whose chain crosses
/// blocks, beside a move out of it of the child behind the crossing: a move landing after the
/// listing's head read and before the search's end turns the search stale, its find through the
/// chain missing; one landing during the listing, at the crossing, leaves the child out of it too.
/// Before the listing or after the search, the search answers none. The claims add the name in
/// every run, and every schedule, within the thirteen preemptions that reach them all, replays
/// clean with its calls.
#[test]
fn a_stage_claim_beside_a_move_out_of_its_directory() {
    let counts: Rc<[Cell<usize>; 4]> = Rc::default();
    let counted = counts.clone();
    explore_with(
        Explore {
            preemptions: Some(13),
            ..Explore::default()
        },
        across(false),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let entries = [("n", true)];
                    let claimed = ops::stage_claim(&tree, tree.id("d"), &entries).await;
                    let claimed = claimed.expect("the claims run");
                    assert!(
                        !claimed.listed.contains(&claimed.nodes[0]),
                        "the claims add 'n'"
                    );
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let (g, e) = (tree.id("d/g"), tree.id("e"));
                    assert!(ops::served_rename(&tree, g, e, "g").await, "g moves out");
                })
            }),
        ],
        vec![],
        move |run| {
            let (claim, g) = (the_done_claim(run), run.tree.id("d/g"));
            let left_out = !(claim.walks[0].yielded.iter()).any(|yielded| yielded.node == g);
            let stale = match claim.lookups.as_slice() {
                [
                    Lookup::Since {
                        searched: AddedSince::Stale,
                        ..
                    },
                    Lookup::Find { found: None, .. },
                ] => true,
                [
                    Lookup::Since {
                        searched: AddedSince::Absent,
                        ..
                    },
                ] => false,
                other => panic!("the claim of 'n' read {other:?}"),
            };
            let class = usize::from(left_out) * 2 + usize::from(stale);
            counted[class].set(counted[class].get() + 1);
        },
    );
    let counts = counts.each_ref().map(|count| count.get());
    assert!(
        counts.iter().all(|&count| count > 0),
        "runs moving g after the search, between the listing and the search's end, before the \
         listing, during it: {counts:?}"
    );
}

/// Beyond the contract, the stage's claim of a name beside a checked create of that name: the claim
/// takes the create's node from its listing where the create published before it, or by its search
/// since the mark where after the listing and before the search, the mark's reattach read answering
/// none; where after the search, the claims add the name, the create finding it held where it
/// checked after their add published, or adding the name a second time where before, which the node
/// module's Limitations, "The stage's listing", keep from happening. Every schedule, within the
/// thirteen preemptions that reach them all, replays clean with its calls.
#[test]
fn a_stage_claim_beside_a_create_of_its_entrys_name() {
    let counts: Rc<[Cell<usize>; 4]> = Rc::default();
    let counted = counts.clone();
    explore_with(
        Explore {
            preemptions: Some(13),
            ..Explore::default()
        },
        tree(&["d/f", "d/g"]),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let entries = [("n", true)];
                    (ops::stage_claim(&tree, tree.id("d"), &entries).await).expect("the claims");
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = ops::checked_create(&tree, tree.id("d"), "n").await;
                })
            }),
        ],
        vec![],
        move |run| {
            let claim = the_done_claim(run);
            let added = !published(run, claim).is_empty();
            let created = match call_of(run, &catalogue::CHECKED_CREATE).outcome {
                Outcome::Done(Answer::Node(node)) => Some(node),
                Outcome::Done(Answer::Held(_)) => None,
                ref other => panic!("the create answered {other:?}"),
            };
            let class = match (claim.lookups.as_slice(), added, created) {
                ([], false, Some(_)) => 0,
                (
                    [
                        Lookup::Since {
                            searched: AddedSince::Found(found),
                            prepends: 0,
                            ..
                        },
                        Lookup::Reattached {
                            reattached: false, ..
                        },
                    ],
                    false,
                    Some(node),
                ) if *found == node => 1,
                (
                    [
                        Lookup::Since {
                            searched: AddedSince::Absent,
                            ..
                        },
                    ],
                    true,
                    created,
                ) => 2 + usize::from(created.is_some()),
                (lookups, added, created) => {
                    panic!("the claim read {lookups:?}, adding: {added}, the create: {created:?}")
                }
            };
            counted[class].set(counted[class].get() + 1);
        },
    );
    let counts = counts.each_ref().map(|count| count.get());
    assert!(
        counts.iter().all(|&count| count > 0),
        "runs taking the node from the listing, finding it, the create finding the name held, \
         adding the name twice: {counts:?}"
    );
}

/// A tree whose `d` holds `h` alone, in block 0, which the files of `e` fill but for its last
/// slot: a first add to `d` takes that slot, ahead of `h` in the same block, a second the next
/// block's first, so a walk from the second to `h` crosses the blocks at the first.
fn one_slot_left() -> Setup<Tree> {
    Box::new(|| {
        Box::pin(async {
            let tree = Tree::new(&["d/h", "e/"]).await;
            let filled = tree.fill_block_but_one(tree.id("e"), "fill").await;
            assert_eq!(NodeBlock::index(filled), 0, "the files fill block 0");
            tree
        })
    })
}

/// The stage's claims of `p`, `q` and `r` in `d` of the tree [`one_slot_left`] builds.
fn three_claims() -> Op<Tree> {
    Box::new(|tree: Arc<Tree>| {
        Box::pin(async move {
            let entries = [("p", true), ("q", true), ("r", true)];
            (ops::stage_claim(&tree, tree.id("d"), &entries).await).expect("the claims run");
        })
    })
}

/// What `claim`'s search since its mark for `r` answered, and the search's interval; `None` where
/// it made none.
fn search_for_r(claim: &Call) -> Option<(AddedSince, Range<u64>)> {
    (claim.lookups.iter()).find_map(|lookup| match *lookup {
        Lookup::Since {
            name,
            searched,
            from,
            to,
            ..
        } if name == hash_string("r") => Some((searched, from..to)),
        _ => None,
    })
}

/// Whether a record `effect` picks, of a call of `kind` in `run`, lies within `seqs`.
fn landed_within(
    run: &Run<'_>,
    kind: &catalogue::Kind,
    seqs: &Range<u64>,
    effect: fn(&Effect) -> bool,
) -> bool {
    (run.calls.iter())
        .filter(|call| std::ptr::eq(call.kind, kind))
        .any(|call| {
            run.records.iter().any(|record| {
                call.holds(record, &run.spawns)
                    && seqs.contains(&record.seq)
                    && effect(&record.effect)
            })
        })
}

/// Beyond the contract, the stage's claims of three missing names in a directory holding one child,
/// their first two adds spanning two blocks ahead of the mark's head, beside a find and a rename in
/// place of the first add to the third's name: a rename landing inside the third's search, at its
/// crossing of the blocks, has it find the node, which the model admits only within the read's own
/// interval, to its end. The rename changes the hash in place, which no caller does beside a stage,
/// as the respellings keep it: it covers the third entry's name it gives the node inside the
/// search. Every schedule, within the seven preemptions that reach them all, replays clean with its
/// calls.
#[test]
fn a_stage_claim_beside_a_rename_landing_inside_its_search() {
    let inside: Rc<Cell<usize>> = Rc::default();
    let counted = inside.clone();
    explore_with(
        Explore {
            preemptions: Some(7),
            ..Explore::default()
        },
        one_slot_left(),
        vec![
            three_claims(),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    if let Some(p) = ops::find(&tree, tree.id("d"), "p").await {
                        ops::rename_then_mark(&tree, p, "r").await;
                    }
                })
            }),
        ],
        vec![],
        move |run| {
            let claim = the_done_claim(run);
            let (searched, seqs) = search_for_r(claim).expect("'r' is never listed");
            let renamed = |effect: &Effect| matches!(effect, Effect::Rename { .. });
            if landed_within(run, &catalogue::RENAME, &seqs, renamed) {
                let p = published(run, claim)[0];
                assert_eq!(
                    searched,
                    AddedSince::Found(p),
                    "the search finds the node renamed inside it"
                );
                counted.set(counted.get() + 1);
            }
        },
    );
    assert!(
        inside.get() > 0,
        "no run had the rename land inside the third claim's search"
    );
}

/// [`a_stage_claim_beside_a_rename_landing_inside_its_search`] beside a checked create of the
/// third's name: a create landing inside the third's search, at its crossing of the blocks,
/// publishes the node ahead of where the walk began, so the search answers none, which the model
/// admits only within the read's own interval, from its start; the claims add the name a second
/// time (Limitations, "The stage's listing"). Every run within four preemptions replays clean with
/// its calls.
#[test]
fn a_stage_claim_beside_a_create_landing_inside_its_search() {
    let inside: Rc<Cell<usize>> = Rc::default();
    let counted = inside.clone();
    explore_with(
        Explore {
            preemptions: Some(4),
            ..Explore::default()
        },
        one_slot_left(),
        vec![
            three_claims(),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = ops::checked_create(&tree, tree.id("d"), "r").await;
                })
            }),
        ],
        vec![],
        move |run| {
            let claim = the_done_claim(run);
            let Some((searched, seqs)) = search_for_r(claim) else {
                return;
            };
            let publishes = |effect: &Effect| matches!(effect, Effect::Publish { .. });
            if landed_within(run, &catalogue::CHECKED_CREATE, &seqs, publishes) {
                assert_eq!(
                    searched,
                    AddedSince::Absent,
                    "the search misses the node published ahead of its walk"
                );
                counted.set(counted.get() + 1);
            }
        },
    );
    assert!(
        inside.get() > 0,
        "no run had the create land inside the third claim's search"
    );
}
