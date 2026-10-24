// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Cross-process exclusion for one on-disk store directory, and the epoch that
//! says whether another process changed it while this one was not holding it.
//!
//! # What the flock means
//!
//! **It says which process is using the store.** It is the lock *between* processes,
//! and nothing else: concurrency between tasks inside one process is the store's own
//! mutexes — the per-bucket `RwLock`, `flush_lock`, `serialize_lock` — and never
//! this.
//!
//! So it is held for as long as this process is using the store, and a *use* is a
//! span, not a call: a `lore_storage_open` handle from open to close, or one
//! repository command. Simultaneous uses in one process are counted, and the flock
//! is taken by the first and released by the last. An operation inside a span takes
//! nothing: it reads the word the span's start published — the flock is held, this
//! store adopted the current epoch, and for a write the epoch has been advanced — and
//! goes on. Only an operation outside any span claims for itself, which is how a test
//! drives a store; every production path opens a span first. Work spawned to outlive the
//! operation that started it is outside the span once the span ends, so the operation joins
//! the span's claim for it before spawning it, without waiting, and the work holds that claim
//! until it ends; where the operation cannot join, the work claims the store as its first
//! step. That claim keeps the flock and covers no operation, so once the span has ended each of
//! the work's operations claims for itself, and no operation outside the work runs on a claim
//! that ends with it. Test builds check this: see [`Claim`].
//!
//! A store kept in memory between uses is not in use. Its state is retained memory with
//! no claim attached, which is why the epoch below exists.
//!
//! One exception to the span rule: a store with unflushed modifications keeps the
//! flock past the end of the span, because a process must not sit on changes another
//! process cannot see. Each store answers whether it has any through [`LockedStore`]
//! as the last guard goes, and a flush that leaves nothing flagged releases the flock
//! then.
//!
//! A store holds no file open past the flock. As the flock is released, each store on
//! the directory closes the packfiles it has open, before the release, so another
//! process the lock admits finds no handle of this one on a file it writes or removes.
//! The store reopens them at the next operation that reaches them. Files still in use, by
//! I/O that outlives the operation that issued it, keep the flock until that I/O ends, and
//! keep it past the store that opened them: see [`FilesInUse`].
//!
//! **A process with nothing open blocks nobody.** That is what makes keeping a store
//! between uses worth having: the state stays, the claim does not.
//!
//! **A repository lock is always taken *inside* the store claims it runs under**, so
//! the two cannot form a cycle. There is one order and every path follows it: store
//! claims first, then the repository flock, released in reverse. `RepositoryLock`
//! enforces the release half by owning the claims it was taken under and declaring
//! its own flock first, so Rust's drop order frees the repository before the stores
//! containing it.
//!
//! **A mutable store's flock is held inside the flock of the immutable store it is opened
//! beside**, for the same reason. The mutable store's lock is nested in the immutable
//! store's, see [`StoreLock::new_within`]: taking the mutable flock first takes a hold on
//! the immutable flock, and the hold is released only after the mutable flock. A command
//! claims the immutable store first, but a path can reach the mutable store first — a
//! store opened while the immutable store came from a cache, an operation outside any
//! span — and the two flocks are taken in one order all the same. A process waiting for
//! the immutable flock therefore holds no mutable flock, and whatever keeps the mutable
//! flock past a span, unflushed modifications or files in use, keeps the immutable flock
//! with it.
//!
//! A single order matters because `FSLock`'s wait is unbounded: a cycle is not a slow
//! path, it is two processes that never return. The containment is what makes a
//! long-held claim safe.
//!
//! The exclusion lives *here* rather than in `lore_revision`, so both ways into a
//! store — a repository context and a storage handle — get it, and neither can go
//! around it.
//!
//! # Why an epoch, and when it is checked
//!
//! Keeping the in-memory state after the claim is released is what lets a store be
//! used again without re-reading it, and it is only sound if the state is proven
//! unchanged. **A store must never serve in-memory state that another process modified
//! on disk while this one held no claim.**
//!
//! So the check belongs to the moment the store goes back into use, not to each
//! operation: that transition is the only point at which another process could have
//! written since this one last looked. Only a store whose settings ask it to reuse its
//! state across holds compares epochs there. Any other store reloads at the first claim
//! after a release, and writers advance the epoch whichever a store does.
//!
//! The epoch is a file in the store directory, read and written only under the flock,
//! and it carries **two** values: a nonce naming the store directory's lifetime, and
//! a count within that lifetime. A writer advances the count **before** its first
//! write, so a writer that dies mid-write has already told every other process that
//! its state is worthless — advancing afterwards would leave a half-written store
//! looking untouched. Readers do not advance it, so two readers never invalidate each
//! other.
//!
//! **The nonce is what a counter alone cannot supply.** Counting restarts when a store
//! directory is deleted and recreated, so the first write to the new store would
//! otherwise be indistinguishable from the first write to the store that stood there
//! before, and a process still holding the older state would compare equal and reuse
//! it. A writer that finds no readable epoch mints a fresh nonce, and finding none is
//! exactly the case where a new lifetime begins — so a recreated directory disagrees
//! with every store that observed its predecessor, whatever the counts happen to be.
//!
//! [`StoreGuard::is_stale`] is the answer, and it fails safe: a store that has
//! observed nothing, an epoch that cannot be read, and an epoch different from the
//! one this store last observed all report stale. Only an exact match reports
//! fresh, and only to a store that reuses its state across holds — two identical
//! stamps, or two looks that both found no epoch file, which is a definite answer
//! describing a store nothing has written.
//!
//! # Within a hold
//!
//! While the flock is held nothing outside this process can write, so an operation
//! inside the hold compares no epochs. What can still move is this process's own
//! record of the epoch — another store object on the directory advancing it — and
//! every change to that record starts a new generation. A store that adopted the
//! current generation is current however the epoch reads, and [`PathLock::live`]
//! publishes that generation, with whether the epoch was advanced in this hold, as
//! one word an operation reads with no lock. That is what makes a store over an epoch
//! it cannot read reload once, when its hold begins, rather than on every operation,
//! and what keeps a reload from running underneath work already in the hold.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicU64;
#[cfg(feature = "test-util")]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::fs::lock::FSLock;
use parking_lot::Mutex;

/// File carrying the store's epoch, inside the store directory.
///
/// Sixteen bytes: the nonce then the count, each a little-endian `u64`. Anything of
/// another length is not an epoch this build wrote and reads as
/// [`Epoch::Unreadable`].
#[lore_macro::test_pub]
const EPOCH_FILE: &str = "epoch";

/// Bytes of [`EPOCH_FILE`], and of the two `u64`s halving it.
#[lore_macro::test_pub]
const EPOCH_BYTES: usize = 2 * size_of::<u64>();

/// Where an advance writes the next epoch before renaming it over [`EPOCH_FILE`].
///
/// A rename replaces the directory entry at the epoch path rather than opening what it
/// names, so an advance can neither write through a link into another file nor wait on
/// a FIFO for a reader. The staging file is created exclusively, which follows no link
/// either.
#[lore_macro::test_pub]
const EPOCH_STAGING_FILE: &str = "epoch.new";

/// The generation a store holds before it has adopted any. [`next_generation`] never
/// produces it.
const NEVER_ADOPTED: u64 = u64::MAX;

/// The bits of [`PathLock::live`] that carry the generation; the remaining bit is
/// [`ADVANCED`]. A generation never reaches this value, so it never equals [`NOT_HELD`].
const GENERATION_MASK: u64 = u64::MAX >> 1;

/// Set in [`PathLock::live`] once the epoch has been advanced in the current hold.
const ADVANCED: u64 = !GENERATION_MASK;

/// The value of [`PathLock::live`] while this process holds no flock.
const NOT_HELD: u64 = u64::MAX;

/// How often a release that files in use kept back is tried again.
const RELEASE_RETRY: Duration = Duration::from_millis(1);

/// A store open on a locked directory, as the lock sees it. Asked as the last guard on
/// the directory goes: whether it has buckets flagged for writing, which keeps the
/// flock, since a process must not sit on changes another process cannot see; and if
/// not, to close the files it holds open, before the flock is released.
pub trait LockedStore {
    /// Whether any bucket of this store is flagged as changed and not yet written.
    fn has_unflushed(&self) -> bool;

