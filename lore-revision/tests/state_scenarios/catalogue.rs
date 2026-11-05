// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The catalogue's registry of kinds: each named once and found by its name, naming the module's
//! entry points it calls and the mode it calls them in, on the tier their callers run them on, run
//! apart and dropped part way only where those callers do. And the ops' compositions alone, each as
//! its caller makes it.

use std::collections::BTreeSet;

use lore_revision::node::MAX_NODE_NAME_LEN;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::AttachOutcome;
use lore_revision::state::NameChange;
use lore_revision::state::StateError;
use lore_storage::hash::hash_string;

use crate::harness::catalogue;
use crate::harness::catalogue::KINDS;
use crate::harness::catalogue::Role;
use crate::harness::catalogue::Runs;
use crate::harness::catalogue::Tier;
use crate::harness::catalogue::ops;
use crate::harness::fixtures::Tree;
use crate::harness::invariants::Report;
use crate::harness::invariants::Rules;

/// The entry points only the C API's shared claim calls.
const SHARED: &[&str] = &[
    "State::node_add_staged",
    "State::node_modify_staged",
    "State::node_delete_level",
    "State::node_restore",
    "State::node_move_staged",
    "State::node_discard_pending",
];

/// The entry points whose callers never drop them part way: the C API delete's batch, which runs
/// in a task the call awaits, and the commit's discard walk and free, free functions. The C API
/// commit's drain, save and prune run in its call, which may be dropped. And what runs in a call
/// nothing drops, as `lore dirty` runs under `block_on_command` or in a detached task: named in
/// the kind's mode, as its entry points are other callers' too.
const UNCANCELLED: &[&str] = &[
    "State::node_delete_level",
    "State::node_discard_pending",
    "node_discard_walk",
    "node_discard_free",
    "in a call nothing drops",
];

/// The entry points readers read through.
const READERS: &[&str] = &[
    "State::find_subnode",
    "State::node_children",
    "StateNodeChildrenIterator",
    "StateNodeChildrenWithNameIterator",
    "ChainWalk",
    "ChainMark::reattached",
    "State::walk_children",
    "State::collect_children_unsorted",
    "State::collect_named_children_unsorted",
    "State::node_path",
    "State::node_name",
    "State::find_relative_node_link",
    "State::collect_dirty_paths",
    "State::node_has_",
    "State::watch_chain",
];

/// The entry points of what a VFS provider runs one at a time on a served State: the served
/// clear, attach and mark, the swap's detaches and builds, the clean rewrite, the flips and keeps
/// in place.
const SWAP: &[&str] = &[
    "ServedNodes::node_clear",
    "ServedNodes::node_attach",
    "ServedNodes::node_mark",
    "SwapNodes::",
    "TreeBuild::",
    "State::node_rewrite",
    "State::node_change_in_place",
];

/// What the contract runs alone on its State: a save, the walks' discards once their walks drain,
/// the pre-serving composition, which ends in `mark_served`, and the sweeps (C15).
const SOLO: &[&str] = &[
    "State::serialize",
    "WalkDiscards::apply",
    "State::mark_served",
    "node_sweep_detached",
];

/// What the contract runs beside readers and few writers: the trusted moves and the commit's
/// prune.
const WRITER_EXCLUSIVE: &[&str] = &["The trusted move", "the commit's prune"];

/// Whether `kind` calls or runs as one of `marks` says.
fn names_any(kind: &catalogue::Kind, marks: &[&str]) -> bool {
    marks
        .iter()
        .any(|mark| kind.primitive.contains(mark) || kind.mode.contains(mark))
}

/// `tree`'s invariants with closure held.
async fn closed(tree: &Tree) -> Report {
    crate::holding(&tree.state, &tree.repository, Rules::CLOSED).await
}

/// [`closed`] but for unique names: a stage's move over a held name leaves the holder, staged
/// deleted, beside the moved node under that name until the commit, as the trusted moves may.
async fn closed_but_held_twice(tree: &Tree) -> Report {
    let rules = Rules {
        unique_names: false,
        ..Rules::CLOSED
    };
    crate::holding(&tree.state, &tree.repository, rules).await
}

#[test]
fn every_kind_is_named_once_and_found_by_its_name() {
    let mut names = BTreeSet::new();
    for kind in KINDS {
        assert!(names.insert(kind.name), "'{}' is named twice", kind.name);
        let found = catalogue::kind(kind.name).expect("a registered kind is found");
        assert!(
            std::ptr::eq(found, *kind),
            "'{}' finds another kind",
            kind.name
        );
    }
    assert!(catalogue::kind("no such kind").is_none());
}

#[test]
fn every_kind_names_what_it_calls_and_how_and_no_two_alike() {
    let mut calls = BTreeSet::new();
    for kind in KINDS {
        assert!(
            !kind.primitive.is_empty(),
            "'{}' names no entry point",
            kind.name
        );
        assert!(!kind.mode.is_empty(), "'{}' names no mode", kind.name);
        assert!(
            calls.insert((kind.primitive, kind.mode)),
            "'{}' calls what another kind calls, as it calls it",
            kind.name
        );
    }
}

/// A kind reads exactly where every entry point it names is a reader's, and a reader runs beside
/// every writer of its tier: on any State, or on a served one where it reads what only a served
/// State holds. What runs on any State only reads.
#[test]
fn every_reader_runs_beside_its_tiers_writers() {
    for kind in KINDS {
        let reads = kind
            .primitive
            .split(", ")
            .all(|called| READERS.iter().any(|reader| called.starts_with(reader)));
        assert_eq!(
            kind.role == Role::Reader,
            reads,
            "'{}' is a {:?}",
            kind.name,
            kind.role
        );
        if kind.tier == Tier::Any {
            assert_eq!(
                kind.role,
                Role::Reader,
                "'{}' writes on any State",
                kind.name
            );
        }
    }
    for kind in KINDS.iter().filter(|kind| kind.role == Role::Reader) {
        assert!(
            matches!(kind.tier, Tier::Any | Tier::Served),
            "the reader '{}' runs on {:?}",
            kind.name,
            kind.tier
        );
        assert_eq!(kind.runs, Runs::Beside, "the reader '{}'", kind.name);
        assert!(kind.cancellable, "the reader '{}' is dropped", kind.name);
    }
}

