// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The checkpoint (`Model::checkpoint`): the model held against the State, the orphans, the
//! invariants under the rules the model gives, and the round trip, at a quiescent point; then the
//! clean blocks evicted and the model going on from the log.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;

use lore_base::types::Hash;
use lore_revision::node::INVALID_NODE;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::NameChange;
use lore_revision::state::State;

use super::STATE;
use crate::harness::catalogue::modelled::Modelling;
use crate::harness::catalogue::ops;
use crate::harness::fixtures::Tree;
use crate::harness::invariants;
use crate::harness::invariants::Report;
use crate::harness::invariants::Rules;
use crate::harness::log::recorded;
use crate::harness::model::Model;
use crate::harness::model::Modelled;
use crate::harness::model::Snapshot;
use crate::harness::model::Start;
use crate::harness::model::checkpoint;
use crate::harness::model::checkpoint::Checkpoint;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Scenario;

/// `body` run on a current-thread runtime, as a scenario's check runs.
fn on_runtime<T>(body: impl Future<Output = T>) -> T {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the runtime starts");
    runtime.block_on(
        lore_base::runtime::LORE_CONTEXT.scope(crate::harness::fixtures::execution(), body),
    )
}

/// `tree`'s model, started where it stands.
async fn modelled(tree: &Tree) -> Model {
    Start::read(tree).await.model(tree, &[]).await
}

/// `change` made with recording on, its records replayed into `model`, which finds nothing.
async fn logged(model: &mut Model, change: impl Future<Output = ()>) {
    let ((), records) = recorded(change).await;
    model.replay(&records);
    let violations: Vec<String> = model.violations().iter().map(ToString::to_string).collect();
    assert!(violations.is_empty(), "the model found: {violations:#?}");
}

/// A checkpoint where an operation of the run was dropped.
fn dropped() -> Checkpoint {
    Checkpoint {
        dropped: true,
        ..Checkpoint::default()
    }
}

/// Asserts `violations` hold one saying `what`.
fn found(violations: &[String], what: &str) {
    assert!(
        violations.iter().any(|violation| violation.contains(what)),
        "a violation saying '{what}': {violations:#?}"
    );
}

/// Asserts `violations` hold none saying `what`.
fn none(violations: &[String], what: &str) {
    assert!(
        !violations.iter().any(|violation| violation.contains(what)),
        "no violation saying '{what}': {violations:#?}"
    );
}

#[test]
fn a_checkpoint_of_changes_the_log_tells_finds_nothing_and_the_model_goes_on() {
    on_runtime(async {
        let tree = Tree::new(&["d/f", "d/g", "e/"]).await;
        let mut model = modelled(&tree).await;
        logged(&mut model, async {
            ops::mark_dirty(&tree, tree.id("d/f")).await;
            let created = ops::checked_create(&tree, tree.id("e"), "n").await;
            assert!(
                ops::attached(created).is_some(),
                "'e/n' attaches: {created:?}"
            );
            assert!(ops::unserved_detach(&tree, tree.id("d/g")).await);
        })
        .await;
        let checked = model.checkpoint(&tree, Checkpoint::default()).await;
        assert!(checked.violations.is_empty(), "{:#?}", checked.violations);
        assert_eq!(
            checked.saved, 1,
            "the State the run wrote, not the committed one"
        );
        let checked = model.checkpoint(&tree, Checkpoint::default()).await;
        assert_eq!(checked.saved, 0, "nothing written since");
        logged(&mut model, async {
            ops::scan_clear(&tree, tree.id("d/f")).await;
        })
        .await;
        let skipped = Checkpoint {
            round_trip: false,
            ..Checkpoint::default()
        };
        assert_eq!(model.checkpoint(&tree, skipped).await.saved, 0);
        let checked = model.checkpoint(&tree, Checkpoint::default()).await;
        assert!(checked.violations.is_empty(), "{:#?}", checked.violations);
        assert_eq!(checked.saved, 1, "written still, as no round trip took it");
    });
}

