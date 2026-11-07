// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! An operation's call, as a run records it: the kind called, the task calling, the State it ran on
//! and its interval there, and what it answered, or that it was dropped. The model judges a call's
//! records, its interval and its outcome together.
//!
//! The interval is in the State's `seq` ([`StateHooks::seq_now`]): `begin` read just before the
//! call, `end` just after it ends, so its sections take `seq`s in `[begin, end)` and the model
//! before `begin` holds what the call found. A dropped call ends as it drops, after its own
//! steps' drop glue. The futures it spawned are its own by the call they were spawned in, not by
//! its interval: a dropped call's may still record after it, as a `JoinSet` drops its tasks at
//! the runtime's next turn. A call is recorded only where its thread runs a recording run
//! ([`crate::harness::log::Collector::enter`]).
//!
//! [`StateHooks::seq_now`]: lore_revision::state::node::hooks::StateHooks::seq_now

use std::cmp::Reverse;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::task::ready;

use lore_base::types::Hash;
use lore_base::types::RepositoryId;
use lore_revision::node::Node;
use lore_revision::node::NodeID;
use lore_revision::state::AddedSince;
use lore_revision::state::Content;
use lore_revision::state::State;
use lore_revision::state::diff;
use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::StateHooks;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Record;

use super::Kind;
use crate::harness::log::Collector;

/// What an operation answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It did what it was called for: an attach attached, a detach detached, a mark marked.
    Done(Answer),
    /// It decided against the State as it found it.
    Refused(Refusal),
    /// A reader's answer.
    Answer(Answer),
    /// An answer the State reports of itself, legal where a run admits it but toward no verified
    /// outcome: it asked its caller to slow down, a name table had no room, or a caller's check
    /// loop spent its retries.
    SelfReported(SelfReported),
    /// An error a run may inject, legal only where a read failed during the call: a failing store,
    /// an evicted block it could not load again.
    Failed(String),
    /// Dropped part way.
    Dropped,
}

impl Outcome {
    /// Whether the call stopped part way, dropped or failed, leaving what its records tell (C12).
    pub fn left_part_way(&self) -> bool {
        matches!(self, Self::Dropped | Self::Failed(_))
    }
}

/// What a call looked up, as its op notes each ([`looked_up`]), held to the model from the `seq` it
/// began at, `from`: a find to the call's end; a read by the mark of the call's own marked listing
/// of `dir` to the `seq` it ended at, `to`, by the mark as the listing's read of the directory's
/// head made it, with the prepends to `dir` the call counted in it by then.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// A find of the hash `name` in `dir`: the node it found, which carried the hash in the
    /// directory at some `seq` from there; or none, where no one node carried it there throughout.
    Find {
        dir: NodeID,
        name: u64,
        found: Option<NodeID>,
        from: u64,
    },
    /// A search for the hash `name` since the mark (`State::find_subnode_added_since`), as a
    /// search by a listing's mark answers ([`Answer::since`]).
    Since {
        dir: NodeID,
        name: u64,
        searched: AddedSince,
        prepends: u32,
        from: u64,
        to: u64,
    },
    /// Whether the mark found `dir` reattached since (`ChainMark::reattached`).
    Reattached {
        dir: NodeID,
        reattached: bool,
        prepends: u32,
        from: u64,
        to: u64,
    },
}

impl Lookup {
    /// The `seq` it began at.
    pub fn begun(&self) -> u64 {
        match *self {
            Self::Find { from, .. } | Self::Since { from, .. } | Self::Reattached { from, .. } => {
                from
            }
        }
    }

    /// The `seq` a read by the call's own mark ended at, which bounds it; none for a find.
    pub fn ended(&self) -> Option<u64> {
        match *self {
            Self::Find { .. } => None,
            Self::Since { to, .. } | Self::Reattached { to, .. } => Some(to),
        }
    }
}

/// A walk of a directory's children a call made, as its op notes it ([`walked`]): the `seq` it
/// began at, each node it yielded, in order, what it reported of its repeats, the live children it
/// passed over for a name it could not read, and its caller's filter. Held to the model from its
/// read of the directory's head, the call's first `Head` of `dir` from `from` on, to the call's end
/// (C6); or, `over` a link, to the revision of a pin the link held; or, `over` a node that takes no
/// children, to none.
#[derive(Clone, Debug)]
pub struct Walk {
    pub dir: NodeID,
    pub from: u64,
    pub yielded: Vec<Yielded>,
    pub repeats: Repeats,
    pub skipped: u32,
    pub filter: Filter,
    pub over: Over,
}

/// What a walk walked, by its node's kind as its op read it: a directory's chain; a link's pinned
/// directory, in the State its pin names, which the read loads afresh, private to it; or nothing,
/// a node that takes no children.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Over {
    Chain,
    Link,
    Leaf,
}

/// What a walk reported of the nodes it may have yielded twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repeats {
    /// None: its directory's count stood from its head read to its end.
    No,
    /// It may have yielded one twice.
    May,
    /// It dropped any before answering and reported nothing, as a listing does: it yields none
    /// twice, and may have walked as one that may repeat.
    Dropped,
}