/// Each kind runs on the tier of the entry points it calls: the swap's on its holder, the served
/// ones on a served State, the C API's on the shared claim, the owned ones on the exclusive claim.
#[test]
fn every_kind_runs_on_its_entry_points_tier() {
    for kind in KINDS {
        let tier = kind.tier;
        if kind.primitive.contains("SwapNodes::") {
            assert_eq!(tier, Tier::Swap, "'{}' calls the swap's", kind.name);
        }
        if kind.primitive.contains("ServedNodes::") {
            assert!(
                matches!(tier, Tier::Served | Tier::Swap),
                "'{}' calls the served entry points on {tier:?}",
                kind.name
            );
        }
        if tier == Tier::Swap {
            for called in kind.primitive.split(", ") {
                assert!(
                    SWAP.iter().any(|swap| called.starts_with(swap)),
                    "the swap's '{}' calls {called}",
                    kind.name
                );
            }
        }
        if tier == Tier::Served && kind.role == Role::Writer {
            for called in kind.primitive.split(", ") {
                assert!(
                    called.starts_with("ServedNodes::") || called == "State::node_change_in_place",
                    "the served '{}' calls {called}",
                    kind.name
                );
            }
        }
        assert_eq!(
            tier == Tier::Shared,
            names_any(kind, SHARED),
            "'{}' on {tier:?}: the shared claim's kinds are the C API's",
            kind.name
        );
        if names_any(kind, &["State::node_change_owned", "State::node_clear_own"]) {
            assert_eq!(tier, Tier::Exclusive, "'{}' is owned", kind.name);
        }
    }
}

#[test]
fn a_kind_is_dropped_part_way_only_where_its_caller_drops_it() {
    for kind in KINDS {
        assert_eq!(
            kind.cancellable,
            !names_any(kind, UNCANCELLED),
            "'{}' cancellable",
            kind.name
        );
    }
}

/// Solo and writer-exclusive kinds are the contract's, and each is an exclusive claim's writer but
/// the served sweep, which a VFS provider runs where it paused its requests, settles and swaps
/// (C15).
#[test]
fn a_kind_runs_apart_only_where_the_contract_runs_it_apart() {
    for kind in KINDS {
        assert_eq!(
            kind.runs == Runs::Solo,
            names_any(kind, SOLO),
            "'{}' runs {:?}",
            kind.name,
            kind.runs
        );
        assert_eq!(
            kind.runs == Runs::WriterExclusive,
            names_any(kind, WRITER_EXCLUSIVE),
            "'{}' runs {:?}",
            kind.name,
            kind.runs
        );
        let served_sweep = std::ptr::eq(*kind, &catalogue::SERVED_SWEEP);
        if kind.runs != Runs::Beside {
            assert!(
                kind.role == Role::Writer && (kind.tier == Tier::Exclusive || served_sweep),
                "'{}' runs apart on {:?}",
                kind.name,
                kind.tier
            );
        }
    }
}

/// Every tier the contract runs writers on has a kind, and so do the readers and every way of
/// running.
#[test]
fn every_tier_and_way_of_running_has_kinds() {
    for tier in [
        Tier::Served,
        Tier::Swap,
        Tier::Shared,
        Tier::Exclusive,
        Tier::Any,
    ] {
        assert!(
            KINDS.iter().any(|kind| kind.tier == tier),
            "no kind on {tier:?}"
        );
    }
    for runs in [Runs::Beside, Runs::WriterExclusive, Runs::Solo] {
        assert!(
            KINDS.iter().any(|kind| kind.runs == runs),
            "no kind runs {runs:?}"
        );
    }
    assert!(KINDS.iter().any(|kind| kind.role == Role::Reader));
}

/// The checked create of a free name: the file added dirty, its parent marked, closure kept.
#[tokio::test]
async fn a_checked_create_adds_the_file_dirty_and_marks_its_parent() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f"]).await;
        let created = ops::checked_create(&tree, tree.id("d"), "n").await;
        let Some(AttachOutcome::Attached(n)) = created else {
            panic!("'d/n' is attached: {created:?}");
        };
        let report = closed(&tree).await;
        let (id, node) = report.at("d/n").expect("'d/n'");
        assert_eq!(id, n);
        assert!(node.is_file() && node.is_dirty_add(), "'d/n' is added");
        assert!(report.at("d").expect("'d'").1.is_dirty(), "'d' is marked");
    })
    .await;
}

/// The checked create of a held name: the holder's staged flags cleared, then the holder marked
/// modified, and its parent marked. The forced mark alone would take the delete's action back
/// but leave the staged bit.
#[tokio::test]
async fn a_checked_create_of_a_held_name_takes_back_its_delete_and_marks_it() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f"]).await;
        let (d, f) = (tree.id("d"), tree.id("d/f"));
        ops::staged_delete_level(&tree, f, d).await;
        let created = ops::checked_create(&tree, d, "f").await;
        assert_eq!(created, Some(AttachOutcome::Exists(f)));
        let report = closed(&tree).await;
        let held = report.at("d/f").expect("'d/f'").1;
        assert!(!held.is_staged_delete(), "the delete is taken back");
        assert!(!held.is_staged(), "the staged flags are cleared");
        assert!(held.is_dirty_modify(), "'d/f' is marked modified");
        assert!(report.at("d").expect("'d'").1.is_dirty(), "'d' is marked");
    })
    .await;
}

