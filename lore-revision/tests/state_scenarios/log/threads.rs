// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The collector across threads, the chaos scheduler's log of operations on several workers, a
//! lock found held, and the runs a missing task or recording off make.

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::Mode;
use lore_revision::state::node::hooks::Point;
use lore_revision::state::node::hooks::Resource;
use lore_revision::state::node::hooks::log::Arm;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;

use super::logged_as;
use super::of;
use super::one;
use super::section_of;
use super::words_of;
use crate::harness::catalogue::modelled::Labels;
use crate::harness::catalogue::modelled::check_log;
use crate::harness::catalogue::ops::file;
use crate::harness::chaos;
use crate::harness::fixtures::Tree;
use crate::harness::invariants;
use crate::harness::log::Collector;
use crate::harness::scheduler;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Scenario;

/// A record of State `state` at `(seq, sub)` made by `task`.
fn record(state: u64, seq: u64, sub: u32, task: Option<usize>, effect: Effect) -> Record {
    Record {
        seq,
        sub,
        section: Section::Unlabelled,
        state,
        task,
        effect,
    }
}

#[test]
fn a_collector_merges_threads_in_order_and_flags_only_tasks_missing() {
    let log = Collector::new();
    log.set_recording(true);
    std::thread::scope(|threads| {
        for parity in 0..2 {
            let log = &log;
            threads.spawn(move || {
                for seq in (parity..20).step_by(2) {
                    log.record(record(1, seq, 0, Some(0), Effect::Served));
                    log.record(record(0, seq, 0, Some(0), Effect::Served));
                }
            });
        }
    });
    // A thread recording for another collector, then this one again.
    let other = Collector::new();
    log.record(record(2, 1, 0, Some(0), Effect::Served));
    other.record(record(2, 0, 0, Some(0), Effect::Served));
    log.record(record(2, 0, 0, Some(0), Effect::Served));
    assert_eq!(log.no_task(), None, "every record so far has a task");
    let drained = log.drain();
    let order: Vec<(u64, u64)> = drained
        .iter()
        .map(|record| (record.state, record.seq))
        .collect();
    let mut expected: Vec<(u64, u64)> = (0..2)
        .flat_map(|state| (0..20).map(move |seq| (state, seq)))
        .collect();
    expected.extend([(2, 0), (2, 1)]);
    assert_eq!(order, expected, "every record, in (state, seq) order");
    assert_eq!(other.drain().len(), 1);
    assert!(log.drain().is_empty(), "a drain takes the records");
    log.record(record(
        3,
        0,
        0,
        None,
        Effect::Loaded {
            block: 0,
            instance: 0,
            zeroed: true,
        },
    ));
    assert!(
        log.no_task().is_some_and(|what| what.contains("Loaded")),
        "a record with no task is flagged, a load's too"
    );
}

/// Operations on four workers at once, yielding often: the log's sections never mix tasks or
/// lose their order, whatever thread a task resumes on.
#[test]
fn a_chaos_run_logs_each_section_whole_in_its_task() {
    let (runtime, chaos) = chaos::runtime(4, 2, 7);
    let tree = Arc::new(
        runtime.block_on(
            lore_base::runtime::LORE_CONTEXT
                .scope(crate::harness::fixtures::execution(), Tree::new(&["d/"])),
        ),
    );
    chaos.log().set_recording(true);
    let handles: Vec<_> = (0..8)
        .map(|task| {
            let tree = tree.clone();
            runtime.spawn(lore_base::runtime::LORE_CONTEXT.scope(
                crate::harness::fixtures::execution(),
                hooks::as_task(task, async move {
                    let name = format!("f{task}");
                    tree.state
                        .node_add(tree.repository.clone(), tree.id("d"), file(&name), &name)
                        .await
                        .expect("the file adds");
                }),
            ))
        })
        .collect();
    for handle in handles {
        joined(&runtime, handle, "the add ends");
    }
    chaos.log().set_recording(false);
    assert_eq!(chaos.log().no_task(), None);
    let log = chaos.log().drain();
    check_log(&log, Labels::Required);
    for task in 0..8 {
        let publishes = log
            .iter()
            .filter(|record| {
                record.task == Some(task) && matches!(record.effect, Effect::Publish { .. })
            })
            .count();
        assert_eq!(publishes, 1, "task {task} published once");
    }
    assert!(chaos.yields() > 0, "the workers yielded");
    assert_eq!(
        chaos.log().grown(),
        0,
        "no record grew a buffer: each grows at a scheduling point"
    );
    assert_eq!(
        lore_revision::node::words_grown(),
        0,
        "no writer's snapshot grew its thread's buffer"
    );
}

