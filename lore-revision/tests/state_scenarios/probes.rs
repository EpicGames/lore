// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The hooks the stress suite reads and drives: the lock probe naming the holders of a block found
//! held, and the scheduler failing a run on it; each accessor's touch, once it holds its lock; the
//! assertion that no task parks holding a block guard or an owned hold (C13); a State's ordinal and
//! its count of live blocks; the lowered retry bound at each of its sites; the restore's lock point
//! and the rename's window; the walks' points; and the wait marks, each from where its wait begins
//! to where it ends.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use lore_base::types::Address;
use lore_base::types::Hash;
use lore_revision::node;
use lore_revision::node::CHAIN_LOG_INITIAL;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeFileMetadataBlock;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::AttachOutcome;
use lore_revision::state::ChainAttempts;
use lore_revision::state::ChainPosition;
use lore_revision::state::ChainStep;
use lore_revision::state::ChainWalk;
use lore_revision::state::Content;
use lore_revision::state::LevelMark;
use lore_revision::state::NameChange;
use lore_revision::state::State;
use lore_revision::state::StateError;
use lore_revision::state::StateNodeChildrenIterator;
use lore_revision::state::StateNodeChildrenWithNameIterator;
use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::Contention;
use lore_revision::state::node::hooks::HOLDER_SLOTS;
use lore_revision::state::node::hooks::Holder;
use lore_revision::state::node::hooks::Mode;
use lore_revision::state::node::hooks::Point;
use lore_revision::state::node::hooks::Resource;
use lore_revision::state::node::hooks::Wait;
use lore_revision::state::node::hooks::Window;
use lore_storage::hash::hash_string;

use crate::current_thread;
use crate::harness::catalogue::modelled::Modelling;
use crate::harness::catalogue::modelled::modelled;
use crate::harness::catalogue::ops::file;
use crate::harness::faults::FailingStore;
use crate::harness::fixtures::Tree;
use crate::harness::invariants::Rules;
use crate::harness::model::Modelled;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Op;
use crate::harness::scheduler::Scenario;

/// What a hook told a [`Recorder`].
#[derive(Clone, Copy, Debug)]
enum Event {
    Reached(Point),
    Touched(Resource, bool),
    Window(Window),
    Waiting(Wait, bool),
    Contended(Contention),
}

/// A scheduler that parks nowhere and records what the hooks tell it, in the order told, from
/// every thread it is installed on.
#[derive(Default)]
struct Recorder {
    /// Whether walks park at their own points.
    reader_points: bool,
    events: Mutex<Vec<Event>>,
}

impl Recorder {
    fn record(&self, event: Event) {
        self.events.lock().expect("the events").push(event);
    }

    fn events(&self) -> Vec<Event> {
        self.events.lock().expect("the events").clone()
    }

    fn points(&self) -> Vec<Point> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                Event::Reached(point) => Some(point),
                _ => None,
            })
            .collect()
    }

    fn touches(&self) -> Vec<(Resource, bool)> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                Event::Touched(resource, write) => Some((resource, write)),
                _ => None,
            })
            .collect()
    }

    fn windows(&self) -> Vec<Window> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                Event::Window(window) => Some(window),
                _ => None,
            })
            .collect()
    }

    fn waits(&self) -> Vec<(Wait, bool)> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                Event::Waiting(wait, begins) => Some((wait, begins)),
                _ => None,
            })
            .collect()
    }

    fn contentions(&self) -> Vec<Contention> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                Event::Contended(contention) => Some(contention),
                _ => None,
            })
            .collect()
    }
}

impl hooks::Scheduler for Recorder {
    fn reached(
        &self,
        point: Point,
    ) -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>> {
        self.record(Event::Reached(point));
        None
    }

    fn touched(&self, resource: Resource, write: bool) {
        self.record(Event::Touched(resource, write));
    }

    fn reader_points(&self) -> bool {
        self.reader_points
    }

    fn window(&self, window: Window) {
        self.record(Event::Window(window));
    }

    fn waiting(&self, wait: Wait, begins: bool) {
        self.record(Event::Waiting(wait, begins));
    }

    fn contended(&self, contention: &Contention) {
        self.record(Event::Contended(*contention));
    }
}

/// `future` run with a fresh [`Recorder`] installed on this thread: what it recorded, and the
/// future's output.
async fn recording<F: Future>(future: F) -> (Arc<Recorder>, F::Output) {
    let recorder = Arc::new(Recorder::default());
    let installed = hooks::install(recorder.clone());
    let output = future.await;
    drop(installed);
    (recorder, output)
}

/// The message a caught panic carries.
pub(crate) fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    match panic.downcast::<String>() {
        Ok(message) => *message,
        Err(panic) => panic
            .downcast::<&'static str>()
            .map_or_else(|_| String::new(), |message| (*message).to_string()),
    }
}

/// A State made on a thread takes the next ordinal there, which names it in every footprint
/// touch: its blocks', its runtime lock's, its moves permit's, the permit's release last. A
/// deterministic run restarts the count before its setup, so it names its States as every
/// other run does.
#[tokio::test]
async fn states_are_named_by_their_creation_ordinal() {
    crate::in_context(async {
        hooks::reset_state_ordinals();
        let (first, second) = (State::new(), State::new());
        assert_eq!((first.hooks().ordinal(), second.hooks().ordinal()), (0, 1));
        hooks::reset_state_ordinals();
        assert_eq!(
            State::new().hooks().ordinal(),
            0,
            "a reset numbers them again"
        );

        let (tree, _store) = Tree::spread(&["a/x"], "fill", &["b/y"])
            .await
            .failing()
            .await;
        let ordinal = tree.state.hooks().ordinal();
        assert_ne!(ordinal, 0, "a State made after others");
        let y = tree.id("b/y");
        let (recorder, ()) = recording(async {
            tree.state
                .node(tree.repository.clone(), y)
                .await
                .expect("'b/y' loads");
            tree.state
                .node_add(tree.repository.clone(), tree.id("b"), file("h"), "h")
                .await
                .expect("the add");
            drop(tree.state.moves_permit().await.expect("the moves permit"));
        })
        .await;
        let touches = recorder.touches();
        for (resource, _) in &touches {
            match *resource {
                Resource::Block(of, _)
                | Resource::MetadataBlock(of, _)
                | Resource::Runtime(of)
                | Resource::Data(of)
                | Resource::Permit(of)
                | Resource::UnusedPermit(of) => {
                    assert_eq!(of, ordinal, "a touch names its State: {touches:?}");
                }
                Resource::Named(_) => {}
            }
        }
        for touch in [
            (Resource::Block(ordinal, NodeBlock::index(y)), false),
            (Resource::Block(ordinal, NodeBlock::index(y)), true),
            (Resource::Runtime(ordinal), false),
            (Resource::Runtime(ordinal), true),
        ] {
            assert!(touches.contains(&touch), "{touch:?} in {touches:?}");
        }
        assert_eq!(
            touches[touches.len() - 2..],
            [
                (Resource::Permit(ordinal), true),
                (Resource::Permit(ordinal), true)
            ],
            "the permit's take and its release come last"
        );
    })
    .await;
}