/// A VFS provider diff's add into a listed directory: the file or directory added dirty, the
/// directory marked, closure kept.
#[tokio::test]
async fn a_diff_add_adds_the_node_dirty_and_marks_its_directory() {
    crate::in_context(async {
        for file in [true, false] {
            let tree = Tree::new(&["d/f"]).await;
            let added = ops::diff_add(&tree, tree.id("d"), "n", file)
                .await
                .expect("'d/n' is added");
            let report = closed(&tree).await;
            let (id, node) = report.at("d/n").expect("'d/n'");
            assert_eq!(id, added);
            assert!(node.is_dirty_add(), "'d/n' is added");
            assert_eq!(
                node.is_file(),
                file,
                "'d/n' is added a file where it is one"
            );
            assert!(report.at("d").expect("'d'").1.is_dirty(), "'d' is marked");
        }
    })
    .await;
}

/// A scan's chain finds what the State holds and adds the rest dirty-added, the first under the
/// directory it marks, the next under the first; a path the State holds whole adds nothing.
#[tokio::test]
async fn a_scan_chain_adds_the_missing_directories_and_marks_above_the_first() {
    crate::in_context(async {
        let tree = Tree::new(&["d/e/f"]).await;
        let last = ops::scan_chain(&tree, ROOT_NODE, "d/e/n1/n2")
            .await
            .expect("the chain is made");
        let report = closed(&tree).await;
        let (n1, first) = report.at("d/e/n1").expect("'d/e/n1'");
        let (n2, second) = report.at("d/e/n1/n2").expect("'d/e/n1/n2'");
        assert_eq!(n2, last, "the chain answers its last directory");
        assert!(
            first.is_dirty_add() && second.is_dirty_add(),
            "both are added"
        );
        assert!(
            report.at("d/e").expect("'d/e'").1.is_dirty(),
            "'d/e' is marked"
        );
        let again = ops::scan_chain(&tree, ROOT_NODE, "d/e/n1").await;
        assert_eq!(again, Some(n1), "a held path is found");
    })
    .await;
}

/// A VFS provider diff's settle of an add under a directory the State lacks: the chain adds it, and
/// the checked create attaches the file in it.
#[tokio::test]
async fn a_chained_checked_create_attaches_in_the_directory_its_chain_added() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f"]).await;
        let created = ops::chain_checked_create(&tree, ROOT_NODE, "d/m", "n").await;
        let Some(AttachOutcome::Attached(n)) = created else {
            panic!("'d/m/n' is attached: {created:?}");
        };
        let report = closed(&tree).await;
        let (id, node) = report.at("d/m/n").expect("'d/m/n'");
        assert_eq!(id, n);
        assert!(node.is_file() && node.is_dirty_add(), "'d/m/n' is added");
        let m = report.at("d/m").expect("'d/m'").1;
        assert!(m.is_dirty_add(), "the chain adds 'd/m'");
        assert!(report.at("d").expect("'d'").1.is_dirty(), "'d' is marked");
    })
    .await;
}

/// `lore dirty`'s add below a missing directory: the directory added and marked dirty, the file
/// or directory added and marked dirty-added, each forced up to the root, closure kept.
#[tokio::test]
async fn a_dirty_add_adds_its_directories_and_marks_the_node_added() {
    crate::in_context(async {
        for file in [true, false] {
            let tree = Tree::new(&["d/f"]).await;
            let added = ops::dirty_add(&tree, tree.id("d"), "m", "n", file)
                .await
                .expect("'d/m/n' is added");
            let report = closed(&tree).await;
            let (id, node) = report.at("d/m/n").expect("'d/m/n'");
            assert_eq!(id, added);
            assert!(node.is_dirty_add(), "'d/m/n' is added");
            assert_eq!(
                node.is_file(),
                file,
                "'d/m/n' is added a file where it is one"
            );
            let m = report.at("d/m").expect("'d/m'").1;
            assert!(m.is_dirty(), "'d/m' is marked dirty");
            assert!(report.at("d").expect("'d'").1.is_dirty(), "'d' is marked");
        }
    })
    .await;
}

/// `lore dirty`'s add whose node the State refuses: the directory it added on the way stands
/// marked dirty, as `lore dirty` marks each at once. The node's name is too long to store, the one
/// refusal of its add alone a test provokes without a store failing; it stands for any, as a slot
/// that fails to load.
#[tokio::test]
async fn a_dirty_add_refused_its_node_leaves_its_directories_marked() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f"]).await;
        let long = "n".repeat(MAX_NODE_NAME_LEN + 1);
        let added = ops::dirty_add(&tree, tree.id("d"), "m", &long, true).await;
        assert_eq!(added, None, "the node is refused");
        let report = closed(&tree).await;
        let m = report.at("d/m").expect("'d/m'").1;
        assert!(m.is_dirty(), "'d/m' is marked dirty");
    })
    .await;
}

/// The stage's claims of a directory's entries: each listed one claimed from its listing, each
/// missing one added staged and dirty added, the directory marked by the climb, the listed
/// children no entry names left unclaimed, in sibling order.
#[tokio::test]
async fn a_stage_claim_claims_listed_entries_and_adds_the_missing_ones_staged() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f", "d/g", "d/h"]).await;
        let entries = [("f", true), ("n", true), ("g", true), ("m", false)];
        let claimed = ops::stage_claim(&tree, tree.id("d"), &entries)
            .await
            .expect("the claims run");
        let report = closed(&tree).await;
        let (n, n_node) = report.at("d/n").expect("'d/n'");
        let (m, m_node) = report.at("d/m").expect("'d/m'");
        assert_eq!(claimed.nodes, [tree.id("d/f"), n, tree.id("d/g"), m]);
        assert_eq!(claimed.unclaimed, [tree.id("d/h")]);
        for (path, node, file) in [("d/n", n_node, true), ("d/m", m_node, false)] {
            assert!(
                node.is_staged_add() && node.is_dirty_add(),
                "'{path}' is staged added"
            );
            assert_eq!(
                node.is_file(),
                file,
                "'{path}' is added a file where it is one"
            );
        }
        let d = report.at("d").expect("'d'").1;
        assert!(d.is_staged() && d.is_dirty(), "'d' is marked by the climb");
    })
    .await;
}

