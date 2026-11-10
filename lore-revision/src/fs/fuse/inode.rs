// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The inode table, its child index, and kernel lookup-count accounting.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use dashmap::mapref::one::Ref;
use dashmap::mapref::one::RefMut;
use lore_base::text::fold_eq;
use lore_base::types::Address;
use lore_fuse::abi::ForgetOne;
use smallvec::SmallVec;

use crate::fs::fuse::inode_number::Identity;
use crate::fs::fuse::inode_number::IdentityKind;
use crate::fs::fuse::inode_number::derive_inode_number;
use crate::fs::fuse::inode_number::identity_matches;
use crate::fs::fuse::inode_number::rehash;
use crate::node::NodeID;
use crate::node::ROOT_NODE;

pub const ROOT_INO: u64 = 1;

/// A name's bytes, inline up to the length most names have so recording one does not allocate.
pub type Name = SmallVec<[u8; 24]>;

pub enum InodeKind {
    Dir {
        filter_state: u8,
        /// The line the view slot's verdict was decided at (`view::view_line`).
        view_line: u16,
        /// The directory's node in the filesystem State, keying its listing.
        node: NodeID,
        /// Whether the directory was added under `write/`, whose host directory answers its
        /// mode and times.
        local: bool,
    },
    File {
        address: Address,
        size: u64,
        executable: bool,
        /// Whether the bytes live under `write/` (`NodeExtFlags::LocalContent`): the host file
        /// answers READ, and its `fstat` the size and times.
        local: bool,
    },
    /// A symbolic link under `write/`.
    Symlink,
}

impl InodeKind {
    /// A directory the revision holds.
    pub fn dir(filter_state: u8, node: NodeID) -> Self {
        Self::Dir {
            filter_state,
            view_line: 0,
            node,
            local: false,
        }
    }

    /// The identity kind an inode of this kind derives its number from.
    pub fn identity(&self) -> IdentityKind {
        match self {
            Self::Dir { .. } => IdentityKind::Dir,
            Self::File { .. } => IdentityKind::File,
            Self::Symlink => IdentityKind::Symlink,
        }
    }

    /// Whether an inode of kind `self` may stand for one of kind `other`: the same variant.
    pub fn same_variant(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }

    /// Whether this is a file whose bytes live under `write/`.
    pub fn is_local_file(&self) -> bool {
        matches!(self, Self::File { local: true, .. })
    }
}

/// What a check of one candidate in a name's index entry answers ([`InodeTable::first_in_entry`]).
enum Pick<R> {
    /// The answer, at once.
    Take(R),
    /// The answer where no candidate of the pass is taken.
    Keep(R),
    /// Nothing.
    Skip,
}

/// What [`InodeTable::lookup_child_matching`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildLookup {
    /// A live child whose identity matched; one lookup was counted.
    Counted(u64),
    /// A live child under the name whose identity differs; nothing was counted.
    Mismatch(u64),
    Missing,
}

pub struct InodeEntry {
    pub ino: u64,
    pub parent: u64,
    pub name_hash: u64,
    /// The exact-case spelling first looked up.
    pub name: Name,
    pub kind: InodeKind,
    pub nlookup: u64,
    /// Milliseconds, as recorded; 0 when nothing was.
    pub mtime: u64,
    pub orphaned: bool,
    pub read_cursor: AtomicU64,
    pub readahead_started: AtomicBool,
    /// Whether this inode's path has already been sent to the access recorder.
    pub access_recorded: AtomicBool,
    /// Two per host operation of a WRITE, SETATTR or FALLOCATE of the local file, one before
    /// and one after, both under this entry's read guard: odd while one runs, and moved by any
    /// that ran, which a commit's flip under the entry's write guard compares.
    pub write_generation: AtomicU64,
    /// Which entry under this number this is, from a count the table keeps: a number a FORGET
    /// freed and a lookup takes again carries a later one and a fresh write generation.
    pub incarnation: u64,
    /// Moved under this entry's write guard each time the file turns local or store-served, so
    /// a host file opened by its path before no longer passes for its descriptor.
    pub host_epoch: u64,
    /// RENAMEs of this directory under way, each counted from before it moves the host directory
    /// until it has respelled the entry and gathered what stands under the old host path. A host
    /// entry made under a path built below the directory reads this and [`Self::rename_changes`]
    /// before and after; renames of one directory may overlap, as an instance rename takes no
    /// kernel lock. A lookup answering the inode at a name as it stands, over a record read
    /// before, takes that inode for the newer state: true while renames into one directory are
    /// ordered, as the kernel's directory lock orders its own.
    pub renames: AtomicU32,
    /// Moved at each start and each end of a RENAME of this directory.
    pub rename_changes: AtomicU64,
    /// A local file's host file, by its inode number on `write/`'s file system, set under this
    /// entry's write guard from the descriptor open where the host file came to be, or where it
    /// was first identified; 0 where the inode is not a local file or nothing has identified it.
    pub host_file: u64,
    /// The node in the filesystem State the entry stands for, as the lookup that made or last
    /// matched it, or the copy-up that made it local, found it.
    pub node: NodeID,
}