    /// Closes the files this store holds open on its directory, and reports whether it
    /// closed them all. One still in use keeps the flock, and the release is tried again.
    ///
    /// Called under the directory's state lock, just before the flock is released, so
    /// it must not block, wait for a lock an operation may hold, or call back into the
    /// store lock.
    fn release_files(&self) -> bool;
}

/// Files a store opened on a locked directory, registered with the lock through
/// [`StoreLock::anchor`]. I/O on them can outlive the operation that issued it, and the store
/// too: the I/O driver runs an operation to its end after its future is dropped. While any is in
/// use the flock is kept, and the [`LockAnchor`] the files hold keeps the lock itself alive past
/// the last store on the directory.
pub trait FilesInUse {
    /// Whether I/O on these files is in flight. Called under the directory's state lock, so it
    /// must not block or call back into the store lock.
    fn in_use(&self) -> bool;
}

/// In use while anything holds the lock, as an I/O whose future was dropped does until the driver
/// completes it: a packfile list that a load, write, sync or stop holds, or a group's writes in
/// flight, see [`crate::local::fan_out::GroupWrites`].
impl<T> FilesInUse for tokio::sync::RwLock<T> {
    fn in_use(&self) -> bool {
        self.try_write().is_err()
    }
}

/// Keeps a store directory's lock, and with it a flock this process holds, alive for as long as
/// the files holding it live. See [`FilesInUse`].
pub struct LockAnchor {
    _lock: Arc<PathLock>,
}

/// What a caller intends to do while it holds the store lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Reads only.
    ///
    /// Does not advance the epoch, so concurrent readers in different processes
    /// never invalidate each other's in-memory state.
    Read,
    /// May modify the store on disk.
    ///
    /// Advances the epoch before the caller writes anything. That ordering is the
    /// point: it costs one small write, and it means a process that is killed
    /// mid-write cannot leave another process reusing state that predates it.
    Write,
}

/// How an acquisition that has to take the flock treats another process holding it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Contention {
    /// Waits for it, as an operation that has to run now does.
    Wait,
    /// Answers `None`, as work that can run another time does.
    Skip,
}

/// The flock this process holds on a store directory, with the hold it took on the flock its lock
/// is nested in, see [`StoreLock::new_within`]. Declared in that order, so the flock is released
/// before the hold.
struct Flock {
    /// `None` once a release has unlocked it under the directory's state lock, see
    /// [`PathLock::release_unless_in_use`].
    lock: Option<FSLock>,
    _enclosing: Option<StoreGuard>,
}

/// Everything about the lock that has to move together.
///
/// Behind a `parking_lot::Mutex` rather than an async one because it is held for
/// a few field updates and never across an `.await` — the flock acquisition
/// itself happens outside it, serialised by [`PathLock::gate`].
#[lore_macro::test_pub]
struct LockState {
    /// The flock, while it is held. `None` means this process holds nothing.
    ///
    /// Closed when released, so the next acquisition opens whatever lock file the path
    /// names then. A descriptor kept across releases would go on locking the file it
    /// opened, which a store directory deleted and recreated no longer contains — while
    /// another process locks the new one, and neither excludes the other. An epoch write in
    /// flight holds the flock too, until it has landed, see [`PathLock::write_epoch`].
    held: Option<Arc<Flock>>,
    /// Guards in flight, across every store object sharing this directory: the spans,
    /// and the operations that run outside any span.
    active: usize,
    /// The guards among [`Self::active`] that are spans. Only a span covers the operations of
    /// the stores on the directory, see [`StoreLock::covers`]: an operation's own guard keeps
    /// the flock for that operation alone, and can go before another operation ends.
    spans: usize,
    /// The stores on this directory, asked whether anything is unflushed as the last
    /// guard goes, and to close their files as the flock is released. `Weak`, so a
    /// store that is gone counts as clean: its state went with it. Dead entries are
    /// swept when a store registers.
    probes: Vec<Weak<dyn LockedStore + Send + Sync>>,
    /// The files stores on this directory opened, asked whether they are in use as the flock
    /// would go. `Weak`, so files no longer held count as idle.
    files: Vec<Weak<dyn FilesInUse + Send + Sync>>,
    /// Whether a task is trying again a release that files in use kept back.
    retrying: bool,
    /// Whether the epoch was already advanced during the current hold.
    ///
    /// One advance per hold is enough — the point is to mark the store as touched
    /// by this process, not to count writes — and it keeps a burst of writes from
    /// paying a file write each.
    advanced: bool,
    /// The epoch on disk, as this process last established it.
    ///
    /// Authoritative while the flock is held, because nothing outside this process
    /// can write it then — which is what lets a joining operation decide staleness
    /// without touching the filesystem. Set only through [`Self::set_current`].
    current: Epoch,
    /// Which setting of [`Self::current`] this is, so a store can tell whether the
    /// record it adopted is still the record without comparing epochs — see
    /// [`StoreLock::is_current`]. Never [`NEVER_ADOPTED`].
    generation: u64,
}

impl LockState {
    /// Records `epoch` as the directory's current epoch, as a new generation.
    ///
    /// Every setting starts one, including a setting to an equal epoch: the flock may
    /// have been released and taken again in between, and a store that adopted an
    /// unreadable epoch before that must not match what was read after it.
    fn set_current(&mut self, epoch: Epoch) {
        self.current = epoch;
        self.generation = next_generation(self.generation);
    }

    /// Counts a guard in.
    fn count_in(&mut self) {
        self.active = self.active.saturating_add(1);
    }

    /// Whether any store on this directory has a bucket flagged and not yet written.
    fn has_unflushed(&self) -> bool {
        self.probes
            .iter()
            .filter_map(Weak::upgrade)
            .any(|store| store.has_unflushed())
    }

    /// Whether files any store on this directory opened are in use, a store dropped since
    /// included, see [`FilesInUse`].
    fn files_in_use(&self) -> bool {
        self.files
            .iter()
            .filter_map(Weak::upgrade)
            .any(|files| files.in_use())
    }

    /// Has every store on this directory close the files it holds open, unless files any
    /// store opened are still in use, and reports whether all are closed.
    fn release_files(&self) -> bool {
        if self.files_in_use() {
            return false;
        }
        self.probes
            .iter()
            .filter_map(Weak::upgrade)
            .all(|store| store.release_files())
    }

    /// Publishes what an operation needs to know into `lock`'s [`PathLock::live`]: nothing
    /// while no span is active, else the generation and whether the epoch was advanced in
    /// this hold. Test builds publish the same into [`PathLock::held_live`] whether or not a
    /// span is active.
    ///
    /// A flock kept by operations' own guards, or past the last guard for unflushed
    /// buckets, is not published: it is held, but no span covers it, so an operation then
    /// claims for itself. Called at the end of every section that changes [`Self::held`],
    /// [`Self::spans`] between zero and one, [`Self::advanced`] or [`Self::generation`],
    /// under the state lock, so the word never runs ahead of the state it describes.
    fn publish(&self, lock: &PathLock) {
        let held = if self.held.is_none() {
            NOT_HELD
        } else if self.advanced {
            self.generation | ADVANCED
        } else {
            self.generation
        };
        let live = if self.spans == 0 { NOT_HELD } else { held };
        lock.live.store(live, Ordering::Release);
        #[cfg(feature = "test-util")]
        lock.held_live.store(held, Ordering::Release);
    }
}