impl Repeats {
    /// What an iterator reporting `may_repeat` reported.
    pub fn reported(may_repeat: bool) -> Self {
        if may_repeat { Self::May } else { Self::No }
    }

    /// Whether the walk may have passed over a node a `Move` within the directory repositioned
    /// behind it: one that may have repeated, reported or not.
    pub fn may_pass_over(self) -> bool {
        self != Self::No
    }
}

/// The children a walk's caller keeps, judging each by the record its walk read as it passed:
/// one staged for deletion only where `deleted`, and only one `keep` keeps, as
/// `State::collect_children_unsorted_where` takes them.
#[derive(Clone, Copy, Debug)]
pub struct Filter {
    pub deleted: bool,
    pub keep: fn(&Node) -> bool,
}

impl Filter {
    /// Every child, as an iterator yields them.
    pub const EVERY: Self = Self {
        deleted: true,
        keep: |_| true,
    };

    /// Every child but one staged for deletion.
    pub const UNDELETED: Self = Self {
        deleted: false,
        keep: |_| true,
    };

    /// Every child a working tree versions, a symbolic link not, as the diff against a working
    /// tree keeps them.
    pub const VERSIONED: Self = Self {
        deleted: true,
        keep: diff::versioned_on_a_working_tree,
    };

    /// Whether a child with `record` is kept.
    pub fn keeps(&self, record: &Node) -> bool {
        (self.deleted || !record.is_staged_delete()) && (self.keep)(record)
    }
}

/// A node a walk yielded: the record it read for it, the hash of its name and the [`spelling`] of
/// the name it read with it, each where it answers it.
#[derive(Clone, Copy, Debug)]
pub struct Yielded {
    pub node: NodeID,
    pub record: Option<Node>,
    pub name: Option<u64>,
    pub spelling: Option<u64>,
}

/// What a call's done answer leaves on its target, as its op notes it ([`leaving`]): held to the
/// model once its last own write on its target (a `Word`, a `Move`, a `Rename`, a repin) applied;
/// where it wrote none there, at some `seq` of its interval, as a change of words already in force
/// writes none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leaves {
    /// The content given: its mode, its size and its address's hash, as a file keeps its own
    /// context where the address gives none.
    Content(Content),
    /// The content given, clean.
    Flipped(Content),
    /// The size given.
    Size(u64),
    /// Dirty modified, as a forced modify mark leaves it.
    Dirty,
    /// Not dirty.
    Clean,
    /// Not staged.
    Unstaged,
    /// Staged and dirty deleted.
    StagedDelete,
    /// Taken back: staged and dirty modified, not staged for deletion.
    Restored,
    /// A link pinned to the revision `hash`, exposing `child` where the pin names one.
    Pinned { hash: Hash, child: Option<NodeID> },
    /// Under the call's destination, carrying its name and spelling where it names one.
    Placed,
    /// Carrying the call's name and spelling.
    Named,
    /// Dirty, holding local content.
    LocalCopy,
}

/// A name's spelling as a number, its case kept, where its hash folds case: a respell keeps the
/// hash and changes the spelling.
pub fn spelling(name: &[u8]) -> u64 {
    spelled(SPELLING_BASIS, name)
}

/// The spelling of the path `names` make, joined by `/` as a path joins them, with no joined copy
/// made.
pub fn path_spelling<'a>(names: impl IntoIterator<Item = &'a [u8]>) -> u64 {
    let mut names = names.into_iter();
    let first = names.next().map_or(SPELLING_BASIS, spelling);
    names.fold(first, |hash, name| spelling_below(Some(hash), name))
}

/// The spelling of the `/`-joined path `path`: [`path_spelling`] of its names, the empty path's
/// for an empty one, which is one empty name.
pub fn path_spelled(path: &str) -> u64 {
    path_spelling(path.split('/').map(str::as_bytes))
}

/// The spellings of `paths`, each `/`-joined ([`path_spelled`]), sorted, as a dirty-paths
/// collection's call keeps them ([`Call::paths`]).
pub fn path_spellings(paths: &[&str]) -> Vec<u64> {
    let mut spelled: Vec<u64> = paths.iter().map(|path| path_spelled(path)).collect();
    spelled.sort_unstable();
    spelled
}

/// The spelling of the path `below` spells joined with `name` below it, as a path's push joins
/// them; `below` none for the empty path, below which `name` is the whole path.
pub fn spelling_below(below: Option<u64>, name: &[u8]) -> u64 {
    below.map_or_else(
        || spelling(name),
        |below| spelled(spelled(below, b"/"), name),
    )
}

/// The FNV-1a offset basis a spelling starts from.
const SPELLING_BASIS: u64 = 0xcbf2_9ce4_8422_2325;