#[test]
fn a_checkpoint_evicts_the_clean_blocks_and_the_model_follows_their_next_load() {
    on_runtime(async {
        let tree = Tree::spread(&["d/f"], "fill", &["e/x"])
            .await
            .reloaded()
            .await;
        let later = NodeBlock::index(tree.id("e/x"));
        assert_ne!(later, NodeBlock::index(tree.id("d/f")));
        let mut model = modelled(&tree).await;
        let read = async {
            tree.state
                .node(tree.repository.clone(), tree.id("e/x"))
                .await
                .expect("'e/x' reads");
        };
        logged(&mut model, async {
            read.await;
            ops::mark_dirty(&tree, tree.id("d/f")).await;
        })
        .await;
        let checked = model.checkpoint(&tree, Checkpoint::default()).await;
        assert!(checked.violations.is_empty(), "{:#?}", checked.violations);
        assert!(
            tree.state.resident_block(later).is_none(),
            "the clean block left memory"
        );
        logged(&mut model, async {
            tree.state
                .node(tree.repository.clone(), tree.id("e/x"))
                .await
                .expect("'e/x' reads again");
        })
        .await;
        let checked = model.checkpoint(&tree, Checkpoint::default()).await;
        assert!(checked.violations.is_empty(), "{:#?}", checked.violations);
    });
}

#[test]
fn a_state_taking_fresh_blocks_keeps_them_across_a_checkpoint() {
    on_runtime(async {
        let tree = Tree::new(&["a/x"]).await;
        tree.state
            .take_new_nodes_from_fresh_blocks(tree.repository.clone())
            .await
            .expect("new nodes come from fresh blocks");
        tree.state.mark_served();
        let mut model = modelled(&tree).await;
        let mut made = None;
        logged(&mut model, async {
            made = ops::served_create(&tree, tree.id("a"), "n").await;
        })
        .await;
        let made = made.expect("'a/n' attaches");
        let checked = model.checkpoint(&tree, Checkpoint::default()).await;
        assert!(checked.violations.is_empty(), "{:#?}", checked.violations);
        assert_eq!(checked.saved, 0, "a served State is never saved");
        assert!(
            tree.state.resident_block(NodeBlock::index(made)).is_some(),
            "the fresh block stays in memory"
        );
        logged(&mut model, async {
            let created = ops::served_create(&tree, tree.id("a"), "m").await;
            assert!(created.is_some(), "'a/m' attaches");
        })
        .await;
        let checked = model.checkpoint(&tree, Checkpoint::default()).await;
        assert!(checked.violations.is_empty(), "{:#?}", checked.violations);
    });
}

#[test]
fn a_name_respelled_behind_the_model_differs() {
    on_runtime(async {
        let tree = Tree::new(&["d/f"]).await;
        let mut model = modelled(&tree).await;
        // No scheduler installed: nothing recorded. A respelling keeps the hash and the count.
        tree.state
            .node_rename(
                tree.repository.clone(),
                tree.id("d/f"),
                NameChange::Rename("F"),
            )
            .await
            .expect("'d/f' is respelled");
        let checked = model.checkpoint(&tree, Checkpoint::default()).await;
        found(&checked.violations, "name");
    });
}

#[test]
fn a_detached_directorys_chain_differs() {
    on_runtime(async {
        let tree = Tree::new(&["d/f", "d/g"]).await;
        let mut model = modelled(&tree).await;
        logged(&mut model, async {
            assert!(ops::unserved_detach(&tree, tree.id("d")).await);
        })
        .await;
        let block = invariants::block_of(&tree.state, &tree.repository, tree.id("d")).await;
        tree.state
            .node_edit_for_tests(&block, tree.id("d"), |node| node.child = INVALID_NODE);
        let checked = model.checkpoint(&tree, Checkpoint::default()).await;
        found(&checked.violations, "'s chain");
    });
}

/// The model of `tree` told `d/f` turned dirty under a clean `d`, by a test's own edit: the model
/// and the State agree, and closure does not hold.
async fn dirty_below_clean(tree: &Tree) -> Model {
    let mut model = modelled(tree).await;
    model.admit_unlabelled();
    logged(&mut model, async {
        let block = invariants::block_of(&tree.state, &tree.repository, tree.id("d/f")).await;
        tree.state
            .node_edit_for_tests(&block, tree.id("d/f"), |node| {
                node.flags |= NodeFlags::Dirty.bits();
            });
    })
    .await;
    model
}

