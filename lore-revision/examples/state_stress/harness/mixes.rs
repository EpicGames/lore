// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Mixes of operations run on threads and replayed: each task one operation, all run at once on a
//! chaos runtime's workers, which yield at random scheduling points, in bursts, each ending in the
//! quiescent reclaim ([`reclaim`]) alone; then each burst's log replayed through the model, started
//! where the mix's setup ended, with a checkpoint between bursts and at the end. Each mix
//! runs its tier's kinds together, more at once than production runs them: the exclusive claim's
//! kinds together, the shared claim's (the C API's) together, the served kinds beside one holder of
//! the swap plan, the one-at-a-time slot a VFS provider runs its settles, swaps and flips in. Three
//! go past the tiers: `exclusive` runs `lore dirty`, the scan, the commit's and the reset's clears
//! at once, where production runs one command per exclusive claim; `listing` runs the stage's
//! marked listings beside served writers in the directories they list; and [`Op::Rename`] renames
//! to a name of another hash beside other writers, which no caller does. An operation the mix
//! leaves nothing to refuse must succeed, so a wrong refusal, which writes nothing and so agrees
//! with the model, still fails the run.
//!
//! A run's [`Variant`] says what it does beyond that: drop operations part way where their callers
//! may, or fail the store's reads, each in bursts with a checkpoint, a save and an eviction between
//! them, so threads meet the drops and the faults the explorations step through one at a time.
//!
//! The deterministic scenarios run each mix in full on fixed seeds; the stress harness runs them
//! over and over, each with a random number of rounds, in a random order, on random settings.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use lore_base::types::Hash;
use lore_revision::node::BLOCK_NODE_COUNT;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::NodeIDExt;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::Content;
use lore_revision::state::DiscardCounts;
use lore_revision::state::Ext;
use lore_revision::state::LinkPin;
use lore_revision::state::Mark;
use lore_revision::state::State;
use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Record;
use lore_storage::hash::hash_string;

use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Outcome;
use crate::harness::catalogue::call::Refusal;
use crate::harness::catalogue::call::any_call_failed;
use crate::harness::catalogue::call::noting_failures;
use crate::harness::catalogue::modelled::Labels;
use crate::harness::catalogue::modelled::check_log;
use crate::harness::catalogue::ops;
use crate::harness::catalogue::ops::Flagged;
use crate::harness::catalogue::ops::NameRead;
use crate::harness::catalogue::ops::directory;
use crate::harness::catalogue::ops::file;
use crate::harness::chaos;
use crate::harness::chaos::Chaos;
use crate::harness::faults::FailingStore;
use crate::harness::fixtures;
use crate::harness::fixtures::Pinned;
use crate::harness::fixtures::Tree;
use crate::harness::model::Model;
use crate::harness::model::Modelled;
use crate::harness::model::Start;
use crate::harness::model::checkpoint::Checkpoint;

/// The most rounds a mix's tree has room for: the exclusive claim's tree holds a private
/// directory for each round's discard.
pub const ROUNDS: usize = 4;

/// What an operation must answer: made, where the mix leaves nothing to refuse it, or either,
/// where another task may take its name, its directory or its file first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Must {
    Make,
    Race,
}