/// A State counts the blocks installed in it until each drops, whoever drops it last, the State
/// itself gone.
#[tokio::test]
async fn the_live_block_count_follows_installs_and_drops() {
    crate::in_context(async {
        let (tree, _store) = Tree::spread(&["a/x"], "fill", &["b/y"])
            .await
            .failing()
            .await;
        let hooks = tree.state.hooks().clone();
        let live = || hooks.live_blocks();
        let before = live();
        let index = NodeBlock::index(tree.id("b/y"));
        assert!(
            tree.state.resident_block(index).is_none(),
            "the block of 'b/y' is not in memory"
        );
        let block = tree
            .state
            .block(tree.repository.clone(), index)
            .await
            .expect("the block loads");
        assert_eq!(live(), before + 1, "the load installed a block");

        tree.state.evict_blocks_for_tests();
        assert_eq!(live(), before + 1, "a block a caller holds stays");
        drop(tree);
        assert_eq!(live(), 1, "a block a caller holds outlives its State");
        drop(block);
        assert_eq!(live(), 0, "its last holder's drop drops it");
    })
    .await;
}

/// A block an add allocates afresh, the tree's one block full, is installed as a loaded one is:
/// counted live, and named by its index in the add's touches.
#[tokio::test]
async fn a_fresh_block_is_counted_and_named() {
    crate::in_context(async {
        let full = Tree::spread(&["d/"], "fill", &[]).await;
        let ordinal = full.state.hooks().ordinal();
        let before = full.state.hooks().live_blocks();
        let (recorder, added) =
            recording(
                full.state
                    .node_add(full.repository.clone(), full.id("d"), file("h"), "h"),
            )
            .await;
        let index = NodeBlock::index(added.expect("the add"));
        assert!(
            index > 0 && index + 1 == full.state.block_count(),
            "the add took a slot in a fresh block, {index}"
        );
        assert_eq!(full.state.hooks().live_blocks(), before + 1);
        let touches = recorder.touches();
        assert!(
            touches.contains(&(Resource::Block(ordinal, index), true)),
            "the fresh block's write names it: {touches:?}"
        );
    })
    .await;
}

/// Two attaches into one directory, and whether either answered `SlowDown`.
struct TwoAttaches {
    tree: Tree,
    slowed: AtomicU64,
}

impl Modelled for TwoAttaches {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }
}

/// An attach into `d`, counting a `SlowDown` answer.
fn attach(name: &'static str) -> Op<TwoAttaches> {
    Box::new(move |context: Arc<TwoAttaches>| {
        Box::pin(async move {
            let tree = &context.tree;
            let node = Node {
                flags: NodeFlags::File.bits(),
                ..Node::default()
            };
            match tree
                .state
                .node_attach(tree.repository.clone(), tree.id("d"), node, name)
                .await
            {
                Ok(AttachOutcome::Attached(_)) => {}
                Err(StateError::SlowDown(_)) => {
                    context.slowed.fetch_add(1, Ordering::Relaxed);
                }
                other => panic!("the attach of {name}: {other:?}"),
            }
        })
    })
}

/// Explores two attaches into one directory with the State's retry bound at `bound`: how many
/// runs had one answer `SlowDown`. Each run leaves nothing in flight, and names its State as the
/// first run did, the scheduler restarting the ordinals before each setup.
fn slowed_runs(bound: Option<u32>) -> u64 {
    let slowed = Arc::new(AtomicU64::new(0));
    let seen = slowed.clone();
    let first_ordinal = Arc::new(OnceLock::new());
    let scenario = Scenario::<TwoAttaches> {
        name: "two attaches under a retry bound",
        setup: Box::new(move || {
            Box::pin(async move {
                let tree = Tree::new(&["d/g"]).await;
                tree.state.set_retry_bound_for_tests(bound);
                TwoAttaches {
                    tree,
                    slowed: AtomicU64::new(0),
                }
            })
        }),
        ops: vec![attach("h"), attach("x")],
        cancellable: vec![],
        check: Box::new(move |context: Arc<TwoAttaches>| {
            let (seen, first_ordinal) = (seen.clone(), first_ordinal.clone());
            Box::pin(async move {
                if context.slowed.load(Ordering::Relaxed) > 0 {
                    seen.fetch_add(1, Ordering::Relaxed);
                }
                let tree = &context.tree;
                let ordinal = tree.state.hooks().ordinal();
                assert_eq!(
                    ordinal,
                    *first_ordinal.get_or_init(|| ordinal),
                    "every run names its State as the first did"
                );
                let report = crate::holding(&tree.state, &tree.repository, Rules::default()).await;
                assert_eq!(report.detached, 0, "nothing is left in flight");
            })
        }),
    };
    let name = scenario.name;
    let covered = modelled(scenario, Modelling::default()).explore(Exploration::default());
    eprintln!("'{name}' at {bound:?}: {covered:?}");
    assert!(
        covered.complete && covered.runs > 1,
        "explored {} runs",
        covered.runs
    );
    slowed.load(Ordering::Relaxed)
}

/// An attach whose publish the other attach's overtook looks its name up again; with the retry
/// bound lowered to one attempt it answers `SlowDown` there instead, its slot freed, as it does
/// with the product's bound only after 256 attempts.
#[test]
fn a_lowered_retry_bound_answers_slow_down() {
    assert_eq!(slowed_runs(None), 0, "the product's bound is never met");
    assert!(
        slowed_runs(Some(1)) > 0,
        "an attach that must try again meets a bound of one"
    );
}