#[test]
fn closure_is_held_where_the_model_says_it_holds() {
    on_runtime(async {
        // Unserved: excused by an operation dropped, or by an unfinished change the State held as
        // the run began, held otherwise; one noted during the run excuses only the breaches its
        // call's records formed, and the test's own edit is none of them.
        let tree = Tree::new(&["d/f"]).await;
        let mut model = dirty_below_clean(&tree).await;
        none(
            &model.checkpoint(&tree, dropped()).await.violations,
            "under clean",
        );
        found(
            &model
                .checkpoint(&tree, Checkpoint::default())
                .await
                .violations,
            "under clean",
        );
        logged(&mut model, async { tree.state.note_unfinished_change() }).await;
        found(
            &model
                .checkpoint(&tree, Checkpoint::default())
                .await
                .violations,
            "under clean",
        );
        let tree = Tree::new(&["d/f"]).await;
        tree.state.note_unfinished_change();
        let mut model = dirty_below_clean(&tree).await;
        none(
            &model
                .checkpoint(&tree, Checkpoint::default())
                .await
                .violations,
            "under clean",
        );
        // Served: held whatever was dropped.
        let tree = Tree::new(&["d/f"]).await;
        tree.state.mark_served();
        let mut model = dirty_below_clean(&tree).await;
        found(
            &model.checkpoint(&tree, dropped()).await.violations,
            "under clean",
        );
    });
}

#[test]
fn extension_words_are_held_to_zero_on_a_state_never_served() {
    on_runtime(async {
        for served in [false, true] {
            let tree = Tree::new(&["d/f"]).await;
            if served {
                tree.state.mark_served();
            }
            let mut model = modelled(&tree).await;
            model.admit_unlabelled();
            logged(&mut model, async {
                let block =
                    invariants::block_of(&tree.state, &tree.repository, tree.id("d/f")).await;
                tree.state
                    .node_edit_for_tests(&block, tree.id("d/f"), |node| {
                        node.ext_flags = NodeExtFlags::LocalContent.bits();
                    });
            })
            .await;
            let checked = model.checkpoint(&tree, Checkpoint::default()).await;
            if served {
                none(&checked.violations, "ext flags");
                assert_eq!(checked.saved, 0, "a served State is never saved");
            } else {
                found(&checked.violations, "ext flags");
            }
        }
    });
}

#[test]
fn the_rules_follow_the_model() {
    let rules = |snapshot: Snapshot, dropped: bool| {
        let mut model = Model::new();
        model.start(STATE, &snapshot);
        let rules: Rules = checkpoint::rules(model.state(STATE).expect("the model"), dropped);
        (rules.dirty_closure, rules.unique_names, rules.no_ext_flags)
    };
    assert_eq!(rules(super::snapshot(), false), (true, false, true));
    assert_eq!(rules(super::snapshot(), true), (false, false, true));
    let unfinished = Snapshot {
        unfinished: true,
        ..super::snapshot()
    };
    assert_eq!(rules(unfinished, false), (false, false, true));
    assert_eq!(rules(super::served(), true), (true, true, false));
}

#[test]
fn the_round_trip_names_what_a_load_holds_otherwise() {
    on_runtime(async {
        let tree = Tree::new(&["d/f", "d/g"]).await;
        let saved = tree.saved().await;
        let walk = async |state| {
            invariants::check(state, &tree.repository, Rules::default())
                .await
                .expect("the State keeps the invariants")
        };
        let loaded = walk(&saved).await;
        assert!(checkpoint::by_path(&walk(&tree.state).await, &loaded).is_empty());
        ops::mark_dirty(&tree, tree.id("d/f")).await;
        assert!(ops::unserved_detach(&tree, tree.id("d/g")).await);
        let differences = checkpoint::by_path(&walk(&tree.state).await, &loaded);
        found(&differences, "'d/f'");
        found(&differences, "'d/g'");
    });
}