/// The flock and epoch state for one store *directory*, shared by every store
/// object in this process that is open on it.
///
/// # Why this is not per store object
///
/// `flock` excludes by open file description, not by process, so two descriptions
/// on one file exclude **each other inside a single process** — verified: the
/// second `flock(LOCK_EX|LOCK_NB)` returns `EWOULDBLOCK`, and
/// [`FSLock::acquire_directory_lock`] retries that without a timeout. Any code that
/// drops one store and opens another on the same directory while a guard is alive
/// would therefore hang against itself, forever. Sharing the flock makes the second
/// open join the first instead.
#[lore_macro::test_pub]
struct PathLock {
    /// The epoch file inside the store directory, resolved once, like the lock file.
    epoch_path: PathBuf,
    /// [`EPOCH_STAGING_FILE`] inside the store directory, resolved once.
    epoch_staging_path: PathBuf,
    /// The lock file inside the store directory, resolved once.
    ///
    /// `FSLock::acquire_directory_lock` canonicalizes on every call, which is one
    /// `lstat` per path component for a path that cannot change — measured at 8.6 µs
    /// at depth 7 and growing with depth, on a path taken at the start of every span.
    lock_path: PathBuf,
    /// A lock holding no store state on the directory whose lock this one is nested in, through
    /// which taking this directory's flock first holds that directory's: see
    /// [`StoreLock::new_within`]. Set by the first lock made with it. A flock taken before then,
    /// through a lock made with [`StoreLock::new`], holds nothing of the enclosing directory,
    /// which only tests do.
    enclosing: std::sync::OnceLock<Arc<StoreLock>>,
    /// The bookkeeping shared by every store on this directory.
    state: Mutex<LockState>,
    /// What [`LockState`] last published for operations inside a hold: [`NOT_HELD`]
    /// while no span is active, else the generation, with [`ADVANCED`] set once the
    /// epoch moved in this hold.
    ///
    /// Read with no lock by [`StoreLock::covers`] on every operation, so the hot paths
    /// touch nothing that contends. Written only under [`Self::state`].
    live: AtomicU64,
    /// [`Self::live`] as it reads while a span is active, whether or not one is: [`NOT_HELD`]
    /// only while the flock is not held. What [`StoreLock::still_covers`] reads. Test builds
    /// only.
    #[cfg(feature = "test-util")]
    held_live: AtomicU64,
    /// Serialises the transitions that touch the filesystem — taking the flock,
    /// reading the epoch, advancing it — and is held across a caller's reload, so
    /// only one task per process does any of them. An advance keeps it until the epoch
    /// it wrote is recorded, whether or not the acquisition that began it is still
    /// awaited: see [`StoreLock::record_advance`].
    gate: Arc<tokio::sync::Mutex<()>>,
    /// Operations running with no guard of their own because a span covers them, each
    /// counted by a [`Claim::Covered`]. Zero whenever the flock is released: see
    /// [`Self::assert_uncovered`].
    #[cfg(feature = "test-util")]
    covered: AtomicUsize,
    /// Loads of the stores on this directory: see [`StoreLock::loads`]. Test builds only.
    #[cfg(feature = "test-util")]
    loads: std::sync::atomic::AtomicUsize,
}

/// Every directory this process holds a lock for, so that one directory has one
/// flock however many stores are open on it.
///
/// `Weak`, so the entry goes when the last store on that directory does.
static PATH_LOCKS: std::sync::OnceLock<Mutex<HashMap<PathBuf, std::sync::Weak<PathLock>>>> =
    std::sync::OnceLock::new();

fn path_locks() -> &'static Mutex<HashMap<PathBuf, std::sync::Weak<PathLock>>> {
    PATH_LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One store object's view of its directory's lock.
///
/// The flock, the in-flight count and the unflushed probes are shared per directory
/// ([`PathLock`]); what this store has adopted is not, because it describes *this
/// store's* in-memory state. Two stores open on one directory therefore never block
/// each other, and neither can serve state the other invalidated.
#[lore_macro::test_pub]
pub struct StoreLock {
    /// The directory's flock and epoch, shared with every other store on it.
    shared: Arc<PathLock>,
    /// The epoch this store's in-memory state was built against.
    ///
    /// `None` until this store has established one, and `None` never matches, so a
    /// store that has observed nothing can never claim its state is current.
    observed: Mutex<Option<Epoch>>,
    /// The generation of [`LockState::current`] this store adopted, or
    /// [`NEVER_ADOPTED`].
    ///
    /// Written only under the directory's state lock. Read there by [`Self::is_current`],
    /// and with no lock at all by [`Self::covers`] and [`Self::still_covers`], which is why
    /// it is an atomic beside that mutex rather than a field inside it.
    observed_generation: AtomicU64,
    /// Whether this store reuses its in-memory state across holds of the flock when the
    /// epoch it observed is unchanged. Without it, the store reloads at the first claim
    /// after any release.
    reuse_state_across_holds: bool,
    /// Whether this lock belongs to a store with in-memory state to reload. The lock through
    /// which a nested lock holds this one's flock has none, and is never stale.
    holds_state: bool,
}

/// Whether state built against `observed` can still be used, given `current`.
///
/// Fails safe. A store that has established nothing is stale; so is one whose last
/// look could not be read, and so is any disagreement. Only two definite answers
/// that match report fresh.
fn is_stale(observed: Option<Epoch>, current: Epoch) -> bool {
    match observed {
        Some(Epoch::At(mine)) => !matches!(current, Epoch::At(theirs) if mine == theirs),
        Some(Epoch::Absent) => !matches!(current, Epoch::Absent),
        Some(Epoch::Unreadable) | None => true,
    }
}

/// The generation after `generation`: below [`GENERATION_MASK`], so it fits beside
/// [`ADVANCED`] in [`PathLock::live`] and never reads as [`NEVER_ADOPTED`] or
/// [`NOT_HELD`].
#[lore_macro::test_pub]
fn next_generation(generation: u64) -> u64 {
    match generation.wrapping_add(1) & GENERATION_MASK {
        GENERATION_MASK => 0,
        next => next,
    }
}

impl StoreLock {
    /// A lock for the store directory at `path`, for a store that reuses its in-memory
    /// state across holds as `reuse_state_across_holds` says.
    ///
    /// The directory must exist: the path is canonicalized so that two spellings of
    /// one directory share one flock, and canonicalizing requires it.
    ///
    /// # Errors
    ///
    /// [`std::io::Error`] if `path` cannot be canonicalized.
    pub fn new(
        path: impl AsRef<Path>,
        reuse_state_across_holds: bool,
    ) -> std::io::Result<Arc<Self>> {
        let key = path.as_ref().canonicalize()?;
        let shared = {
            let mut locks = path_locks().lock();
            if let Some(existing) = locks.get(&key).and_then(std::sync::Weak::upgrade) {
                existing
            } else {
                // Dead entries are swept here rather than on drop, which would
                // need the key on a path that must stay allocation-free.
                locks.retain(|_, held| held.strong_count() > 0);
                let created = Arc::new(PathLock {
                    lock_path: key.join("lock"),
                    epoch_path: key.join(EPOCH_FILE),
                    epoch_staging_path: key.join(EPOCH_STAGING_FILE),
                    enclosing: std::sync::OnceLock::new(),
                    state: Mutex::new(LockState {
                        held: None,
                        active: 0,
                        spans: 0,
                        probes: Vec::new(),
                        files: Vec::new(),
                        retrying: false,
                        advanced: false,
                        current: Epoch::Absent,
                        generation: 0,
                    }),
                    live: AtomicU64::new(NOT_HELD),
                    #[cfg(feature = "test-util")]
                    held_live: AtomicU64::new(NOT_HELD),
                    gate: Arc::new(tokio::sync::Mutex::new(())),
                    #[cfg(feature = "test-util")]
                    covered: AtomicUsize::new(0),
                    #[cfg(feature = "test-util")]
                    loads: std::sync::atomic::AtomicUsize::new(0),
                });
                locks.insert(key, Arc::downgrade(&created));
                created
            }
        };
        Ok(Arc::new(Self {
            shared,
            observed: Mutex::new(None),
            observed_generation: AtomicU64::new(NEVER_ADOPTED),
            reuse_state_across_holds,
            holds_state: true,
        }))
    }

    /// As [`Self::new`], with the lock nested in `enclosing`: taking this directory's flock first
    /// takes a hold on `enclosing`'s, released only after this one, so the two flocks are taken in
    /// one order whichever lock a path reaches first. Every lock on a directory is nested in the
    /// same one.
    ///
    /// # Errors
    ///
    /// As [`Self::new`].
    pub fn new_within(
        path: impl AsRef<Path>,
        reuse_state_across_holds: bool,
        enclosing: &Self,
    ) -> std::io::Result<Arc<Self>> {
        let lock = Self::new(path, reuse_state_across_holds)?;
        let nested_in = lock.shared.enclosing.get_or_init(|| {
            Arc::new(Self {
                shared: Arc::clone(&enclosing.shared),
                observed: Mutex::new(None),
                observed_generation: AtomicU64::new(NEVER_ADOPTED),
                reuse_state_across_holds: false,
                holds_state: false,
            })
        });
        debug_assert!(
            Arc::ptr_eq(&nested_in.shared, &enclosing.shared),
            "every lock on a directory is nested in the same one"
        );
        Ok(lock)
    }