/// A walk of a chain meeting a node moved out, back in and out again restarts twice; with the
/// retry bound lowered to one restart, read from the walked State through the parent's block, its
/// second answers `SlowDown`.
#[tokio::test]
async fn a_chain_walk_meets_the_restart_bound_its_state_sets() {
    crate::in_context(async {
        for (bound, slowed) in [(None, false), (Some(1), true)] {
            let tree = Tree::new(&["dir/a", "dir/y", "other/x"]).await;
            tree.state.set_retry_bound_for_tests(bound);
            let (dir, other, y) = (tree.id("dir"), tree.id("other"), tree.id("dir/y"));
            let block = tree
                .state
                .block(tree.repository.clone(), NodeBlock::index(dir))
                .await
                .expect("the parent's block");
            let (_, changes) = block.node_with_chain_changes(Node::index(dir));
            let mut walk = ChainWalk::new(dir, block, changes);
            let move_y = async |into: NodeID| {
                tree.state
                    .node_reattach(tree.repository.clone(), y, into, "y")
                    .await
                    .expect("the move");
                tree.state
                    .node(tree.repository.clone(), y)
                    .await
                    .expect("y")
            };

            let moved_out = move_y(other).await;
            assert_eq!(
                walk.step(y, &moved_out).expect("the first restart"),
                ChainStep::Restart
            );
            move_y(dir).await;
            walk.head_again();
            let moved_out_again = move_y(other).await;
            let second = walk.step(y, &moved_out_again);
            if slowed {
                assert!(
                    matches!(second, Err(StateError::SlowDown(_))),
                    "a bound of one restart is met at the second: {second:?}"
                );
            } else {
                assert_eq!(second.expect("the second restart"), ChainStep::Restart);
            }
        }
    })
    .await;
}

/// Attempts at a change to a chain that each missed their node under one parent judge it missing
/// from that chain once the State's bound has run out, and churn, `SlowDown`, before.
#[tokio::test]
async fn chain_attempts_run_out_at_the_bound_their_state_sets() {
    crate::in_context(async {
        let tree = Tree::new(&["dir/a"]).await;
        let dir = tree.id("dir");
        for (bound, missing) in [(Some(1), true), (None, false)] {
            tree.state.set_retry_bound_for_tests(bound);
            let mut attempts = ChainAttempts::default();
            assert!(matches!(
                attempts.found(dir, ChainPosition::Missing),
                Ok(None)
            ));
            let error = attempts
                .exhausted(&tree.state, &tree.repository, tree.id("dir/a"))
                .await;
            if missing {
                assert!(
                    error.is_internal(),
                    "one miss runs out a bound of one: {error:?}"
                );
            } else {
                assert!(
                    matches!(error, StateError::SlowDown(_)),
                    "one miss is short of the product's bound: {error:?}"
                );
            }
        }
    })
    .await;
}

/// The nodes of the probes' tree an accessor reads: a directory and a file in it.
#[derive(Clone, Copy)]
struct Nodes {
    dir: usize,
    file: usize,
}

/// A block of `d/f` with its names, a State made before the tree's so that the tree's names its
/// footprints by an ordinal other than 0, and the block's nodes.
fn probed_block(runtime: &tokio::runtime::Runtime) -> (Tree, Arc<NodeBlock>, Nodes) {
    runtime.block_on(async {
        hooks::reset_state_ordinals();
        let _first = State::new();
        let tree = Tree::new(&["d/f"]).await;
        assert_ne!(tree.state.hooks().ordinal(), 0);
        let block = tree
            .state
            .block_with_nametable(tree.repository.clone(), 0)
            .await
            .expect("block 0 with its names");
        assert_eq!(NodeBlock::index(tree.id("d/f")), 0);
        let nodes = Nodes {
            dir: Node::index(tree.id("d")),
            file: Node::index(tree.id("d/f")),
        };
        (tree, block, nodes)
    })
}

/// How an accessor reads or writes a block.
type Access = fn(&NodeBlock, Nodes);

/// Each accessor of a block, alone, touches the block once it holds its lock, reading or writing
/// as it takes the lock; a watch installing its directory's log reads, then writes.
#[test]
fn each_accessor_touches_its_block_as_it_takes_its_lock() {
    let runtime = current_thread();
    let (tree, block, nodes) = probed_block(&runtime);
    let ordinal = tree.state.hooks().ordinal();
    let cases: [(&str, Access, &[bool]); 16] = [
        ("read", |block, _| drop(block.read()), &[false]),
        ("write", |block, _| drop(block.write_for_tests()), &[true]),
        ("saving", |block, _| drop(block.saving_for_tests()), &[true]),
        (
            "a save's image",
            |block, _| {
                drop(block.saving_for_tests().image(Hash::default()));
            },
            &[true],
        ),
        ("read_owned", |block, _| drop(block.read_owned()), &[false]),
        (
            "node_name_ref",
            |block, nodes| drop(block.node_name_ref(nodes.file)),
            &[false],
        ),
        (
            "node_and_name_ref",
            |block, nodes| drop(block.node_and_name_ref(nodes.file)),
            &[false],
        ),
        (
            "read_node_and_name",
            |block, nodes| block.read_node_and_name(nodes.file, |_, _| ()),
            &[false],
        ),
        (
            "read_node_names",
            |block, nodes| block.read_node_names(nodes.file, |_, _| ()),
            &[false],
        ),
        (
            "node_name_clone",
            |block, nodes| drop(block.node_name_clone(nodes.file)),
            &[false],
        ),
        (
            "chain_log_bytes",
            |block, _| {
                let _ = block.chain_log_bytes();
            },
            &[false],
        ),
        (
            "memory_bytes",
            |block, _| {
                let _ = block.memory_bytes();
            },
            &[false],
        ),
        (
            "node",
            |block, nodes| {
                let _ = block.node(nodes.file);
            },
            &[false],
        ),
        (
            "chain_changes",
            |block, nodes| {
                let _ = block.chain_changes(nodes.dir);
            },
            &[false],
        ),
        (
            "watch_chain, installing a log",
            |block, nodes| {
                let _ = block.watch_chain(nodes.dir, None);
            },
            &[false, true],
        ),
        (
            "watch_chain, its log installed",
            |block, nodes| {
                let _ = block.watch_chain(nodes.dir, None);
            },
            &[false],
        ),
    ];
    for (access, run, writes) in cases {
        let recorder = Arc::new(Recorder::default());
        let installed = hooks::install(recorder.clone());
        run(&block, nodes);
        drop(installed);
        let expected: Vec<(Resource, bool)> = writes
            .iter()
            .map(|write| (Resource::Block(ordinal, 0), *write))
            .collect();
        assert_eq!(recorder.touches(), expected, "{access}");
    }
}

/// Whatever a holder holds of a block's lock.
trait Held {}

impl<T> Held for T {}

/// How a holder holds a block's lock: by a guard it keeps.
type Hold = for<'a> fn(&'a NodeBlock, Nodes) -> Box<dyn Held + 'a>;