/// The stage's claim of a name created since its listing: its search since the mark finds the
/// node, which it takes in place of an add of its own.
#[tokio::test]
async fn a_stage_claim_takes_a_node_created_since_its_listing() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f"]).await;
        let d = tree.id("d");
        let created = async {
            assert!(
                ops::checked_create(&tree, d, "n").await.is_some(),
                "'d/n' is created"
            );
        };
        let claimed = ops::stage_claim_waiting(&tree, d, &[("n", true)], created)
            .await
            .expect("the claims run");
        let report = closed(&tree).await;
        let (n, node) = report.at("d/n").expect("'d/n'");
        assert_eq!(claimed.nodes, [n]);
        assert_eq!(claimed.unclaimed, [tree.id("d/f")]);
        assert!(
            !node.is_staged(),
            "'d/n' is the create's, not staged by the claim"
        );
    })
    .await;
}

/// The stage's claim of a name its listing holds under another, renamed in place since: its search
/// since the mark stale, its find through the chain finds the renamed node, which the mark's
/// reattach claims in the listing too, so the pass over what nothing claimed leaves it. The rename
/// changes the hash in place, which no caller does beside a stage, as the respellings keep it:
/// it covers the mark, which counts such a rename as a reattach.
#[tokio::test]
async fn a_stage_claim_claims_a_node_renamed_since_its_listing_in_the_listing_too() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f", "d/g"]).await;
        let (d, g) = (tree.id("d"), tree.id("d/g"));
        let renamed = ops::rename_then_mark(&tree, g, "n");
        let claimed = ops::stage_claim_waiting(&tree, d, &[("n", true)], renamed)
            .await
            .expect("the claims run");
        assert_eq!(claimed.nodes, [g]);
        assert_eq!(claimed.unclaimed, [tree.id("d/f")]);
    })
    .await;
}

/// The stage's claims stop at the first add that fails, as the stage stops at a failed entry: what
/// it added before stands staged, nothing after it is added. The name is too long to store, the
/// one failure of an add alone a test provokes without a store failing.
#[tokio::test]
async fn a_stage_claim_stops_at_an_add_that_fails() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f"]).await;
        let long = "n".repeat(MAX_NODE_NAME_LEN + 1);
        let entries = [("m", true), (long.as_str(), true), ("o", true)];
        let claimed = ops::stage_claim(&tree, tree.id("d"), &entries).await;
        assert!(claimed.is_none(), "the claims stop: {claimed:?}");
        let report = closed(&tree).await;
        assert!(
            report.at("d/m").expect("'d/m'").1.is_staged_add(),
            "'d/m' stands staged"
        );
        assert!(report.at("d/o").is_none(), "'d/o' is not added");
    })
    .await;
}

/// Beyond the contract, the stage's claims of a name its listing holds under another, renamed in
/// place since, and of that other name too: the renamed node answers the first, claimed in the
/// listing too, and the second, whose stale search's find through the chain misses, from the
/// listing, claimed or not, as `claim_child` answers; nothing is added. The rename changes the hash
/// in place, which no caller does beside a stage, as the respellings keep it: it covers the second
/// name it gives the node.
#[tokio::test]
async fn a_stage_claim_of_both_names_of_a_node_renamed_since_answers_it_twice() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f", "d/g"]).await;
        let (d, g) = (tree.id("d"), tree.id("d/g"));
        let renamed = ops::rename_then_mark(&tree, g, "n");
        let entries = [("n", true), ("g", true)];
        let claimed = ops::stage_claim_waiting(&tree, d, &entries, renamed)
            .await
            .expect("the claims run");
        assert_eq!(claimed.nodes, [g, g]);
        assert_eq!(claimed.unclaimed, [tree.id("d/f")]);
        let report = closed(&tree).await;
        assert!(report.at("d/g").is_none(), "no 'd/g' is added");
    })
    .await;
}

/// The stage's respelling of a node its claim found under another case, in the claims' window
/// after the claim, as `lore stage --case rename` respells there: the node renamed in place to the
/// entry's spelling, its hash and its place in the chain kept, staged moved, its directory staged
/// with it and nothing dirtied; closure kept.
#[tokio::test]
async fn a_stage_respell_in_the_claims_window_stages_the_node_moved_in_place() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f", "d/g"]).await;
        let (d, f) = (tree.id("d"), tree.id("d/f"));
        let chain = ops::listed_children(&tree, d).await;
        let respell = |_, node| ops::stage_respell(&tree, node, "F");
        let claimed = ops::stage_claim_descending(&tree, d, &[("F", true)], respell)
            .await
            .expect("the claims run");
        assert_eq!(claimed.nodes, [f]);
        let report = closed(&tree).await;
        assert!(report.at("d/f").is_none(), "'d/f' is respelled");
        let (id, node) = report.at("d/F").expect("'d/F'");
        assert_eq!(id, f);
        assert_eq!(node.name_hash, hash_string("f"), "the hash is kept");
        assert!(
            node.is_staged_move() && !node.is_dirty(),
            "flags {:#x}",
            node.flags
        );
        let after = ops::listed_children(&tree, d).await;
        assert_eq!(after, chain, "the place is kept");
        let dir = report.at("d").expect("'d'").1;
        assert!(dir.is_staged() && !dir.is_dirty(), "flags {:#x}", dir.flags);
    })
    .await;
}

