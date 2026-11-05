// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The arms and permit records one operation reaches on its own: a watch losing its install or
//! its grow, a build's publish meeting a log installed since its lookup, a served rewrite
//! following its file, a copy-up marking its directory, and the moves permit taken and released;
//! the resident find's walk again, with a rename on another thread in its window; and the resident
//! attach meeting a block out of memory at each step that needs one, answering `None` with the
//! block left unread.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;

use lore_revision::node::CHAIN_LOG_INITIAL;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::AttachCheck;
use lore_revision::state::AttachOutcome;
use lore_revision::state::Content;
use lore_revision::state::State;
use lore_revision::state::TreeAttach;
use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::log;
use lore_revision::state::node::hooks::log::Arm;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Expected;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;
use lore_storage::hash::hash_string;

use super::Body;
use super::InWindow;
use super::Setup;
use super::logged;
use super::logged_runs;
use super::of;
use super::one;
use super::word_records;
use crate::current_thread;
use crate::harness::catalogue::modelled::Labels;
use crate::harness::catalogue::modelled::check_log;
use crate::harness::catalogue::ops;
use crate::harness::catalogue::ops::file;
use crate::harness::faults::FailingStore;
use crate::harness::fixtures::Tree;
use crate::harness::log::Collector;
use crate::harness::model::Modelled;
use crate::harness::model::Start;
use crate::harness::model::compared;
use crate::harness::scheduler::Exploration;
use crate::in_context;

/// A watch whose install hook installs the directory's log first loses its install, then marks
/// the log the other installed.
#[test]
fn a_watch_beaten_to_its_install_logs_its_loss() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/f"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let d = tree.id("d");
                let block = tree
                    .state
                    .block(tree.repository.clone(), NodeBlock::index(d))
                    .await
                    .expect("the directory's block");
                let index = Node::index(d);
                let beaten = || {
                    let _ = block.watch_chain(index, None);
                };
                let _ = block.watch_chain(index, Some(&beaten));
            })
        }),
    );
    let records = of(&records, &tree.state);
    one(&records, "lost install", |effect| {
        *effect == Effect::Arm(Arm::WatchLostInstall)
    });
}

/// A watch growing a full log, beaten to the grow by another in its gap, logs its loss.
#[test]
fn a_watch_beaten_to_its_grow_logs_its_loss() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/f"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let d = tree.id("d");
                let block = tree
                    .state
                    .block(tree.repository.clone(), NodeBlock::index(d))
                    .await
                    .expect("the directory's block");
                let index = Node::index(d);
                let _ = block.watch_chain(index, None);
                for added in 0..=CHAIN_LOG_INITIAL {
                    let name = format!("g{added}");
                    tree.state
                        .node_add(tree.repository.clone(), d, file(&name), &name)
                        .await
                        .expect("an add");
                }
                let beaten = || {
                    let _ = block.watch_chain(index, None);
                };
                let _ = block.watch_chain(index, Some(&beaten));
            })
        }),
    );
    let records = of(&records, &tree.state);
    one(&records, "lost grow", |effect| {
        *effect == Effect::Arm(Arm::WatchLostGrow)
    });
}

/// A build's publish that finds a log installed on its parent since its lookup records the arm in
/// its section, and publishes against the count its lookup read.
#[test]
fn a_build_publish_meeting_a_log_since_its_lookup_logs_its_arm() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let d = tree.id("d");
                let root = Node {
                    name_hash: hash_string("c"),
                    ..Node::default()
                };
                let build = match tree
                    .state
                    .swap()
                    .node_attach_tree(tree.repository.clone(), d, &root, "c")
                    .await
                    .expect("the build begins")
                {
                    TreeAttach::Building(build) => build,
                    TreeAttach::Exists(_) => panic!("'d/c' is free"),
                };
                // A listing's watch of 'd' between the lookup and the publish.
                let block = tree
                    .state
                    .block(tree.repository.clone(), NodeBlock::index(d))
                    .await
                    .expect("the directory's block");
                let _ = block.watch_chain(Node::index(d), None);
                let published = build.publish().await.expect("the publish runs");
                assert!(matches!(published, AttachOutcome::Attached(_)));
            })
        }),
    );
    let records = of(&records, &tree.state);
    let d = tree.id("d");
    let arm = one(&records, "arm", |effect| {
        *effect == Effect::Arm(Arm::LogBetweenLookupAndPublish)
    });
    let publish = one(&records, "publish", |effect| {
        matches!(effect, Effect::Publish { parent, check: log::Check::Unchanged, expected: Expected::Count { .. }, .. }
            if *parent == d)
    });
    assert_eq!(
        (arm.seq, arm.section),
        (publish.seq, Section::Publish),
        "the arm is in the publish's section"
    );
    assert!(arm.sub < publish.sub);
}