/// `hash`, a spelling so far, carried on over `bytes` (FNV-1a).
fn spelled(hash: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(hash, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// A self-reported answer ([`Outcome::SelfReported`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SelfReported {
    /// `SlowDown`: a chain changed under the op at every attempt its bound allows.
    SlowDown,
    /// A name table with no room for the op's name.
    NameTableFull,
    /// A caller's check loop spent its retries, as RMDIR answers `EAGAIN`.
    RetriesSpent,
    /// A resident op found a block it needed not in memory: `find_subnode_resident`'s `Ok(None)`.
    NeedLoad,
}

/// What an operation answered beyond its outcome, for the rules that read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Nothing beyond the outcome.
    None,
    /// The node it published.
    Node(NodeID),
    /// The live child holding the name, which it changed in place of an attach.
    Held(NodeID),
    /// A reattach's move, and whether the node counted as added as it moved.
    Moved { added: bool },
    /// A find's node, or none.
    Found(Option<NodeID>),
    /// A resolution's node, in the revision a link it crossed pins, of another repository or at
    /// another revision than the call's State's.
    Linked {
        node: NodeID,
        repository: RepositoryId,
        revision: Hash,
    },
    /// What a search of a directory since a listing's `ChainMark` found ahead of the mark's head,
    /// or in the whole chain where the head left it: a child carrying the name, or none.
    Since(Option<NodeID>),
    /// A search since a listing's `ChainMark` that could not tell what lies ahead of the mark's
    /// head.
    Stale,
    /// Whether a listing's `ChainMark` found its directory reattached since.
    Reattached(bool),
    /// The spelling of the name a name read read, or none where it skipped a name the read path
    /// refuses.
    Name(Option<u64>),
    /// Whether a node's stored name was the name a comparison by name compared.
    NameIs(bool),
    /// The spelling of the path a path read answered, or none where the ancestor it read up to
    /// does not hold the node.
    Path(Option<u64>),
    /// Whether a directory had a child carrying the flags a has-children read asks for.
    Has(bool),
    /// The paths a dirty-paths collection reported, each spelled on its call ([`Call::paths`]).
    Collected,
    /// What a discard freed.
    Freed { files: u64, directories: u64 },
}

impl Answer {
    /// What a search since a listing's `ChainMark` answering `searched` answered: the child it
    /// found, none, or stale.
    pub fn since(searched: AddedSince) -> Self {
        match searched {
            AddedSince::Found(node) => Self::Since(Some(node)),
            AddedSince::Absent => Self::Since(None),
            AddedSince::Stale => Self::Stale,
        }
    }
}

/// Why an operation decided against the State as it found it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A live child held the name.
    Exists,
    /// The node, or the directory it changes, gone: Discarded, or no longer where it was.
    NotFound,
    /// A directory without local content, moved to another place.
    NotLocal,
    /// The node was dirty.
    IfClean,
    /// A directory still held a child.
    NotEmpty,
    /// A settle's directory, standing at its built place, did not match the committed one: its
    /// children or its name differed, or the committed one is no directory.
    Unequal,
    /// A change below a settle's directory, or its move, landed before its clear, as the State
    /// counts one: told apart where one settle at a time runs on a State, as the contract runs
    /// them.
    Overtaken,
    /// A settle's directory, kept, found away from its built place after its clear: moved or
    /// renamed while the settle ran.
    Displaced,
    /// A settle that did not begin: the directory gone, clean, or not at its built place.
    NotBegun,
    /// An in-place or owned change found the node gone, moved or renamed, or nothing to change.
    Declined,
    /// A C API modify or a rewrite refused: the node Discarded, not a file, or staged for deletion.
    Modify,
    /// A C API restore refused: the node Discarded or not staged for deletion, or its parent
    /// Discarded or staged for deletion.
    Restore,
    /// A C API delete's level of a node already staged for deletion.
    DeleteLevelStaged,
    /// A C API delete's level of a node added, Discarded, or no longer in its parent.
    DeleteLevelOther,
    /// A C API add refused: its parent staged for deletion.
    Add,
    /// A C API move refused: the node, or a parent it leaves or joins, Discarded or staged for
    /// deletion, the destination no directory, the node under it by that name already, or the
    /// move into the node's own subtree.
    Move,
    /// A served rename into the node's own subtree, or under a node that is no directory.
    Subtree,
}

/// How a task came to be, for a scheduler that tells a spawned future a task of its own: the task
/// that spawned it, and the call open on that task as it did, in the call's own steps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Spawn {
    pub spawner: Option<usize>,
    pub call: Option<u64>,
}

/// What a sweep's call is made on beyond its kind, as its op notes it ([`sweeping`]): the nodes its
/// retained test names, and the blocks in memory as it began, which it scans where they are
/// flagged. Both sorted.
#[derive(Clone, Debug, Default)]
pub struct Sweeping {
    pub retained: Vec<NodeID>,
    pub resident: Vec<usize>,
}

/// How each task of a run came to be, kept by a scheduler that tells a spawned future a task of
/// its own, by task: its operations' own first, then each future spawned, numbered as spawned. Each
/// spawned task also keeps the scope of the call open on its spawner as it spawned, so a read
/// failed in it counts on that call ([`failed_read`]), which the future, running outside the call's
/// task-local scope, cannot name itself. Installed on each thread the run's tasks run on
/// ([`Self::install`]).
#[derive(Default)]
pub struct Spawns {
    tasks: Mutex<Vec<Spawned>>,
}