/// The stage's respelling of a node already spelled as the entry changes nothing: no rename, no
/// mark.
#[tokio::test]
async fn a_stage_respell_of_a_name_spelled_so_changes_nothing() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f"]).await;
        ops::stage_respell(&tree, tree.id("d/f"), "f").await;
        untouched(&closed(&tree).await, &["d/f", "d"]);
    })
    .await;
}

/// The pre-serving respelling of a node already spelled so changes nothing: no rename, no mark.
#[tokio::test]
async fn a_rename_of_a_name_spelled_so_changes_nothing() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f"]).await;
        ops::rename_then_mark(&tree, tree.id("d/f"), "f").await;
        untouched(&closed(&tree).await, &["d/f", "d"]);
    })
    .await;
}

/// The pre-serving respelling of a node to another case: the node renamed in place and marked
/// with the base dirty bit alone, its directory dirty, nothing staged; closure kept.
#[tokio::test]
async fn a_rename_respells_the_node_and_marks_it_dirty_where_its_spelling_differs() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f", "d/g"]).await;
        let f = tree.id("d/f");
        ops::rename_then_mark(&tree, f, "F").await;
        let report = closed(&tree).await;
        let (id, node) = report.at("d/F").expect("'d/F'");
        assert_eq!(id, f);
        assert_eq!(
            node.flags & NodeFlags::DirtyBits.bits(),
            NodeFlags::Dirty.bits(),
            "flags {:#x}",
            node.flags
        );
        assert!(!node.is_staged(), "flags {:#x}", node.flags);
        assert!(report.at("d").expect("'d'").1.is_dirty(), "'d' is marked");
    })
    .await;
}

/// Each of `paths` in `report` neither staged nor dirty.
fn untouched(report: &Report, paths: &[&str]) {
    for path in paths {
        let node = report.at(path).expect(path).1;
        assert!(
            !node.is_staged() && !node.is_dirty(),
            "{path}: flags {:#x}",
            node.flags
        );
    }
}

/// `d/s/a`, `d/s/b` and `d/s/t/x` in `d/s`, beside `d/s/u/y` and `e/`: `d/s/b` marked deleted,
/// `d/s/u` marked deleted over a clean `d/s/u/y`, and `d/s/c` added; the tree and `d/s/c`.
async fn subtree_to_move() -> (Tree, NodeID) {
    let tree = Tree::new(&["d/s/a", "d/s/b", "d/s/t/x", "d/s/u/y", "e/"]).await;
    ops::mark_deleted(&tree, tree.id("d/s/b")).await;
    ops::mark_deleted(&tree, tree.id("d/s/u")).await;
    let c = ops::dirty_add(&tree, tree.id("d/s"), "", "c", true)
        .await
        .expect("'d/s/c' is added");
    (tree, c)
}

/// `d/s` and every node below it, `d/s/a` and `d/s/t/x`, marked deleted, as a directory's deletion
/// stamps itself on its whole subtree, beside `e/`.
async fn deleted_subtree() -> Tree {
    let tree = Tree::new(&["d/s/a", "d/s/t/x", "e/"]).await;
    for path in ["d/s", "d/s/a", "d/s/t", "d/s/t/x"] {
        ops::mark_deleted(&tree, tree.id(path)).await;
    }
    tree
}

/// The directory `d/m` `lore dirty` added, holding the added `d/m/n` and `d/m/k/x`, where `d/m/k`
/// is a directory its chain added dirty with no add action; beside `e/`. The tree and `d/m`.
async fn added_subtree() -> (Tree, NodeID) {
    let tree = Tree::new(&["d/f", "e/"]).await;
    let m = ops::dirty_add(&tree, tree.id("d"), "", "m", false)
        .await
        .expect("'d/m' is added");
    ops::dirty_add(&tree, m, "", "n", true)
        .await
        .expect("'d/m/n' is added");
    ops::dirty_add(&tree, m, "k", "x", true)
        .await
        .expect("'d/m/k/x' is added");
    (tree, m)
}

/// `lore dirty move`'s move of a file: the file dirty-moved, the destination climbed and the
/// directory it left marked dirty, nothing staged, closure kept.
#[tokio::test]
async fn a_dirty_move_marks_the_file_and_both_directories_dirty() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f", "d/g", "e/"]).await;
        ops::dirty_move(&tree, tree.id("d/f"), tree.id("e"), NameChange::Keep)
            .await
            .expect("'d/f' moves");
        let report = closed(&tree).await;
        let f = report.at("e/f").expect("'e/f'").1;
        assert!(f.is_dirty_move() && !f.is_staged(), "flags {:#x}", f.flags);
        for path in ["e", "d"] {
            let level = report.at(path).expect(path).1;
            assert!(level.is_dirty() && !level.is_staged(), "{path}");
        }
    })
    .await;
}

/// The dirty move of a file `lore dirty` added keeps it an add: it is in no revision a move
/// could name.
#[tokio::test]
async fn a_dirty_move_of_an_added_file_keeps_it_added() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f", "e/"]).await;
        let n = ops::dirty_add(&tree, tree.id("d"), "", "n", true)
            .await
            .expect("'d/n' is added");
        ops::dirty_move(&tree, n, tree.id("e"), NameChange::Keep)
            .await
            .expect("'d/n' moves");
        let added = closed(&tree).await.at("e/n").expect("'e/n'").1;
        assert!(added.is_dirty_add(), "flags {:#x}", added.flags);
    })
    .await;
}