    /// Takes the lock for one operation, reporting whether the in-memory state
    /// built against the last acquisition can still be trusted.
    ///
    /// Cheap when this process already holds the flock and this store is current: a
    /// counter increment, with no filesystem work. Everything else goes through the
    /// gate.
    ///
    /// **A guard reporting [`StoreGuard::is_stale`] holds the gate** until the caller
    /// either calls [`StoreGuard::note_refreshed`] or drops it. This store has adopted
    /// nothing in the meantime, so every other acquisition on it goes to the gate and
    /// waits there, which is what keeps an operation from running against state being
    /// dropped underneath it.
    ///
    /// # Errors
    ///
    /// [`std::io::Error`] if the flock cannot be taken, or if a writer cannot
    /// advance the epoch — which fails the acquisition rather than proceeding,
    /// because a write nobody else can detect is the one thing this must not
    /// allow.
    pub async fn acquire(self: &Arc<Self>, intent: Intent) -> std::io::Result<StoreGuard> {
        if let Some(guard) = self.try_join(intent) {
            return Ok(guard);
        }
        // Boxed: the gate path is a small share of acquisitions, and carried inline its
        // state — the gate wait, the flock wait, the epoch I/O — would be part of the future
        // of every operation that claims the store. An acquisition that waits never answers
        // `None`; mapped rather than asserted.
        Box::pin(async move {
            let gate = Arc::clone(&self.shared.gate).lock_owned().await;
            self.acquire_at_gate(gate, intent, Contention::Wait).await
        })
        .await?
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::WouldBlock))
    }

    /// Takes the lock for work that can be put off, answering `None` rather than waiting
    /// for anything else that has the store in a transition.
    ///
    /// `None` when another process holds the flock, or when another task in this process
    /// holds the gate — taking the flock, reloading, or advancing the epoch. Either can
    /// last without bound, and work that can run another time has no reason to queue
    /// behind it, keeping whatever it holds for as long as it waits.
    ///
    /// # Errors
    ///
    /// As [`Self::acquire`], contention aside.
    pub async fn try_acquire(
        self: &Arc<Self>,
        intent: Intent,
    ) -> std::io::Result<Option<StoreGuard>> {
        if let Some(guard) = self.try_join(intent) {
            return Ok(Some(guard));
        }
        let Ok(gate) = Arc::clone(&self.shared.gate).try_lock_owned() else {
            return Ok(None);
        };
        // Boxed, for the reason `acquire` gives.
        Box::pin(self.acquire_at_gate(gate, intent, Contention::Skip)).await
    }

    /// The part of an acquisition that holds the gate: joining a flock another task took
    /// while this one waited, or taking the flock and reading the epoch. A flock taken here
    /// is taken after a hold on the flock its lock is nested in, which it keeps, so the two
    /// are taken in one order whichever lock a path reaches first.
    ///
    /// `None` only under [`Contention::Skip`], when another process holds the flock or the
    /// one its lock is nested in.
    ///
    /// # Errors
    ///
    /// As [`Self::acquire`].
    async fn acquire_at_gate(
        self: &Arc<Self>,
        gate: tokio::sync::OwnedMutexGuard<()>,
        intent: Intent,
        contention: Contention,
    ) -> std::io::Result<Option<StoreGuard>> {
        // Re-checked under the gate: another task may have taken the flock, advanced
        // the epoch, or finished a refresh while this one waited.
        if let Some(guard) = self.try_join(intent) {
            return Ok(Some(guard));
        }

        // **Observed and counted in under one lock.** `release_if_idle` fires only at
        // `active == 0`, so counting this operation in before the lock is released is
        // what stops the flock going away underneath the rest of this function. Read
        // the flock in one critical section and increment in another and the last
        // outstanding guard can drop in between — leaving this one to write the epoch,
        // and its caller to run a whole span, holding nothing. The gate does not help:
        // it serialises acquirers, and a guard is released from `Drop`, which is
        // synchronous and never touches it.
        //
        // **Staleness is read here, before the advance below, never after.** A store is
        // not made stale by its own write: it is judged against what the epoch was when
        // it last agreed with disk, and advancing is how it announces the write it is
        // about to make.
        let joined = {
            let mut state = self.shared.state.lock();
            state.held.is_some().then(|| {
                let stale = !self.is_current(&state);
                let advance = intent == Intent::Write && !state.advanced;
                state.count_in();
                (stale, state.held.clone().filter(|_| advance), state.current)
            })
        };
        if let Some((stale, advance, known)) = joined {
            // Built before the advance below, so the count it owns is released by its
            // `Drop` if the advance fails, rather than leaked by an early return.
            let mut guard = self.counted_guard();
            // A stale guard's caller reloads, sizing the packfiles from disk, and a write
            // that a store object dropped before this one left in flight would land past
            // the end it reads, at the offset its next write reserves. Files in use keep
            // the flock, so a flock taken afresh below has none to wait for.
            if stale && self.shared.state.lock().files_in_use() {
                // Boxed: files in use are rare, and carried inline the wait would be part of
                // every acquisition's future.
                Box::pin(self.shared.await_files_idle()).await;
            }
            let gate = match advance {
                // The current epoch is authoritative while the flock is held, so this
                // needs no read of its own — nothing outside this process can have
                // written it since.
                Some(held) => self.advance(next_epoch(known), held, gate, stale).await?,
                None => gate,
            };
            if stale {
                guard.keep_gate(gate);
            }
            return Ok(Some(guard));
        }

        let enclosing = match (self.shared.enclosing.get(), contention) {
            (None, _) => None,
            (Some(enclosing), Contention::Wait) => Some(enclosing.acquire(Intent::Read).await?),
            (Some(enclosing), Contention::Skip) => {
                match enclosing.try_acquire(Intent::Read).await? {
                    Some(guard) => Some(guard),
                    None => return Ok(None),
                }
            }
        };
        let Some((lock, recreated)) = self.shared.take_flock(contention).await? else {
            return Ok(None);
        };
        let held = Arc::new(Flock {
            lock: Some(lock),
            _enclosing: enclosing,
        });
        let on_disk = read_epoch(&self.shared.epoch_path).await;
        // Against what was on disk when the flock was taken, before this store's own
        // advance below. Another process may have written while the flock was not held,
        // so a generation decides nothing here.
        let stale = (recreated && self.holds_state) || self.is_stale_against(on_disk);
        let advance = intent == Intent::Write || recreated;
        let gate = if advance {
            self.advance(next_epoch(on_disk), Arc::clone(&held), gate, stale)
                .await?
        } else {
            gate
        };
        // A hold that did not advance records the epoch it read, and this store's adoption of
        // it, as it counts itself in; an advance recorded both as it landed.
        {
            let mut state = self.shared.state.lock();
            state.held = Some(held);
            state.advanced = advance;
            if !advance {
                state.set_current(on_disk);
                if !stale {
                    self.adopt(&state);
                }
            }
            state.count_in();
            state.publish(&self.shared);
        }
        let mut guard = self.counted_guard();
        if stale {
            guard.keep_gate(gate);
        }
        Ok(Some(guard))
    }

    /// Advances the epoch to `stamp` in a task of its own, see [`Self::record_advance`], and
    /// waits for it. Hands `gate` back once the advance is recorded.
    ///
    /// # Errors
    ///
    /// As [`PathLock::write_epoch`], or if the task does not complete.
    async fn advance(
        self: &Arc<Self>,
        stamp: Stamp,
        held: Arc<Flock>,
        gate: tokio::sync::OwnedMutexGuard<()>,
        stale: bool,
    ) -> std::io::Result<tokio::sync::OwnedMutexGuard<()>> {
        lore_base::lore_spawn!(Arc::clone(self).record_advance(stamp, held, gate, stale))
            .await
            .map_err(std::io::Error::other)?
    }

    /// Writes `stamp` as the epoch and records it as the directory's current one, with this
    /// store's adoption of it unless it is `stale` and reloads first, and `advanced` as whether
    /// the directory's state holds the flock. On the joined path it does. The path that takes
    /// the flock stores the flock and `advanced` itself once the advance returns, so an
    /// acquisition dropped there leaves the next hold to begin unadvanced. Holds `gate`
    /// throughout and hands it back.
    ///
    /// A task of its own rather than part of the acquisition's future: the write runs to its
    /// end once dispatched, so an acquisition dropped at the await would leave the epoch
    /// advanced on disk with nothing in this process recording it. The store would then
    /// reload at its next claim after a release, for a write of its own, and a writer
    /// passing the gate the dropped acquisition let go would advance alongside the write in
    /// flight, the two contending for the one staging file.
    ///
    /// # Errors
    ///
    /// As [`PathLock::write_epoch`].
    async fn record_advance(
        self: Arc<Self>,
        stamp: Stamp,
        held: Arc<Flock>,
        gate: tokio::sync::OwnedMutexGuard<()>,
        stale: bool,
    ) -> std::io::Result<tokio::sync::OwnedMutexGuard<()>> {
        let stamp = self.shared.write_epoch(stamp, held).await?;
        #[cfg(feature = "test-util")]
        crate::test_util::pause_point(crate::test_util::PausePoint::Advanced).await;
        let mut state = self.shared.state.lock();
        state.advanced = state.held.is_some();
        state.set_current(Epoch::At(stamp));
        if !stale {
            self.adopt(&state);
        }
        state.publish(&self.shared);
        Ok(gate)
    }

    /// Builds the guard for an operation this call has **already counted in**, with
    /// [`LockState::count_in`]: a fresh one, until [`StoreGuard::keep_gate`] makes it stale.
    fn counted_guard(self: &Arc<Self>) -> StoreGuard {
        // The invariant the counting exists to preserve, checked where it is supposed
        // to hold. A guard handed out over an unheld flock is invisible from outside —
        // by the time a caller could look, another acquisition has usually taken the
        // flock again — so this is the only place the violation is observable at all.
        debug_assert!(
            self.shared.state.lock().held.is_some(),
            "a guard must cover a flock this process holds"
        );
        StoreGuard {
            lock: Arc::clone(self),
            stale: false,
            span: false,
            gate: None,
        }
    }

    /// Whether an operation with `intent` may run with no guard of its own: a span of this
    /// process is active on the flock, this store adopted the epoch the hold established,
    /// and for a write the epoch has been advanced in this hold.
    ///
    /// Two relaxed loads and no lock, which is what every store operation pays inside a
    /// span. The span's guard is what keeps the flock; this only reports that it is there.
    /// An operation that gets `false` claims for itself through [`Self::acquire`].
    #[must_use]
    pub fn covers(&self, intent: Intent) -> bool {
        let live = self.shared.live.load(Ordering::Acquire);
        if live == NOT_HELD
            || live & GENERATION_MASK != self.observed_generation.load(Ordering::Relaxed)
        {
            return false;
        }
        intent == Intent::Read || live & ADVANCED != 0
    }

    /// The claim of an operation with `intent` that [`Self::covers`] covers: nothing, or in
    /// test builds a claim that counts the operation on the directory until it ends.
    ///
    /// A test build counts the operation, then fails it if the flock was released between the
    /// check and the count: the release read the count before the operation counted itself,
    /// so did not see it. The release publishes the flock as not held before it reads the
    /// count, and a fence on each side orders the two, so one of them sees the other. A span
    /// that ended while a guard keeps the flock, as one of work spawned to outlive an
    /// operation does, fails nothing. The re-check is [`Self::still_covers`].
    #[inline]
    #[must_use]
    pub fn covered(&self, intent: Intent) -> Option<Claim> {
        #[cfg(feature = "test-util")]
        {
            self.shared.covered.fetch_add(1, Ordering::Relaxed);
            let claim = Claim::Covered(Covered {
                shared: Arc::clone(&self.shared),
            });
            std::sync::atomic::fence(Ordering::SeqCst);
            assert!(
                self.still_covers(intent),
                "an operation was counted as covered after the flock it relied on was released"
            );
            Some(claim)
        }
        #[cfg(not(feature = "test-util"))]
        {
            let _ = intent;
            None
        }
    }

    /// Whether the flock a span admitted an operation with `intent` on through [`Self::covers`]
    /// is still held for it: as [`Self::covers`], but over [`PathLock::held_live`], whether or
    /// not a span is still active, and this store may hold the generation after the word's. A
    /// write's advance of the epoch adopts the new generation before [`LockState::publish`]
    /// carries it into the word, see [`Self::acquire_at_gate`]. Each is loaded once: a word
    /// loaded before that publication pairs with either generation, and one loaded after it with
    /// the adoption it follows. Test builds only.
    #[cfg(feature = "test-util")]
    fn still_covers(&self, intent: Intent) -> bool {
        let live = self.shared.held_live.load(Ordering::Acquire);
        if live == NOT_HELD {
            return false;
        }
        let generation = live & GENERATION_MASK;
        let observed = self.observed_generation.load(Ordering::Relaxed);
        (observed == generation || observed == next_generation(generation))
            && (intent == Intent::Read || live & ADVANCED != 0)
    }

    /// Registers the store this lock belongs to, so the directory can ask it whether
    /// anything is unflushed when its last guard goes, and have it close its files as
    /// the flock is released. Registering the same store again does nothing.
    pub fn watch(&self, store: Weak<dyn LockedStore + Send + Sync>) {
        let mut state = self.shared.state.lock();
        state.probes.retain(|probe| probe.strong_count() > 0);
        if !state.probes.iter().any(|probe| Weak::ptr_eq(probe, &store)) {
            state.probes.push(store);
        }
    }

    /// Registers files the store opens on this directory, and returns the anchor they hold.
    /// See [`FilesInUse`].
    pub fn anchor(&self, files: Weak<dyn FilesInUse + Send + Sync>) -> LockAnchor {
        let mut state = self.shared.state.lock();
        state.files.retain(|held| held.strong_count() > 0);
        state.files.push(files);
        LockAnchor {
            _lock: Arc::clone(&self.shared),
        }
    }

    /// Releases the flock if nothing is in flight and no store on the directory has
    /// anything unflushed. For a flush that has just finished, which may be what was
    /// keeping the flock past the last span.
    pub fn release_if_clean(&self) {
        PathLock::release_if_idle(&self.shared, self.shared.state.lock());
    }

    /// Whether this store's in-memory state matches the directory's current epoch, for
    /// an acquisition joining a flock this process already holds.
    ///
    /// A store that adopted the current generation is current without looking at the
    /// epoch: nothing outside this process can write while the flock is held, so only a
    /// new generation can have left it behind. One that adopted an earlier generation is
    /// current only as [`Self::is_stale_against`] answers, and records that by taking the
    /// generation — so a store compares epochs at most once per generation.
    ///
    /// Called under the directory's state lock, which keeps the generation this
    /// records the one it compared against.
    fn is_current(&self, state: &LockState) -> bool {
        if self.observed_generation.load(Ordering::Relaxed) == state.generation {
            return true;
        }
        if self.is_stale_against(state.current) {
            return false;
        }
        self.observed_generation
            .store(state.generation, Ordering::Relaxed);
        true
    }

    /// Whether this store's state, adopted under an earlier generation, must be reloaded
    /// against `current`: always, unless the store reuses its state across holds, and then
    /// as [`is_stale`] answers. Within one hold a new generation comes with a new epoch, so
    /// the setting decides only reuse across a release of the flock. Never for a lock that
    /// holds no store state.
    fn is_stale_against(&self, current: Epoch) -> bool {
        self.holds_state
            && (!self.reuse_state_across_holds || is_stale(*self.observed.lock(), current))
    }

    /// Adopts the directory's current epoch and generation as this store's own.
    ///
    /// Called under the directory's state lock, and before the gate this acquisition
    /// holds is released: an operation on this store that passed the gate first would
    /// find it not yet adopted and reload it underneath the acquisition adopting.
    fn adopt(&self, state: &LockState) {
        *self.observed.lock() = Some(state.current);
        self.observed_generation
            .store(state.generation, Ordering::Relaxed);
    }

    /// Joins a hold this process already has, when doing so needs no filesystem
    /// work and this store's state is current. `None` sends the caller to the gate.
    pub(crate) fn try_join(self: &Arc<Self>, intent: Intent) -> Option<StoreGuard> {
        let mut state = self.shared.state.lock();
        // Nothing held is nothing to join.
        state.held.as_ref()?;
        if intent == Intent::Write && !state.advanced {
            return None;
        }
        // **This is what keeps an operation off a store being reloaded**, as well as
        // catching another store object on this directory advancing the epoch during
        // this very hold. A reload in flight has not adopted anything yet — only
        // `note_refreshed` does that — so every other acquisition on this store is not
        // current, goes to the gate, and waits there behind the guard doing the
        // reloading. A store that *is* current joins with no gate and no waiting,
        // which is what keeps a nested hold from blocking behind an unrelated one.
        if !self.is_current(&state) {
            return None;
        }
        state.count_in();
        Some(StoreGuard {
            lock: Arc::clone(self),
            stale: false,
            span: false,
            gate: None,
        })
    }

    /// Whether this process is holding the flock right now. For tests and
    /// diagnostics; callers should hold a [`StoreGuard`] instead of asking.
    #[must_use]
    pub fn is_held(&self) -> bool {
        self.shared.state.lock().held.is_some()
    }

    /// How many times the stores on this directory have loaded their state from disk in
    /// this process, each load at a store's opening included, as acknowledged through
    /// [`StoreGuard::note_refreshed`]. Counted while any lock on the directory lives.
    /// Test builds only.
    #[cfg(feature = "test-util")]
    #[must_use]
    pub fn loads(&self) -> usize {
        self.shared.loads.load(Ordering::Relaxed)
    }
}