/// How a taker takes a block: each of the block's takes that probes its lock.
#[derive(Clone, Copy, Debug)]
enum Take {
    Read,
    Write,
    /// A save's hold.
    Saving,
    /// An owned reader's.
    Owned,
    /// A listing's watch of a directory's chain.
    Watch,
}

impl Take {
    /// The mode the probe names for this take.
    fn mode(self) -> Mode {
        match self {
            Self::Write => Mode::Write,
            Self::Saving => Mode::Upgradable,
            Self::Read | Self::Owned | Self::Watch => Mode::Read,
        }
    }
}

/// `tree` made on `runtime`'s workers, as their scheduler names its blocks.
fn made(runtime: &tokio::runtime::Runtime, paths: &[&str]) -> Tree {
    runtime.block_on(
        lore_base::runtime::LORE_CONTEXT
            .scope(crate::harness::fixtures::execution(), Tree::new(paths)),
    )
}

/// `handle`'s task joined, aborting the process where it does not end within a minute: a worker
/// deadlocked on a block's lock would hang the runtime's drop, and the test with it, so a panic
/// would never be reported.
fn joined<T>(
    runtime: &tokio::runtime::Runtime,
    handle: tokio::task::JoinHandle<T>,
    what: &str,
) -> T {
    let joined =
        runtime.block_on(async { tokio::time::timeout(Duration::from_secs(60), handle).await });
    let Ok(joined) = joined else {
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "{what}: not within a minute, a deadlock");
        std::process::abort();
    };
    joined.unwrap_or_else(|error| panic!("{what}: {error}"))
}

/// Waits until a probe found a lock held, `seen` contentions having been found before.
fn contended(chaos: &chaos::Chaos, seen: u64) {
    let started = Instant::now();
    while chaos.contentions() == seen {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the taker finds the lock held"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A lock found held is logged once it is taken, not before, naming the block, the task taking it
/// and the holder: a writer on another thread, outside every task. Every take that probes.
#[test]
fn a_lock_found_held_logs_its_contention_once_taken() {
    for take in [
        Take::Read,
        Take::Write,
        Take::Saving,
        Take::Owned,
        Take::Watch,
    ] {
        let (runtime, chaos) = chaos::runtime(1, u64::MAX, 1);
        let tree = made(&runtime, &["d/f"]);
        let d = tree.id("d");
        let index = NodeBlock::index(d);
        let block = tree
            .state
            .resident_block(index)
            .expect("the block is in memory");
        chaos.log().set_recording(true);
        let seen = chaos.contentions();
        let writer = block.write_for_tests();
        let taking = block.clone();
        let taker = runtime.spawn(hooks::as_task(0, async move {
            match take {
                Take::Read => drop(taking.read()),
                Take::Write => drop(taking.write_for_tests()),
                Take::Saving => drop(taking.saving_for_tests()),
                Take::Owned => drop(taking.read_owned()),
                Take::Watch => drop(taking.watch_chain(Node::index(d), None)),
            }
        }));
        contended(&chaos, seen);
        assert!(
            !chaos
                .log()
                .drain()
                .iter()
                .any(|record| matches!(record.effect, Effect::Arm(Arm::Contended { .. }))),
            "nothing is logged while the taker waits"
        );
        drop(writer);
        joined(&runtime, taker, "the taker ends");
        chaos.log().set_recording(false);
        let records = chaos.log().drain();
        let contended: Vec<_> = records
            .iter()
            .filter_map(|record| match &record.effect {
                Effect::Arm(Arm::Contended {
                    block,
                    waiter,
                    mode,
                    holders,
                }) => Some((record.state, *block, *waiter, *mode, *holders)),
                _ => None,
            })
            .collect();
        assert_eq!(contended.len(), 1, "{take:?}: one contention: {records:#?}");
        let (state, named, waiter, taken, holders) = contended[0];
        assert_eq!(state, tree.state.hooks().id());
        assert_eq!(
            named,
            Resource::Block(tree.state.hooks().ordinal(), index),
            "the block by its State and index"
        );
        assert_eq!((waiter, taken), (Some(0), take.mode()), "{take:?}");
        assert!(
            holders
                .iter()
                .flatten()
                .any(|holder| holder.task.is_none() && holder.mode == Mode::Write),
            "the writer is named: {holders:?}"
        );
    }
}

/// The run itself fails on a record with no task, whatever its check reads.
#[test]
#[should_panic(expected = "scenario 'log: no task': a record with no task")]
fn a_record_with_no_task_fails_the_run() {
    let scenario = Scenario::<Tree> {
        name: "log: no task",
        setup: Box::new(|| Box::pin(Tree::new(&["d/f"]))),
        ops: vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                // Spawned with no `inherit`: its write names no operation.
                let unwrapped = lore_base::lore_spawn!(async move {
                    let f = tree.id("d/f");
                    let block = invariants::block_of(&tree.state, &tree.repository, f).await;
                    tree.state.node_edit_for_tests(&block, f, |node| {
                        node.flags |= NodeFlags::Dirty.bits();
                    });
                });
                let _ = unwrapped.await;
            })
        })],
        cancellable: vec![],
        check: Box::new(|_: Arc<Tree>| Box::pin(async {})),
    };
    scenario.explore(Exploration {
        recording: true,
        ..Exploration::default()
    });
}