/// How a holder holds a block's lock inside a closure, which calls `wait` while it holds it.
type Within = fn(&NodeBlock, Nodes, &mut dyn FnMut());

/// How a probe case's holder holds the lock.
#[derive(Clone, Copy)]
enum Holding {
    Guard(Hold),
    Within(Within),
}

/// A probe case: what it is, how the holder holds, whether in a task, how the taker takes, and
/// the mode and holders the probe names.
type Pair = (
    &'static str,
    Hold,
    bool,
    Access,
    Mode,
    [Option<Holder>; HOLDER_SLOTS],
);

fn writer(block: &NodeBlock, _: Nodes) -> Box<dyn Held + '_> {
    Box::new(block.write_for_tests())
}

fn reader(block: &NodeBlock, _: Nodes) -> Box<dyn Held + '_> {
    Box::new(block.read())
}

fn owned_reader_and_name(block: &NodeBlock, nodes: Nodes) -> Box<dyn Held + '_> {
    Box::new((
        block.read_owned(),
        block.node_name_ref(nodes.file).expect("the name"),
    ))
}

fn name_held(block: &NodeBlock, nodes: Nodes) -> Box<dyn Held + '_> {
    Box::new(block.node_name_ref(nodes.file).expect("the name"))
}

fn node_and_name(block: &NodeBlock, nodes: Nodes) -> Box<dyn Held + '_> {
    Box::new(block.node_and_name_ref(nodes.file).1.expect("the name"))
}

fn saving(block: &NodeBlock, _: Nodes) -> Box<dyn Held + '_> {
    Box::new(block.saving_for_tests())
}

fn image(block: &NodeBlock, _: Nodes) -> Box<dyn Held + '_> {
    Box::new(block.saving_for_tests().image(Hash::default()))
}

/// A save and four owned readers, the reader slots all taken, after a fifth reader, which found
/// no slot, let go.
fn saving_and_readers(block: &NodeBlock, _: Nodes) -> Box<dyn Held + '_> {
    let saving = block.saving_for_tests();
    let readers: Vec<_> = (0..HOLDER_SLOTS - 1).map(|_| block.read_owned()).collect();
    drop(block.read_owned());
    Box::new((saving, readers))
}

fn read(block: &NodeBlock, _: Nodes) {
    drop(block.read());
}

fn write(block: &NodeBlock, _: Nodes) {
    drop(block.write_for_tests());
}

fn save(block: &NodeBlock, _: Nodes) {
    drop(block.saving_for_tests());
}

fn name(block: &NodeBlock, nodes: Nodes) {
    drop(block.node_name_ref(nodes.file).expect("the name"));
}

/// `block`'s lock held as `hold` holds it, on one thread, in task 7 or outside a task, until task
/// 9 on another, taking it as `take` does, finds it held: what the probe told the taker's
/// scheduler. Asserts the taker touched the block only once it held the lock, after the probe.
fn contention(
    block: &Arc<NodeBlock>,
    nodes: Nodes,
    hold: Holding,
    in_task: bool,
    take: Access,
) -> Contention {
    let recorder = Arc::new(Recorder::default());
    let (held, holding) = std::sync::mpsc::channel();
    let holder = {
        let (block, recorder) = (block.clone(), recorder.clone());
        std::thread::spawn(move || {
            let _installed = hooks::install(recorder.clone());
            let holds = async move {
                let mut wait = || {
                    held.send(()).expect("the taker waits");
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while recorder.contentions().is_empty() {
                        assert!(
                            Instant::now() < deadline,
                            "the taker never found the lock held"
                        );
                        std::thread::sleep(Duration::from_millis(1));
                    }
                };
                match hold {
                    Holding::Guard(hold) => {
                        let guard = hold(&block, nodes);
                        wait();
                        drop(guard);
                    }
                    Holding::Within(within) => within(&block, nodes, &mut wait),
                }
            };
            if in_task {
                current_thread().block_on(hooks::as_task(7, holds));
            } else {
                current_thread().block_on(holds);
            }
        })
    };
    let taker = {
        let (block, recorder) = (block.clone(), recorder.clone());
        std::thread::spawn(move || {
            let _installed = hooks::install(recorder);
            holding.recv().expect("the holder holds");
            current_thread().block_on(hooks::as_task(9, async move { take(&block, nodes) }));
        })
    };
    holder.join().expect("the holder");
    taker.join().expect("the taker");
    let events = recorder.events();
    let contended: Vec<(usize, Contention)> = events
        .iter()
        .enumerate()
        .filter_map(|(at, event)| match event {
            Event::Contended(contention) => Some((at, *contention)),
            _ => None,
        })
        .collect();
    let [(at, contention)] = contended.as_slice() else {
        panic!("one contention: {events:?}");
    };
    let write = contention.mode != Mode::Read;
    assert!(
        events[at + 1..].iter().any(|event| matches!(
            event,
            Event::Touched(resource, touched) if *resource == contention.block && *touched == write
        )),
        "the taker touches the block once it holds it: {events:?}"
    );
    *contention
}