impl PathLock {
    /// Drops the flock once nothing is in flight, no store has anything unflushed and no
    /// files are in use, and publishes the result. A flock that files in use keep has one
    /// task try the release again every [`RELEASE_RETRY`] until they are not, or a guard or an
    /// unflushed bucket takes the flock over. Polled rather than signalled by the I/O as it
    /// ends, which would run the release on an I/O completion thread.
    ///
    /// Takes the state lock's guard, and lets it go before the hold on the flock this one's lock
    /// is nested in goes, since that release takes the enclosing directory's state lock.
    fn release_if_idle(self: &Arc<Self>, mut state: parking_lot::MutexGuard<'_, LockState>) {
        let mut released = None;
        if !self.release_unless_in_use(&mut state, &mut released) && !state.retrying {
            state.retrying = true;
            let shared = Arc::clone(self);
            lore_base::lore_spawn!(async move {
                loop {
                    tokio::time::sleep(RELEASE_RETRY).await;
                    let mut released = None;
                    let mut state = shared.state.lock();
                    if shared.release_unless_in_use(&mut state, &mut released) {
                        state.retrying = false;
                        drop(state);
                        drop(released);
                        return;
                    }
                }
            });
        }
        drop(state);
        drop(released);
    }

    /// Waits until no files the stores on this directory opened are in use, polling every
    /// [`RELEASE_RETRY`] as a release they keep back does.
    async fn await_files_idle(&self) {
        while self.state.lock().files_in_use() {
            tokio::time::sleep(RELEASE_RETRY).await;
        }
    }