#[test]
#[should_panic(expected = "reached with no task")]
fn a_point_reached_with_no_task_fails_the_run() {
    logged_as(
        || Box::pin(Tree::new(&["d/f"])),
        Box::new(|_: Arc<Tree>| {
            Box::pin(async move {
                let unwrapped = lore_base::lore_spawn!(hooks::reached(Point::Named("unwrapped")));
                // The spawned future panics; the run fails on the flag.
                let _ = unwrapped.await;
            })
        }),
        Labels::Required,
    );
}

#[test]
fn a_run_not_recording_logs_nothing() {
    let scenario = Scenario::<Tree> {
        name: "log: not recording",
        setup: Box::new(|| Box::pin(Tree::new(&["d/"]))),
        ops: vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                tree.state
                    .node_add(tree.repository.clone(), ROOT_NODE, file("f"), "f")
                    .await
                    .expect("'f' adds");
            })
        })],
        cancellable: vec![],
        check: Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                assert!(scheduler::run_log().is_empty());
                assert_eq!(tree.state.hooks().seq_now(), 0, "no section numbered");
            })
        }),
    };
    assert!(scenario.explore(Exploration::default()).complete);
}

/// The contention record of `records`, the one there is.
fn contention(records: &[Record]) -> &Record {
    let found: Vec<&Record> = records
        .iter()
        .filter(|record| matches!(record.effect, Effect::Arm(Arm::Contended { .. })))
        .collect();
    assert_eq!(found.len(), 1, "one contention: {records:#?}");
    found[0]
}