/// A task taking a block's lock that another holds tells the scheduler before it blocks, naming
/// the block, itself, how it takes it, and each holder by task and mode. The holders: a writer, a
/// reader, owned readers, a name's holder, a save and its image, which reads, and a save beside
/// four readers, a fifth gone. The takers: a read, a write, a save, a name, a node and its name,
/// an owned reader, and the direct reads of a name, a watch and the block's sizes. Each pair runs
/// on one block in turn, so a holder named that has let go would show in the next pair.
#[test]
fn the_lock_probe_names_the_holders_of_a_block_found_held() {
    let runtime = current_thread();
    let (tree, block, nodes) = probed_block(&runtime);
    let held = |task, mode| Some(Holder { task, mode });
    let (seven, write_mode, read_mode) = (Some(7), Mode::Write, Mode::Read);
    let a_writer = [held(seven, write_mode), None, None, None, None];
    let a_reader = [None, held(seven, read_mode), None, None, None];
    let pairs: [Pair; 16] = [
        ("a writer, a read", writer, true, read, read_mode, a_writer),
        (
            "a reader, a write",
            reader,
            true,
            write,
            write_mode,
            a_reader,
        ),
        (
            "two owned readers, a write",
            owned_reader_and_name,
            true,
            write,
            write_mode,
            [
                None,
                held(seven, read_mode),
                held(seven, read_mode),
                None,
                None,
            ],
        ),
        (
            "a node and its name, a write",
            node_and_name,
            true,
            write,
            write_mode,
            a_reader,
        ),
        (
            "a save, a write",
            saving,
            true,
            write,
            write_mode,
            [held(seven, Mode::Upgradable), None, None, None, None],
        ),
        (
            "a writer, a save",
            writer,
            true,
            save,
            Mode::Upgradable,
            a_writer,
        ),
        (
            "a writer outside a task, a name",
            writer,
            false,
            name,
            read_mode,
            [held(None, write_mode), None, None, None, None],
        ),
        (
            "a save's image, a write",
            image,
            true,
            write,
            write_mode,
            a_reader,
        ),
        (
            "a save and four readers, a fifth gone, a write",
            saving_and_readers,
            true,
            write,
            write_mode,
            [
                held(seven, Mode::Upgradable),
                held(seven, read_mode),
                held(seven, read_mode),
                held(seven, read_mode),
                held(seven, read_mode),
            ],
        ),
        (
            "a name, a write",
            name_held,
            true,
            write,
            write_mode,
            a_reader,
        ),
        (
            "a writer, a node and its name",
            writer,
            true,
            |block, nodes| drop(block.node_and_name_ref(nodes.file)),
            read_mode,
            a_writer,
        ),
        (
            "a writer, an owned reader",
            writer,
            true,
            |block, _| drop(block.read_owned()),
            read_mode,
            a_writer,
        ),
        (
            "a writer, a name's copy",
            writer,
            true,
            |block, nodes| drop(block.node_name_clone(nodes.file)),
            read_mode,
            a_writer,
        ),
        (
            "a writer, a watch",
            writer,
            true,
            |block, nodes| {
                let _ = block.watch_chain(nodes.dir, None);
            },
            read_mode,
            a_writer,
        ),
        (
            "a writer, the chain logs' bytes",
            writer,
            true,
            |block, _| {
                let _ = block.chain_log_bytes();
            },
            read_mode,
            a_writer,
        ),
        (
            "a writer, the block's bytes",
            writer,
            true,
            |block, _| {
                let _ = block.memory_bytes();
            },
            read_mode,
            a_writer,
        ),
    ];
    let ordinal = tree.state.hooks().ordinal();
    for (pair, hold, in_task, take, mode, holders) in pairs {
        let contention = contention(&block, nodes, Holding::Guard(hold), in_task, take);
        assert_eq!(contention.block, Resource::Block(ordinal, 0), "{pair}");
        assert_eq!(
            (contention.waiter, contention.mode, contention.holders),
            (Some(9), mode, holders),
            "{pair}"
        );
    }
}

/// The closure readers hold their block's lock, a reader the probe names, for as long as their
/// closure runs.
#[test]
fn the_closure_readers_are_named_while_they_read() {
    let runtime = current_thread();
    let (tree, block, nodes) = probed_block(&runtime);
    let ordinal = tree.state.hooks().ordinal();
    let readers: [(&str, Within); 2] = [
        ("read_node_and_name", |block, nodes, wait| {
            block.read_node_and_name(nodes.file, |_, _| wait());
        }),
        ("read_node_names", |block, nodes, wait| {
            block.read_node_names(nodes.file, |_, _| wait());
        }),
    ];
    for (reader, within) in readers {
        let contention = contention(&block, nodes, Holding::Within(within), true, write);
        assert_eq!(
            (
                contention.block,
                contention.waiter,
                contention.mode,
                contention.holders
            ),
            (
                Resource::Block(ordinal, 0),
                Some(9),
                Mode::Write,
                [
                    None,
                    Some(Holder {
                        task: Some(7),
                        mode: Mode::Read
                    }),
                    None,
                    None,
                    None
                ]
            ),
            "{reader}"
        );
    }
}

/// A watch of a directory whose chain log has filled grows the log, reading the block and then
/// writing it; the next watch only reads.
#[tokio::test]
async fn a_watch_growing_a_full_log_reads_then_writes() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f"]).await;
        let ordinal = tree.state.hooks().ordinal();
        let d = tree.id("d");
        let block = tree
            .state
            .block(tree.repository.clone(), NodeBlock::index(d))
            .await
            .expect("the directory's block");
        let _ = block.watch_chain(Node::index(d), None);
        for index in 0..=CHAIN_LOG_INITIAL {
            let name = format!("g{index}");
            tree.state
                .node_add(tree.repository.clone(), d, file(&name), &name)
                .await
                .expect("an add");
        }
        let touched = |write| (Resource::Block(ordinal, NodeBlock::index(d)), write);
        for expected in [vec![touched(false), touched(true)], vec![touched(false)]] {
            let recorder = Arc::new(Recorder::default());
            let installed = hooks::install(recorder.clone());
            let _ = block.watch_chain(Node::index(d), None);
            drop(installed);
            assert_eq!(recorder.touches(), expected);
        }
    })
    .await;
}

/// The probes' scenario context: a tree, and whether its writer has begun its write.
struct NameHeld {
    tree: Tree,
    writing: AtomicBool,
}