/// Sets `gate` for the one caller that finds it clear: a relaxed load, then one compare-exchange,
/// so a gate already set costs no write. Answers whether this caller set it.
pub fn claim_once(gate: &AtomicBool) -> bool {
    !gate.load(Ordering::Relaxed)
        && gate
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
}

/// Slots of [`InodeTable::written_forgotten_since`], by node.
const FORGOTTEN_WRITE_SLOTS: usize = 4096;

#[lore_macro::test_pub]
pub struct InodeTable {
    by_ino: DashMap<u64, InodeEntry>,
    children: DashMap<(u64, u64), SmallVec<[u64; 1]>>,
    /// The count incarnations ([`InodeEntry::incarnation`]) and written forgets draw from.
    incarnations: AtomicU64,
    /// By node modulo the slot count, the count drawn by the latest FORGET of a written local
    /// file whose node falls in the slot.
    written_forgets: Box<[AtomicU64]>,
    /// Runs once a lookup has copied a name's index entry, before it checks the copy.
    #[cfg(feature = "test-util")]
    candidates_read: std::sync::OnceLock<CandidatesHook>,
}

#[cfg(feature = "test-util")]
type CandidatesHook = Box<dyn Fn(&InodeTable) + Send + Sync>;

impl Default for InodeTable {
    fn default() -> Self {
        Self::new()
    }
}

// Lock order: a `by_ino` entry lock may be held while taking a `children` lock, never the
// reverse; `children` refs are dropped before any `by_ino` access. A `by_ino` entry lock may
// also be held while taking a node block's lock (a lookup's identity check and an insertion's
// admission, under the shard lock, read a record; a settle and a flip write one), never the
// reverse: no path takes an inode or `children` lock under a block lock. The `by_ino` entry
// lock is the linearization point: nlookup changes and child-index removal on FORGET both
// happen under it.
impl InodeTable {
    /// A table holding only the root, with the one lookup the kernel never
    /// counts.
    pub fn new() -> Self {
        let by_ino = DashMap::new();
        by_ino.insert(
            ROOT_INO,
            InodeEntry {
                ino: ROOT_INO,
                parent: ROOT_INO,
                name_hash: 0,
                name: Name::new(),
                kind: InodeKind::dir(0, ROOT_NODE),
                nlookup: 1,
                mtime: 0,
                orphaned: false,
                read_cursor: AtomicU64::new(0),
                readahead_started: AtomicBool::new(false),
                access_recorded: AtomicBool::new(false),
                write_generation: AtomicU64::new(0),
                incarnation: 0,
                host_epoch: 0,
                renames: AtomicU32::new(0),
                rename_changes: AtomicU64::new(0),
                host_file: 0,
                node: ROOT_NODE,
            },
        );
        Self {
            by_ino,
            children: DashMap::new(),
            incarnations: AtomicU64::new(1),
            written_forgets: (0..FORGOTTEN_WRITE_SLOTS)
                .map(|_| AtomicU64::new(0))
                .collect(),
            #[cfg(feature = "test-util")]
            candidates_read: std::sync::OnceLock::new(),
        }
    }

    /// The count's next value: every entry made and every written forget from now on draws one
    /// at least this, and every one before drew one below it.
    pub fn next_incarnation(&self) -> u64 {
        self.incarnations.load(Ordering::SeqCst)
    }

    /// Whether an entry of a written local file standing for `node`, or for a node sharing its
    /// slot, has been forgotten since the count stood at `since`, whenever the entry was made.
    pub fn written_forgotten_since(&self, node: NodeID, since: u64) -> bool {
        self.written_forgets[slot(node)].load(Ordering::SeqCst) >= since
    }