/// A report of `nodes`, each at its path.
fn report(nodes: &[(NodeID, &str, Node)]) -> Report {
    let mut report = Report::default();
    for (id, path, node) in nodes {
        report.nodes.insert(*id, *node);
        report.paths.insert(*id, (*path).to_string());
    }
    report
}

/// An edit of a node's record.
type Edit = fn(&mut Node);

#[test]
fn by_path_names_each_way_a_load_differs() {
    let file = Node {
        flags: NodeFlags::File.bits(),
        size: 3,
        mode: 0o644,
        ..Node::default()
    };
    let link = Node {
        flags: NodeFlags::Link.bits(),
        child: 9,
        ..Node::default()
    };
    let live = report(&[(1, "d/f", file), (2, "d/l", link), (3, "d/g", file)]);
    assert!(checkpoint::by_path(&live, &live).is_empty());
    let edits: [(NodeID, &str, Edit); 6] = [
        (1, "'d/f'", |node| node.flags |= NodeFlags::Dirty.bits()),
        (1, "'d/f'", |node| {
            node.ext_flags = NodeExtFlags::LocalContent.bits();
        }),
        (1, "'d/f'", |node| node.address.hash = Hash::from_u64(7)),
        (1, "'d/f'", |node| node.size = 4),
        (1, "'d/f'", |node| node.mode = 0o600),
        (2, "'d/l'", |node| node.child = 10),
    ];
    for (id, path, edit) in edits {
        let mut loaded = live.clone();
        edit(loaded.nodes.get_mut(&id).expect("the node"));
        found(&checkpoint::by_path(&live, &loaded), path);
    }
    let mut lost = live.clone();
    lost.nodes.remove(&3);
    lost.paths.remove(&3);
    found(
        &checkpoint::by_path(&live, &lost),
        "'d/g' is live and not in the load",
    );
    let mut elsewhere = live.clone();
    elsewhere.paths.insert(1, "d/k".to_string());
    found(
        &checkpoint::by_path(&live, &elsewhere),
        "is at 'd/f' live, at 'd/k' loaded",
    );
    let mut gained = live.clone();
    gained.nodes.insert(4, file);
    gained.paths.insert(4, "d/h".to_string());
    found(
        &checkpoint::by_path(&live, &gained),
        "'d/h' is in the load and not live",
    );
}

/// A tree whose context tells a fault the run met, or none.
struct Faulting {
    tree: Tree,
    faulted: bool,
}

impl Modelled for Faulting {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }

    fn faulted(&self) -> bool {
        self.faulted
    }
}

#[test]
fn a_fault_the_context_tells_excuses_closure_on_an_unserved_state() {
    on_runtime(async {
        for faulted in [false, true] {
            let tree = Tree::new(&["d/f"]).await;
            let mut model = dirty_below_clean(&tree).await;
            let context = Faulting { tree, faulted };
            let checked = model.checkpoint(&context, Checkpoint::default()).await;
            if faulted {
                none(&checked.violations, "under clean");
            } else {
                found(&checked.violations, "under clean");
            }
        }
    });
}

#[test]
fn an_orphan_below_a_root_detached_clean_counts_unserved_and_fails_served() {
    on_runtime(async {
        for served in [false, true] {
            let tree = Tree::new(&["d/f", "e/"]).await;
            if served {
                tree.state.mark_served();
            }
            let mut model = modelled(&tree).await;
            model.admit_unlabelled();
            logged(&mut model, async {
                assert!(ops::swap_detach_if_clean(&tree, tree.id("d")).await);
                // `f` turned dirty below the root detached clean: what H7 left.
                let block =
                    invariants::block_of(&tree.state, &tree.repository, tree.id("d/f")).await;
                tree.state
                    .node_edit_for_tests(&block, tree.id("d/f"), |node| {
                        node.flags |= NodeFlags::Dirty.bits();
                    });
            })
            .await;
            let checked = model.checkpoint(&tree, Checkpoint::default()).await;
            if served {
                found(&checked.violations, "dirty below a root detached clean");
                assert_eq!(checked.orphans, 0);
            } else {
                assert!(checked.violations.is_empty(), "{:#?}", checked.violations);
                assert_eq!(checked.orphans, 1);
            }
        }
    });
}