/// A task that holds a name across an await that is no scheduling point, against C13, keeps a
/// task the scheduler runs meanwhile from the name's block: the scheduler fails the run there, as
/// the deadlock it is on the run's one thread, naming the block by its State's ordinal and the
/// holder by its task.
#[test]
fn a_task_finding_a_block_held_fails_the_run() {
    let holds_a_name: Op<NameHeld> = Box::new(|context: Arc<NameHeld>| {
        Box::pin(async move {
            let tree = &context.tree;
            let f = tree.id("d/f");
            let block = tree
                .state
                .block_with_nametable(tree.repository.clone(), NodeBlock::index(f))
                .await
                .expect("the block with its names");
            let name = block.node_name_ref(Node::index(f)).expect("the name");
            // Yields, which is no scheduling point, until the writer has begun.
            for _ in 0..1_000_000 {
                if context.writing.load(Ordering::Relaxed) {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(context.writing.load(Ordering::Relaxed), "the writer ran");
            drop(name);
        })
    });
    let writes: Op<NameHeld> = Box::new(|context: Arc<NameHeld>| {
        Box::pin(async move {
            let tree = &context.tree;
            let block = tree
                .state
                .block(tree.repository.clone(), NodeBlock::index(tree.id("d/f")))
                .await
                .expect("the block");
            context.writing.store(true, Ordering::Relaxed);
            drop(block.write_for_tests());
        })
    });
    let scenario = Scenario::<NameHeld> {
        name: "a name held across an await",
        setup: Box::new(|| {
            Box::pin(async {
                // So the tree's State takes ordinal 1.
                let _first = State::new();
                NameHeld {
                    tree: Tree::new(&["d/f"]).await,
                    writing: AtomicBool::new(false),
                }
            })
        }),
        ops: vec![holds_a_name, writes],
        cancellable: vec![],
        check: Box::new(|_| Box::pin(async {})),
    };
    let explored = std::panic::catch_unwind(AssertUnwindSafe(|| {
        scenario.explore(Exploration::default())
    }));
    let message = panic_message(explored.expect_err("the run fails"));
    assert!(
        message.contains("task Some(1) found Block(1, 0) held, taking it Write")
            && message.contains("Holder { task: Some(0), mode: Read }"),
        "{message}"
    );
}

/// A task reaching a scheduling point holding a block's borrowed guard is caught there, before
/// it can keep every other task from the block (C13); so is one at a walk's point, where the
/// scheduler parks walks, and a thread in a window. Blocks of two States lock in order each by its
/// own State.
#[test]
fn a_task_parking_holding_a_block_guard_is_caught() {
    let runtime = current_thread();
    let (_tree, block, _) = probed_block(&runtime);
    let installed = hooks::install(Arc::new(Recorder::default()));
    let parked = std::panic::catch_unwind(AssertUnwindSafe(|| {
        runtime.block_on(async {
            let reader = block.read();
            hooks::reached(Point::Named("holding")).await;
            drop(reader);
        });
    }));
    let message = panic_message(parked.expect_err("the park is caught"));
    assert!(message.contains("holding 1 node block guards"), "{message}");

    let windowed = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let writer = block.write_for_tests();
        hooks::window(Window::Rename);
        drop(writer);
    }));
    let message = panic_message(windowed.expect_err("the window is caught"));
    assert!(message.contains("holds 1 node block guards"), "{message}");
    drop(installed);

    let walks = hooks::install(Arc::new(Recorder {
        reader_points: true,
        ..Recorder::default()
    }));
    let walked = std::panic::catch_unwind(AssertUnwindSafe(|| {
        runtime.block_on(async {
            let reader = block.read();
            hooks::reached_reader(Point::IterStep).await;
            drop(reader);
        });
    }));
    let message = panic_message(walked.expect_err("the walk's point is caught"));
    assert!(message.contains("holding 1 node block guards"), "{message}");
    drop(walks);

    // Blocks of two States lock in order each by its own State: block 0 of each, nested.
    let (_other_tree, other, _) = probed_block(&runtime);
    let first = block.read();
    let second = other.read();
    drop((second, first));
}

/// A wait's end is told to the scheduler that heard it begin, whatever thread it ends on.
#[test]
fn a_wait_ends_where_it_began() {
    let recorder = Arc::new(Recorder::default());
    let installed = hooks::install(recorder.clone());
    let waiting = hooks::waiting(Wait::BlockLoad);
    std::thread::spawn(move || drop(waiting))
        .join()
        .expect("the other thread");
    drop(installed);
    assert_eq!(
        recorder.waits(),
        [(Wait::BlockLoad, true), (Wait::BlockLoad, false)]
    );
}

/// A task reaching a scheduling point holding an owned hold, a name, a node's name or an owned
/// reader, is caught there too (C13); a save's, its upgradable read or its image, which a save
/// keeps across its store writes, is not counted.
#[test]
fn a_task_parking_holding_an_owned_hold_is_caught_and_a_save_is_not() {
    let runtime = current_thread();
    let (_tree, block, nodes) = probed_block(&runtime);
    let _installed = hooks::install(Arc::new(Recorder::default()));
    let holds: [(&str, Hold, usize); 3] = [
        ("a name", name_held, 1),
        ("a node and its name", node_and_name, 1),
        ("an owned reader and a name", owned_reader_and_name, 2),
    ];
    for (hold, take, count) in holds {
        let parked = std::panic::catch_unwind(AssertUnwindSafe(|| {
            runtime.block_on(hooks::as_task(3, async {
                let held = take(&block, nodes);
                hooks::reached(Point::Named("holding")).await;
                drop(held);
            }));
        }));
        let message = panic_message(parked.expect_err("the park is caught"));
        assert!(
            message.contains(&format!("holding {count} owned node block holds")),
            "{hold}: {message}"
        );
    }

    runtime.block_on(hooks::as_task(4, async {
        let saving = block.saving_for_tests();
        hooks::reached(Point::Named("saving")).await;
        let image = saving.image(Hash::default());
        hooks::reached(Point::Named("imaged")).await;
        drop(image);
        let name = block.node_name_ref(nodes.file).expect("the name");
        drop(name);
        hooks::reached(Point::Named("released")).await;
    }));
}

/// Content for a restore.
fn content() -> Content {
    Content {
        mode: 0,
        size: 7,
        address: Address::zero_context_hash(Hash::from_u64(hash_string("restored"))),
    }
}

/// A restore reaches its point between its read of the node's parent and its lock of the pair,
/// where no future can be dropped; a rename passes its window between its read of the parent
/// and its section.
#[tokio::test]
async fn a_restore_reaches_its_lock_point_and_a_rename_its_window() {
    crate::in_context(async {
        assert!(
            !Point::RestoreLock.droppable(),
            "no drop lands at the point"
        );
        let tree = Tree::new(&["p/b", "p/c"]).await;
        let (p, b, c) = (tree.id("p"), tree.id("p/b"), tree.id("p/c"));
        let marked = tree
            .state
            .node_delete_level(&tree.repository, b, p)
            .await
            .expect("the delete marks its level");
        assert!(matches!(marked, LevelMark::Marked(_)), "{marked:?}");

        let (recorder, ()) = recording(async {
            tree.state
                .node_restore(&tree.repository, b, &content())
                .await
                .expect("the restore");
            tree.state
                .node_rename(tree.repository.clone(), c, NameChange::Rename("e"))
                .await
                .expect("the rename");
        })
        .await;
        assert!(
            recorder.points().contains(&Point::RestoreLock),
            "{:?}",
            recorder.points()
        );
        assert_eq!(recorder.windows(), [Window::Rename]);
    })
    .await;
}