/// Any other publish checking its count, as an attach's, records no arm for a log it meets.
#[test]
fn an_attach_meeting_a_log_logs_no_build_arm() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let d = tree.id("d");
                let block = tree
                    .state
                    .block(tree.repository.clone(), NodeBlock::index(d))
                    .await
                    .expect("the directory's block");
                let _ = block.watch_chain(Node::index(d), None);
                let attached = tree
                    .state
                    .node_attach(tree.repository.clone(), d, file("x"), "x")
                    .await
                    .expect("the attach runs");
                assert!(matches!(attached, AttachOutcome::Attached(_)));
            })
        }),
    );
    let records = of(&records, &tree.state);
    one(&records, "publish", |effect| {
        matches!(
            effect,
            Effect::Publish {
                check: log::Check::Unchanged,
                ..
            }
        )
    });
    assert!(
        !records
            .iter()
            .any(|record| record.effect == Effect::Arm(Arm::LogBetweenLookupAndPublish)),
        "only a build's publish records the arm: {records:#?}"
    );
}

/// A served rewrite told the directory a file left follows the file, recording the arm in its
/// section.
#[test]
fn a_served_rewrite_of_a_moved_file_logs_its_follow() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f", "e/"]).await;
                tree.state
                    .node_reattach(tree.repository.clone(), tree.id("d/f"), tree.id("e"), "f")
                    .await
                    .expect("'d/f' moves");
                tree.state.mark_served();
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                tree.state
                    .served()
                    .node_rewrite(
                        &tree.repository,
                        tree.id("d/f"),
                        tree.id("d"),
                        &Content::default(),
                        (NodeFlags::DirtyModify, NodeExtFlags::NoFlags),
                    )
                    .await
                    .expect("the rewrite runs");
            })
        }),
    );
    let records = of(&records, &tree.state);
    let follow = one(&records, "follow", |effect| {
        *effect == Effect::Arm(Arm::RewriteFollow)
    });
    assert_eq!(follow.section, Section::Rewrite);
}

/// A copy-up below a clean served directory marks it and rewrites again: the arm, then the
/// file's word.
#[test]
fn a_copy_up_below_a_clean_directory_logs_its_remark() {
    let (records, tree) = logged(
        || {
            Box::pin(async {
                let tree = Tree::new(&["d/f"]).await;
                tree.state.mark_served();
                tree
            })
        },
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                assert!(
                    crate::harness::catalogue::ops::served_copy_up(
                        &tree,
                        tree.id("d/f"),
                        tree.id("d")
                    )
                    .await
                );
            })
        }),
    );
    let records = of(&records, &tree.state);
    let f = tree.id("d/f");
    let remark = one(&records, "re-mark", |effect| {
        *effect == Effect::Arm(Arm::CopyUpRemark)
    });
    let words = word_records(&records, f);
    assert_eq!(words.len(), 1, "one rewrite of 'd/f': {words:#?}");
    assert!(remark.seq < words[0].seq, "marked, then rewritten");
    assert_eq!(words[0].section, Section::Rewrite);
}

/// The moves permit's take and release, each recorded once, in order.
#[test]
fn the_moves_permit_logs_its_take_and_its_release() {
    let (records, tree) = logged(
        || Box::pin(Tree::new(&["d/"])),
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let permit = tree.state.moves_permit().await.expect("the permit");
                drop(permit);
            })
        }),
    );
    let records = of(&records, &tree.state);
    let permit = one(&records, "take", |effect| *effect == Effect::Permit);
    let unpermit = one(&records, "release", |effect| *effect == Effect::Unpermit);
    assert!(permit.seq < unpermit.seq);
}

/// The worker's resident find, whose walk no await separates, stalled in its window before the
/// first node it reads, `d/g`, while another thread renames `d/f` to another spelling, which keeps
/// its name's hash and moves it to the chain's head: the walk reaches the end without the name,
/// the count moved, and it walks again, logging its restart, to find `d/f`.
#[test]
fn a_resident_find_walks_again_past_a_rename_in_its_first_window() {
    walks_again_past_a_rename(&["d/f", "d/g"], 1);
}