#[test]
fn an_unserved_state_taking_fresh_blocks_takes_no_round_trip() {
    on_runtime(async {
        let tree = Tree::new(&["a/x"]).await;
        tree.state
            .take_new_nodes_from_fresh_blocks(tree.repository.clone())
            .await
            .expect("new nodes come from fresh blocks");
        let mut model = modelled(&tree).await;
        logged(&mut model, async {
            let created = ops::checked_create(&tree, tree.id("a"), "n").await;
            assert!(
                ops::attached(created).is_some(),
                "'a/n' attaches: {created:?}"
            );
        })
        .await;
        let checked = model.checkpoint(&tree, Checkpoint::default()).await;
        assert!(checked.violations.is_empty(), "{:#?}", checked.violations);
        assert_eq!(
            checked.saved, 0,
            "a State taking fresh blocks is never saved"
        );
    });
}

#[test]
fn the_round_trip_names_a_save_or_a_load_that_fails() {
    on_runtime(async {
        let tree = Tree::new(&["d/f"]).await;
        let walk = async || {
            invariants::check(&tree.state, &tree.repository, Rules::default())
                .await
                .expect("the State keeps the invariants")
        };
        let saved = tree.save().await.expect("the State saves");
        assert!(
            checkpoint::round_trip(7, &tree.repository, walk().await, Ok(saved))
                .await
                .is_empty()
        );
        let refused = tree
            .state
            .node(tree.repository.clone(), 99_999)
            .await
            .expect_err("no node 99999");
        found(
            &checkpoint::round_trip(7, &tree.repository, walk().await, Err(refused)).await,
            "does not save",
        );
        found(
            &checkpoint::round_trip(7, &tree.repository, walk().await, Ok(Hash::from_u64(99)))
                .await,
            "does not load",
        );
    });
}

/// A walk's record of a node below `parent`: a file holding content `seed` with mode 0o644, or a
/// directory.
fn below(parent: NodeID, file: bool, seed: u64) -> Node {
    Node {
        flags: if file { NodeFlags::File.bits() } else { 0 },
        parent,
        name_hash: seed,
        address: lore_base::types::Address {
            hash: Hash::from_u64(seed),
            ..Default::default()
        },
        mode: if file { 0o644 } else { 0 },
        ..Node::default()
    }
}