/// A task's spawn, with the scope of the call it was spawned in, where one recorded.
#[derive(Clone, Default)]
struct Spawned {
    spawn: Spawn,
    scope: Option<Arc<Scope>>,
}

thread_local! {
    /// The spawns of the run whose tasks this thread runs ([`Spawns::install`]).
    static SPAWNS: std::cell::RefCell<Option<Arc<Spawns>>> = const { std::cell::RefCell::new(None) };
}

impl Spawns {
    /// The spawns of a run whose first `tasks` tasks are its operations' own.
    pub fn new(tasks: usize) -> Arc<Self> {
        let spawns = Arc::new(Self::default());
        spawns.reserve(tasks);
        spawns
    }

    fn tasks(&self) -> std::sync::MutexGuard<'_, Vec<Spawned>> {
        self.tasks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Numbers the first `tasks` tasks as the run's operations' own, where fewer are numbered.
    pub fn reserve(&self, tasks: usize) {
        let mut numbered = self.tasks();
        if numbered.len() < tasks {
            numbered.resize(tasks, Spawned::default());
        }
    }

    /// Whether any task is numbered: a driver that names its operations' tasks reserved them.
    pub fn numbered(&self) -> bool {
        !self.tasks().is_empty()
    }

    /// Task `spawner` spawned a future, told in its poll, where its call's scope is set: the task
    /// the future runs as, the next number.
    pub fn spawned(&self, spawner: usize) -> usize {
        let scope = own_scope();
        let spawn = Spawn {
            spawner: Some(spawner),
            call: scope.as_ref().map(|scope| scope.id),
        };
        let mut tasks = self.tasks();
        tasks.push(Spawned { spawn, scope });
        tasks.len() - 1
    }

    /// How each task came to be, by task.
    pub fn spawns(&self) -> Vec<Spawn> {
        self.tasks().iter().map(|spawned| spawned.spawn).collect()
    }

    /// The scope of the call `task` was spawned in, up its spawns: the call open on the first
    /// spawner up them that spawned in one.
    fn scope_of(&self, task: usize) -> Option<Arc<Scope>> {
        let tasks = self.tasks();
        let mut at = task;
        for _ in 0..tasks.len() {
            let spawned = tasks.get(at)?;
            if let Some(scope) = &spawned.scope {
                return Some(scope.clone());
            }
            at = spawned.spawn.spawner?;
        }
        None
    }

    /// Installs these spawns on the calling thread until the guard drops.
    pub fn install(self: &Arc<Self>) -> InstalledSpawns {
        SPAWNS.with_borrow_mut(|slot| {
            assert!(slot.is_none(), "one run's spawns per thread");
            *slot = Some(self.clone());
        });
        InstalledSpawns { _private: () }
    }
}

/// A run's spawns installed on this thread; dropping it uninstalls them.
pub struct InstalledSpawns {
    _private: (),
}

impl Drop for InstalledSpawns {
    fn drop(&mut self) {
        SPAWNS.with_borrow_mut(Option::take);
    }
}

/// Counts a read a failing store failed on the call it failed in: the call whose own steps the
/// running future is, or, for a future spawned in a call's own steps, at any depth, that call, as
/// the thread's spawns tell ([`Spawns`]). Whether it failed in a call's own steps.
pub(crate) fn failed_read() -> bool {
    if let Some(scope) = own_scope() {
        scope.failed_reads.fetch_add(1, Ordering::Relaxed);
        return true;
    }
    let spawned = hooks::task().and_then(|task| {
        SPAWNS.with_borrow(|spawns| spawns.as_ref().and_then(|spawns| spawns.scope_of(task)))
    });
    if let Some(scope) = spawned {
        scope.failed_reads.fetch_add(1, Ordering::Relaxed);
    }
    false
}