/// The dirty move of a directory: the directory and each node below it dirty-moved, the nested
/// ones too, an added one kept an add, and a deleted one left as it is and not descended into, so
/// the clean node below a deleted directory stays clean; closure kept.
#[tokio::test]
async fn a_dirty_move_of_a_directory_moves_its_subtree_but_its_adds_and_deletes() {
    crate::in_context(async {
        let (tree, c) = subtree_to_move().await;
        ops::dirty_move(&tree, tree.id("d/s"), tree.id("e"), NameChange::Keep)
            .await
            .expect("'d/s' moves");
        let report = closed(&tree).await;
        for path in ["e/s", "e/s/a", "e/s/t", "e/s/t/x"] {
            assert!(report.at(path).expect(path).1.is_dirty_move(), "{path}");
        }
        for path in ["e/s/b", "e/s/u"] {
            assert!(report.at(path).expect(path).1.is_dirty_delete(), "{path}");
        }
        let y = report.at("e/s/u/y").expect("'e/s/u/y'").1;
        assert!(!y.is_dirty(), "flags {:#x}", y.flags);
        let (id, added) = report.at("e/s/c").expect("'e/s/c'");
        assert_eq!(id, c);
        assert!(added.is_dirty_add(), "'e/s/c' stays added");
    })
    .await;
}

/// The dirty move of a deleted directory takes the deletion back off its subtree: each node
/// below dirty-moved.
#[tokio::test]
async fn a_dirty_move_of_a_deleted_directory_takes_its_subtree_back() {
    crate::in_context(async {
        let tree = deleted_subtree().await;
        ops::dirty_move(&tree, tree.id("d/s"), tree.id("e"), NameChange::Keep)
            .await
            .expect("'d/s' moves");
        let report = closed(&tree).await;
        for path in ["e/s/a", "e/s/t", "e/s/t/x"] {
            assert!(report.at(path).expect(path).1.is_dirty_move(), "{path}");
        }
    })
    .await;
}

/// The dirty move of a directory `lore dirty` added keeps every node below it an add, the
/// directory its chain added with no add action too: each takes the directory's add.
#[tokio::test]
async fn a_dirty_move_of_an_added_directory_keeps_every_node_below_it_added() {
    crate::in_context(async {
        let (tree, m) = added_subtree().await;
        ops::dirty_move(&tree, m, tree.id("e"), NameChange::Keep)
            .await
            .expect("'d/m' moves");
        let report = closed(&tree).await;
        for path in ["e/m", "e/m/n", "e/m/k", "e/m/k/x"] {
            assert!(report.at(path).expect(path).1.is_dirty_add(), "{path}");
        }
    })
    .await;
}

/// A move renaming `d/f` to `r` into `e` and within `d`, on a tree holding `d/f`, `d/g` and `e/`,
/// as `moves` makes it: the file under its new name, `moved` holding for it, its old name gone,
/// and `landed` holding for the directory it lands in.
async fn renames_across_and_within(
    moves: impl AsyncFn(&Tree, NodeID, NodeID) -> Result<(), StateError>,
    moved: fn(&Node) -> bool,
    landed: fn(&Node) -> bool,
) {
    for (to, path) in [("e", "e/r"), ("d", "d/r")] {
        let tree = Tree::new(&["d/f", "d/g", "e/"]).await;
        moves(&tree, tree.id("d/f"), tree.id(to))
            .await
            .expect("'d/f' moves");
        let report = closed(&tree).await;
        assert!(moved(&report.at(path).expect(path).1), "{path}");
        assert!(report.at("d/f").is_none(), "{path}: 'd/f' is gone");
        assert!(landed(&report.at(to).expect(to).1), "{to}");
    }
}

/// The dirty move renaming a file into another directory and within its own: the file under its
/// new name, dirty-moved, its old name gone, the directory it lands in marked.
#[tokio::test]
async fn a_dirty_move_renames_across_and_within_directories() {
    crate::in_context(async {
        renames_across_and_within(
            async |tree: &Tree, node: NodeID, to: NodeID| {
                ops::dirty_move(tree, node, to, NameChange::Rename("r")).await
            },
            Node::is_dirty_move,
            Node::is_dirty,
        )
        .await;
    })
    .await;
}

/// The stage's move of a dirty file into a clean directory two levels down: the file staged moved,
/// each level climbed staged and dirty, as a dirty node's ancestors are dirty, and the directory
/// it left staged, closure kept.
#[tokio::test]
async fn a_stage_move_of_a_dirty_file_climbs_dirty_and_stages_the_directory_it_left() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f", "d/g", "e/deep/o"]).await;
        ops::mark_dirty(&tree, tree.id("d/f")).await;
        ops::stage_move(
            &tree,
            tree.id("d/f"),
            tree.id("e/deep"),
            NameChange::Keep,
            None,
        )
        .await
        .expect("'d/f' moves");
        let report = closed(&tree).await;
        let f = report.at("e/deep/f").expect("'e/deep/f'").1;
        assert!(f.is_staged_move() && f.is_dirty(), "flags {:#x}", f.flags);
        for path in ["e/deep", "e"] {
            let level = report.at(path).expect(path).1;
            assert!(level.is_staged() && level.is_dirty(), "{path}");
        }
        assert!(report.at("d").expect("'d'").1.is_staged(), "'d' is staged");
    })
    .await;
}

/// The stage's move of a clean file dirties nothing: the file staged moved, both directories
/// staged alone.
#[tokio::test]
async fn a_stage_move_of_a_clean_file_dirties_nothing() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f", "d/g", "e/"]).await;
        ops::stage_move(&tree, tree.id("d/f"), tree.id("e"), NameChange::Keep, None)
            .await
            .expect("'d/f' moves");
        let report = closed(&tree).await;
        let f = report.at("e/f").expect("'e/f'").1;
        assert!(f.is_staged_move() && !f.is_dirty(), "flags {:#x}", f.flags);
        for path in ["e", "d"] {
            let level = report.at(path).expect(path).1;
            assert!(level.is_staged() && !level.is_dirty(), "{path}");
        }
    })
    .await;
}