/// Walks park at their points inside a chain and a path only where the scheduler asks for them:
/// a path's walk once per node each way, and each children iterator once per child and once at
/// the chain's end, holding nothing once a yielded name is dropped.
#[tokio::test]
async fn walks_park_at_their_points_only_where_asked() {
    crate::in_context(async {
        let tree = Tree::new(&["p/a", "p/b"]).await;
        let p = tree.id("p");
        for reader_points in [false, true] {
            let recorder = Arc::new(Recorder {
                reader_points,
                ..Recorder::default()
            });
            let installed = hooks::install(recorder.clone());
            hooks::as_task(0, async {
                tree.state
                    .node_path(tree.repository.clone(), tree.id("p/a"))
                    .await
                    .expect("the path");
                let mut named = StateNodeChildrenWithNameIterator::new(
                    tree.state.clone(),
                    tree.repository.clone(),
                    p,
                )
                .await
                .expect("the walk");
                while let Some((_, _, name)) = named.next().await.expect("a child") {
                    drop(name);
                }
                let mut children =
                    StateNodeChildrenIterator::new(tree.state.clone(), tree.repository.clone(), p)
                        .await
                        .expect("the walk");
                while children.next().await.expect("a child").is_some() {}
            })
            .await;
            drop(installed);
            let points = recorder.points();
            let count = |point: Point| points.iter().filter(|reached| **reached == point).count();
            let expected = if reader_points { (4, 6) } else { (0, 0) };
            assert_eq!(
                (count(Point::NodePath), count(Point::IterStep)),
                expected,
                "{points:?}"
            );
        }
    })
    .await;
}

/// The deterministic scheduler parks a walk at its points where its exploration asks, and only
/// there: a path's walk of two nodes takes one decision point at its start, and four more with
/// them.
#[test]
fn the_scheduler_parks_walks_where_its_exploration_asks() {
    let walk: Op<Tree> = Box::new(|tree: Arc<Tree>| {
        Box::pin(async move {
            tree.state
                .node_path(tree.repository.clone(), tree.id("p/a"))
                .await
                .expect("the path");
        })
    });
    let scenario = Scenario::<Tree> {
        name: "a path's walk",
        setup: Box::new(|| Box::pin(Tree::new(&["p/a"]))),
        ops: vec![walk],
        cancellable: vec![],
        check: Box::new(|_| Box::pin(async {})),
    };
    let deepest = |reader_points| {
        scenario
            .explore(Exploration {
                reader_points,
                ..Exploration::default()
            })
            .deepest
    };
    assert_eq!((deepest(false), deepest(true)), (1, 5));
}

/// Polls `future` until the recorder holds the beginning of a wait on `wait`, the future still
/// pending: the wait stands while what it waits on is held.
async fn until_waiting<F: Future>(
    mut future: std::pin::Pin<&mut F>,
    recorder: &Recorder,
    wait: Wait,
) {
    for _ in 0..10_000 {
        if recorder.waits().contains(&(wait, true)) {
            return;
        }
        tokio::select! {
            biased;
            _ = future.as_mut() => panic!("finished while its {wait:?} was held"),
            () = tokio::task::yield_now() => {}
        }
    }
    panic!("never began waiting on {wait:?}");
}

/// Each wait on `wait`, begun and ended, `waits` times over.
fn pairs(wait: Wait, waits: usize) -> Vec<(Wait, bool)> {
    (0..waits)
        .flat_map(|_| [(wait, true), (wait, false)])
        .collect()
}

/// Each wait an operation makes on another task is marked where it begins and where it ends,
/// alone: the moves permit, and an add's unused permit, for the full block it retires and for
/// the fresh one it allocates.
#[tokio::test]
async fn waits_are_marked_where_they_begin_and_end() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f"]).await;
        let full = Tree::spread(&["d/"], "fill", &[]).await;

        let (recorder, permit) = recording(tree.state.moves_permit()).await;
        drop(permit.expect("the moves permit"));
        assert_eq!(recorder.waits(), pairs(Wait::MovesPermit, 1), "the permit");
        no_touch_while_waiting(&recorder.events());

        let (recorder, added) =
            recording(
                full.state
                    .node_add(full.repository.clone(), full.id("d"), file("h"), "h"),
            )
            .await;
        added.expect("the add");
        let unused: Vec<(Wait, bool)> = recorder
            .waits()
            .into_iter()
            .filter(|(wait, _)| *wait == Wait::UnusedPermit)
            .collect();
        // The add loads its slot's file metadata block too, which the fresh block's slot needs.
        assert_eq!(unused, pairs(Wait::UnusedPermit, 2), "the add");
        no_touch_while_waiting(&recorder.events());
    })
    .await;
}

/// Polls `future` until `store` has stalled more reads than `seen`, or the future ends: its
/// output where it ended.
async fn until_stalled<F: Future>(
    mut future: std::pin::Pin<&mut F>,
    store: &FailingStore,
    seen: u64,
) -> Option<F::Output> {
    for _ in 0..100_000 {
        if store.stalls() > seen {
            return None;
        }
        tokio::select! {
            biased;
            output = future.as_mut() => return Some(output),
            () = tokio::task::yield_now() => {}
        }
    }
    panic!("neither stalled nor finished");
}

/// `future` run with every read of `store` stalled until let through: the waits recorded as
/// each read stalled, every event recorded, and the future's output.
async fn at_each_stall<F: Future>(
    future: F,
    store: &FailingStore,
) -> (Vec<Vec<(Wait, bool)>>, Vec<Event>, F::Output) {
    let recorder = Arc::new(Recorder::default());
    let installed = hooks::install(recorder.clone());
    let before = store.stalls();
    store.stall(true);
    let mut future = std::pin::pin!(future);
    let mut stalls = Vec::new();
    let output = loop {
        let seen = before + stalls.len() as u64;
        if let Some(output) = until_stalled(future.as_mut(), store, seen).await {
            break output;
        }
        stalls.push(recorder.waits());
        store.pass();
    };
    store.stall(false);
    drop(installed);
    (stalls, recorder.events(), output)
}

/// The waits among `events`.
fn waits_of(events: &[Event]) -> Vec<(Wait, bool)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Waiting(wait, begins) => Some((*wait, *begins)),
            _ => None,
        })
        .collect()
}

/// Asserts no lock is touched while a wait stands, for an operation on one path: a mark ending
/// late, past the next step its operation runs, would count the operation as waiting there.
fn no_touch_while_waiting(events: &[Event]) {
    let mut open = 0usize;
    for (at, event) in events.iter().enumerate() {
        match event {
            Event::Waiting(_, true) => open += 1,
            Event::Waiting(_, false) => open -= 1,
            Event::Touched(..) => {
                assert_eq!(open, 0, "a touch inside a wait at {at}: {events:?}");
            }
            _ => {}
        }
    }
}