/// An operation's call in a run.
#[derive(Clone, Debug)]
pub struct Call {
    pub kind: &'static Kind,
    /// What a sweep's call is made on, where its op noted it ([`sweeping`]).
    pub sweeping: Option<Sweeping>,
    /// Its id, which the futures spawned in its own steps carry ([`Spawn::call`]).
    pub id: u64,
    /// The scheduler's task that made it.
    pub task: Option<usize>,
    /// The State it ran on, by its log id.
    pub state: u64,
    /// The node it was called on: the node it changes, or the directory it changes below; none for
    /// a call on the whole State.
    pub target: Option<NodeID>,
    /// The directory it moves its node into, where it moves one, or the ancestor a path read reads
    /// up to ([`call_on`]).
    pub destination: Option<NodeID>,
    /// The hash of the name it attaches or moves under, where it names one ([`call_on`]): the
    /// name a held-name answer's holder carries.
    pub name: Option<u64>,
    /// That name's [`spelling`], where it moves or renames under one, or the spelling a comparison
    /// by name compares ([`call_on`]).
    pub spelling: Option<u64>,
    /// The holder it replaces, where it replaces one ([`call_on`]).
    pub holder: Option<NodeID>,
    /// The parent and the name hash it finds its node at, where it finds it by place
    /// ([`call_on`]).
    pub place: Option<(NodeID, u64)>,
    /// The marked listing whose `ChainMark` it reads by, by the listing's call's id, where it reads
    /// by one, and the prepends to its target the mark's holder counted in it ([`call_on`]).
    pub listing: Option<u64>,
    pub prepends: u32,
    /// The nodes it found taken by another task after a step it answered by ([`taken`]), each with
    /// the `seq` it found it at, to be gone at some `seq` from there to its end.
    pub taken: Vec<(NodeID, u64)>,
    /// What its done answer leaves on its target, where its op notes it ([`leaving`]).
    pub leaves: Option<Leaves>,
    /// What it looked up, in order ([`looked_up`]).
    pub lookups: Vec<Lookup>,
    /// The walks of a directory's children it made, in order ([`walked`]).
    pub walks: Vec<Walk>,
    /// The spellings of the paths a dirty-paths collection reported, each relative to its root,
    /// sorted ([`collected`]).
    pub paths: Vec<u64>,
    /// The hashes of the names of the path a resolution walks below its target, in order
    /// ([`resolving_names`]).
    pub names: Vec<u64>,
    /// The reads a failing store failed in its own steps or in a future spawned there
    /// ([`failed_read`]): an error is legal only where one did.
    pub failed_reads: u64,
    /// Its interval in that State's `seq`: `[begin, end)`.
    pub begin: u64,
    pub end: u64,
    /// The `seq` ranges of its windows: futures of its caller's it awaited between its own steps
    /// ([`window`]), whose records are not its own.
    pub windows: Vec<(u64, u64)>,
    pub outcome: Outcome,
}

impl Call {
    /// The `seq` of `walk`'s read of its directory's head among `records`: the first `Head` of the
    /// directory from where the walk began that this call holds, `spawns` telling which futures
    /// are its own ([`Self::holds`]).
    pub fn head_read(&self, walk: &Walk, records: &[Record], spawns: &[Spawn]) -> Option<u64> {
        (records.iter())
            .find(|record| {
                matches!(record.effect, Effect::Head { node, .. } if node == walk.dir)
                    && record.seq >= walk.from
                    && self.holds(record, spawns)
            })
            .map(|record| record.seq)
    }

    /// The `seq` of this call's first read of `dir`'s head among `records`, `spawns` telling which
    /// futures are its own ([`Self::holds`]): a has-children read's listing's, or a dirty-paths
    /// collection's root's.
    pub fn first_head(&self, dir: NodeID, records: &[Record], spawns: &[Spawn]) -> Option<u64> {
        (records.iter())
            .find(|record| {
                matches!(record.effect, Effect::Head { node, .. } if node == dir)
                    && self.holds(record, spawns)
            })
            .map(|record| record.seq)
    }

    /// Whether `record` is this call's: on its State, made by its task within its interval and
    /// outside its windows, or by a future spawned in its own steps, at any depth, from its begin
    /// on. `spawns` tells how each task came to be ([`crate::harness::scheduler::run_spawns`]);
    /// where a scheduler tells spawned futures no task of their own, as chaos does where its driver
    /// numbers no tasks, they carry their spawner's, and its interval alone holds them.
    pub fn holds(&self, record: &Record, spawns: &[Spawn]) -> bool {
        let Some(task) = self.task else {
            return false;
        };
        if record.state != self.state || record.seq < self.begin {
            return false;
        }
        if record.task == Some(task) {
            return record.seq < self.end
                && !self
                    .windows
                    .iter()
                    .any(|window| within(record.seq, *window));
        }
        let mut at = record.task;
        while let Some(spawned) = at {
            let Some(spawn) = spawns.get(spawned) else {
                return false;
            };
            if spawn.spawner == Some(task) {
                return spawn.call == Some(self.id);
            }
            at = spawn.spawner;
        }
        false
    }

    /// Whether `other` lies within one of this call's windows.
    fn windows(&self, other: &Call) -> bool {
        self.windows
            .iter()
            .any(|&(begin, end)| begin <= other.begin && other.end <= end)
    }
}

fn within(seq: u64, (begin, end): (u64, u64)) -> bool {
    (begin..end).contains(&seq)
}

/// Two calls of one task on one State overlapping, if any, but for one made in the other's window:
/// their records could not be told apart. A call that took no `seq` holds no record and overlaps
/// none.
pub fn overlapping(calls: &[Call]) -> Option<(&Call, &Call)> {
    let mut by_task: Vec<&Call> = calls
        .iter()
        .filter(|call| call.task.is_some() && call.begin < call.end)
        .collect();
    by_task.sort_by_key(|call| (call.state, call.task, call.begin, Reverse(call.end)));
    // The calls still open where the next one begins, each made in a window of the one before it,
    // so each ends before the one below it.
    let mut enclosing: Vec<&Call> = Vec::new();
    'calls: for call in by_task {
        while let Some(&outer) = enclosing.last() {
            if (outer.state, outer.task) != (call.state, call.task) || outer.end <= call.begin {
                enclosing.pop();
                continue;
            }
            if outer.windows(call) {
                break;
            }
            // Equal intervals sort either way: the one made in the other's window may come first.
            if (outer.begin, outer.end) == (call.begin, call.end) && call.windows(outer) {
                enclosing.pop();
                enclosing.push(call);
                enclosing.push(outer);
                continue 'calls;
            }
            return Some((outer, call));
        }
        enclosing.push(call);
    }
    None
}