/// [`a_resident_find_walks_again_past_a_rename_in_its_first_window`], stalled in its window before
/// the second node it reads, `d/g`, behind `d/h`: the find reaches its window before each node.
#[test]
fn a_resident_find_walks_again_past_a_rename_in_a_later_window() {
    walks_again_past_a_rename(&["d/f", "d/g", "d/h"], 2);
}

/// The worker's resident find of `d/f` in the served tree of `paths`, each added at its
/// directory's head, stalled in its `window`th window while another thread renames `d/f` to
/// another spelling, finds it after walking again once. Both threads record a log that keeps every
/// log's rules, and the model replays their calls clean and matches the State.
fn walks_again_past_a_rename(paths: &'static [&'static str], window: usize) {
    let tree = Arc::new(current_thread().block_on(in_context(async {
        let tree = Tree::new(paths).await;
        tree.state.mark_served();
        tree
    })));
    let (d, f) = (tree.id("d"), tree.id("d/f"));
    let head = current_thread()
        .block_on(in_context(tree.state.node(tree.repository.clone(), d)))
        .expect("'d' reads")
        .child();
    let last = paths.last().expect("a path");
    assert_eq!(head, Some(tree.id(last)), "the walk reads '{last}' first");
    let start = current_thread().block_on(in_context(Start::read(&*tree)));
    let log = Collector::new();
    log.set_recording(true);
    let (scheduler, in_window, resume) = InWindow::stalling(log.clone(), window);
    let (finding, finding_log) = (tree.clone(), log.clone());
    let found = std::thread::spawn(move || {
        let installed = hooks::install(scheduler);
        let entered = finding_log.enter();
        let found = current_thread().block_on(in_context(hooks::as_task(
            0,
            ops::find_resident(&finding, d, "f"),
        )));
        drop(entered);
        drop(installed);
        found
    });
    in_window.recv().expect("the find reaches its window");
    let installed = hooks::install(InWindow::never_stalling(log.clone()));
    let entered = log.enter();
    let renamed = current_thread().block_on(in_context(hooks::as_task(
        1,
        ops::served_rename(&tree, f, d, "F"),
    )));
    drop(entered);
    drop(installed);
    resume.send(()).expect("the find waits");
    assert!(renamed, "'d/f' renames");
    assert_eq!(found.join().expect("the finding thread ends"), Some(f));
    let records = log.drain();
    check_log(&records, Labels::Required);
    let restarts = records
        .iter()
        .filter(|record| record.task == Some(0) && record.effect == Effect::Arm(Arm::FindRestart))
        .count();
    assert_eq!(restarts, 1, "the find walked again once: {records:#?}");
    let calls = log.take_calls();
    assert_eq!(log.no_task(), None);
    let mut model = current_thread().block_on(in_context(start.model(&*tree, &records)));
    model.replay_called(&records, calls, Vec::new());
    let violations: Vec<String> = model.violations().iter().map(ToString::to_string).collect();
    assert!(violations.is_empty(), "the model found: {violations:#?}");
    assert_eq!(compared(&model, &*tree), Vec::<String>::new());
}

/// A tree loaded afresh through a failing store, the store, and what a test keeps beside them.
struct Failing<T> {
    tree: Tree,
    store: Arc<FailingStore>,
    kept: T,
}

impl<T: Send + Sync> Modelled for Failing<T> {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }

    fn mend(&self) {
        self.store.fail(false);
    }

    fn faulted(&self) -> bool {
        self.store.failed_outside_calls() > 0
    }
}

/// `tree` loaded afresh through a failing store and served, taking new nodes from fresh blocks,
/// as the provider serves a State.
async fn served_fresh(tree: Tree) -> Failing<()> {
    let (tree, store) = tree.failing().await;
    tree.state.mark_served();
    tree.state
        .take_new_nodes_from_fresh_blocks(tree.repository.clone())
        .await
        .expect("the State takes fresh blocks");
    Failing {
        tree,
        store,
        kept: (),
    }
}

/// Whether `node`'s block is in memory.
fn resident(tree: &Tree, node: NodeID) -> bool {
    tree.state.resident_block(NodeBlock::index(node)).is_some()
}