    /// Takes the flock, waiting for another process holding it as `contention` says, with
    /// whether the store directory had to be recreated first. `None` only under
    /// [`Contention::Skip`], when another process holds it.
    ///
    /// **A store directory deleted since this process last held it is recreated**, as
    /// opening a store creates its directory: a store this process kept outlives the
    /// directory, and the next use of the same path is handed that store. The epoch went
    /// with the directory, and an absent epoch compares fresh against a store that observed
    /// none, so the acquisition makes a recreated directory stale and begins a new lifetime
    /// in it, whatever the store and every other process observed of its predecessor.
    async fn take_flock(&self, contention: Contention) -> std::io::Result<Option<(FSLock, bool)>> {
        let take = || async {
            match contention {
                Contention::Wait => FSLock::acquire_exact_path(&self.lock_path).await.map(Some),
                Contention::Skip => FSLock::try_acquire_exact_path(&self.lock_path).await,
            }
        };
        match take().await {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // Boxed: a deleted directory is rare, and carried inline the recreation would
                // be part of every acquisition's future.
                Box::pin(async {
                    if let Some(directory) = self.lock_path.parent() {
                        lore_io::IoDriver::global()
                            .create_dir_all(directory)
                            .await?;
                    }
                    Ok(take().await?.map(|held| (held, true)))
                })
                .await
            }
            taken => Ok(taken?.map(|held| (held, false))),
        }
    }

    /// The release [`Self::release_if_idle`] makes, answering `false` where files in use keep a
    /// flock that would otherwise go.
    ///
    /// The stores are asked under the state lock, as the last guard goes, so a bucket
    /// flagged by an operation inside the span is seen: the span outlives its
    /// operations, and an operation outside any span holds a guard of its own.
    ///
    /// The stores close their files before the flock goes, not after: in between,
    /// another process could take the flock and open a packfile this one still had open.
    /// The flock released is unlocked here unless an epoch write in flight still holds it, and
    /// moved into `released` with its hold on the flock its lock is nested in, see
    /// [`Self::release_if_idle`].
    fn release_unless_in_use(
        &self,
        state: &mut LockState,
        released: &mut Option<Arc<Flock>>,
    ) -> bool {
        let mut done = true;
        if state.active == 0 && !state.has_unflushed() {
            if state.held.is_some() && !state.release_files() {
                done = false;
            } else {
                #[cfg(feature = "test-util")]
                self.assert_uncovered(state);
                *released = state.held.take();
                if let Some(flock) = released.as_mut().and_then(Arc::get_mut) {
                    drop(flock.lock.take());
                }
                state.advanced = false;
            }
        }
        state.publish(self);
        done
    }

    /// Fails a release of the flock while an operation a span covered is still running: the
    /// span ended under work that took no guard of its own, which goes on with no flock.
    ///
    /// A panic rather than a record, so that the test ending the span fails where it ends it.
    /// Not while unwinding, where a second panic would abort the process. The flock is
    /// published as not held in [`Self::held_live`] before the count is read, for the reason
    /// [`StoreLock::covered`] gives. Test builds only.
    #[cfg(feature = "test-util")]
    fn assert_uncovered(&self, state: &LockState) {
        self.held_live.store(NOT_HELD, Ordering::Release);
        std::sync::atomic::fence(Ordering::SeqCst);
        let covered = self.covered.load(Ordering::Relaxed);
        assert!(
            state.held.is_none() || covered == 0 || std::thread::panicking(),
            "the flock was released while operations a span covered still ran: {covered}"
        );
    }

    /// Writes `stamp` as the store directory's epoch, returning it.
    ///
    /// Staged and renamed into place, so what is replaced is the directory entry at the
    /// epoch path — never a file a link there names, and never a FIFO opened and
    /// waited on while the flock and the gate are held. Not for atomicity: the epoch is
    /// only ever read or written by a process holding the flock, so there is no
    /// concurrent reader to tear, and a torn file reads as [`Epoch::Unreadable`], which
    /// is the safe direction.
    ///
    /// The staging file is created exclusively, so it is always a file this call created:
    /// an entry already at the staging path, a link or a FIFO included, fails the creation
    /// instead of being opened. A staging entry left by an advance that did not finish is
    /// removed and the creation retried once; an entry placed there in between fails the
    /// retry the same way. A write or rename that fails removes the staging file it created.
    ///
    /// One job on the I/O driver's syscall pool, as the store's other atomic file writes are,
    /// so the flock and the gate are held across a dispatch rather than across a blocking call
    /// on a runtime thread. The job holds `held`, the flock, until the rename has landed: an
    /// acquisition dropped part way through the advance releases the flock only once the epoch
    /// it wrote is in place, so the epoch cannot go back past an advance another process makes
    /// once it has the flock. The sixteen bytes are handed to the driver owned, one small
    /// allocation per advance. This call keeps `held` too until the write it awaits has
    /// returned, so the last reference to the flock, and to the hold on the flock its lock is
    /// nested in, is not dropped on the syscall pool.
    ///
    /// # Errors
    ///
    /// [`std::io::Error`] if the staging file cannot be created or written, or cannot be
    /// renamed over the epoch path.
    async fn write_epoch(&self, stamp: Stamp, held: Arc<Flock>) -> std::io::Result<Stamp> {
        #[cfg(feature = "test-util")]
        epoch_write_barrier::wait_if_installed(&self.epoch_path);
        #[cfg(feature = "test-util")]
        let held = crate::test_util::park_store_write(held).await;
        let driver = lore_io::IoDriver::global();
        // Exclusive, so an entry already at the path, whatever it is, fails the open rather
        // than being opened; an exclusive create never follows a link or waits on a FIFO.
        let options = lore_io::OpenOptions::new().write(true).create_new(true);
        let write = |held| {
            driver.write_file_segments_atomic(
                &self.epoch_staging_path,
                &self.epoch_path,
                &options,
                EpochSegment(bytes::Bytes::copy_from_slice(&stamp.to_bytes())),
                false,
                held,
            )
        };
        match write(Arc::clone(&held)).await {
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                driver.remove_file(&self.epoch_staging_path).await?;
                write(Arc::clone(&held)).await
            }
            written => written,
        }
        .map(|()| stamp)
    }
}