/// C11c's check of a served walk against a committed one holds a clean directory to the children
/// of the committed directory it stands for, as a multiset of name hash, kind, content and a file's
/// mode: each difference is found; the root, a dirty directory, one along a swap's changed paths
/// and one standing for no committed node are passed over, and a committed counterpart that is no
/// directory is found.
#[test]
fn committed_children_holds_a_clean_directory_to_its_committed_ones() {
    let dir = below(ROOT_NODE, false, 1);
    let served = |edit: Edit| {
        let mut g = below(1, true, 3);
        edit(&mut g);
        report(&[
            (ROOT_NODE, "", Node::default()),
            (1, "d", dir),
            (2, "d/f", below(1, true, 2)),
            (3, "d/g", g),
            // The root's children differ, the root never marked.
            (4, "r", below(ROOT_NODE, true, 4)),
        ])
    };
    let committed = report(&[
        (ROOT_NODE, "", Node::default()),
        (11, "d", dir),
        (13, "d/g", below(11, true, 3)),
        (12, "d/f", below(11, true, 2)),
    ]);
    let committed_of: BTreeMap<NodeID, NodeID> = [(1, 11), (2, 12), (3, 13)].into();
    let check = |served: &Report, committed_of: &BTreeMap<NodeID, NodeID>, swapped: &[NodeID]| {
        let swapped: BTreeSet<NodeID> = swapped.iter().copied().collect();
        invariants::committed_children(served, (&committed, committed_of), &swapped)
    };
    let same: Edit = |_| {};
    assert!(check(&served(same), &committed_of, &[]).is_empty());
    let edits: [Edit; 4] = [
        |g| g.address.hash = Hash::from_u64(7),
        |g| g.mode = 0o755,
        |g| g.name_hash = 7,
        |g| g.flags = NodeFlags::Link.bits(),
    ];
    for edit in edits {
        found(
            &check(&served(edit), &committed_of, &[]),
            "clean 'd' (1) holds",
        );
    }
    let mut missing = served(same);
    missing.nodes.remove(&3);
    missing.paths.remove(&3);
    found(&check(&missing, &committed_of, &[]), "holds 1 children");
    let mut extra = served(same);
    extra.nodes.insert(5, below(1, true, 5));
    extra.paths.insert(5, "d/h".to_string());
    found(&check(&extra, &committed_of, &[]), "holds 3 children");

    let differing = served(edits[0]);
    let mut dirty = differing.clone();
    if let Some(d) = dirty.nodes.get_mut(&1) {
        d.flags |= NodeFlags::Dirty.bits();
    }
    assert!(
        check(&dirty, &committed_of, &[]).is_empty(),
        "a dirty directory"
    );
    assert!(
        check(&differing, &committed_of, &[1]).is_empty(),
        "a swap's path"
    );
    let unmapped: BTreeMap<NodeID, NodeID> = [(2, 12), (3, 13)].into();
    assert!(
        check(&differing, &unmapped, &[]).is_empty(),
        "no committed node"
    );
    let to_a_file: BTreeMap<NodeID, NodeID> = [(1, 12)].into();
    found(&check(&served(same), &to_a_file, &[]), "no live directory");
}

/// The checkpoint holds a served State's clean directories to their committed ones, found by node
/// (C11c): a clean `d` whose file holds other content than the committed one's, as no served
/// change leaves it, is found; not where `d` is dirty, nor where a change the model could not tell
/// a swap's from came since, its changed paths unknown.
#[test]
fn the_checkpoint_holds_a_served_states_clean_directories_to_their_committed_ones() {
    on_runtime(async {
        for (dirty, uncalled, finds) in [
            (false, false, true),
            (true, false, false),
            (false, true, false),
        ] {
            let tree = Tree::new(&["d/f", "d/g", "e/"]).await;
            tree.state.mark_served();
            let f = tree.id("d/f");
            let block = invariants::block_of(&tree.state, &tree.repository, f).await;
            tree.state.node_edit_for_tests(&block, f, |node| {
                node.address.hash = Hash::from_u64(77);
            });
            if dirty {
                assert!(ops::served_mark_dir(&tree, tree.id("d")).await);
            }
            let mut model = modelled(&tree).await;
            if uncalled {
                logged(&mut model, async {
                    assert!(ops::served_mark_dir(&tree, tree.id("e")).await);
                })
                .await;
            }
            let checked = model.checkpoint(&tree, Checkpoint::default()).await;
            if finds {
                found(&checked.violations, "(C11c)");
            } else {
                none(&checked.violations, "(C11c)");
            }
        }
    });
}

/// A swap's rewrite of a file in a clean directory leaves the directory holding other content than
/// its committed one's until the swap's settle, along its changed path, which the checkpoint passes
/// over, its call telling it a swap's (C11c).
#[test]
fn a_swaps_change_below_a_clean_directory_is_passed_over() {
    let scenario = Scenario::<Tree> {
        name: "a swap's rewrite below a clean directory",
        setup: Box::new(|| {
            Box::pin(async {
                let tree = Tree::new(&["d/f", "e/"]).await;
                tree.state.mark_served();
                tree
            })
        }),
        ops: vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let content = lore_revision::state::Content {
                    size: 2000,
                    ..Default::default()
                };
                assert!(ops::swap_rewrite(&tree, tree.id("d/f"), &content).await);
            })
        })],
        cancellable: Vec::new(),
        check: Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let d = tree.state.node(tree.repository.clone(), tree.id("d")).await;
                assert!(!d.expect("'d' reads").is_dirty(), "'d' stays clean");
            })
        }),
    };
    crate::explored(scenario, Modelling::default(), Exploration::default());
}