/// The worker's attach of a file `h` into directory `dir`, reading nothing from the store
/// (`AttachCheck::Resident`), with no absence to check the name against.
async fn resident_attach(tree: &Tree, dir: NodeID) -> Option<AttachOutcome> {
    tree.state
        .served()
        .node_attach(
            tree.repository.clone(),
            dir,
            file("h"),
            "h",
            AttachCheck::Resident(None),
        )
        .await
        .expect("the attach runs")
}

/// [`resident_attach`] in the unserved tier (`State::node_attach_resident`).
async fn unserved_resident_attach(tree: &Tree, dir: NodeID) -> Option<AttachOutcome> {
    tree.state
        .node_attach_resident(tree.repository.clone(), dir, file("h"), "h", None)
        .await
        .expect("the attach runs")
}

/// An attach a test runs on a tree.
type Attach =
    for<'a> fn(&'a Tree) -> Pin<Box<dyn Future<Output = Option<AttachOutcome>> + Send + 'a>>;

/// Runs `attach` on the tree `setup` builds, once the blocks are evicted and those of `held`
/// loaded and held again, `out`'s block out of memory, with the store failing, so a read answers
/// an error: its answer `None`, the store unread, with one `NeedLoad` in the log.
fn needs_a_load(
    setup: Setup<Failing<()>>,
    (held, out): (&'static [&'static str], &'static str),
    attach: Attach,
) -> Arc<Failing<()>> {
    let (records, context) = logged(
        setup,
        Box::new(move |context: Arc<Failing<()>>| {
            Box::pin(async move {
                let tree = &context.tree;
                tree.state.evict_blocks_for_tests();
                let mut blocks = Vec::new();
                for path in held {
                    blocks.push(
                        tree.state
                            .block(tree.repository.clone(), NodeBlock::index(tree.id(path)))
                            .await
                            .expect("a held block loads"),
                    );
                }
                assert!(
                    !resident(tree, tree.id(out)),
                    "the block needed is out of memory"
                );
                let reads = context.store.reads();
                context.store.fail(true);
                let attached = attach(tree).await;
                context.store.fail(false);
                assert_eq!(attached, None);
                assert_eq!(context.store.reads(), reads, "the attach read nothing");
                drop(blocks);
            })
        }),
    );
    let records = of(&records, &context.tree.state);
    one(&records, "need load", |effect| {
        *effect == Effect::Arm(Arm::NeedLoad)
    });
    context
}

/// The worker's attach into a directory whose block is out of memory needs a load.
#[test]
fn a_resident_attach_into_a_directory_out_of_memory_needs_a_load() {
    needs_a_load(
        || Box::pin(async { served_fresh(Tree::new(&["d/f"]).await).await }),
        (&[], "d"),
        |tree| Box::pin(resident_attach(tree, tree.id("d"))),
    );
}

/// The worker's attach whose walk of the directory's chain for the name meets a block out of
/// memory needs a load: `p`, held with `p/a` in the first block, heads its chain with `p/b`, in
/// the second, not.
#[test]
fn a_resident_attach_whose_name_walk_meets_a_block_out_of_memory_needs_a_load() {
    needs_a_load(
        || Box::pin(async { served_fresh(Tree::spread(&["p/a"], "fill", &["p/b"]).await).await }),
        (&["p"], "p/b"),
        |tree| Box::pin(resident_attach(tree, tree.id("p"))),
    );
}

/// The worker's attach into a clean directory whose served mark climbs to a block out of memory
/// needs a load, marking nothing: `q/r`, its chain and its block held, below `q`, in the first
/// block, not.
#[test]
fn a_resident_attach_whose_mark_climbs_to_a_block_out_of_memory_needs_a_load() {
    let context = needs_a_load(
        || {
            Box::pin(async {
                served_fresh(Tree::spread(&["q/"], "fill", &["q/r/f", "q/r/g"]).await).await
            })
        },
        (&["q/r"], "q"),
        |tree| Box::pin(resident_attach(tree, tree.id("q/r"))),
    );
    let tree = &context.tree;
    let r = tree.id("q/r");
    let block = tree
        .state
        .resident_block(NodeBlock::index(r))
        .expect("'q/r's block is in memory");
    assert!(
        !block.node(Node::index(r)).is_dirty(),
        "the attach marked nothing"
    );
}