/// A record two calls hold, if any, with the two.
pub fn held_twice<'a>(
    records: &'a [Record],
    calls: &'a [Call],
    spawns: &[Spawn],
) -> Option<(&'a Record, &'a Call, &'a Call)> {
    if calls.len() < 2 {
        return None;
    }
    records.iter().find_map(|record| {
        let mut holding = calls.iter().filter(|call| call.holds(record, spawns));
        let first = holding.next()?;
        holding.next().map(|second| (record, first, second))
    })
}

/// What a call's future is part of: its own steps, with its scope where it records, or one of its
/// windows.
#[derive(Clone)]
enum Inside {
    Call(Option<Arc<Scope>>),
    Window,
}

tokio::task_local! {
    /// The call the running future is part of, if any: a call made in another's own steps would
    /// be one inside another.
    static INSIDE: Inside;
}

tokio::task_local! {
    /// Whether a call the running future made failed ([`noting_failures`]).
    static FAILED: std::cell::Cell<bool>;
}

/// `future`, noting whether a call it makes answers it failed ([`any_call_failed`]): for a driver
/// whose operations meet a failing store, where a call failing on an injected error leaves the
/// operation's outcome to the model alone.
pub fn noting_failures<F: Future>(future: F) -> impl Future<Output = F::Output> {
    FAILED.scope(std::cell::Cell::new(false), future)
}

/// Whether a call the running future made answered it failed, inside [`noting_failures`].
pub fn any_call_failed() -> bool {
    FAILED.try_with(std::cell::Cell::get).unwrap_or(false)
}

/// The ids calls take, unique in the process, so a future spawned in one names that call alone.
static NEXT_CALL: AtomicU64 = AtomicU64::new(0);

/// What a call is made on beyond its kind.
#[derive(Clone, Copy, Debug, Default)]
pub struct On {
    /// The node it changes, or the directory it changes below; none for a call on the whole State.
    pub target: Option<NodeID>,
    /// The directory it moves its node into, where it moves one, or the ancestor a path read reads
    /// up to.
    pub destination: Option<NodeID>,
    /// The hash of the name it attaches or moves under, where it names one.
    pub name: Option<u64>,
    /// That name's [`spelling`], where it moves or renames under one, or the spelling a comparison
    /// by name compares.
    pub spelling: Option<u64>,
    /// The holder it replaces, where it replaces one.
    pub holder: Option<NodeID>,
    /// The parent and the name hash it finds its node at, where it finds it by place.
    pub place: Option<(NodeID, u64)>,
    /// The marked listing whose `ChainMark` it reads by, by the listing's call's id, and the
    /// prepends to the target the mark's holder counted in it.
    pub listing: Option<u64>,
    pub prepends: u32,
}

impl On {
    /// On `target` alone.
    pub fn at(target: Option<NodeID>) -> Self {
        Self {
            target,
            ..Self::default()
        }
    }
}

/// A recorded call's id and State, what it is made on, the reads failed in it, and its notes.
struct Scope {
    id: u64,
    state: u64,
    on: On,
    /// Counted wherever the reads fail, on whichever thread ([`failed_read`]).
    failed_reads: AtomicU64,
    notes: Mutex<Notes>,
}

/// What a call's steps note on it as it runs.
#[derive(Default)]
struct Notes {
    /// What a sweep is made on ([`sweeping`]).
    sweeping: Option<Sweeping>,
    /// Its windows, which [`window`] adds to as each ends.
    windows: Vec<(u64, u64)>,
    /// The nodes it found taken ([`taken`]).
    taken: Vec<(NodeID, u64)>,
    /// What its done answer leaves ([`leaving`]).
    leaves: Option<Leaves>,
    /// What it looked up ([`looked_up`]).
    lookups: Vec<Lookup>,
    /// The walks it made ([`walked`]).
    walks: Vec<Walk>,
    /// The paths it collected ([`collected`]).
    paths: Vec<u64>,
    /// The names of the path it resolved ([`resolving_names`]).
    names: Vec<u64>,
}

impl Scope {
    fn note(&self, note: impl FnOnce(&mut Notes)) {
        note(&mut self.notes.lock().unwrap_or_else(PoisonError::into_inner));
    }