/// Asserts that at each stalled read the last wait recorded is a wait on `wait` begun and not
/// ended, the only one open, and that there were at least `reads` stalls.
fn open_at_each_stall(stalls: &[Vec<(Wait, bool)>], wait: Wait, reads: usize, what: &str) {
    assert!(stalls.len() >= reads, "{what}: {} stalls", stalls.len());
    for waits in stalls {
        let open = waits.iter().filter(|(_, begins)| *begins).count()
            - waits.iter().filter(|(_, begins)| !*begins).count();
        assert_eq!(
            (waits.last(), open),
            (Some(&(wait, true)), 1),
            "{what}: a read stalled outside its wait: {waits:?}"
        );
    }
}

/// Each store read is marked for as long as the read stands, every mark ending once its read
/// goes on, before the operation's next touch: a first block load's reads of the tree, the block
/// list and the block, besides its waits on the list's lock and the loading permit; a name
/// table's; a file metadata block's reads of its list and itself, besides its waits on its loading
/// permit and the list's lock; and a prefetch's reads of the tree and the block list, besides its
/// wait on the list's lock.
#[tokio::test]
async fn a_store_read_is_marked_for_as_long_as_it_stands() {
    crate::in_context(async {
        let (spread, spread_store) = Tree::spread(&["a/x"], "fill", &["b/y"])
            .await
            .failing()
            .await;
        let (stalls, events, y) = at_each_stall(
            spread
                .state
                .node(spread.repository.clone(), spread.id("b/y")),
            &spread_store,
        )
        .await;
        y.expect("'b/y' reads");
        open_at_each_stall(&stalls, Wait::BlockLoad, 3, "the load");
        assert_eq!(waits_of(&events), pairs(Wait::BlockLoad, 5), "the load");
        no_touch_while_waiting(&events);

        let (tree, store, freed) = Tree::with_a_free_slot(Some("d/g")).await;
        let (stalls, events, names) = at_each_stall(
            tree.state
                .block_with_nametable(tree.repository.clone(), NodeBlock::index(freed)),
            &store,
        )
        .await;
        names.expect("the names");
        open_at_each_stall(&stalls, Wait::NameTable, 1, "the names");
        assert_eq!(waits_of(&events), pairs(Wait::NameTable, 1), "the names");
        no_touch_while_waiting(&events);

        let (stalls, events, metadata) = at_each_stall(
            tree.state.block_file_metadata(
                tree.repository.clone(),
                NodeFileMetadataBlock::index(node::node_to_file_metadata(freed)),
            ),
            &store,
        )
        .await;
        metadata.expect("the file metadata block");
        open_at_each_stall(&stalls, Wait::FileMetadata, 2, "the file metadata");
        assert_eq!(
            waits_of(&events),
            pairs(Wait::FileMetadata, 4),
            "the file metadata"
        );
        no_touch_while_waiting(&events);

        let (fresh, fresh_store) = Tree::new(&["d/f"]).await.failing().await;
        let (stalls, events, lists) = at_each_stall(
            fresh.state.block_address_lists(fresh.repository.clone()),
            &fresh_store,
        )
        .await;
        lists.expect("the lists");
        open_at_each_stall(&stalls, Wait::BlockLoad, 2, "the lists");
        // The tree is the first revision's, which stores no file metadata list. The two lists'
        // reads run joined in one task, so the file metadata list's touch lands inside the block
        // list's wait: no touch check here.
        assert_eq!(waits_of(&events), pairs(Wait::BlockLoad, 3), "the lists");
    })
    .await;
}

/// A wait on a permit another holds stands until the holder lets go: a second take of the moves
/// permit, a directory's move, an add taking the unused permit, and an add whose slot fails to
/// load freeing it again.
#[tokio::test]
async fn a_wait_on_a_held_permit_stands_until_it_is_released() {
    crate::in_context(async {
        let tree = Tree::new(&["a/d/x", "b/y"]).await;
        for mover in [false, true] {
            let held = tree.state.moves_permit().await.expect("the moves permit");
            let recorder = Arc::new(Recorder::default());
            let installed = hooks::install(recorder.clone());
            let waiter = async {
                if mover {
                    tree.state
                        .node_reattach(tree.repository.clone(), tree.id("a/d"), tree.id("b"), "d")
                        .await
                        .map(drop)
                } else {
                    tree.state.moves_permit().await.map(drop)
                }
            };
            let mut waiter = std::pin::pin!(waiter);
            until_waiting(waiter.as_mut(), &recorder, Wait::MovesPermit).await;
            assert_eq!(
                recorder.waits(),
                [(Wait::MovesPermit, true)],
                "waits while held"
            );
            drop(held);
            waiter.await.expect("the take once released");
            drop(installed);
            assert_eq!(
                recorder.waits(),
                [(Wait::MovesPermit, true), (Wait::MovesPermit, false)],
                "the wait ends once released"
            );
        }

        let full = Tree::spread(&["d/"], "fill", &[]).await;
        let held = full
            .state
            .acquire_unused()
            .await
            .expect("the unused permit");
        let recorder = Arc::new(Recorder::default());
        let installed = hooks::install(recorder.clone());
        let mut add = std::pin::pin!(full.state.node_add(
            full.repository.clone(),
            full.id("d"),
            file("h"),
            "h"
        ));
        until_waiting(add.as_mut(), &recorder, Wait::UnusedPermit).await;
        assert_eq!(recorder.waits(), [(Wait::UnusedPermit, true)]);
        drop(held);
        add.await.expect("the add");
        drop(installed);
        assert_eq!(
            recorder.waits()[..2],
            [(Wait::UnusedPermit, true), (Wait::UnusedPermit, false)]
        );

        let (tree, store, _freed) = Tree::with_a_free_slot(Some("d/f")).await;
        store.fail(true);
        let held = tree
            .state
            .acquire_unused()
            .await
            .expect("the unused permit");
        let recorder = Arc::new(Recorder::default());
        let installed = hooks::install(recorder.clone());
        let mut add = std::pin::pin!(tree.state.node_add(
            tree.repository.clone(),
            tree.id("d"),
            file("h"),
            "h"
        ));
        until_waiting(add.as_mut(), &recorder, Wait::UnusedPermit).await;
        let begun = recorder.waits();
        assert_eq!(
            begun.last(),
            Some(&(Wait::UnusedPermit, true)),
            "the release waits while held: {begun:?}"
        );
        drop(held);
        let added = add.await;
        drop(installed);
        store.fail(false);
        assert!(added.is_err(), "the slot fails to load: {added:?}");
        assert_eq!(
            recorder.waits()[begun.len()..],
            [(Wait::UnusedPermit, false)],
            "the release's wait ends once released"
        );
    })
    .await;
}