    pub fn get(&self, ino: u64) -> Option<Ref<'_, u64, InodeEntry>> {
        self.by_ino.get(&ino)
    }

    pub fn get_mut(&self, ino: u64) -> Option<RefMut<'_, u64, InodeEntry>> {
        self.by_ino.get_mut(&ino)
    }

    /// The live child of `parent` whose name folds equal to `name`, without
    /// counting a lookup. The answer may be stale by the time it is used;
    /// [`Self::lookup_child`] is the counted, race-free form.
    pub fn find_child(&self, parent: u64, name_hash: u64, name: &str) -> Option<u64> {
        self.first_in_entry(parent, name_hash, |ino, _| {
            if self
                .by_ino
                .get(&ino)
                .is_some_and(|entry| is_child(&entry, parent, name))
            {
                Pick::Take(ino)
            } else {
                Pick::Skip
            }
        })
    }

    /// The live inodes indexed under `name` in `parent`, each with the node it stands for. For
    /// tests.
    pub fn live_children(&self, parent: u64, name_hash: u64, name: &str) -> Vec<(u64, NodeID)> {
        self.candidates(parent, name_hash)
            .into_iter()
            .filter_map(|ino| {
                let entry = self.by_ino.get(&ino)?;
                is_child(&entry, parent, name).then_some((ino, entry.node))
            })
            .collect()
    }

    /// [`Self::find_child`] plus one lookup, counted under the entry lock so a
    /// concurrent FORGET either sees the new count or has already removed
    /// the entry (a miss, and the caller inserts afresh).
    pub fn lookup_child(&self, parent: u64, name_hash: u64, name: &str) -> Option<u64> {
        match self.lookup_child_matching(parent, name_hash, name, |_, _| true) {
            ChildLookup::Counted(ino) => Some(ino),
            ChildLookup::Mismatch(_) | ChildLookup::Missing => None,
        }
    }

    /// [`Self::lookup_child`] that counts only when `matches` accepts the entry, run under the
    /// entry lock so it may also refresh the entry's mutable fields, and told whether the entry
    /// is the only inode the pass's copy indexes under the name's hash, colliding names and
    /// entries gone since the copy included: the first live candidate it accepts is counted, and
    /// the first it refuses answers a mismatch only where it accepts none, so a stale inode ahead
    /// of the right one at a name is passed over.
    pub fn lookup_child_matching(
        &self,
        parent: u64,
        name_hash: u64,
        name: &str,
        mut matches: impl FnMut(&mut InodeEntry, bool) -> bool,
    ) -> ChildLookup {
        self.first_in_entry(parent, name_hash, |ino, candidates| {
            let Some(mut entry) = self.by_ino.get_mut(&ino) else {
                return Pick::Skip;
            };
            if !is_child(&entry, parent, name) {
                return Pick::Skip;
            }
            if matches(&mut entry, candidates == 1) {
                entry.nlookup += 1;
                return Pick::Take(ChildLookup::Counted(ino));
            }
            Pick::Keep(ChildLookup::Mismatch(ino))
        })
        .unwrap_or(ChildLookup::Missing)
    }

    /// What `check` first takes of the inodes indexed under `parent` and `name_hash`, each with
    /// the pass's candidate count, else the first it keeps in the same pass, each checked after
    /// the index entry is copied and its lock dropped, per the lock order; `None` once a copy
    /// answers nothing and the entry still holds what it did.
    fn first_in_entry<R>(
        &self,
        parent: u64,
        name_hash: u64,
        mut check: impl FnMut(u64, usize) -> Pick<R>,
    ) -> Option<R> {
        let mut seen = self.candidates(parent, name_hash);
        #[cfg(feature = "test-util")]
        if let Some(hook) = self.candidates_read.get() {
            hook(self);
        }
        loop {
            let mut kept = None;
            for &ino in &seen {
                match check(ino, seen.len()) {
                    Pick::Take(found) => return Some(found),
                    Pick::Keep(found) => {
                        kept.get_or_insert(found);
                    }
                    Pick::Skip => {}
                }
            }
            if kept.is_some() {
                return kept;
            }
            // A RENAME over the name may have swapped the entry since the copy, its mover
            // unchecked. Unbounded: missing early is the fault. An entry back at the set checked
            // (a revive, a FORGET and re-insert, a rename away and back) passed through a moment
            // with no live inode at the name, so answering absent stays linearizable.
            let now = self.candidates(parent, name_hash);
            if now == seen {
                return None;
            }
            seen = now;
        }
    }

    /// The inode for `child` with one more lookup, and what `read` takes from its entry: an
    /// existing entry with the same identity (reviving a file orphan of the same content, whose
    /// pages stay valid), else a new one under the derived number, rehashed past entries of
    /// other identities or content.
    pub fn insert_child<R>(
        &self,
        child: NewChild<'_>,
        read: impl FnOnce(&InodeEntry) -> R,
    ) -> (u64, R) {
        match self.insert_child_if(child, || true, read) {
            Some(inserted) => inserted,
            None => unreachable!("an insertion admitted unconditionally"),
        }
    }

    /// [`Self::insert_child`] that creates a new entry, revives an orphan or counts a live entry
    /// only if `admit` agrees: for a new or revived entry both before it is indexed by name and
    /// after, under the shard lock the insertion holds, so a change `admit` sees after the entry
    /// is found by name is undone before any other lookup can count it; for a live entry once,
    /// under its entry lock, before it is counted. `None` where it refused.
    pub fn insert_child_if<R>(
        &self,
        child: NewChild<'_>,
        admit: impl Fn() -> bool,
        read: impl FnOnce(&InodeEntry) -> R,
    ) -> Option<(u64, R)> {
        let NewChild {
            parent,
            name,
            name_hash,
            kind,
            mtime,
            node,
        } = child;
        let identity = Identity {
            parent_ino: parent,
            name,
            name_hash,
            kind: kind.identity(),
        };
        let mut n = derive_inode_number(&identity);
        let mut round = 0u32;
        let vacant = loop {
            // An existing entry through a plain lookup: the entry API reserves room first, and
            // grows a full shard even for a key it then finds.
            if let Some(mut entry) = self.by_ino.get_mut(&n) {
                match self.reuse(&mut entry, &identity, (&kind, node), &admit) {
                    Reuse::Counted => return Some((n, read(&entry))),
                    Reuse::Refused => return None,
                    Reuse::Other => {}
                }
            } else {
                // Holds one `by_ino` shard; takes `children` only inside, per the lock order.
                #[allow(clippy::disallowed_methods)]
                match self.by_ino.entry(n) {
                    Entry::Vacant(vacant) => break vacant,
                    Entry::Occupied(mut occupied) => {
                        match self.reuse(occupied.get_mut(), &identity, (&kind, node), &admit) {
                            Reuse::Counted => return Some((n, read(occupied.get()))),
                            Reuse::Refused => return None,
                            Reuse::Other => {}
                        }
                    }
                }
            }
            loop {
                round += 1;
                n = rehash(n, round);
                if n >= 2 {
                    break;
                }
            }
        };
        if !admit() {
            return None;
        }
        let mut held = vacant.insert_entry(InodeEntry {
            ino: n,
            parent,
            name_hash,
            name: Name::from_slice(name.as_bytes()),
            kind,
            nlookup: 1,
            mtime,
            orphaned: false,
            read_cursor: AtomicU64::new(0),
            readahead_started: AtomicBool::new(false),
            access_recorded: AtomicBool::new(false),
            write_generation: AtomicU64::new(0),
            incarnation: 0,
            host_epoch: 0,
            renames: AtomicU32::new(0),
            rename_changes: AtomicU64::new(0),
            host_file: 0,
            node,
        });
        self.index_child(parent, name_hash, n);
        if !admit() {
            self.unindex_child(parent, name_hash, n);
            held.remove();
            return None;
        }
        // Taken last under the shard lock, once the entry is found by name: a reader that missed
        // the entry by name read the count before this.
        held.get_mut().incarnation = self.incarnations.fetch_add(1, Ordering::SeqCst);
        let read = read(held.get());
        Some((n, read))
    }

    /// Detaches `ino` from its path; it lives on until its lookups are
    /// forgotten.
    pub fn orphan(&self, ino: u64) {
        if ino == ROOT_INO {
            return;
        }
        if let Some(mut entry) = self.by_ino.get_mut(&ino)
            && !entry.orphaned
        {
            entry.orphaned = true;
            self.unindex_child(entry.parent, entry.name_hash, ino);
        }
    }

    /// Moves live `ino` to `name` in `parent`, as a RENAME does; the number stays. Where the name
    /// held `replacing`, the name's index entry swaps it for `ino` under one lock, so a lookup by
    /// name never finds the name without a live inode, and `replacing` is orphaned after.
    pub fn move_child(
        &self,
        ino: u64,
        replacing: Option<u64>,
        parent: u64,
        name: &str,
        name_hash: u64,
    ) {
        // The root has no name to leave; an inode replacing itself is a respelling.
        if ino == ROOT_INO {
            return;
        }
        let replacing = replacing.filter(|replaced| *replaced != ino);
        if let Some(mut entry) = self.by_ino.get_mut(&ino)
            && !entry.orphaned
        {
            self.unindex_child(entry.parent, entry.name_hash, ino);
            entry.parent = parent;
            entry.name_hash = name_hash;
            entry.name = Name::from_slice(name.as_bytes());
            // Inner lock of the `by_ino` -> `children` order.
            #[allow(clippy::disallowed_methods)]
            let mut inos = self.children.entry((parent, name_hash)).or_default();
            if let Some(replaced) = replacing {
                inos.retain(|candidate| *candidate != replaced);
            }
            if !inos.contains(&ino) {
                inos.push(ino);
            }
        }
        if let Some(replaced) = replacing {
            self.orphan(replaced);
        }
    }

    /// Counts `n` more lookups on an existing inode; false when it is gone.
    pub fn add_lookups(&self, ino: u64, n: u64) -> bool {
        self.by_ino.get_mut(&ino).is_some_and(|mut entry| {
            entry.nlookup += n;
            true
        })
    }

    /// Drops `n` lookups; at zero the entry and its child-index key go,
    /// under the entry lock so a racing lookup cannot be lost. True when the entry went.
    pub fn forget(&self, ino: u64, n: u64) -> bool {
        self.forget_with(ino, n, || ()).is_some()
    }

    /// [`Self::forget`], running `gone` as the entry goes, under its shard lock, so no inode
    /// under the number since can meet what `gone` clears; its answer, where the entry went.
    pub fn forget_with<T>(&self, ino: u64, n: u64, gone: impl FnOnce() -> T) -> Option<T> {
        if ino == ROOT_INO {
            if let Some(mut root) = self.by_ino.get_mut(&ino) {
                root.nlookup = root.nlookup.saturating_sub(n);
            }
            return None;
        }
        let mut cleared = None;
        // Holds one `by_ino` shard; takes `children` only inside, per the lock order. Unlike the
        // entry API, it finds the key without first reserving room.
        self.by_ino
            .remove_if_mut(&ino, |_, entry| {
                entry.nlookup = entry.nlookup.saturating_sub(n);
                if entry.nlookup != 0 {
                    return false;
                }
                if !entry.orphaned {
                    self.unindex_child(entry.parent, entry.name_hash, ino);
                }
                if entry.kind.is_local_file() && entry.write_generation.load(Ordering::Acquire) != 0
                {
                    let drawn = self.incarnations.fetch_add(1, Ordering::SeqCst);
                    self.written_forgets[slot(entry.node)].fetch_max(drawn, Ordering::SeqCst);
                }
                cleared = Some(gone());
                true
            })
            .and(cleared)
    }

    pub fn forget_batch(&self, items: &[ForgetOne]) {
        for item in items {
            self.forget(item.nodeid, item.nlookup);
        }
    }

    /// Undoes lookups whose reply never reached the kernel. True when the entry went.
    pub fn rollback(&self, ino: u64, n: u64) -> bool {
        self.forget(ino, n)
    }

    pub fn len(&self) -> usize {
        self.by_ino.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_ino.is_empty()
    }

    /// Appends every inode number not orphaned, root included.
    pub fn live(&self, out: &mut Vec<u64>) {
        out.extend(
            self.by_ino
                .iter()
                .filter(|entry| !entry.orphaned)
                .map(|entry| entry.ino),
        );
    }

    /// Every non-root inode still holding lookups; for teardown checks.
    pub fn check_all_zero(&self) -> Result<(), Vec<u64>> {
        let outstanding: Vec<u64> = self
            .by_ino
            .iter()
            .filter(|entry| entry.ino != ROOT_INO && entry.nlookup > 0)
            .map(|entry| entry.ino)
            .collect();
        if outstanding.is_empty() {
            Ok(())
        } else {
            Err(outstanding)
        }
    }

    /// Copies the key's inode numbers out so no `children` ref is held while
    /// `by_ino` is consulted.
    fn candidates(&self, parent: u64, name_hash: u64) -> SmallVec<[u64; 4]> {
        self.children
            .get(&(parent, name_hash))
            .map(|inos| inos.iter().copied().collect())
            .unwrap_or_default()
    }

    fn index_child(&self, parent: u64, name_hash: u64, ino: u64) {
        // Inner lock of the `by_ino` -> `children` order.
        #[allow(clippy::disallowed_methods)]
        let mut inos = self.children.entry((parent, name_hash)).or_default();
        if !inos.contains(&ino) {
            inos.push(ino);
        }
    }

    fn unindex_child(&self, parent: u64, name_hash: u64, ino: u64) {
        // Inner lock of the `by_ino` -> `children` order; found without reserving room.
        self.children
            .remove_if_mut(&(parent, name_hash), |_, inos| {
                inos.retain(|candidate| *candidate != ino);
                inos.is_empty()
            });
    }

    /// One more lookup on `entry` if it is `identity`'s and holds `kind`'s variant, where `admit`
    /// agrees: a live entry takes `node` only then, as a lookup that read the node may have been
    /// overtaken by a RENAME bringing this entry back to the name with another. An orphan is
    /// revived only for the same content, and, like a new entry, only where `admit` agrees both
    /// before it is indexed by name and after; a revival refused after is undone.
    fn reuse(
        &self,
        entry: &mut InodeEntry,
        identity: &Identity<'_>,
        (kind, node): (&InodeKind, NodeID),
        admit: &impl Fn() -> bool,
    ) -> Reuse {
        let reusable = identity_matches(entry, identity)
            && entry.kind.same_variant(kind)
            && (!entry.orphaned || same_content(&entry.kind, kind));
        if !reusable {
            return Reuse::Other;
        }
        if !admit() {
            return Reuse::Refused;
        }
        if !entry.orphaned {
            entry.nlookup += 1;
            entry.node = node;
            return Reuse::Counted;
        }
        entry.orphaned = false;
        entry.nlookup += 1;
        self.index_child(entry.parent, entry.name_hash, entry.ino);
        if !admit() {
            self.unindex_child(entry.parent, entry.name_hash, entry.ino);
            entry.orphaned = true;
            entry.nlookup -= 1;
            return Reuse::Refused;
        }
        entry.node = node;
        Reuse::Counted
    }
}