/// The unserved resident attach whose grab finds the unused chain's head out of memory needs a
/// load: `d`'s block, held, full, and `e`'s, freed `e/f`'s slot free, not.
#[test]
fn a_resident_attach_whose_grab_meets_a_block_out_of_memory_needs_a_load() {
    needs_a_load(
        || {
            Box::pin(async {
                let tree = Tree::spread(&["e/f", "e/g"], "fill", &["d/", "k/"]).await;
                let k = tree.id("k");
                let last = tree.fill_block(k, "k").await;
                assert_eq!(
                    NodeBlock::index(last),
                    NodeBlock::index(k),
                    "'k' fills its block"
                );
                lore_revision::state::node_discard_fused(
                    tree.state.clone(),
                    tree.repository.clone(),
                    tree.id("e/f"),
                )
                .await
                .expect("'e/f' is discarded");
                let (tree, store) = tree.failing().await;
                Failing {
                    tree,
                    store,
                    kept: (),
                }
            })
        },
        (&["d"], "e/f"),
        |tree| Box::pin(unserved_resident_attach(tree, tree.id("d"))),
    );
}

/// The unserved resident attach whose slot's names are out of memory needs a load: `d`'s block
/// loaded again without them, freed `d/f`'s slot free in it.
#[test]
fn a_resident_attach_whose_slot_names_are_out_of_memory_needs_a_load() {
    let (records, context) = logged(
        || {
            Box::pin(async {
                let (tree, store, _freed) = Tree::with_a_free_slot(None).await;
                Failing {
                    tree,
                    store,
                    kept: (),
                }
            })
        },
        Box::new(|context: Arc<Failing<()>>| {
            Box::pin(async move {
                let tree = &context.tree;
                tree.state.evict_blocks_for_tests();
                let d = tree.id("d");
                let block = tree
                    .state
                    .block(tree.repository.clone(), NodeBlock::index(d))
                    .await
                    .expect("'d's block loads");
                assert!(
                    !block.is_nametable_deserialized(),
                    "its names are out of memory"
                );
                let reads = context.store.reads();
                context.store.fail(true);
                let attached = unserved_resident_attach(tree, d).await;
                context.store.fail(false);
                assert_eq!(attached, None);
                assert_eq!(context.store.reads(), reads, "the attach read nothing");
                drop(block);
            })
        }),
    );
    let records = of(&records, &context.tree.state);
    one(&records, "need load", |effect| {
        *effect == Effect::Arm(Arm::NeedLoad)
    });
}

/// What [`deep_path`]'s tree holds that its names leave out, and what an attach into it answered.
struct DeepPath {
    v: NodeID,
    /// A file in `v`.
    x: NodeID,
    /// A file below the root, at its chain's head.
    z: NodeID,
    /// Whether the attach answered `None`, where it answered at all.
    needed_load: OnceLock<bool>,
}

/// A served tree whose path `r/s/t/u/v` crosses three blocks: `r` in the first, with the root,
/// `s`, `t` and `u` in the second, and `v`, holding a file `x`, in the third, with `z`, a file at
/// the head of the root's chain. A resident attach into `v` finds `r`, four levels up, clean, so it
/// marks `r` first, then climbs from `v` again. The path's blocks are in memory.
async fn deep_path() -> Failing<DeepPath> {
    let tree = Tree::spread(&["r/"], "fill", &["r/s/t/u/", "pad/"]).await;
    tree.fill_block(tree.id("pad"), "pad").await;
    let add = async |dir: NodeID, node: Node, name: &str| {
        tree.state
            .node_add(tree.repository.clone(), dir, node, name)
            .await
            .expect("a node adds")
    };
    let u = tree.id("r/s/t/u");
    let v = add(u, ops::directory("v"), "v").await;
    let x = add(v, file("x"), "x").await;
    let z = add(ROOT_NODE, file("z"), "z").await;
    let blocks = [tree.id("r"), tree.id("r/s"), u, v, x, z].map(NodeBlock::index);
    assert_eq!(blocks, [0, 1, 1, 2, 2, 2], "the path crosses three blocks");
    let served = served_fresh(tree).await;
    let tree = &served.tree;
    for node in [tree.id("r"), u, v] {
        tree.state
            .node(tree.repository.clone(), node)
            .await
            .expect("a node of the path reads");
    }
    Failing {
        tree: served.tree,
        store: served.store,
        kept: DeepPath {
            v,
            x,
            z,
            needed_load: OnceLock::new(),
        },
    }
}