/// The one segment an epoch write hands the I/O driver.
///
/// Heap-allocated, as the driver's buffer contract requires: the bytes stay where they are
/// while the write is in flight, whatever moves the value.
struct EpochSegment(bytes::Bytes);

impl lore_io::StableBufList for EpochSegment {
    fn byte_segments(&self) -> impl Iterator<Item = &[u8]> {
        std::iter::once(self.0.as_ref())
    }
}

/// Stops an epoch advance part way, for tests that need an acquisition held inside
/// that window. Keyed by the epoch path, so tests running at once cannot stop each
/// other's advances.
#[cfg(feature = "test-util")]
pub mod epoch_write_barrier {
    use std::collections::HashMap;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::OnceLock;

    use parking_lot::Condvar;
    use parking_lot::Mutex;

    /// Advances that reached one barrier, and whether they may continue.
    #[derive(Default)]
    struct Barrier {
        state: Mutex<(usize, bool)>,
        released: Condvar,
    }

    static INSTALLED: OnceLock<Mutex<HashMap<PathBuf, Arc<Barrier>>>> = OnceLock::new();

    fn installed() -> &'static Mutex<HashMap<PathBuf, Arc<Barrier>>> {
        INSTALLED.get_or_init(Mutex::default)
    }

    /// Blocks an advance of the epoch at `path` while a barrier installed there is not
    /// released. Returns at once where none is installed.
    pub fn wait_if_installed(path: &Path) {
        let Some(barrier) = installed().lock().get(path).cloned() else {
            return;
        };
        let mut state = barrier.state.lock();
        state.0 += 1;
        while !state.1 {
            barrier.released.wait(&mut state);
        }
    }

    /// A barrier on the epoch at one path, released and removed when this drops.
    pub struct Installed {
        path: PathBuf,
        barrier: Arc<Barrier>,
    }

    /// Installs a barrier on advances of the epoch at `path`.
    pub fn install(path: &Path) -> Installed {
        let barrier = Arc::new(Barrier::default());
        installed()
            .lock()
            .insert(path.to_path_buf(), Arc::clone(&barrier));
        Installed {
            path: path.to_path_buf(),
            barrier,
        }
    }

    impl Installed {
        /// How many advances have reached the barrier.
        pub fn reached(&self) -> usize {
            self.barrier.state.lock().0
        }

        /// Lets every advance at the barrier, and every later one, continue.
        pub fn release(&self) {
            self.barrier.state.lock().1 = true;
            self.barrier.released.notify_all();
        }
    }

    impl Drop for Installed {
        fn drop(&mut self) {
            self.release();
            installed().lock().remove(&self.path);
        }
    }
}

/// The stamp a writer records next, given what the directory says now.
///
/// A readable epoch continues its lifetime and advances the count, which wraps
/// rather than saturating: comparison is equality and never ordering, so a count
/// that comes back around reads as changed rather than as older.
///
/// **Anything else begins a lifetime**, with a nonce drawn at random. Both cases that
/// reach it mean the same thing — an absent epoch names a store nothing has written,
/// and an unreadable one names a store whose history this build cannot join — so
/// neither may continue a count that some other store already observed.
fn next_epoch(current: Epoch) -> Stamp {
    match current {
        Epoch::At(stamp) => Stamp {
            nonce: stamp.nonce,
            count: stamp.count.wrapping_add(1),
        },
        Epoch::Absent | Epoch::Unreadable => Stamp {
            nonce: rand::random(),
            count: 1,
        },
    }
}

/// One recorded epoch: which store lifetime, and how far into it.
///
/// The pair is compared whole and only for equality. Neither half is an ordering:
/// counts wrap, and nonces are random.
#[lore_macro::test_pub]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    /// Names the store directory's lifetime. Drawn at random by the first writer to
    /// find no readable epoch, so a directory deleted and recreated cannot present
    /// the counts of the store it replaced.
    nonce: u64,
    /// Advances once per hold that writes, before the write it announces.
    count: u64,
}

impl Stamp {
    /// The stamp's on-disk form: the nonce then the count, each little-endian.
    #[lore_macro::test_pub]
    fn to_bytes(self) -> [u8; EPOCH_BYTES] {
        let mut bytes = [0_u8; EPOCH_BYTES];
        let (nonce, count) = bytes.split_at_mut(size_of::<u64>());
        nonce.copy_from_slice(&self.nonce.to_le_bytes());
        count.copy_from_slice(&self.count.to_le_bytes());
        bytes
    }

    /// The stamp `bytes` records, in the layout [`Self::to_bytes`] writes.
    #[lore_macro::test_pub]
    fn from_bytes(bytes: [u8; EPOCH_BYTES]) -> Self {
        let (head, tail) = bytes.split_at(size_of::<u64>());
        let mut nonce = [0_u8; size_of::<u64>()];
        let mut count = [0_u8; size_of::<u64>()];
        nonce.copy_from_slice(head);
        count.copy_from_slice(tail);
        Self {
            nonce: u64::from_le_bytes(nonce),
            count: u64::from_le_bytes(count),
        }
    }
}

/// What the store directory says about its epoch.
///
/// Three states, not two, and the distinction is load-bearing. **Absent is a
/// definite answer**: a store nobody has written since epochs existed has no file,
/// two successive looks agree, and state built against it is reusable. **Unreadable
/// is not an answer at all** — a short or unopenable file could be anything — so it
/// never compares equal, not even to itself.
///
/// Folding the two together would make a store that never gets written stale on
/// every single acquisition, reloading all 256 groups each time and never reusing
/// anything, which is the whole benefit the epoch exists to enable.
#[lore_macro::test_pub]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Epoch {
    /// No epoch file. Definite, and reusable against another absence.
    Absent,
    /// A file that could not be read as one. Never reusable.
    Unreadable,
    /// The stamp a writer recorded.
    At(Stamp),
}