    fn take(&self) -> Notes {
        std::mem::take(&mut self.notes.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// Notes on the call whose own steps the running future is, where it records, that `node` was
/// found taken by another task, by a step that began at `seq` of `state` and that the call answers
/// by: the node's own not-found, or an ancestor's, passed over. Outside a call's own steps, in a
/// window or a future it spawned, it notes nothing.
pub(crate) fn taken(state: &State, node: NodeID, seq: u64) {
    note_on(state, "a node taken", |notes| notes.taken.push((node, seq)));
}

/// Notes on the call whose own steps the running future is, where it records, what its done answer
/// leaves on its target. Outside a call's own steps it notes nothing.
pub(crate) fn leaving(leaves: Leaves) {
    note(|_| {}, |notes| notes.leaves = Some(leaves));
}

/// Notes on the call whose own steps the running future is, where it records, what it looked up
/// on `state`. Outside a call's own steps it notes nothing.
pub(crate) fn looked_up(state: &State, lookup: Lookup) {
    note_on(state, "a lookup", |notes| notes.lookups.push(lookup));
}

/// Notes on the call whose own steps the running future is, where it records, the spellings of the
/// paths a dirty-paths collection on `state` reported, sorted. Outside a call's own steps it notes
/// nothing.
pub(crate) fn collected(state: &State, paths: Vec<u64>) {
    note_on(state, "collected paths", |notes| notes.paths = paths);
}

/// Notes on the call whose own steps the running future is, where it records, the hashes of the
/// names of the path it resolves on `state`, in order. Outside a call's own steps it notes nothing.
pub(crate) fn resolving_names(state: &State, names: Vec<u64>) {
    note_on(state, "a resolved path", |notes| notes.names = names);
}

/// Notes on the call whose own steps the running future is, where it records, a walk it made of a
/// directory's children on `state`. Outside a call's own steps it notes nothing.
pub(crate) fn walked(state: &State, walk: Walk) {
    note_on(state, "a walk", |notes| notes.walks.push(walk));
}

/// Notes on the call whose own steps the running future is, where it records, what its sweep of
/// `state` is made on. Outside a call's own steps it notes nothing.
pub(crate) fn sweeping(state: &State, sweeping: Sweeping) {
    note_on(state, "a sweep", |notes| notes.sweeping = Some(sweeping));
}

/// Notes `note` on the call whose own steps the running future is, where it records, `what` on
/// `state`, whose `seq`s it carries, which must be its call's State. Outside a call's own steps it
/// notes nothing.
fn note_on(state: &State, what: &str, noted: impl FnOnce(&mut Notes)) {
    let on = |scope: &Scope| {
        assert_eq!(
            scope.state,
            state.hooks().id(),
            "{what} on another State than its call's"
        );
    };
    note(on, noted);
}

/// Notes `noted` on the call whose own steps the running future is, where it records, once `check`
/// passed its scope. Outside a call's own steps it notes nothing.
fn note(check: impl FnOnce(&Scope), noted: impl FnOnce(&mut Notes)) {
    let _ = INSIDE.try_with(|inside| {
        if let Inside::Call(Some(scope)) = inside {
            check(scope);
            scope.note(noted);
        }
    });
}

/// The id of the call whose own steps the running future is, where it records: a future spawned
/// here is that call's ([`Spawn::call`]).
pub(crate) fn open_call() -> Option<u64> {
    own_scope().map(|scope| scope.id)
}

/// The scope of the call whose own steps the running future is, where it records.
fn own_scope() -> Option<Arc<Scope>> {
    INSIDE
        .try_with(|inside| match inside {
            Inside::Call(scope) => scope.clone(),
            Inside::Window => None,
        })
        .ok()
        .flatten()
}

/// `body`, a call of `kind` on `state` and `target`, recorded where the thread runs a recording
/// run: its id and task, its interval and windows, and the outcome `body` gives with its answer, or
/// that it was dropped. A call made inside another's own steps is a composition the catalogue does
/// not declare, which panics.
pub fn call<'a, T, F: Future<Output = (T, Outcome)>>(
    state: &'a State,
    (kind, target): (&'static Kind, Option<NodeID>),
    body: F,
) -> Called<'a, F> {
    call_on(state, (kind, On::at(target)), body)
}

/// [`call`] on what `on` says: its target, the directory it moves its node into, the name it
/// attaches or moves under, which its answer and its refusals are held to.
pub fn call_on<'a, T, F: Future<Output = (T, Outcome)>>(
    state: &'a State,
    (kind, on): (&'static Kind, On),
    body: F,
) -> Called<'a, F> {
    // Read where the call is made, which is where its first poll runs: a call outside a recording
    // run keeps nothing, windows included.
    let recording = Collector::entered().map(|log| {
        let scope = Scope {
            id: NEXT_CALL.fetch_add(1, Ordering::Relaxed),
            state: state.hooks().id(),
            on,
            failed_reads: AtomicU64::new(0),
            notes: Mutex::default(),
        };
        (log, Arc::new(scope))
    });
    let scope = recording.as_ref().map(|(_, scope)| scope.clone());
    Called {
        body: INSIDE.scope(Inside::Call(scope), body),
        calling: Keeping(Calling::Unpolled {
            state,
            kind,
            recording,
        }),
    }
}

/// [`call`]'s future, holding `body` once: its call opened at the first poll, and kept as it
/// ends, or as it drops, after `body`, so its drop glue is inside the call and its windows close
/// first.
#[pin_project::pin_project]
pub struct Called<'a, F> {
    #[pin]
    body: tokio::task::futures::TaskLocalFuture<Inside, F>,
    calling: Keeping<'a>,
}