/// The worker's attach into `v`, at the end of a clean path, beside a swap's detach of `r`, the
/// path's top, where it finds it clean, then an eviction failing the store: no run reads the
/// store, and one ending `None` logs one `NeedLoad`. Some run's deep directory turns out detached
/// with the check of whether `v` went with it meeting the second block out of memory, and some
/// run's climb from `v` again, after `r` is marked, meets it.
#[test]
fn a_resident_attach_below_a_deep_directory_reads_nothing() {
    let attach: Body<Failing<DeepPath>> = Box::new(|context: Arc<Failing<DeepPath>>| {
        Box::pin(async move {
            let tree = &context.tree;
            let attached = tree
                .state
                .served()
                .node_attach(
                    tree.repository.clone(),
                    context.kept.v,
                    file("h"),
                    "h",
                    AttachCheck::Resident(None),
                )
                .await;
            match attached {
                Ok(outcome) => context
                    .kept
                    .needed_load
                    .set(outcome.is_none())
                    .expect("one attach"),
                Err(error) if error.is_node_not_found() => {}
                Err(error) => panic!("the resident attach read the store: {error}"),
            }
        })
    });
    let detach: Body<Failing<DeepPath>> = Box::new(|context: Arc<Failing<DeepPath>>| {
        Box::pin(async move {
            let tree = &context.tree;
            // Refused where the attach marked `r` first.
            let _ = ops::swap_detach_if_clean(tree, tree.id("r")).await;
            tree.state.evict_blocks_for_tests();
            context.store.fail(true);
        })
    });
    let runs = logged_runs(
        || Box::pin(deep_path()),
        vec![attach, detach],
        vec![],
        Labels::Required,
        // One preemption reaches both, the attach stopped in a pass while the other op runs; four,
        // the widest a few minutes allow, reach the orders of the other passes around it. Sleep
        // sets off: the eviction and the store's failure touch no resource the hooks record.
        Exploration {
            sleep_sets: false,
            preemptions: 4,
            ..Exploration::default()
        },
    );
    let (mut detached_check, mut climb_again) = (false, false);
    for (records, context) in &runs {
        let records: Vec<&Record> = of(records, &context.tree.state)
            .into_iter()
            .filter(|record| record.task == Some(0))
            .collect();
        let need_load = |record: &Record| record.effect == Effect::Arm(Arm::NeedLoad);
        let loads = records.iter().filter(|record| need_load(record)).count();
        let needed = context.kept.needed_load.get() == Some(&true);
        assert_eq!(loads, usize::from(needed), "{records:#?}");
        let Some(load) = records.iter().position(|record| need_load(record)) else {
            continue;
        };
        let before = &records[..load];
        // The detached check follows the level its pass found gone at once.
        detached_check |= before.last().is_some_and(|record| {
            matches!(
                record.effect,
                Effect::Refused {
                    reason: log::Reason::NotFound,
                    ..
                }
            )
        });
        let last_arm = before.iter().rev().find_map(|record| match record.effect {
            Effect::Arm(arm) => Some(arm),
            _ => None,
        });
        if last_arm == Some(Arm::MarkWaiting) {
            // `r`, marked first, stays marked, as a cancelled mark leaves it.
            let (tree, r) = (&context.tree, context.tree.id("r"));
            let block = tree
                .state
                .resident_block(NodeBlock::index(r))
                .expect("'r's block, dirty, is in memory");
            assert!(block.node(Node::index(r)).is_dirty(), "'r' stays marked");
            climb_again = true;
        }
    }
    assert!(
        detached_check,
        "no run's detached check met a block out of memory in {} runs",
        runs.len()
    );
    assert!(
        climb_again,
        "no run's climb from `v` again met a block out of memory in {} runs",
        runs.len()
    );
}

/// An op a test runs on a [`deep_path`] tree.
type PathOp = fn(Arc<Failing<DeepPath>>) -> Pin<Box<dyn Future<Output = ()> + Send>>;