/// A section numbers itself once its locks are taken: a writer, or a section of several locks,
/// that found a lock held logs the contention first, outside the section, never in it.
#[test]
fn a_section_found_held_takes_its_seq_after_its_locks() {
    // A writer of one block, the block held by another writer.
    {
        let (runtime, chaos) = chaos::runtime(1, u64::MAX, 1);
        let tree = Arc::new(made(&runtime, &["f"]));
        let f = tree.id("f");
        let block = tree
            .state
            .resident_block(NodeBlock::index(f))
            .expect("the block is in memory");
        chaos.log().set_recording(true);
        let seen = chaos.contentions();
        let writer = block.write_for_tests();
        let (writing, taking) = (tree.clone(), block.clone());
        let taker = runtime.spawn(hooks::as_task(0, async move {
            writing.state.node_edit_for_tests(&taking, f, |node| {
                node.flags |= NodeFlags::Dirty.bits();
            });
        }));
        contended(&chaos, seen);
        drop(writer);
        joined(&runtime, taker, "the writer ends");
        chaos.log().set_recording(false);
        let log = chaos.log().drain();
        check_log(&log, Labels::Free);
        let records = of(&log, &tree.state);
        let words = words_of(&records, f);
        assert_eq!(words.len(), 1);
        let word = records
            .iter()
            .find(|record| matches!(record.effect, Effect::Word { node, .. } if node == f))
            .expect("the word");
        assert!(
            contention(&log).seq < word.seq,
            "the contention before the writer's section"
        );
    }
    // A detach's section: the parent's block taken, then the node's, held by a reader.
    {
        let (runtime, chaos) = chaos::runtime(1, u64::MAX, 1);
        let tree = Arc::new(runtime.block_on(lore_base::runtime::LORE_CONTEXT.scope(
            crate::harness::fixtures::execution(),
            Tree::spread(&["d/"], "fill", &["d/f"]),
        )));
        let f = tree.id("d/f");
        assert_eq!(NodeBlock::index(f), 1);
        let block = tree
            .state
            .resident_block(NodeBlock::index(f))
            .expect("the block is in memory");
        chaos.log().set_recording(true);
        let seen = chaos.contentions();
        let reader = block.read();
        let detaching = tree.clone();
        let taker = runtime.spawn(lore_base::runtime::LORE_CONTEXT.scope(
            crate::harness::fixtures::execution(),
            hooks::as_task(0, async move {
                let detached = detaching
                    .state
                    .node_detach(
                        detaching.repository.clone(),
                        f,
                        lore_revision::state::Detach::Always,
                    )
                    .await
                    .expect("the detach runs");
                assert!(detached);
            }),
        ));
        contended(&chaos, seen);
        drop(reader);
        joined(&runtime, taker, "the detach ends");
        chaos.log().set_recording(false);
        let log = chaos.log().drain();
        check_log(&log, Labels::Required);
        let records = of(&log, &tree.state);
        let unlink = one(
            &records,
            "unlink",
            |effect| matches!(effect, Effect::Detach { node, .. } if *node == f),
        );
        let held = contention(&log);
        assert!(
            matches!(
                held.effect,
                Effect::Arm(Arm::Contended {
                    mode: Mode::Write,
                    ..
                })
            ),
            "the section's write lock found the reader: {held:?}"
        );
        assert!(
            held.seq < unlink.seq,
            "the contention before the section's seq: {:#?}",
            section_of(&records, unlink)
        );
    }
    // A served rewrite's pair: the file's block written, then its directory's, in the block
    // after, read, held by a writer.
    {
        let (runtime, chaos) = chaos::runtime(1, u64::MAX, 1);
        let tree = Arc::new(runtime.block_on(lore_base::runtime::LORE_CONTEXT.scope(
            crate::harness::fixtures::execution(),
            async {
                let tree = Tree::spread(&["f"], "fill", &["d/"]).await;
                tree.state
                    .node_reattach(tree.repository.clone(), tree.id("f"), tree.id("d"), "f")
                    .await
                    .expect("'f' moves into 'd'");
                tree.state.mark_served();
                tree.state
                    .served()
                    .node_mark(&tree.repository, tree.id("d"))
                    .await
                    .expect("'d' is marked");
                tree
            },
        )));
        let (d, f) = (tree.id("d"), tree.id("f"));
        assert_eq!((NodeBlock::index(f), NodeBlock::index(d)), (0, 1));
        let block = tree
            .state
            .resident_block(NodeBlock::index(d))
            .expect("the block is in memory");
        chaos.log().set_recording(true);
        let seen = chaos.contentions();
        let writer = block.write_for_tests();
        let rewriting = tree.clone();
        let taker = runtime.spawn(lore_base::runtime::LORE_CONTEXT.scope(
            crate::harness::fixtures::execution(),
            hooks::as_task(0, async move {
                let content = lore_revision::state::Content {
                    mode: 0o644,
                    size: 3,
                    ..lore_revision::state::Content::default()
                };
                let refusing = rewriting
                    .state
                    .served()
                    .node_rewrite(
                        &rewriting.repository,
                        f,
                        d,
                        &content,
                        (
                            NodeFlags::DirtyModify,
                            lore_revision::node::NodeExtFlags::NoFlags,
                        ),
                    )
                    .await
                    .expect("the rewrite runs");
                assert_eq!(refusing, None, "'d' admits the rewrite");
            }),
        ));
        contended(&chaos, seen);
        drop(writer);
        joined(&runtime, taker, "the rewrite ends");
        chaos.log().set_recording(false);
        let log = chaos.log().drain();
        check_log(&log, Labels::Required);
        let records = of(&log, &tree.state);
        let word = one(
            &records,
            "rewrite",
            |effect| matches!(effect, Effect::Word { node, .. } if *node == f),
        );
        assert_eq!(word.section, Section::Rewrite);
        let held = contention(&log);
        assert!(
            matches!(
                held.effect,
                Effect::Arm(Arm::Contended {
                    mode: Mode::Read,
                    ..
                })
            ),
            "the pair's read of the directory found the writer: {held:?}"
        );
        assert!(held.seq < word.seq, "the contention before the pair's seq");
    }
}