/// The stage's move of a file `lore dirty` added stages it an add: it is in no revision a move
/// could name.
#[tokio::test]
async fn a_stage_move_of_an_added_file_stages_it_added() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f", "e/"]).await;
        let n = ops::dirty_add(&tree, tree.id("d"), "", "n", true)
            .await
            .expect("'d/n' is added");
        ops::stage_move(&tree, n, tree.id("e"), NameChange::Keep, None)
            .await
            .expect("'d/n' moves");
        let added = closed(&tree).await.at("e/n").expect("'e/n'").1;
        assert!(added.is_staged_add(), "flags {:#x}", added.flags);
    })
    .await;
}

/// The stage's move of a directory: the directory and each node below it staged moved, the nested
/// ones too, an added one staged an add, and a deleted one left as it is and not descended into,
/// so the clean node below a deleted directory stays unstaged; closure kept.
#[tokio::test]
async fn a_stage_move_of_a_directory_moves_its_subtree_but_its_adds_and_deletes() {
    crate::in_context(async {
        let (tree, c) = subtree_to_move().await;
        ops::stage_move(&tree, tree.id("d/s"), tree.id("e"), NameChange::Keep, None)
            .await
            .expect("'d/s' moves");
        let report = closed(&tree).await;
        for path in ["e/s", "e/s/a", "e/s/t", "e/s/t/x"] {
            assert!(report.at(path).expect(path).1.is_staged_move(), "{path}");
        }
        for path in ["e/s/b", "e/s/u"] {
            let deleted = report.at(path).expect(path).1;
            assert!(
                deleted.is_dirty_delete() && !deleted.is_staged(),
                "{path}: flags {:#x}",
                deleted.flags
            );
        }
        let y = report.at("e/s/u/y").expect("'e/s/u/y'").1;
        assert!(!y.is_staged(), "flags {:#x}", y.flags);
        let (id, added) = report.at("e/s/c").expect("'e/s/c'");
        assert_eq!(id, c);
        assert!(added.is_staged_add(), "'e/s/c' is staged an add");
    })
    .await;
}

/// The stage's move of a deleted directory takes the deletion back off its subtree: each node
/// below staged moved.
#[tokio::test]
async fn a_stage_move_of_a_deleted_directory_takes_its_subtree_back() {
    crate::in_context(async {
        let tree = deleted_subtree().await;
        ops::stage_move(&tree, tree.id("d/s"), tree.id("e"), NameChange::Keep, None)
            .await
            .expect("'d/s' moves");
        let report = closed(&tree).await;
        for path in ["e/s/a", "e/s/t", "e/s/t/x"] {
            assert!(report.at(path).expect(path).1.is_staged_move(), "{path}");
        }
    })
    .await;
}

/// The stage's move of a directory `lore dirty` added stages every node below it an add, the
/// directory its chain added with no add action too: each takes the directory's add.
#[tokio::test]
async fn a_stage_move_of_an_added_directory_stages_every_node_below_it_added() {
    crate::in_context(async {
        let (tree, m) = added_subtree().await;
        ops::stage_move(&tree, m, tree.id("e"), NameChange::Keep, None)
            .await
            .expect("'d/m' moves");
        let report = closed(&tree).await;
        for path in ["e/m", "e/m/n", "e/m/k", "e/m/k/x"] {
            assert!(report.at(path).expect(path).1.is_staged_add(), "{path}");
        }
    })
    .await;
}

/// The stage's move out of a directory no commit recorded, as `lore dirty` adds one above a file
/// it marks, its address zero, leaves that directory unstaged, for the commit's prune.
#[tokio::test]
async fn a_stage_move_out_of_a_directory_no_commit_recorded_leaves_it_unstaged() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f", "e/"]).await;
        let n = ops::dirty_add(&tree, tree.id("d"), "m", "n", true)
            .await
            .expect("'d/m/n' is added");
        ops::stage_move(&tree, n, tree.id("e"), NameChange::Keep, None)
            .await
            .expect("'d/m/n' moves");
        let report = closed(&tree).await;
        let m = report.at("d/m").expect("'d/m'").1;
        assert!(m.address.is_zero(), "no commit recorded 'd/m'");
        assert!(!m.is_staged(), "flags {:#x}", m.flags);
    })
    .await;
}

/// The stage's move renaming a file into another directory and within its own: the file under its
/// new name, staged moved, its old name gone, the directory it lands in staged.
#[tokio::test]
async fn a_stage_move_renames_across_and_within_directories() {
    crate::in_context(async {
        renames_across_and_within(
            async |tree: &Tree, node: NodeID, to: NodeID| {
                ops::stage_move(tree, node, to, NameChange::Rename("r"), None).await
            },
            Node::is_staged_move,
            Node::is_staged,
        )
        .await;
    })
    .await;
}

/// The stage's move of a file over the file its name holds: the holder staged and dirty deleted by
/// its own node, once the moved file left its directory, the moved file staged moved; closure
/// kept.
#[tokio::test]
async fn a_stage_move_over_a_held_name_stages_the_holder_deleted() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f", "e/f"]).await;
        let (moved, held) = (tree.id("d/f"), tree.id("e/f"));
        ops::stage_move(&tree, moved, tree.id("e"), NameChange::Keep, Some(held))
            .await
            .expect("'d/f' moves");
        let report = closed_but_held_twice(&tree).await;
        let holder = report.nodes[&held];
        assert!(
            holder.is_staged_delete() && holder.is_dirty_delete(),
            "flags {:#x}",
            holder.flags
        );
        let node = report.nodes[&moved];
        assert!(node.is_staged_move(), "flags {:#x}", node.flags);
    })
    .await;
}