/// Runs `op` on a [`deep_path`] tree once its blocks are evicted, asserting that its served mark
/// loaded one block not in memory (`MarkLoad`), as a mark that may read the store does, and left
/// none unread (`NeedLoad`).
fn marks_through_a_load(op: PathOp) {
    let (records, context) = logged(
        || Box::pin(deep_path()),
        Box::new(move |context: Arc<Failing<DeepPath>>| {
            Box::pin(async move {
                context.tree.state.evict_blocks_for_tests();
                op(context).await;
            })
        }),
    );
    let records = of(&records, &context.tree.state);
    one(&records, "mark load", |effect| {
        *effect == Effect::Arm(Arm::MarkLoad)
    });
    assert!(
        !records
            .iter()
            .any(|record| record.effect == Effect::Arm(Arm::NeedLoad)),
        "a mark that may read the store needs no load: {records:#?}"
    );
}

/// A served move of `v/x` out of `v`, clean, into `fill`, whose mark of `v` climbs from the third
/// block to `u` in the second, out of memory, loads it and moves the file: the destination's
/// ancestors, which the move walks first, lie in the first block.
#[test]
fn a_served_move_whose_mark_of_the_directory_it_leaves_climbs_to_a_block_out_of_memory_loads_it() {
    marks_through_a_load(|context| {
        Box::pin(async move {
            let tree = &context.tree;
            let moved = ops::served_rename(tree, context.kept.x, tree.id("fill"), "x").await;
            assert!(moved, "the file moves");
        })
    });
}

/// A served attach that may read the store (`AttachCheck::Walk`), of a file into `u`, clean,
/// whose mark climbs from the second block to `r` in the first, out of memory, loads it and
/// attaches: the attach takes the mark that loads, not the resident one.
#[test]
fn a_served_attach_whose_mark_climbs_to_a_block_out_of_memory_loads_it() {
    marks_through_a_load(|context| {
        Box::pin(async move {
            let tree = &context.tree;
            let attached = tree
                .state
                .served()
                .node_attach(
                    tree.repository.clone(),
                    tree.id("r/s/t/u"),
                    file("h"),
                    "h",
                    AttachCheck::Walk,
                )
                .await;
            assert!(
                matches!(attached, Ok(Some(AttachOutcome::Attached(_)))),
                "the file attaches: {attached:?}"
            );
        })
    });
}

/// A served detach of `v` from `u`, clean, whose mark climbs from the second block to `r` in the
/// first, out of memory, loads it and detaches.
#[test]
fn a_served_detach_whose_mark_climbs_to_a_block_out_of_memory_loads_it() {
    marks_through_a_load(|context| {
        Box::pin(async move {
            assert!(
                ops::served_detach(&context.tree, context.kept.v).await,
                "'v' detaches"
            );
        })
    });
}

/// A served move of `z` into `v`, clean, beside an eviction: the move's walk of `v`'s ancestors
/// loads them, so its mark of `v` loads `u`'s block only where the eviction lands between the
/// two, which some run shows (`MarkLoad`); no run leaves a block unread (`NeedLoad`), and the
/// file moves in each.
#[test]
fn a_served_move_beside_an_eviction_loads_what_its_mark_of_its_new_directory_needs() {
    let move_in: Body<Failing<DeepPath>> = Box::new(|context: Arc<Failing<DeepPath>>| {
        Box::pin(async move {
            let (tree, kept) = (&context.tree, &context.kept);
            assert!(
                ops::served_rename(tree, kept.z, kept.v, "z").await,
                "'z' moves"
            );
        })
    });
    let evict: Body<Failing<DeepPath>> = Box::new(|context: Arc<Failing<DeepPath>>| {
        Box::pin(async move { context.tree.state.evict_blocks_for_tests() })
    });
    let runs = logged_runs(
        || Box::pin(deep_path()),
        vec![move_in, evict],
        vec![],
        Labels::Required,
        // Sleep sets off: the eviction touches no resource the hooks record.
        Exploration {
            sleep_sets: false,
            preemptions: 1,
            ..Exploration::default()
        },
    );
    let mut loaded = false;
    for (records, context) in &runs {
        let records = of(records, &context.tree.state);
        assert!(
            !records
                .iter()
                .any(|record| record.effect == Effect::Arm(Arm::NeedLoad)),
            "a mark that may read the store needs no load: {records:#?}"
        );
        loaded |= records
            .iter()
            .any(|record| record.effect == Effect::Arm(Arm::MarkLoad));
    }
    assert!(
        loaded,
        "no run's mark loaded a block in {} runs",
        runs.len()
    );
}