/// What the epoch file at `path` records right now.
///
/// Opened rather than probed, so absence is learned from the open itself, and read in
/// the same dispatch to the I/O driver, which also reports the file's length: a file of
/// any other length is caught without a second read. **Only `NotFound` is absence.**
/// Any other failure to open or read proves nothing and reads as
/// [`Epoch::Unreadable`], so a transient error is never mistaken for a store with no
/// epoch — which would compare fresh against a store that observed none.
///
/// On Unix the open follows no link and does not block: a link at the path reads as
/// unreadable rather than as whatever file it names, and a FIFO reads as unreadable
/// rather than waiting for a writer while the flock and the gate are held.
#[lore_macro::test_pub]
async fn read_epoch(path: &Path) -> Epoch {
    #[allow(unused_mut)]
    let mut options = lore_io::OpenOptions::new().read(true);
    #[cfg(unix)]
    {
        options = options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let (_file, metadata, head) = match lore_io::IoDriver::global()
        .open_read_head(path, &options, EPOCH_BYTES)
        .await
    {
        Ok(opened) => opened,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Epoch::Absent,
        Err(_) => return Epoch::Unreadable,
    };
    if metadata.len() != EPOCH_BYTES as u64 {
        return Epoch::Unreadable;
    }
    match head.as_ref().first_chunk::<EPOCH_BYTES>() {
        Some(bytes) if head.len() == EPOCH_BYTES => Epoch::At(Stamp::from_bytes(*bytes)),
        _ => Epoch::Unreadable,
    }
}

/// One operation's hold on a store.
///
/// Releasing is `Drop`, so an operation that returns early — or panics — cannot
/// leave the flock held. Deliberately not `Clone`: a hold is a resource with a
/// release, and copying one would let the count outlive what it was counting.
pub struct StoreGuard {
    /// The lock this guard is counted against.
    lock: Arc<StoreLock>,
    /// Whether another process changed the store since this one last looked.
    stale: bool,
    /// Whether this is a span's guard, see [`Self::into_span`].
    span: bool,
    /// The directory's gate, held only by a stale guard.
    ///
    /// A stale guard is one whose caller is about to drop and rebuild the store's
    /// in-memory state. Holding the gate for that span is what stops another
    /// operation joining the hold and running against state being torn down.
    gate: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl StoreGuard {
    /// Whether this store's in-memory state could not be shown to be current when the
    /// guard was taken.
    ///
    /// `true` means another process — or another store object on this directory —
    /// wrote, or that nothing can prove neither did. The caller must discard what it
    /// has and reload before serving anything, then say so with
    /// [`Self::note_refreshed`]. It stays `true` afterwards, as the record that this
    /// acquisition reloaded.
    #[must_use]
    pub const fn is_stale(&self) -> bool {
        self.stale
    }

    /// Makes this the guard of a stale acquisition, keeping `gate` until its caller has
    /// reloaded: no other acquisition may join a store whose state is being dropped, see
    /// [`Self::note_refreshed`].
    fn keep_gate(&mut self, gate: tokio::sync::OwnedMutexGuard<()>) {
        self.stale = true;
        self.gate = Some(gate);
    }

    /// Records that the caller has rebuilt this store's state from disk.
    ///
    /// **Only this adopts the epoch.** An acquisition that reports stale deliberately
    /// leaves the store's observed epoch alone, so a reload that fails — or a
    /// `hold()` future dropped part way through one — leaves the store still knowing
    /// it is behind. The next acquisition reports stale again and the reload is
    /// retried, where committing the epoch at acquisition time would have left the
    /// store claiming to be current over state that predates another process's write.
    ///
    /// Adopts, then releases the gate, so operations refused while the reload ran may
    /// proceed — in that order, because an operation on this store passing the gate
    /// before the adoption would reload it again, underneath the caller that has just
    /// reloaded. A no-op on a guard that was not stale.
    pub fn note_refreshed(&mut self) {
        let Some(gate) = self.gate.take() else {
            return;
        };
        // Nothing has moved the epoch since this guard was taken: every change to it is
        // made under the gate, which this guard has held throughout.
        self.lock.adopt(&self.lock.shared.state.lock());
        #[cfg(feature = "test-util")]
        self.lock.shared.loads.fetch_add(1, Ordering::Relaxed);
        drop(gate);
    }

    /// The lock this guard holds.
    #[must_use]
    pub fn lock(&self) -> &Arc<StoreLock> {
        &self.lock
    }

    /// Makes this the guard of a span, which covers the operations of every store on the
    /// directory while it lives, see [`StoreLock::covers`]. For a use of the store: a command,
    /// a handle, a server or an open.
    #[must_use]
    #[lore_macro::test_pub]
    pub(crate) fn into_span(mut self) -> Self {
        let mut state = self.lock.shared.state.lock();
        state.spans = state.spans.saturating_add(1);
        state.publish(&self.lock.shared);
        drop(state);
        self.span = true;
        self
    }
}

impl Drop for StoreGuard {
    fn drop(&mut self) {
        // A stale guard dropped without `note_refreshed` is a reload that failed or
        // was cancelled. Its gate goes with it, so the directory is never wedged, and
        // the observed epoch is left behind, so the next acquisition retries.
        let mut state = self.lock.shared.state.lock();
        state.active = state.active.saturating_sub(1);
        if self.span {
            state.spans = state.spans.saturating_sub(1);
        }
        self.lock.shared.release_if_idle(state);
    }
}

/// What an operation holds of its store's lock while it runs.
///
/// A guard of its own outside any span. Inside one the span's guard keeps the flock and the
/// operation holds nothing, or in test builds a claim that counts it on its directory until it
/// ends, so that releasing the flock while it runs fails. The covered case fits in the guard's
/// niche, so an `Option<Claim>` is no larger than an `Option<StoreGuard>` in either build.
#[must_use]
pub enum Claim {
    /// A guard of the operation's own.
    Held(StoreGuard),
    /// An operation a span covers, counted on its directory until it ends.
    #[cfg(feature = "test-util")]
    Covered(Covered),
}

impl Claim {
    /// Whether this is a guard of the operation's own that found its store stale: see
    /// [`StoreGuard::is_stale`].
    #[must_use]
    pub fn is_stale(&self) -> bool {
        matches!(self, Self::Held(guard) if guard.is_stale())
    }
}

/// An operation a span covers, counted on its directory from [`StoreLock::covered`] until it is
/// dropped. Test builds only.
#[cfg(feature = "test-util")]
pub struct Covered {
    shared: Arc<PathLock>,
}

#[cfg(feature = "test-util")]
impl Drop for Covered {
    fn drop(&mut self) {
        self.shared.covered.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Every store lock a command holds, kept together so a repository lock can be
/// built on top of them.
///
/// # Why this type exists
///
/// The repository flock must be **fully contained** within the store locks: it
/// may never be acquired without them, and it may never outlive them. Both halves
/// of that are ordering, and ordering left to reviewers does not hold: a store
/// flock held by a `lore_storage_open` handle while a `lore_revision` command takes
/// the repository flock in the other order is a cycle neither process returns from.
///
/// So the invariant is a type rather than a rule. A repository lock holder owns
/// one of these, and there is no way to build one without it, which makes
/// "repository flock without store flocks" unrepresentable rather than
/// discouraged. The holder must declare its own flock **before** this field, so
/// that Rust's field drop order releases the repository flock first and the store
/// locks second — the reverse of acquisition, which is what containment means on
/// the way out.
///
/// A hold covering no on-disk stores is legitimate and empty: an in-memory store
/// has no flock to take, and a remote one has no local state to guard.
#[derive(Default)]
pub struct StoreHold {
    /// The guards, released together when this drops.
    guards: Vec<StoreGuard>,
}

impl StoreHold {
    /// A hold over `guards`, which are released when it drops.
    #[must_use]
    pub fn new(guards: Vec<StoreGuard>) -> Self {
        Self { guards }
    }

    /// Whether any of the stores reported that another process changed it.
    ///
    /// A command that sees `true` must let each store reload before it reads
    /// anything, which the stores do for themselves on their own guards; this is
    /// for callers that want to know a reload happened.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.guards.iter().any(StoreGuard::is_stale)
    }

    /// How many on-disk stores this hold covers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.guards.len()
    }

    /// Whether this hold covers no on-disk store, which is the in-memory case.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.guards.is_empty()
    }
}

/// A store's opening, `F`, that releases the claim the store was opened under as it completes:
/// what a store's `new` and `create` return over `new_claimed` and `create_claimed`, for a caller
/// that does not claim the store next.
///
/// Polled through in place, so it is no larger than `F`. An `async fn` awaiting `F` would hold the
/// opening's arguments a second time, and a mapping combinator would add a state of its own.
#[pin_project::pin_project]
pub(crate) struct ReleasingOpeningClaim<F>(#[pin] pub(crate) F);

impl<F, Store, Error> Future for ReleasingOpeningClaim<F>
where
    F: Future<Output = Result<(Store, Option<StoreGuard>), Error>>,
{
    type Output = Result<Store, Error>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.project()
            .0
            .poll(cx)
            .map(|opened| opened.map(|(store, _opening)| store))
    }
}