/// The stage's move of a directory over the directory above it, its name the parent's: the old
/// parent and what stays in it staged and dirty deleted, the moved directory's subtree staged
/// moved, as the deletion is staged once the directory left it.
#[tokio::test]
async fn a_stage_move_of_a_directory_over_its_parent_keeps_its_subtree() {
    crate::in_context(async {
        let tree = Tree::new(&["a/b/x", "a/y"]).await;
        let (a, b) = (tree.id("a"), tree.id("a/b"));
        ops::stage_move(&tree, b, ROOT_NODE, NameChange::Rename("a"), Some(a))
            .await
            .expect("'a/b' moves");
        let report = closed_but_held_twice(&tree).await;
        for (path, node) in [("a", a), ("a/y", tree.id("a/y"))] {
            let deleted = report.nodes[&node];
            assert!(
                deleted.is_staged_delete() && deleted.is_dirty_delete(),
                "{path}: flags {:#x}",
                deleted.flags
            );
        }
        for (path, node) in [("a/b", b), ("a/b/x", tree.id("a/b/x"))] {
            let moved = report.nodes[&node];
            assert!(moved.is_staged_move(), "{path}: flags {:#x}", moved.flags);
        }
    })
    .await;
}

/// The stage's move whose name's lookup found the node itself, as a rename of the case alone
/// does, deletes nothing: the node renamed in place, staged moved, its old spelling gone.
#[tokio::test]
async fn a_stage_move_replacing_itself_deletes_nothing() {
    crate::in_context(async {
        let tree = Tree::new(&["d/F"]).await;
        let node = tree.id("d/F");
        ops::stage_move(
            &tree,
            node,
            tree.id("d"),
            NameChange::Rename("f"),
            Some(node),
        )
        .await
        .expect("'d/F' moves");
        let report = closed(&tree).await;
        assert!(report.at("d/F").is_none(), "'d/F' is renamed");
        let (id, renamed) = report.at("d/f").expect("'d/f'");
        assert_eq!(id, node);
        assert!(
            renamed.is_staged_move() && !renamed.is_staged_delete(),
            "flags {:#x}",
            renamed.flags
        );
    })
    .await;
}

/// The stage's move over a directory a C API delete has begun on, its first level staged for
/// deletion and not yet the levels below, as the delete stages them one at a time: the move's
/// deletion leaves a node staged for deletion as it is and does not descend into it, so a clean
/// node below one stays clean, whether that node is the holder's child or the holder itself;
/// closure kept.
#[tokio::test]
async fn a_stage_move_over_a_staged_deletion_leaves_what_it_staged() {
    crate::in_context(async {
        let tree = Tree::new(&["p/s/n", "q/r/a/x", "q/r/b"]).await;
        let (r, a) = (tree.id("q/r"), tree.id("q/r/a"));
        ops::staged_delete_level(&tree, a, r).await;
        let s = tree.id("p/s");
        ops::stage_move(&tree, s, tree.id("q"), NameChange::Rename("r"), Some(r))
            .await
            .expect("'p/s' moves");
        let report = closed_but_held_twice(&tree).await;
        for (path, node) in [("q/r", r), ("q/r/b", tree.id("q/r/b"))] {
            let deleted = report.nodes[&node];
            assert!(
                deleted.is_staged_delete(),
                "{path}: flags {:#x}",
                deleted.flags
            );
        }
        let x = report.nodes[&tree.id("q/r/a/x")];
        assert!(!x.is_staged() && !x.is_dirty(), "flags {:#x}", x.flags);

        let tree = Tree::new(&["p/s/n", "q/r/a"]).await;
        let r = tree.id("q/r");
        ops::staged_delete_level(&tree, r, tree.id("q")).await;
        let s = tree.id("p/s");
        ops::stage_move(&tree, s, tree.id("q"), NameChange::Rename("r"), Some(r))
            .await
            .expect("'p/s' moves");
        let report = closed_but_held_twice(&tree).await;
        let a = report.nodes[&tree.id("q/r/a")];
        assert!(!a.is_staged() && !a.is_dirty(), "flags {:#x}", a.flags);
    })
    .await;
}

/// The realized move of a file: staged moved, merged in a merge alone, the destination climbed
/// and the directory it left staged, nothing dirtied, closure kept.
#[tokio::test]
async fn a_realize_move_stages_the_directory_it_left_and_marks_a_merge() {
    crate::in_context(async {
        for merge in [false, true] {
            let tree = Tree::new(&["d/f", "d/g", "e/"]).await;
            ops::realize_move(&tree, tree.id("d/f"), tree.id("e"), NameChange::Keep, merge)
                .await
                .expect("'d/f' moves");
            let report = closed(&tree).await;
            let f = report.at("e/f").expect("'e/f'").1;
            assert!(f.is_staged_move() && !f.is_dirty(), "flags {:#x}", f.flags);
            assert_eq!(f.is_staged_merge(), merge, "flags {:#x}", f.flags);
            for path in ["e", "d"] {
                let level = report.at(path).expect(path).1;
                assert!(level.is_staged() && !level.is_dirty(), "{path}");
            }
        }
    })
    .await;
}

/// The realized move renaming a file into another directory and within its own: the file under
/// its new name, staged moved, its old name gone, the directory it lands in staged.
#[tokio::test]
async fn a_realize_move_renames_across_and_within_directories() {
    crate::in_context(async {
        renames_across_and_within(
            async |tree: &Tree, node: NodeID, to: NodeID| {
                ops::realize_move(tree, node, to, NameChange::Rename("r"), false).await
            },
            Node::is_staged_move,
            Node::is_staged,
        )
        .await;
    })
    .await;
}