/// What one task does.
#[derive(Clone, Copy, Debug)]
pub enum Op {
    /// The provider diff's checked create in a directory: attached dirty-added, then the
    /// directory marked.
    Create(&'static str),
    /// A checked create, then its removal.
    CreateRemove(&'static str),
    /// The provider diff's add into a directory its walk listed: added dirty-added without a name
    /// check, as each task's name is its own, then the directory marked.
    DiffAdd(&'static str),
    /// `lore dirty`'s add of a file below a directory of the task's own it adds first, each marked,
    /// forced.
    DirtyAdd(&'static str),
    /// A scan's chain of two directories of the task's own below a directory.
    ScanChain(&'static str),
    /// An add of a file, then its discard: in a directory nothing else changes, as a discard
    /// runs where nothing writes or walks the chain it unlinks from (discard.rs,
    /// `detach_exclusive`).
    AddDiscard(&'static str),
    /// An add of a directory holding a file, then the directory's discard, there too.
    AddDiscardTree(&'static str),
    /// A rename of a node in place, then its own mark, through the pre-serving respelling's op, to
    /// a name of another hash, which no caller does beside other writers, as the respellings keep
    /// it: it covers the model's handling of a renamed hash among the writers the runs mix.
    Rename(&'static str),
    /// A scan's forced modify mark of a file.
    Mark(&'static str),
    /// An unserved clear of a file and the ancestors nothing keeps dirty.
    Clear(&'static str),
    /// A commit's clear of a node's staged flags, the node alone.
    ClearStaged(&'static str),
    /// A reset's clear of a directory and the ancestors nothing keeps dirty.
    Vacated(&'static str),
    /// A scan's rewrite of a file to new content, dirty in the same write, then its parent's
    /// mark: the ancestors marked after, from the parent up.
    RewriteDirty(&'static str),
    /// An in-place change of a file's size, where it still stands under its built parent and
    /// name.
    InPlace(&'static str),
    /// An owned change of a file no task frees, moves or renames.
    Owned(&'static str),
    /// An owned clear of that file's dirty flags.
    OwnedClear(&'static str),
    /// A repin of a link to one of the revisions its tree's links may be pinned to.
    Repin(&'static str, Repinning),
    /// A C API add into a directory; one raced is below a pending delete's root, published
    /// before the delete's marks reach its directory, or refused after.
    StagedAdd(&'static str, Must),
    /// A C API modify of a file; one raced is a file a delete may stage first.
    StagedModify(&'static str, Must),
    /// A C API delete of a file below its parent, under the moves permit, then its restore,
    /// which another task's restore may make first.
    DeleteRestore(&'static str, &'static str),
    /// A C API move of a node to a directory, under the moves permit: where `walked`, after the
    /// plan's walk of the destination's ancestors, as the C API moves an entry whose entries before
    /// it all applied; otherwise walked by the move itself, as after an entry that failed.
    StagedMove(&'static str, &'static str, bool),
    /// A C API add of a directory holding a file, then the C API delete of the directory:
    /// detached, queued pending, its file marked.
    AddPend(&'static str),
    /// The C API delete of a directory a setup added staged, below its parent, under the moves
    /// permit, held until another task's add has published, or a few seconds passed, so a publish
    /// lands while a task holds the permit however the workers run.
    Pend(&'static str, &'static str),
    /// A C API delete of the subtrees at the paths given, tracked and added, in one batch, its
    /// levels and detaches spread over futures of its own, under the moves permit it holds for
    /// the batch.
    DeleteBatch(&'static [&'static str]),
    /// A served create of a file in a directory; one raced is in a directory an RMDIR may take
    /// first.
    ServedCreate(&'static str, Must),
    /// A served create of a file of the name given, in a directory, which another task may take
    /// first.
    ServedCreateAs(&'static str, &'static str),
    /// A served create, then the served removal of the file made.
    ServedCreateRemove(&'static str),
    /// A served create of a file in a directory, then its rename over the file at the path given,
    /// one the revision holds, whose path it takes over (H20).
    ServedCreateReplacing(&'static str, &'static str),
    /// A served rename of a file to a free name in a directory.
    ServedRename(&'static str, &'static str),
    /// A served rename of a file to the name given, in a directory, which another task may take
    /// first.
    ServedRenameAs(&'static str, &'static str, &'static str),
    /// A served rename of a directory holding local content to a free name in a directory, as the
    /// provider's rename moves a directory a create made.
    ServedMoveDirectory(&'static str, &'static str),
    /// A served rename of a directory holding no local content to a free name in a directory,
    /// which the provider's rename refuses.
    ServedMoveNotLocal(&'static str, &'static str),
    /// A served RMDIR of a directory; one raced is a directory a create may fill first.
    ServedRmdir(&'static str, Must),
    /// A served mark of a directory.
    ServedMark(&'static str),
    /// A served copy-up of a file below its directory, which a rename may take away first.
    CopyUp(&'static str, &'static str),
    /// A settle of a directory against the committed tree, waiting a while between comparing and
    /// clearing, so the changes beside it land there; then a settle of the second, a directory
    /// nothing changes below, which must clear, and of each directory of the list, one after
    /// another, as a commit's settle goes on through the tree's dirty directories: a swap-plan
    /// holder.
    Settle(&'static str, &'static str, &'static [&'static str]),
    /// A swap's copy of a directory holding three files into a directory: a build, published
    /// whole, a wait before each add as the swap copies each file's content there.
    SwapAttach(&'static str),
    /// A swap's detach of a directory it finds clean.
    SwapDetach(&'static str),
    /// A swap's rewrite of a file it finds clean to the content its target holds.
    SwapRewrite(&'static str),
    /// A commit's flip of a dirty file to its committed content, clean.
    Flip(&'static str),
    /// A walk of a directory's children, by the op given.
    Children(Children, &'static str),
    /// A listing reader's create of a file in a directory, through its absence where its lookup
    /// found one.
    ListingCreate(&'static str),
    /// A listing reader's create of a file of the name given, in a directory, which another task
    /// may take first, waiting a little between its lookup and its attach, so a contender's create
    /// lands there however the workers run.
    ListingCreateAs(&'static str, &'static str),
    /// A listing reader's rename of a file to a free name in a directory.
    ListingRename(&'static str, &'static str),
    /// A marked listing of a directory, as the stage lists one, then a served create of the task's
    /// own there, counted in the mark, a wait, so other tasks' changes land after the mark, the
    /// searches since the mark for the create's name and for the name the mix contends for, and
    /// the mark's reattach read.
    MarkedReads(&'static str),
    /// Each name read of a node, then a comparison of its name with the one it was built with: of a
    /// node the mix never frees, as a skip, which the model holds no fixture to make, could come
    /// of a freed slot's stale name.
    Names(&'static str),
    /// A path read of a node from the root, then one below the directory it was built in.
    Paths(&'static str),
    /// Each has-children read of a directory, [`HAS_ROUNDS`] times, a short wait between, so the
    /// reads meet the directory as other tasks' changes of it go on.
    Has(&'static str),
    /// [`COLLECTIONS`] collections of the dirty paths below a directory, as the stage collects
    /// them, a wait of [`BETWEEN_COLLECTIONS`] between, so other tasks' changes of it land.
    DirtyPaths(&'static str),
    /// [`LINK_ROUNDS`] listings of a link by id and by name hash, and resolutions through it of
    /// `a` and of `c`, which the first and the second revision hold below some node a pin
    /// exposes, a short wait between, so the reads meet the repins of it.
    LinkReads(&'static str),
}

/// A repin's pin: whole, of `src` in the revision named; or of the revision alone, keeping the node
/// the link exposes, which there may be another directory, or a file.
#[derive(Clone, Copy, Debug)]
pub enum Repinning {
    Whole(Pinned),
    Revision(Pinned),
}

/// How many times an [`Op::Has`] makes each has-children read, each judged against the changes
/// around it.
const HAS_ROUNDS: usize = 6;

/// How many times an [`Op::LinkReads`] lists its link each way and resolves each path through it.
const LINK_ROUNDS: usize = 4;

/// How many collections an [`Op::DirtyPaths`] makes.
const COLLECTIONS: usize = 2;

/// The wait between an [`Op::DirtyPaths`]'s collections.
const BETWEEN_COLLECTIONS: Duration = Duration::from_millis(2);

/// A walk of a directory's children, by the op making it: an iterator's, a listing by id's or a
/// collection's.
#[derive(Clone, Copy, Debug)]
pub enum Children {
    Iterator,
    FromParent,
    NamedIterator,
    Listed,
    ListedNameHashes,
    Collection,
    VersionedCollection,
    NamedCollection,
}

impl Children {
    pub const EVERY: [Self; 8] = [
        Self::Iterator,
        Self::FromParent,
        Self::NamedIterator,
        Self::Listed,
        Self::ListedNameHashes,
        Self::Collection,
        Self::VersionedCollection,
        Self::NamedCollection,
    ];

    /// This walk of `dir`, noted on its call, which the model judges.
    async fn of(self, tree: &Tree, dir: NodeID) {
        let _ = match self {
            Self::Iterator => ops::children(tree, dir).await,
            Self::FromParent => ops::children_from_parent(tree, dir).await,
            Self::NamedIterator => ops::named_children(tree, dir).await,
            Self::Listed => ops::listed_children(tree, dir).await,
            Self::ListedNameHashes => ops::listed_name_hashes(tree, dir).await,
            Self::Collection => ops::collected_children(tree, dir).await,
            Self::VersionedCollection => ops::collected_versioned_children(tree, dir).await,
            Self::NamedCollection => ops::collected_named_children(tree, dir).await,
        };
    }
}

impl Op {
    /// Whether the operation holds the swap plan: one at a time beside the served kinds.
    pub fn holds_plan(self) -> bool {
        matches!(
            self,
            Self::Settle(..)
                | Self::SwapAttach(_)
                | Self::SwapDetach(_)
                | Self::SwapRewrite(_)
                | Self::Flip(_)
        )
    }

    /// Whether the operation starts first in its mix: a swap-plan holder, which the others wait
    /// for; or the pending delete of `s/t`, which holds the moves permit until another task's add
    /// has published, so it must take the permit before the adds have all published.
    fn starts_first(self) -> bool {
        self.holds_plan() || matches!(self, Self::Pend(..))
    }

    /// Whether it reads a revision a link pins: the listings and resolutions through a link, and
    /// the collections of `q`, which follow its dirty link. [`Variant::Fault`] runs none, as the
    /// reads of a link's revision answer a failed read in the store's own words, which no rule of
    /// the model's holds.
    pub fn reads_links(self) -> bool {
        matches!(self, Self::LinkReads(_) | Self::DirtyPaths("q"))
    }

    /// Whether its production callers may drop it part way, so [`Variant::Drop`] may: each of its
    /// calls is of a cancellable kind ([`crate::harness::catalogue::Kind`]); not `lore dirty`'s
    /// add, nor the C API delete's levels and pends, which run in a task the call awaits.
    pub fn cancellable(self) -> bool {
        !matches!(
            self,
            Self::DirtyAdd(_)
                | Self::DeleteRestore(..)
                | Self::AddPend(_)
                | Self::Pend(..)
                | Self::DeleteBatch(_)
        )
    }
}

/// What runs alone after a mix's operations, recording still.
pub type After = fn(Arc<Tree>) -> Pin<Box<dyn Future<Output = ()> + Send>>;

/// A mix's run: its log, its calls, the id of the State its operations changed, the nodes its
/// tree was built with, and what the chaos scheduler counted.
pub struct Run {
    pub log: Vec<Record>,
    pub calls: Vec<Call>,
    pub state: u64,
    pub nodes: Vec<(String, NodeID)>,
    /// How many times a task yielded at a scheduling point, and how many times a block's lock was
    /// found held.
    pub yields: u64,
    pub contentions: u64,
}

impl Run {
    /// The node the run's tree built at `path`.
    pub fn id(&self, path: &str) -> NodeID {
        self.nodes
            .iter()
            .find(|(built, _)| built == path)
            .map_or_else(|| panic!("no node built at '{path}'"), |(_, id)| *id)
    }

    /// The run's records of the State its operations changed.
    pub fn on_state(&self) -> impl Iterator<Item = &Record> {
        self.log
            .iter()
            .filter(move |record| record.state == self.state)
    }
}

/// How a mix runs: on `workers` threads, each yielding at one scheduling point in `every`,
/// seeded from `seed`, as `variant` says.
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub workers: usize,
    pub every: u64,
    pub seed: u64,
    pub variant: Variant,
}

impl Settings {
    /// Four workers yielding at every other point, from `seed`, plain: the tests' settings.
    pub fn seeded(seed: u64) -> Self {
        Self {
            workers: 4,
            every: 2,
            seed,
            variant: Variant::Plain,
        }
    }
}

/// How many bursts a variant but the plain one splits a mix's operations into.
pub const BURSTS: usize = 2;

/// The share of reads [`Variant::Fault`] fails after its first burst: one in this many.
const FAIL_ONE_IN: u64 = 6;

/// What a mix's run does beyond running its operations. A variant but the plain one runs them in
/// [`BURSTS`] bursts, in the order given, each beginning once the one before has ended, the State
/// checkpointed, saved and its clean blocks evicted between them ([`between_bursts`]), so the
/// bursts after the first load what they read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// Every operation runs to its end, all in one burst.
    Plain,
    /// One in two of the operations its production callers may drop ([`Op::cancellable`])
    /// aborted after a random number of yields of another task: dropped where it stands then,
    /// in a call or between two, as a caller dropping its future leaves it. The workers yield only
    /// at points a future can be dropped at, so no drop lands where no caller's could.
    Drop,
    /// The tree built in a repository over a store that fails reads on demand, which from the
    /// second burst on fails one read in [`FAIL_ONE_IN`], chosen from the run's seed: an
    /// operation's call meeting one answers it failed, and leaves what its records tell. The
    /// store is mended before each checkpoint. Only the mixes [`Mix::runs`] names run it.
    Fault,
}

impl Variant {
    pub const EVERY: [Self; 3] = [Self::Plain, Self::Drop, Self::Fault];

    /// Its name, which names it on the command line and in reports.
    pub fn name(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Drop => "drop",
            Self::Fault => "fault",
        }
    }

    /// The variant named `name`.
    pub fn named(name: &str) -> Option<Self> {
        Self::EVERY
            .into_iter()
            .find(|variant| variant.name() == name)
    }

    /// How many bursts it runs a mix's operations in.
    fn bursts(self) -> usize {
        if self == Self::Plain { 1 } else { BURSTS }
    }
}

/// A mix's tree, and the store failing its reads where the run's variant fails some, as a
/// scenario's context the model follows.
struct Mixed {
    tree: Arc<Tree>,
    store: Option<Arc<FailingStore>>,
}

impl Modelled for Mixed {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }

    fn mend(&self) {
        if let Some(store) = &self.store {
            store.fail(false);
        }
    }

    fn faulted(&self) -> bool {
        self.store
            .as_ref()
            .is_some_and(|store| store.failed_outside_calls() > 0)
    }

    fn revisions(&self) -> Vec<(Hash, Arc<RepositoryContext>)> {
        self.tree.revisions()
    }
}

/// What failed a mix's run, by class: `op`, an operation's task panicked, as one does that answers
/// what its mix leaves it no room to; `log`, a record or a point with no task; `calls`, a record
/// held by no call or by two; `model`, the model's replay of the log; `checkpoint`, the checkpoint
/// at the run's end. A panic anywhere else fails the run as a panic. So does an operation's in a
/// build that aborts on a panic, as the release build does, where the process ends with it.
#[derive(Clone, Debug)]
pub struct Failure {
    pub class: &'static str,
    pub details: Vec<String>,
}

impl Failure {
    fn one(class: &'static str, detail: String) -> Self {
        Self {
            class,
            details: vec![detail],
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:", self.class)?;
        for detail in &self.details {
            write!(f, "\n  {detail}")?;
        }
        Ok(())
    }
}

/// `future` in the execution context the State's work reads.
fn in_context<F: Future>(future: F) -> impl Future<Output = F::Output> {
    lore_base::runtime::LORE_CONTEXT.scope(crate::harness::fixtures::execution(), future)
}

/// A panic's message.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(ToString::to_string))
        .unwrap_or_else(|| "a panic without a message".to_string())
}

/// `ops`, operations of `mix`, run on `settings.workers` workers from the mix's tree as
/// `settings.variant` says, each in a task of its own, in its bursts, but those the variant runs
/// none of ([`Op::reads_links`]), then the mix's task after them alone, and at each burst's end
/// the reclaim ([`reclaim`]) alone, as the point between bursts is quiescent, recording still;
/// each burst's log replayed through the model, started where the setup ended, the State
/// checkpointed between bursts and at the run's end. Where a swap-plan
/// holder is among them, the others wait for it to begin, so they run beside it however the
/// workers are scheduled: the holder's own waits, the settle's window and the swap copy's, leave
/// them room. Every task of a burst is joined before the burst ends, whatever failed.
pub fn replayed_on_workers(mix: Mix, ops: &[Op], settings: Settings) -> Result<Run, Failure> {
    let variant = settings.variant;
    let ops: Vec<Op> = (ops.iter().copied())
        .filter(|op| variant != Variant::Fault || !op.reads_links())
        .collect();
    let ops = ops.as_slice();
    let (runtime, chaos) = chaos::runtime(settings.workers, settings.every, settings.seed);
    // The operations' tasks, then the task after them and the reclaim's.
    chaos.number_tasks(ops.len() + 2);
    if variant == Variant::Drop {
        chaos.yield_droppable();
    }
    let (context, mut model) = runtime.block_on(in_context(async {
        let (tree, store) = mix.setup(variant).await;
        let context = Mixed {
            tree: Arc::new(tree),
            store,
        };
        let model = Start::read(&context).await.model(&context, &[]).await;
        (context, model)
    }));
    let tree = context.tree.clone();
    let (begin, begun) = tokio::sync::watch::channel(!ops.iter().any(|op| op.holds_plan()));
    let begin = Arc::new(begin);
    let published = Arc::new(tokio::sync::watch::Sender::new(0usize));
    let mut draws = xorshift_state(settings.seed ^ DROPS);
    let mut run = Run {
        log: Vec::new(),
        calls: Vec::new(),
        state: tree.state.hooks().id(),
        nodes: tree.nodes.clone(),
        yields: 0,
        contentions: 0,
    };
    let bursts = variant.bursts();
    let size = ops.len().div_ceil(bursts).max(1);
    for burst in 0..bursts {
        let tasks = (burst * size).min(ops.len())..((burst + 1) * size).min(ops.len());
        chaos.log().set_recording(true);
        let handles: Vec<(usize, tokio::task::JoinHandle<()>)> = tasks
            .map(|task| {
                let (tree, op, begin, mut begun) =
                    (tree.clone(), ops[task], begin.clone(), begun.clone());
                let published = published.clone();
                let future: Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(hooks::as_task(
                    task,
                    noting_failures(async move {
                        if op.holds_plan() {
                            begin.send_replace(true);
                        } else {
                            let _ = begun.wait_for(|begun| *begun).await;
                        }
                        run_op(&tree, task, op, &published).await;
                    }),
                ));
                (task, runtime.spawn(in_context(future)))
            })
            .collect();
        let mut droppers: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        for (task, handle) in handles.iter().filter(|_| variant == Variant::Drop) {
            if !ops[*task].cancellable() || !xorshift(&mut draws).is_multiple_of(2) {
                continue;
            }
            let (abort, yields) = (handle.abort_handle(), xorshift(&mut draws) % DROP_YIELDS);
            let mut begun = begun.clone();
            droppers.push(runtime.spawn(async move {
                // Not before a swap-plan holder began, which the others wait for.
                let _ = begun.wait_for(|begun| *begun).await;
                for _ in 0..yields {
                    tokio::task::yield_now().await;
                }
                abort.abort();
            }));
        }
        let mut panicked = Vec::new();
        for (task, handle) in handles {
            if let Err(message) = joined(&runtime, handle) {
                panicked.push(format!("task {task}, {:?}: {message}", ops[task]));
            }
        }
        for dropper in droppers {
            if let Err(message) = joined(&runtime, dropper) {
                panicked.push(format!("a dropper: {message}"));
            }
        }
        // Not after a task panicked, which may have left the State where the task after would
        // wait on it forever.
        if let Some(after) = mix
            .after()
            .filter(|_| burst + 1 == bursts && panicked.is_empty())
        {
            let future: Pin<Box<dyn Future<Output = ()> + Send>> =
                Box::pin(hooks::as_task(ops.len(), after(tree.clone())));
            if let Err(message) = joined(&runtime, runtime.spawn(in_context(future))) {
                panicked.push(format!("the task after the mix: {message}"));
            }
        }
        // The point between bursts, where nothing runs, as a stage or a commit sweeps before its
        // save and a VFS provider where it paused its requests.
        if panicked.is_empty() {
            let (reclaimed, seed) = (tree.clone(), settings.seed ^ burst as u64);
            let future: Pin<Box<dyn Future<Output = ()> + Send>> =
                Box::pin(hooks::as_task(ops.len() + 1, async move {
                    reclaim(&reclaimed, seed).await;
                }));
            if let Err(message) = joined(&runtime, runtime.spawn(in_context(future))) {
                panicked.push(format!("the reclaim after burst {burst}: {message}"));
            }
        }
        chaos.log().set_recording(false);
        if !panicked.is_empty() {
            return Err(Failure {
                class: "op",
                details: panicked,
            });
        }
        let (log, calls) = burst_taken(&chaos, run.state)?;
        let spawns = chaos.spawns();
        runtime.block_on(in_context(async {
            model.provide_needed(&context, &log).await;
            model.replay_called(&log, calls.clone(), spawns);
        }));
        let violations: Vec<String> = model.violations().iter().map(ToString::to_string).collect();
        if !violations.is_empty() {
            return Err(Failure {
                class: "model",
                details: violations,
            });
        }
        run.log.extend(log);
        run.calls.extend(calls);
        if burst + 1 < bursts {
            let found = checkpointed(between_bursts(
                &context,
                &mut model,
                variant,
                settings.seed ^ burst as u64,
            ));
            if !found.is_empty() {
                return Err(Failure {
                    class: "checkpoint",
                    details: found,
                });
            }
        }
    }
    if run.calls.is_empty() {
        return Err(Failure::one(
            "calls",
            "the mix's operations recorded no call".to_string(),
        ));
    }
    // Each operation the drop variant aborts runs wholly through calls, so one dropped between two
    // leaves nothing part way: no drop excuses closure whole.
    let checked = checkpointed(model.checkpoint(&context, Checkpoint::ending(false)));
    if !checked.violations.is_empty() {
        return Err(Failure {
            class: "checkpoint",
            details: checked.violations,
        });
    }
    run.yields = chaos.yields();
    run.contentions = chaos.contentions();
    Ok(run)
}

/// The seed the drop variant's draws mix in, so they draw apart from the order's and the yields'.
const DROPS: u64 = 0xd209_5eed;

/// The most yields of another task before the drop variant aborts an operation: enough to land
/// anywhere in most operations' calls, which run in microseconds between their awaits.
const DROP_YIELDS: u64 = 64;

/// The quiescent point between two bursts of a run, its model replayed to there: the checkpoint,
/// which holds the model against the State, saves each unserved State the run wrote and evicts
/// every clean block, so the next burst loads what it reads; then, for [`Variant::Fault`], the
/// store armed to fail one read in [`FAIL_ONE_IN`], drawn from `seed`. A change at a quiescent
/// point of a run, as a reclaim between bursts, goes here. What the checkpoint found.
async fn between_bursts(
    context: &Mixed,
    model: &mut Model,
    variant: Variant,
    seed: u64,
) -> Vec<String> {
    let checked = model.checkpoint(context, Checkpoint::default()).await;
    if let Some(store) = context.store.as_ref().filter(|_| variant == Variant::Fault) {
        store.fail_one_in(FAIL_ONE_IN, seed);
    }
    checked.violations
}

/// `checkpoint`, run on a thread no chaos worker shares, so its own records are its alone.
fn checkpointed<T>(checkpoint: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the checkpoint's runtime starts")
        .block_on(in_context(checkpoint))
}

/// A burst's log and calls, taken at quiescence, and held to the rules every run keeps: no record
/// or point with no task, no record pushed into a full buffer, each record of the State, `state`,
/// one call's. A spawned task's records are its operation's, up its spawns.
fn burst_taken(chaos: &Chaos, state: u64) -> Result<(Vec<Record>, Vec<Call>), Failure> {
    // Taken at quiescence, so a call left open or overlapping another of its task's fails here.
    let calls = chaos.log().take_calls();
    if let Some(record) = chaos.log().no_task() {
        return Err(Failure::one(
            "log",
            format!("a record or a point with no task: {record:?}"),
        ));
    }
    let log = chaos.log().drain();
    let grown = chaos.log().grown();
    if grown != 0 {
        return Err(Failure::one(
            "log",
            format!("{grown} records pushed into a full buffer, which may allocate in a section"),
        ));
    }
    check_log(&log, Labels::Required);
    // Chaos tells a spawned future its spawner's task, and the spawns tell which operation's it
    // is, so a call holds by its task alone. Every operation, and the tasks after them, the drain
    // and the reclaim, run wholly through the catalogue's calls.
    let spawns = chaos.spawns();
    let mut unheld = Vec::new();
    for record in log.iter().filter(|record| record.state == state) {
        let holding = calls
            .iter()
            .filter(|call| call.holds(record, &spawns))
            .count();
        if holding > 1 {
            unheld.push(format!("{record:?} is {holding} calls'"));
        } else if holding == 0 && record.task.is_some() {
            unheld.push(format!("{record:?} is no call's"));
        }
    }
    if !unheld.is_empty() {
        return Err(Failure {
            class: "calls",
            details: unheld,
        });
    }
    Ok((log, calls))
}

/// `handle`'s task joined: whether it ended, or was aborted; the message it panicked with, where
/// it did. Where it does not end within a minute, a worker deadlocked on a block's lock would hang
/// the runtime's drop, so the process says so on stderr and exits 100, the stress harness's code
/// for a hang.
#[expect(
    clippy::exit,
    reason = "a deadlocked worker would hang the runtime's drop, and the process with it"
)]
fn joined(
    runtime: &tokio::runtime::Runtime,
    handle: tokio::task::JoinHandle<()>,
) -> Result<bool, String> {
    let joined =
        runtime.block_on(async { tokio::time::timeout(Duration::from_secs(60), handle).await });
    let Ok(joined) = joined else {
        let _ = writeln!(
            std::io::stderr(),
            "HANG: a task did not end within a minute"
        );
        std::process::exit(100);
    };
    match joined {
        Ok(()) => Ok(true),
        Err(error) if error.is_cancelled() => Ok(false),
        Err(error) => Err(match error.try_into_panic() {
            Ok(payload) => panic_message(&*payload),
            Err(error) => error.to_string(),
        }),
    }
}

/// The parent and name hash `path` was built at.
fn built_place(tree: &Tree, path: &str) -> (NodeID, u64) {
    let (parent, name) = path.rsplit_once('/').expect("a path below a directory");
    (tree.id(parent), hash_string(name))
}

/// Fails the operation with `what` where `holds` does not, unless a call of the task failed on a
/// read the store failed, which leaves the operation's outcome to the model alone.
#[track_caller]
fn must(holds: bool, what: impl FnOnce() -> String) {
    if !holds && !any_call_failed() {
        panic!("{}", what());
    }
}

/// One task's operation. One the mix leaves nothing to refuse must succeed; one that may lose a
/// race to another task answers only the refusal that race explains. A C API add that published
/// counts in `published`.
async fn run_op(tree: &Tree, task: usize, op: Op, published: &tokio::sync::watch::Sender<usize>) {
    let name = format!("n{task}");
    let at = |path: &str| tree.id(path);
    match op {
        Op::Create(dir) => {
            let created = ops::checked_create(tree, at(dir), &name).await;
            must(ops::attached(created).is_some(), || {
                format!("{op:?} attaches: {created:?}")
            });
        }
        Op::CreateRemove(dir) => {
            let created = ops::checked_create(tree, at(dir), &name).await;
            let Some(node) = ops::attached(created) else {
                must(false, || format!("{op:?} attaches: {created:?}"));
                return;
            };
            let detached = ops::unserved_detach(tree, node).await;
            must(detached, || format!("{op:?} detaches"));
        }
        Op::DiffAdd(dir) => {
            let added = ops::diff_add(tree, at(dir), &name, true).await;
            must(added.is_some(), || format!("{op:?} adds"));
        }
        Op::DirtyAdd(dir) => {
            let added = ops::dirty_add(tree, at(dir), &format!("m{task}"), &name, true).await;
            must(added.is_some(), || format!("{op:?} adds"));
        }
        Op::ScanChain(dir) => {
            let chained = ops::scan_chain(tree, at(dir), &format!("s{task}/{name}")).await;
            must(chained.is_some(), || format!("{op:?} chains"));
        }
        Op::AddDiscard(dir) => ops::add_discard(tree, at(dir), &name).await,
        Op::AddDiscardTree(dir) => ops::add_discard_tree(tree, at(dir), &name).await,
        Op::Rename(path) => ops::rename_then_mark(tree, at(path), &name).await,
        Op::Mark(path) => ops::scan_mark_modified(tree, at(path)).await,
        Op::Clear(path) => ops::scan_clear(tree, at(path)).await,
        Op::ClearStaged(path) => ops::staged_clear(tree, at(path)).await,
        Op::Vacated(dir) => ops::vacated_clear(tree, at(dir)).await,
        Op::RewriteDirty(path) => {
            let content = Content {
                size: 1000 + task as u64,
                ..Content::default()
            };
            let (parent, _) = built_place(tree, path);
            ops::dirty_rewrite(tree, at(path), parent, &content).await;
        }
        Op::InPlace(path) => {
            let changed =
                ops::in_place_size(tree, at(path), built_place(tree, path), task as u64).await;
            must(changed, || {
                format!("{op:?} finds the file at its built place")
            });
        }
        Op::Owned(path) => {
            let changed = ops::owned_size(tree, at(path), task as u64).await;
            must(changed, || format!("{op:?} changes"));
        }
        Op::OwnedClear(path) => ops::owned_clear(tree, at(path)).await,
        Op::Repin(path, pin) => {
            let linked = tree.linked.as_ref().expect("a tree with a link");
            let pin = match pin {
                Repinning::Whole(pinned) => linked.pin(pinned),
                Repinning::Revision(pinned) => LinkPin::Revision(linked.revision(pinned).signature),
            };
            ops::repin(tree, at(path), pin).await;
        }
        Op::StagedAdd(dir, must_add) => {
            match ops::staged_add(tree, at(dir), file(&name), &name).await {
                Some(_) => published.send_modify(|count| *count += 1),
                None => must(must_add == Must::Race, || format!("{op:?} adds")),
            }
        }
        Op::StagedModify(path, must_modify) => {
            let content = Content {
                size: task as u64,
                ..Content::default()
            };
            let modified = ops::staged_modify(tree, at(path), &content).await;
            must(must_modify == Must::Race || modified, || {
                format!("{op:?} modifies")
            });
        }
        Op::DeleteRestore(path, parent) => {
            ops::staged_delete_level(tree, at(path), at(parent)).await;
            // Another task's restore may take it back first.
            ops::restore(tree, at(path), &Content::default()).await;
        }
        Op::StagedMove(path, to, walked) => {
            ops::staged_move_planned(tree, (at(path), at(to), &name), walked).await;
        }
        Op::AddPend(dir) => {
            let added = ops::staged_add(tree, at(dir), directory(&name), &name).await;
            let Some(node) = added else {
                must(false, || format!("{op:?} adds its directory"));
                return;
            };
            let below = ops::staged_add(tree, node, file("f"), "f").await;
            must(below.is_some(), || {
                format!("{op:?} adds below its directory")
            });
            let place = (node, at(dir), hash_string(&name));
            let detached = ops::pend(tree, place, async {}).await;
            must(detached, || format!("{op:?} detaches its directory"));
        }
        Op::Pend(path, parent) => {
            let (_, name_hash) = built_place(tree, path);
            let mut seen = published.subscribe();
            let holding = async move {
                let before = *seen.borrow_and_update();
                let _ = tokio::time::timeout(
                    Duration::from_secs(5),
                    seen.wait_for(|count| *count > before),
                )
                .await;
            };
            let place = (at(path), at(parent), name_hash);
            let detached = ops::pend(tree, place, holding).await;
            must(detached, || format!("{op:?} detaches its directory"));
        }
        Op::DeleteBatch(paths) => {
            let roots: Vec<NodeID> = paths.iter().map(|path| at(path)).collect();
            let went = ops::delete_batch(tree, &roots).await;
            // An entry that failed on a failed read went as its records tell.
            must(went.iter().all(|went| *went != Some(false)), || {
                format!("{op:?} deletes every entry whole: {went:?}")
            });
        }
        Op::ServedCreate(dir, must_attach) => {
            let created = ops::served_create(tree, at(dir), &name).await;
            must(must_attach == Must::Race || created.is_some(), || {
                format!("{op:?} attaches")
            });
        }
        Op::ServedCreateAs(dir, named) => {
            ops::served_create(tree, at(dir), named).await;
        }
        Op::ServedCreateRemove(dir) => {
            let Some(node) = ops::served_create(tree, at(dir), &name).await else {
                must(false, || format!("{op:?} attaches"));
                return;
            };
            let detached = ops::served_detach(tree, node).await;
            must(detached, || format!("{op:?} detaches"));
        }
        Op::ServedCreateReplacing(dir, held) => {
            let Some(node) = ops::served_create(tree, at(dir), &name).await else {
                must(false, || format!("{op:?} attaches"));
                return;
            };
            let (to, held_name) = held.rsplit_once('/').expect("a path below a directory");
            let moved =
                ops::served_rename_replacing(tree, node, (at(to), held_name), at(held)).await;
            must(moved, || format!("{op:?} moves"));
        }
        Op::ServedRename(path, to) | Op::ServedMoveDirectory(path, to) => {
            let renamed = ops::served_rename(tree, at(path), at(to), &name).await;
            must(renamed, || format!("{op:?} moves"));
        }
        Op::ServedRenameAs(path, to, named) => {
            ops::served_rename(tree, at(path), at(to), named).await;
        }
        Op::ServedMoveNotLocal(path, to) => {
            let refused = ops::served_rename_outcome(tree, at(path), at(to), &name).await;
            must(refused == Outcome::Refused(Refusal::NotLocal), || {
                format!("{op:?} is refused: {refused:?}")
            });
        }
        Op::ServedRmdir(dir, must_detach) => {
            let detached = ops::served_rmdir(tree, at(dir)).await.removed;
            must(must_detach == Must::Race || detached, || {
                format!("{op:?} detaches")
            });
        }
        Op::ServedMark(dir) => {
            let marked = ops::served_mark_dir(tree, at(dir)).await;
            must(marked, || format!("{op:?} marks"));
        }
        Op::CopyUp(path, dir) => {
            ops::served_copy_up(tree, at(path), at(dir)).await;
        }
        Op::Settle(dir, clears, then) => {
            let window = async { tokio::time::sleep(Duration::from_millis(20)).await };
            ops::settle_waiting(tree, at(dir), window).await;
            let settled = ops::settle(tree, at(clears)).await;
            must(settled == ops::Settle::Cleared, || {
                format!("{op:?} clears {clears}: {settled:?}")
            });
            for &dir in then {
                ops::settle(tree, at(dir)).await;
            }
        }
        Op::SwapAttach(dir) => {
            let pace = || tokio::time::sleep(Duration::from_millis(2));
            let copied = ops::swap_copy_paced(tree, (at(dir), &name), &["i", "j", "k"], pace).await;
            must(copied.is_some(), || format!("{op:?} attaches"));
        }
        Op::SwapDetach(path) => {
            let detached = ops::swap_detach_if_clean(tree, at(path)).await;
            must(detached, || format!("{op:?} detaches"));
        }
        Op::SwapRewrite(path) => {
            let content = Content {
                size: 2000 + task as u64,
                ..Content::default()
            };
            let rewrote = ops::swap_rewrite(tree, at(path), &content).await;
            must(rewrote, || format!("{op:?} rewrites"));
        }
        Op::Flip(path) => {
            let flipped = ops::flip(tree, at(path)).await;
            must(flipped, || format!("{op:?} flips"));
        }
        Op::Children(children, dir) => children.of(tree, at(dir)).await,
        Op::ListingCreate(dir) => {
            let created = ops::listing_create(tree, at(dir), &name).await;
            must(created.is_some(), || format!("{op:?} attaches"));
        }
        Op::ListingCreateAs(dir, named) => {
            let between = tokio::time::sleep(Duration::from_millis(2));
            ops::listing_create_waiting(tree, at(dir), named, (async {}, between)).await;
        }
        Op::ListingRename(path, to) => {
            let renamed = ops::listing_rename(tree, at(path), at(to), &name).await;
            must(renamed, || format!("{op:?} moves"));
        }
        Op::MarkedReads(dir) => {
            let Some((_, mut marked)) = ops::listed_marked(tree, at(dir)).await else {
                must(false, || format!("{op:?} lists with a mark"));
                return;
            };
            let created = ops::served_create(tree, at(dir), &name).await;
            if created.is_none() {
                must(false, || format!("{op:?} attaches"));
                return;
            }
            marked.count_own_prepend();
            tokio::time::sleep(Duration::from_millis(2)).await;
            for searched in [name.as_str(), "same"] {
                let _ = ops::added_since(tree, &marked, searched).await;
            }
            let _ = ops::reattached_since(tree, &marked).await;
        }
        Op::Names(path) => {
            for read in NameRead::EVERY {
                let named = ops::name_read(tree, at(path), read).await;
                must(named.is_some(), || format!("{op:?} reads its name"));
            }
            let built = path.rsplit_once('/').map_or(path, |(_, name)| name);
            let compared = ops::name_is(tree, at(path), built).await;
            must(compared.is_some(), || format!("{op:?} compares its name"));
        }
        Op::Paths(path) => {
            let (parent, _) = path.rsplit_once('/').expect("a path below a directory");
            let from_root = ops::node_path(tree, at(path), None).await;
            must(from_root.is_some(), || format!("{op:?} reads its path"));
            let below = ops::node_path(tree, at(path), Some(at(parent))).await;
            must(below.is_some(), || {
                format!("{op:?} reads its path below its parent")
            });
        }
        Op::DirtyPaths(dir) => {
            for round in 0..COLLECTIONS {
                if round > 0 {
                    tokio::time::sleep(BETWEEN_COLLECTIONS).await;
                }
                let paths = ops::dirty_paths(tree, at(dir), dir).await;
                must(paths.is_some(), || format!("{op:?} collects"));
            }
        }
        Op::LinkReads(path) => {
            for round in 0..LINK_ROUNDS {
                if round > 0 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                let by_id = ops::listed_children(tree, at(path)).await;
                must(by_id.is_some(), || format!("{op:?} lists the link"));
                let by_name = ops::listed_name_hashes(tree, at(path)).await;
                must(by_name.is_some(), || {
                    format!("{op:?} lists the link by name hash")
                });
                for below in ["a", "c"] {
                    let through = format!("{path}/{below}");
                    let _ = ops::find_linked(tree, ROOT_NODE, &through).await;
                }
            }
        }
        Op::Has(dir) => {
            for round in 0..HAS_ROUNDS {
                if round > 0 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                for flagged in Flagged::EVERY {
                    let has = ops::has_children(tree, at(dir), flagged).await;
                    must(has.is_some(), || {
                        format!("{op:?} reads its children's flags")
                    });
                }
            }
        }
    }
}

/// How many of `records` there are of each kind, by a name for the kind: the effect's, a `Word`'s
/// with its section, an `Unlink`'s with its mode, a `Free`'s with its section.
pub fn kinds<'a>(records: impl IntoIterator<Item = &'a Record>) -> BTreeMap<String, usize> {
    let mut kinds = BTreeMap::new();
    for record in records {
        let kind = match &record.effect {
            Effect::Word { .. } => format!("Word in {:?}", record.section),
            Effect::Detach { how, .. } => format!("Detach {how:?}"),
            Effect::Free { .. } => format!("Free in {:?}", record.section),
            effect => format!("{effect:?}")
                .split([' ', '(', '{'])
                .next()
                .unwrap_or_default()
                .to_string(),
        };
        *kinds.entry(kind).or_default() += 1;
    }
    kinds
}

/// The exclusive claim's tree: three directories its changes share, one for each discard, a
/// dirty file only the owned changes touch, a dirty link pinning a second repository's revision
/// holding markers, as the settle leaves one, which only the repins, the listings of it and the
/// collections through it touch, a file only the dirty rewrites touch, a staged add the staged
/// clear takes back, and a dirty directory only the reset's clear touches.
pub async fn exclusive_tree(spread: bool, repository: Arc<RepositoryContext>) -> Tree {
    let before = ["a/x", "b/y", "p0/", "p1/", "p2/", "p3/"];
    let after = ["c/d/e", "q/o", "q/w", "v/k"];
    let mut tree = if spread {
        // The discards' directories in the block the filler fills, the nodes added into them in
        // the next: each discard unlinks across two blocks.
        Tree::spread_in(repository, &before, "fill", &after).await
    } else {
        Tree::new_in(repository, &[&before[..], &after[..]].concat()).await
    };
    tree.dirty_link("q/l", Pinned::Dirty).await;
    let (state, repository) = (tree.state.clone(), tree.repository.clone());
    let s = state
        .node_add_staged(repository.clone(), tree.id("q"), file("s"), "s")
        .await
        .expect("the staged add runs");
    tree.nodes.push(("q/s".to_string(), s));
    let modified = Mark::dirty(NodeFlags::DirtyModify, Ext::Keep, false);
    state
        .node_mark(&repository, tree.id("q/o"), modified)
        .await
        .expect("'q/o' is marked");
    let dirty = Mark::dirty(NodeFlags::Dirty, Ext::Keep, false);
    state
        .node_mark(&repository, tree.id("v"), dirty)
        .await
        .expect("'v' is marked");
    tree
}

/// The exclusive claim's mix: checked creates, creates removed, the provider diff's and `lore
/// dirty`'s adds, scans' chains, adds of files and directories discarded, renames, marks, clears,
/// dirty rewrites, in-place and owned changes, owned clears, repins to each revision, listings
/// through the link and collections of `q`'s dirty paths following it; a staged clear and a
/// reset's clear: `rounds` of them, at most [`ROUNDS`].
pub fn exclusive(rounds: usize) -> Vec<Op> {
    let mut ops = Vec::new();
    for round in 0..rounds.min(ROUNDS) {
        let private = ["p0", "p1", "p2", "p3"][round];
        ops.extend([
            Op::Create("a"),
            Op::CreateRemove("b"),
            [
                Op::DiffAdd("c"),
                Op::DirtyAdd("a"),
                Op::ScanChain("c/d"),
                Op::DiffAdd("b"),
            ][round],
            if round < 2 {
                Op::AddDiscard(private)
            } else {
                Op::AddDiscardTree(private)
            },
            Op::Rename("b/y"),
            Op::Mark(["a/x", "c/d/e", "a/x", "c/d/e"][round]),
            Op::Clear(["c/d/e", "a/x", "c/d/e", "a/x"][round]),
            Op::InPlace("a/x"),
            [Op::Owned("q/o"), Op::OwnedClear("q/o")][round % 2],
            Op::Repin(
                "q/l",
                [
                    Repinning::Whole(Pinned::Second),
                    Repinning::Whole(Pinned::First),
                    Repinning::Revision(Pinned::Second),
                    Repinning::Revision(Pinned::First),
                ][round],
            ),
            Op::LinkReads("q/l"),
            // Back to the revision holding markers, which the collections of `q` follow.
            Op::Repin("q/l", Repinning::Whole(Pinned::Dirty)),
            Op::DirtyPaths("q"),
            Op::RewriteDirty("q/w"),
        ]);
    }
    ops.extend([Op::ClearStaged("q/s"), Op::Vacated("v")]);
    ops
}

/// The shared claim's tree: two directories its adds share, files to modify, delete and move,
/// and `s/t`, a directory added staged, which a C API delete queues pending while adds publish
/// into `s/t/u` below it. `u` was added first, so it lies last in `t`'s chain, and the delete's
/// marks, top-down, reach it last. That an add lands between the detach and the marks is races.rs'
/// to show, in every interleaving. In `g`, which a batch delete takes whole, the tracked `h`,
/// holding `i`, and `j`, and the added `n`, holding `f`, and `m`.
pub async fn shared_tree(repository: Arc<RepositoryContext>) -> Tree {
    let paths = ["a/x", "a/w", "b/y", "c/d/e", "s/", "g/h/i", "g/j"];
    let mut tree = Tree::new_in(repository, &paths).await;
    let (state, repository) = (tree.state.clone(), tree.repository.clone());
    let t = state
        .node_add_staged(repository.clone(), tree.id("s"), directory("t"), "t")
        .await
        .expect("'s/t' adds");
    let u = state
        .node_add_staged(repository.clone(), t, directory("u"), "u")
        .await
        .expect("'s/t/u' adds");
    for index in 0..32 {
        let name = format!("f{index}");
        state
            .node_add_staged(repository.clone(), t, file(&name), &name)
            .await
            .expect("a file below 's/t' adds");
    }
    tree.nodes
        .extend([("s/t".to_string(), t), ("s/t/u".to_string(), u)]);
    let g = tree.id("g");
    let n = state
        .node_add_staged(repository.clone(), g, directory("n"), "n")
        .await
        .expect("'g/n' adds");
    let f = state
        .node_add_staged(repository.clone(), n, file("f"), "f")
        .await
        .expect("'g/n/f' adds");
    let m = state
        .node_add_staged(repository.clone(), g, file("m"), "m")
        .await
        .expect("'g/m' adds");
    tree.nodes.extend([
        ("g/n".to_string(), n),
        ("g/n/f".to_string(), f),
        ("g/m".to_string(), m),
    ]);
    tree
}

/// The shared claim's mix, the C API's kinds: staged adds and modifies, deletes restored, moves,
/// every other round's after the plan's walk, deletes of adds and of `s/t`, those three under the
/// moves permit, beside the has-children reads of `a` and `b` and the collections of the dirty
/// paths below them, `rounds` of them, at most [`ROUNDS`], each with an add into `g/h` beside the
/// batch delete of `g`'s children, once; then the drain alone.
pub fn shared(rounds: usize) -> Vec<Op> {
    let mut ops = vec![
        Op::Pend("s/t", "s"),
        Op::DeleteBatch(&["g/h", "g/j", "g/n", "g/m"]),
    ];
    for round in 0..rounds.min(ROUNDS) {
        ops.extend([
            Op::StagedAdd("a", Must::Make),
            Op::StagedAdd("s/t/u", Must::Race),
            [
                Op::StagedModify("a/x", Must::Make),
                Op::StagedModify("b/y", Must::Race),
                Op::StagedModify("a/w", Must::Race),
                Op::StagedModify("c/d/e", Must::Make),
            ][round],
            Op::DeleteRestore(
                ["a/w", "b/y", "a/w", "b/y"][round],
                ["a", "b", "a", "b"][round],
            ),
            Op::StagedMove("c/d", ["a", "b", "c", "s"][round], round % 2 == 0),
            Op::AddPend("s"),
            Op::Has("a"),
            Op::Has("b"),
            Op::DirtyPaths(["a", "b", "a", "b"][round]),
            Op::StagedAdd("g/h", Must::Race),
        ]);
    }
    ops
}

/// The pending drain, alone after the shared claim's changes: the exclusive claim's.
pub fn drain(tree: Arc<Tree>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    Box::pin(async move {
        ops::drain(&tree).await;
    })
}

/// The quiescent reclaim, at a point between bursts of operations where nothing runs, as a stage
/// or a commit sweeps before its save and a VFS provider where it pauses its requests, settles and
/// swaps (C15): on a served State the provider's sweep, retaining a set drawn from `seed` of the
/// nodes neither live nor free in blocks in memory, as the kernel still references some; on
/// another the sweep, its drain first. What it freed.
pub async fn reclaim(tree: &Tree, seed: u64) -> DiscardCounts {
    if tree.state.is_served() {
        ops::served_sweep(tree, &retained_drawn(tree, seed)).await
    } else {
        ops::sweep(tree).await
    }
}

/// Of the slots neither live nor free in `tree`'s blocks in memory, a third or so, drawn from
/// `seed`: every `Discarded` slot holding a node and the nodes its chains reach in blocks in
/// memory, read with no lock beside a record, as the provider's inode table would name them.
pub fn retained_drawn(tree: &Tree, seed: u64) -> Vec<NodeID> {
    let state = &tree.state;
    let record = |node: NodeID| {
        state
            .resident_block(NodeBlock::index(node))
            .map(|block| block.node(Node::index(node)))
    };
    let mut reached: Vec<NodeID> = Vec::new();
    for index in 0..state.block_count() {
        let Some(block) = state.resident_block(index) else {
            continue;
        };
        let roots: Vec<NodeID> = {
            let reader = block.read();
            let data = reader.node_block();
            let mut free = [false; BLOCK_NODE_COUNT];
            let mut at = data.node_unused as usize;
            for _ in 0..data.node_unused_count {
                if at >= BLOCK_NODE_COUNT || free[at] {
                    break;
                }
                free[at] = true;
                at = data.node[at].sibling as usize;
            }
            (0..data.node_count as usize)
                .filter(|&slot| !free[slot] && data.node[slot].is_discarded())
                .map(|slot| (index * BLOCK_NODE_COUNT + slot) as NodeID)
                .collect()
        };
        reached.extend(roots);
    }
    let mut at = 0;
    let most = state.block_count() * BLOCK_NODE_COUNT;
    while let Some(&node) = reached.get(at) {
        at += 1;
        let Some(directory) = record(node).filter(|record| record.is_directory()) else {
            continue;
        };
        let mut child = directory.child;
        while child.is_valid_node_id() && reached.len() < most {
            let Some(held) = record(child).filter(|held| held.parent == node) else {
                break;
            };
            if !reached.contains(&child) {
                reached.push(child);
            }
            child = held.sibling;
        }
    }
    let mut draw = xorshift_state(seed ^ 0x5eed_5eed);
    reached.retain(|_| xorshift(&mut draw).is_multiple_of(3));
    reached
}

/// The served mix: one swap-plan holder first, then creates, creates removed, creates renamed over
/// a revision's file, renames of a file and of a directory holding local content, renames refused
/// a directory holding none, marks and copy-ups, creates and a rename contending for one name, a
/// create beside an RMDIR, an RMDIR alone, and a rename leaving `h` clean children other than its
/// committed ones; `rounds` of the first, at most [`ROUNDS`].
pub fn served_mix(holder: Op, rounds: usize) -> Vec<Op> {
    let mut ops = vec![holder];
    for round in 0..rounds.min(ROUNDS) {
        ops.extend([
            Op::ServedCreate("a", Must::Make),
            Op::ServedCreateRemove("b"),
            Op::ServedCreateReplacing("a", ["b/k0", "b/k1", "b/k2", "b/k3"][round]),
            Op::ServedRename("a/x", ["b", "c", "a", "b"][round]),
            Op::ServedMoveDirectory("c/d", ["a", "b", "c", "a"][round]),
            Op::ServedMoveNotLocal("g", ["a", "b", "c", "a"][round]),
            Op::ServedMark(["c", "c/d", "a", "b"][round]),
            Op::CopyUp("b/y", "b"),
        ]);
    }
    ops.extend([
        Op::ServedCreateAs("a", "same"),
        Op::ServedCreateAs("a", "same"),
        Op::ServedRenameAs("b/y", "a", "same"),
        Op::ServedCreate("e", Must::Race),
        Op::ServedRmdir("e", Must::Race),
        Op::ServedRmdir("f", Must::Make),
        Op::ServedRename("h/v", "b"),
    ]);
    ops
}

/// A served tree as a provider serves one: taking new nodes from fresh blocks; `b` marked dirty,
/// as local changes leave it, and `a` clean, so the first change in it marks it; in `b`, the files
/// `k0` to `k3` the creates take the paths of; in `c`, beside `c/d`, holding local content as a
/// directory a create made holds it, which the served renames move, a target for each swap-plan
/// holder that only the holder touches: `c/h` to detach, `c/r` to rewrite and `c/f`, copied up,
/// to flip; `e` and `f` for the RMDIRs; `g`, marked dirty with nothing changed below it, which a
/// settle clears and the renames refused leave where it is; and `h`, which a rename of `h/v` leaves
/// holding `h/u` alone, clean, so a settle must keep it.
pub async fn served(repository: Arc<RepositoryContext>) -> Tree {
    let paths = [
        "a/x", "b/y", "b/k0", "b/k1", "b/k2", "b/k3", "c/d/e", "c/h/i", "c/r", "c/f", "e/", "f/",
        "g/z", "h/u", "h/v",
    ];
    let tree = Tree::new_in(repository, &paths).await;
    tree.state
        .take_new_nodes_from_fresh_blocks(tree.repository.clone())
        .await
        .expect("new nodes come from fresh blocks");
    tree.state.mark_served();
    for dir in ["b", "g"] {
        assert!(ops::served_mark_dir(&tree, tree.id(dir)).await);
    }
    assert!(ops::served_copy_up(&tree, tree.id("c/f"), tree.id("c")).await);
    ops::hold_local(&tree, tree.id("c/d")).await;
    tree
}

/// The listing mix: in `a` and `b`, walks of every kind, marked listings and the reads by their
/// marks, the has-children reads, the collections of the dirty paths, and listing readers' creates
/// and renames, beside served creates, removals and renames in and out of them; four tasks
/// contending for one name in `a`; the name and path reads of `a/x` and `b/y`, which renames move
/// between them. No task removes a listed directory.
pub fn listing_mix() -> Vec<Op> {
    let mut ops = Vec::new();
    for dir in ["a", "b"] {
        ops.extend(Children::EVERY.map(|children| Op::Children(children, dir)));
        ops.extend([
            Op::MarkedReads(dir),
            Op::Has(dir),
            Op::DirtyPaths(dir),
            Op::ServedCreate(dir, Must::Make),
            Op::ServedCreateRemove(dir),
            Op::ServedRename("a/x", dir),
            Op::ListingCreate(dir),
            Op::ListingRename("b/y", dir),
        ]);
    }
    ops.extend([
        Op::Names("a/x"),
        Op::Paths("a/x"),
        Op::Names("b/y"),
        Op::Paths("b/y"),
        Op::ListingCreateAs("a", "same"),
        Op::ServedCreateAs("a", "same"),
        Op::ListingCreateAs("a", "same"),
        Op::ServedRenameAs("b/k0", "a", "same"),
    ]);
    ops
}

/// A mix: its setup's tree, its operations and what runs alone after them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mix {
    /// The exclusive claim's changes.
    Exclusive,
    /// The exclusive claim's changes, the discards' directories in one block and the nodes added
    /// into them in the next, so each discard unlinks across two blocks.
    ExclusiveAcrossBlocks,
    /// The shared claim's changes, the C API's, then the pending drain alone.
    Shared,
    /// The served changes beside a settle of a directory they change.
    ServedSettle,
    /// The served changes beside a swap's copy of a directory into one.
    ServedSwapCopy,
    /// The served changes beside a swap's detach of a clean directory.
    ServedSwapDetach,
    /// The served changes beside a swap's rewrite of a clean file.
    ServedSwapRewrite,
    /// The served changes beside a commit's flip of a dirty file.
    ServedFlip,
    /// Walks, listings and reads of two directories beside served changes of them.
    Listing,
}

impl Mix {
    pub const EVERY: [Self; 9] = [
        Self::Exclusive,
        Self::ExclusiveAcrossBlocks,
        Self::Shared,
        Self::ServedSettle,
        Self::ServedSwapCopy,
        Self::ServedSwapDetach,
        Self::ServedSwapRewrite,
        Self::ServedFlip,
        Self::Listing,
    ];

    /// Its name, which names it on the command line and in reports.
    pub fn name(self) -> &'static str {
        match self {
            Self::Exclusive => "exclusive",
            Self::ExclusiveAcrossBlocks => "exclusive-across-blocks",
            Self::Shared => "shared",
            Self::ServedSettle => "served-settle",
            Self::ServedSwapCopy => "served-swap-copy",
            Self::ServedSwapDetach => "served-swap-detach",
            Self::ServedSwapRewrite => "served-swap-rewrite",
            Self::ServedFlip => "served-flip",
            Self::Listing => "listing",
        }
    }

    /// The mix named `name`.
    pub fn named(name: &str) -> Option<Self> {
        Self::EVERY.into_iter().find(|mix| mix.name() == name)
    }

    /// Whether it runs `variant`: [`Variant::Fault`] only on the unserved mixes. A served tree is
    /// never saved, so its blocks stay dirty in memory, and no burst after the first loads one the
    /// store could fail.
    pub fn runs(self, variant: Variant) -> bool {
        variant != Variant::Fault
            || matches!(
                self,
                Self::Exclusive | Self::ExclusiveAcrossBlocks | Self::Shared
            )
    }

    /// The tree its operations start from, and, for [`Variant::Fault`], the store its repository
    /// reads through, which fails reads on demand: its committed State read through it too, as a
    /// settle reads one.
    pub async fn setup(self, variant: Variant) -> (Tree, Option<Arc<FailingStore>>) {
        let (repository, store) = if variant == Variant::Fault {
            let (repository, store) = fixtures::failing_repository().await;
            (repository, Some(store))
        } else {
            (fixtures::repository().await, None)
        };
        let mut tree = match self {
            Self::Exclusive => exclusive_tree(false, repository).await,
            Self::ExclusiveAcrossBlocks => exclusive_tree(true, repository).await,
            Self::Shared => shared_tree(repository).await,
            Self::ServedSettle
            | Self::ServedSwapCopy
            | Self::ServedSwapDetach
            | Self::ServedSwapRewrite
            | Self::ServedFlip
            | Self::Listing => served(repository).await,
        };
        if store.is_some() {
            tree.reload_committed().await;
        }
        (tree, store)
    }

    /// Whether it repeats its operations in rounds: all but the listing mix.
    pub fn repeats(self) -> bool {
        self != Self::Listing
    }

    /// Whether [`Self::makes`] lists the records of its holder's task alone, task 0, which a
    /// swap's or a flip's mix runs first.
    pub fn makes_by_holder(self) -> bool {
        matches!(
            self,
            Self::ServedSwapCopy
                | Self::ServedSwapDetach
                | Self::ServedSwapRewrite
                | Self::ServedFlip
        )
    }

    /// Its served mix's swap-plan holder, where it has one.
    pub fn holder(self) -> Option<Op> {
        match self {
            // Then the directories the mix moves none of: `g`, which must clear, and the others,
            // which stay dirty.
            Self::ServedSettle => Some(Op::Settle("b", "g", &["h", "a", "c", "e", "f"])),
            Self::ServedSwapCopy => Some(Op::SwapAttach("c")),
            Self::ServedSwapDetach => Some(Op::SwapDetach("c/h")),
            Self::ServedSwapRewrite => Some(Op::SwapRewrite("c/r")),
            Self::ServedFlip => Some(Op::Flip("c/f")),
            Self::Exclusive | Self::ExclusiveAcrossBlocks | Self::Shared | Self::Listing => None,
        }
    }

    /// Its operations, `rounds` of those it repeats, at most [`ROUNDS`]; the listing mix repeats
    /// none.
    pub fn ops(self, rounds: usize) -> Vec<Op> {
        match self {
            Self::Exclusive | Self::ExclusiveAcrossBlocks => exclusive(rounds),
            Self::Shared => shared(rounds),
            Self::Listing => listing_mix(),
            Self::ServedSettle
            | Self::ServedSwapCopy
            | Self::ServedSwapDetach
            | Self::ServedSwapRewrite
            | Self::ServedFlip => {
                served_mix(self.holder().expect("a served mix has a holder"), rounds)
            }
        }
    }

    /// What runs alone after its operations.
    pub fn after(self) -> Option<After> {
        (self == Self::Shared).then_some(drain as After)
    }

    /// The records, by [`kinds`]' names, its runs make between them over a few seeds: what the
    /// mix is for. The holder's alone, for a swap's or a flip's mix, which those runs' holder task
    /// makes.
    pub fn makes(self) -> &'static [&'static str] {
        match self {
            Self::Exclusive => &[
                "Publish",
                "Detach Always",
                "Detach Discard",
                "Free in DiscardMarks",
                "Rename",
                "Repin",
                "Word in UnservedMark",
                "Word in UnservedClear",
                "Word in Rewrite",
                "Word in InPlace",
                "Word in Owned",
                "Word in Repin",
            ],
            Self::ExclusiveAcrossBlocks => &["Publish", "Detach Discard", "Rename"],
            Self::Shared => &[
                "Publish",
                "Move",
                "Pend",
                "Discarding",
                "Free in DiscardFree",
                "Word in UnservedMark",
                "Word in StagedModify",
                "Word in DeleteLevel",
                "Word in Restore",
            ],
            Self::ServedSettle => &[
                "Allocated",
                "Publish",
                "Detach Always",
                "Detach IfUnchanged",
                "Move",
                "Word in ServedMark",
                "Word in Rewrite",
                "Word in SettlePhase1",
                "Head",
                "Free in DiscardFree",
            ],
            Self::ServedSwapCopy => &["Grab", "BuildAdd", "Publish"],
            Self::ServedSwapDetach => &["Detach IfClean"],
            Self::ServedSwapRewrite => &["Word in Rewrite"],
            Self::ServedFlip => &["Word in InPlace"],
            Self::Listing => &[
                "Head",
                "Parent",
                "Named",
                "Publish",
                "Move",
                "Detach Always",
                "Free in DiscardFree",
            ],
        }
    }
}

/// The next draw of a xorshift stream at `state`, which must not be 0.
pub fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// A xorshift stream's state for `seed`, never 0: the seed spread over every bit, so nearby seeds
/// start streams of their own.
pub fn xorshift_state(seed: u64) -> u64 {
    seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1
}

/// `ops` in an order drawn from `seed`, an operation that starts first kept first.
pub fn shuffled(mut ops: Vec<Op>, seed: u64) -> Vec<Op> {
    let mut state = xorshift_state(seed);
    let mut draw = move || xorshift(&mut state);
    let first = usize::from(ops.first().is_some_and(|op| op.starts_first()));
    for at in (first + 1..ops.len()).rev() {
        let span = (at - first + 1) as u64;
        let other = first + (draw() % span) as usize;
        ops.swap(at, other);
    }
    ops
}