/// What [`InodeTable::insert_child_if`] made of an existing entry.
enum Reuse {
    /// It is the child's; one more lookup is counted.
    Counted,
    /// It is the child's, which admission refused to count or revive.
    Refused,
    /// It belongs to something else.
    Other,
}

/// A child [`InodeTable::insert_child`] creates.
pub struct NewChild<'a> {
    pub parent: u64,
    pub name: &'a str,
    pub name_hash: u64,
    pub kind: InodeKind,
    /// Milliseconds, as recorded; 0 when nothing was.
    pub mtime: u64,
    /// The node in the filesystem State the child stands for.
    pub node: NodeID,
}

/// The slot of `node` in [`InodeTable::written_forgotten_since`].
fn slot(node: NodeID) -> usize {
    node as usize % FORGOTTEN_WRITE_SLOTS
}

/// Whether two file kinds hold the same stored content; a directory's is never the same, since
/// a directory orphan is someone's working directory at the old path, and neither is a local
/// file's, whose orphan holds a host file its path no longer reaches.
fn same_content(held: &InodeKind, wanted: &InodeKind) -> bool {
    match (held, wanted) {
        (
            InodeKind::File {
                address,
                executable,
                local: false,
                ..
            },
            InodeKind::File {
                address: wanted_address,
                executable: wanted_executable,
                local: false,
                ..
            },
        ) => address == wanted_address && executable == wanted_executable,
        _ => false,
    }
}

fn is_child(entry: &InodeEntry, parent: u64, name: &str) -> bool {
    !entry.orphaned
        && entry.parent == parent
        && std::str::from_utf8(&entry.name).is_ok_and(|own| fold_eq(own, name))
}