impl<T, F: Future<Output = (T, Outcome)>> Future for Called<'_, F> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<T> {
        let this = self.project();
        let calling = &mut this.calling.0;
        if let Calling::Unpolled {
            state,
            kind,
            recording,
        } = calling
        {
            if matches!(
                INSIDE.try_with(|inside| matches!(inside, Inside::Call(_))),
                Ok(true)
            ) {
                panic!("a call of '{}' inside another call", kind.name);
            }
            let (state, kind) = (*state, *kind);
            *calling = match recording.take() {
                Some((log, scope)) => Calling::Open(Open::begin(log, scope, state, kind)),
                None => Calling::Closed,
            };
        }
        let (answer, outcome) = ready!(this.body.poll(context));
        if matches!(outcome, Outcome::Failed(_)) {
            let _ = FAILED.try_with(|failed| failed.set(true));
        }
        calling.end(outcome);
        Poll::Ready(answer)
    }
}

/// `future`, a window of the running call: a future of its caller's it awaits between its own
/// steps, as a settle awaits its window before its last phase. The window's `seq` range is kept on
/// the call, so the records made there are not the call's, and a call made there is one of its
/// own. It opens at its first poll and closes as it ends or drops, after `future`.
pub(crate) fn window<F: Future>(state: &State, future: F) -> Windowed<'_, F> {
    Windowed {
        future: INSIDE.scope(Inside::Window, future),
        window: Closing(Window::Unpolled(state)),
    }
}

/// [`window`]'s future.
#[pin_project::pin_project]
pub(crate) struct Windowed<'a, F> {
    #[pin]
    future: tokio::task::futures::TaskLocalFuture<Inside, F>,
    window: Closing<'a>,
}

impl<F: Future> Future for Windowed<'_, F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<F::Output> {
        let this = self.project();
        let window = &mut this.window.0;
        if let Window::Unpolled(state) = *window {
            let scope = own_scope();
            let hooks = &**state.hooks();
            if let Some(scope) = &scope {
                // A window's range is in its call's State's `seq`.
                assert_eq!(
                    scope.state,
                    hooks.id(),
                    "a window on another State than its call's"
                );
            }
            *window = scope.map_or(Window::Closed, |scope| Window::Open {
                scope,
                hooks,
                begin: hooks.seq_now(),
            });
        }
        let output = ready!(this.future.poll(context));
        window.close();
        Poll::Ready(output)
    }
}

/// A window, kept on its call's scope as it closes.
enum Window<'a> {
    Unpolled(&'a State),
    Open {
        scope: Arc<Scope>,
        hooks: &'a StateHooks,
        begin: u64,
    },
    Closed,
}

impl Window<'_> {
    fn close(&mut self) {
        if let Window::Open {
            scope,
            hooks,
            begin,
        } = std::mem::replace(self, Window::Closed)
        {
            scope.note(|notes| notes.windows.push((begin, hooks.seq_now())));
        }
    }
}

/// A window closed as it drops, declared after its future, so the future's drop glue is inside it.
struct Closing<'a>(Window<'a>);

impl Drop for Closing<'_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// A call before its first poll, open, or kept: kept in its run's collector as it ends, or as it
/// drops first.
enum Calling<'a> {
    Unpolled {
        state: &'a State,
        kind: &'static Kind,
        recording: Option<(Arc<Collector>, Arc<Scope>)>,
    },
    Open(Open<'a>),
    Closed,
}

struct Open<'a> {
    log: Arc<Collector>,
    scope: Arc<Scope>,
    kind: &'static Kind,
    task: Option<usize>,
    hooks: &'a StateHooks,
    begin: u64,
}

impl<'a> Open<'a> {
    fn begin(
        log: Arc<Collector>,
        scope: Arc<Scope>,
        state: &'a State,
        kind: &'static Kind,
    ) -> Self {
        log.call_opened();
        let task = hooks::task();
        if task.is_none() {
            log.flag(format!("a call of '{}' with no task", kind.name));
        }
        let hooks = &**state.hooks();
        Self {
            log,
            scope,
            kind,
            task,
            hooks,
            begin: hooks.seq_now(),
        }
    }
}

impl Calling<'_> {
    fn end(&mut self, outcome: Outcome) {
        let Calling::Open(open) = std::mem::replace(self, Calling::Closed) else {
            return;
        };
        let Notes {
            sweeping,
            windows,
            taken,
            leaves,
            lookups,
            walks,
            paths,
            names,
        } = open.scope.take();
        let on = open.scope.on;
        let call = Call {
            kind: open.kind,
            sweeping,
            id: open.scope.id,
            task: open.task,
            state: open.hooks.id(),
            target: on.target,
            destination: on.destination,
            name: on.name,
            spelling: on.spelling,
            holder: on.holder,
            place: on.place,
            listing: on.listing,
            prepends: on.prepends,
            taken,
            leaves,
            lookups,
            walks,
            paths,
            names,
            failed_reads: open.scope.failed_reads.load(Ordering::Relaxed),
            begin: open.begin,
            end: open.hooks.seq_now(),
            windows,
            outcome,
        };
        open.log.call_closed(call);
    }
}

/// A call kept as it drops, declared after its body, so the body's drop glue is inside it.
struct Keeping<'a>(Calling<'a>);

impl Drop for Keeping<'_> {
    fn drop(&mut self) {
        self.0.end(Outcome::Dropped);
    }
}