/// Adds into one directory on four workers at once replay in `seq` order: each publish links its
/// node to the one published before it, the chain ends in reverse `seq` order, and the slots of the
/// block are taken in index order.
#[test]
fn adds_on_several_workers_replay_in_seq_order() {
    const ADDS: usize = 32;
    let (runtime, chaos) = chaos::runtime(4, 2, 11);
    let tree = Arc::new(made(&runtime, &["d/"]));
    let d = tree.id("d");
    let block = tree
        .state
        .resident_block(NodeBlock::index(d))
        .expect("the block is in memory");
    chaos.log().set_recording(true);
    // Held until every worker waits on it, so their sections overlap.
    let seen = chaos.contentions();
    let writer = block.write_for_tests();
    let handles: Vec<_> = (0..ADDS)
        .map(|task| {
            let tree = tree.clone();
            runtime.spawn(lore_base::runtime::LORE_CONTEXT.scope(
                crate::harness::fixtures::execution(),
                hooks::as_task(task, async move {
                    let name = format!("f{task}");
                    tree.state
                        .node_add(tree.repository.clone(), d, file(&name), &name)
                        .await
                        .expect("the file adds");
                }),
            ))
        })
        .collect();
    let started = Instant::now();
    while chaos.contentions() < seen + 4 {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "every worker waits on the block"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(writer);
    for handle in handles {
        joined(&runtime, handle, "the add ends");
    }
    chaos.log().set_recording(false);
    assert_eq!(chaos.log().no_task(), None);
    let log = chaos.log().drain();
    check_log(&log, Labels::Required);
    let records = of(&log, &tree.state);
    let published: Vec<NodeID> = records
        .iter()
        .filter_map(|record| match record.effect {
            Effect::Publish { node, parent, .. } if parent == d => Some(node),
            _ => None,
        })
        .collect();
    assert_eq!(published.len(), ADDS);
    for (at, record) in records
        .iter()
        .filter(|record| matches!(record.effect, Effect::Publish { parent, .. } if parent == d))
        .enumerate()
        .skip(1)
    {
        let node = published[at];
        let linked = words_of(&section_of(&records, record), node)
            .last()
            .map(|word| word.sibling);
        assert_eq!(
            linked,
            Some(published[at - 1]),
            "publish {at} links to the one before it in seq order"
        );
    }
    let chain = runtime
        .block_on(lore_base::runtime::LORE_CONTEXT.scope(
            crate::harness::fixtures::execution(),
            tree.state.node_children(tree.repository.clone(), d),
        ))
        .expect("the chain reads");
    let mut reversed = published.clone();
    reversed.reverse();
    assert_eq!(chain, reversed, "the chain in reverse seq order");
    let grabbed: Vec<NodeID> = records
        .iter()
        .filter_map(|record| match record.effect {
            Effect::Grab { node, .. } => Some(node),
            _ => None,
        })
        .collect();
    assert!(
        grabbed.windows(2).all(|pair| pair[0] < pair[1]),
        "the slots taken in index order: {grabbed:?}"
    );
}
