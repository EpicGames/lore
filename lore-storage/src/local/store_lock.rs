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
//! is taken by the first and released by the last. An operation inside a span joins
//! the claim already held; taking one per operation would say nothing about whether
//! anyone has the store, and would put a lock acquisition and an epoch read on every
//! call.
//!
//! A store that has been *closed* is not in use, even while the keep-alive cache
//! still holds its in-memory state. That state is retained memory with no claim
//! attached, which is exactly why the epoch below exists.
//!
//! One exception to the span rule: a store with unflushed modifications keeps the
//! flock past the end of the span, because a process must not sit on changes another
//! process cannot see.
//!
//! **A process with nothing open blocks nobody.** That is what makes a keep-alive
//! worth having: the state stays, the claim does not.
//!
//! **A repository lock is always taken *inside* the store claims it runs under**, so
//! the two cannot form a cycle. There is one order and every path follows it: store
//! claims first, then the repository flock, released in reverse. `RepositoryLock`
//! enforces the release half by owning the claims it was taken under and declaring
//! its own flock first, so Rust's drop order frees the repository before the stores
//! containing it.
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
//! Keeping the in-memory state after the claim is released is the whole point — it
//! is what lets a closed store be reopened without re-reading it — and it is only
//! sound if the state is proven unchanged. **A store must never serve in-memory
//! state that another process modified on disk while this one held no claim.**
//!
//! So the check belongs to the moment the store goes back into use, not to each
//! operation: that transition is the only point at which another process could have
//! written since this one last looked.
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
//! fresh — two identical stamps, or two looks that both found no epoch file, which
//! is a definite answer describing a store nothing has written.
//!
//! # Within a hold
//!
//! While the flock is held nothing outside this process can write, so an operation
//! joining the hold does not compare epochs. What can still move is this process's
//! own record of the epoch — another store object on the directory advancing it —
//! and every change to that record starts a new generation. A store that adopted the
//! current generation is current however the epoch reads. That is what makes a store
//! over an epoch it cannot read reload once, when its hold begins, rather than on
//! every operation, and what keeps a reload from running underneath work already
//! joined to the hold.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

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
    /// another process locks the new one, and neither excludes the other.
    held: Option<FSLock>,
    /// Operations in flight, across every store object sharing this directory.
    active: usize,
    /// Whether some store on this directory has modifications not yet on disk.
    ///
    /// Keeps the flock past the last operation: a process must not hold
    /// modifications that another process has no way to see.
    dirty: bool,
    /// Write claims in flight.
    ///
    /// A write claim marks the store dirty as it is taken, but the operation sets its
    /// bucket's own flag later. Between the two the store looks clean to anything
    /// scanning buckets, so a flush finishing in that gap would declare it clean and
    /// the flock would be released the moment the writer's own guard dropped — over a
    /// bucket that had by then been marked. Counting writers is what makes "clean"
    /// mean "and nobody is in the middle of making it dirty".
    writing: usize,
    /// Write claims that have ended, counted.
    ///
    /// A clean scan is evidence only if no write finished while it ran: a writer that
    /// joins after the scan starts and marks a bucket the scan has already passed is
    /// no longer in flight by the time the scan reports. Bumped as each write claim
    /// drops, so a scan that read it first can tell.
    write_generation: u64,
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

    /// Counts an operation in, and a writer with it when `intent` is to write.
    fn count_in(&mut self, intent: Intent) {
        self.active = self.active.saturating_add(1);
        if intent == Intent::Write {
            self.writing = self.writing.saturating_add(1);
        }
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
    /// The bookkeeping shared by every store on this directory.
    state: Mutex<LockState>,
    /// Serialises the transitions that touch the filesystem — taking the flock,
    /// reading the epoch, advancing it — and is held across a caller's reload, so
    /// only one task per process does any of them.
    gate: Arc<tokio::sync::Mutex<()>>,
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
/// The flock, the in-flight count and the dirty flag are shared per directory
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
    /// Written only under the directory's state lock, which is also where it is read,
    /// so it never disagrees with [`Self::observed`] where anything can see it. An
    /// atomic beside that mutex rather than inside it, so an operation joining a hold
    /// decides freshness under the state lock alone.
    observed_generation: AtomicU64,
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

/// The generation after `generation`, which is never [`NEVER_ADOPTED`].
fn next_generation(generation: u64) -> u64 {
    match generation.wrapping_add(1) {
        NEVER_ADOPTED => 0,
        next => next,
    }
}

impl StoreLock {
    /// A lock for the store directory at `path`.
    ///
    /// The directory must exist: the path is canonicalized so that two spellings of
    /// one directory share one flock, and canonicalizing requires it.
    ///
    /// # Errors
    ///
    /// [`std::io::Error`] if `path` cannot be canonicalized.
    pub fn new(path: impl AsRef<Path>) -> std::io::Result<Arc<Self>> {
        let key = path.as_ref().canonicalize()?;
        let shared = {
            let mut locks = path_locks().lock();
            match locks.get(&key).and_then(std::sync::Weak::upgrade) {
                Some(existing) => existing,
                None => {
                    // Dead entries are swept here rather than on drop, which would
                    // need the key on a path that must stay allocation-free.
                    locks.retain(|_, held| held.strong_count() > 0);
                    let created = Arc::new(PathLock {
                        lock_path: key.join("lock"),
                        epoch_path: key.join(EPOCH_FILE),
                        epoch_staging_path: key.join(EPOCH_STAGING_FILE),
                        state: Mutex::new(LockState {
                            held: None,
                            active: 0,
                            dirty: false,
                            writing: 0,
                            write_generation: 0,
                            advanced: false,
                            current: Epoch::Absent,
                            generation: 0,
                        }),
                        gate: Arc::new(tokio::sync::Mutex::new(())),
                    });
                    locks.insert(key, Arc::downgrade(&created));
                    created
                }
            }
        };
        Ok(Arc::new(Self {
            shared,
            observed: Mutex::new(None),
            observed_generation: AtomicU64::new(NEVER_ADOPTED),
        }))
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
    /// while this one waited, or taking the flock and reading the epoch.
    ///
    /// `None` only under [`Contention::Skip`], when another process holds the flock.
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
                state.count_in(intent);
                (stale, advance, state.current)
            })
        };
        if let Some((stale, advance, known)) = joined {
            // A stale guard keeps the gate until its caller has reloaded. A fresh one
            // lets it go — but only once any advance below is committed and adopted.
            // Released sooner, a second writer would pass the gate while `advanced` is
            // still false and advance the same epoch again, and any other operation on
            // this store would find it behind its own write and reload it underneath
            // this one.
            let (kept, pending) = if stale {
                (Some(gate), None)
            } else {
                (None, Some(gate))
            };
            // Built before the write below, so the count it owns is released by its
            // `Drop` if that write fails, rather than leaked by an early return.
            let guard = self.counted_guard(kept, intent);
            if advance {
                // The current epoch is authoritative while the flock is held, so this
                // needs no read of its own — nothing outside this process can have
                // written it since.
                //
                // Outside the state lock: this writes a file.
                let stamp = self.shared.write_epoch(next_epoch(known))?;
                let mut state = self.shared.state.lock();
                state.advanced = true;
                state.set_current(Epoch::At(stamp));
                // A store that has just announced its own write records the epoch it
                // wrote rather than the one it replaced. A stale one adopts nothing
                // until its caller has reloaded.
                if !stale {
                    self.adopt(&state);
                }
            }
            drop(pending);
            return Ok(Some(guard));
        }

        let held = match contention {
            Contention::Wait => FSLock::acquire_exact_path(&self.shared.lock_path).await?,
            Contention::Skip => {
                match FSLock::try_acquire_exact_path(&self.shared.lock_path).await? {
                    Some(held) => held,
                    None => return Ok(None),
                }
            }
        };
        let on_disk = read_epoch(&self.shared.epoch_path);
        // Against what was on disk when the flock was taken, before this store's own
        // advance below. Another process may have written while the flock was not held,
        // so this compares epochs and never generations.
        let stale = is_stale(*self.observed.lock(), on_disk);
        let current = match intent {
            Intent::Write => Epoch::At(self.shared.write_epoch(next_epoch(on_disk))?),
            Intent::Read => on_disk,
        };
        // Committed, adopted and counted in together, for the reasons the joined path
        // gives above. Nothing can release a flock this call has just taken and not yet
        // counted itself against, but keeping both paths atomic is what stops the
        // shape recurring rather than this instance of it.
        {
            let mut state = self.shared.state.lock();
            state.held = Some(held);
            state.advanced = intent == Intent::Write;
            state.set_current(current);
            state.count_in(intent);
            if !stale {
                self.adopt(&state);
            }
        }
        Ok(Some(self.counted_guard(stale.then_some(gate), intent)))
    }

    /// Builds the guard for an operation this call has **already counted in**, with
    /// [`LockState::count_in`].
    ///
    /// `gate` is the gate this guard keeps, and whether it keeps one is what makes it
    /// stale: a stale guard holds the gate because its caller has not reloaded yet and
    /// no other acquisition may join a store whose state is being dropped — see
    /// [`StoreGuard::note_refreshed`]. A fresh guard is built with `None`.
    fn counted_guard(
        self: &Arc<Self>,
        gate: Option<tokio::sync::OwnedMutexGuard<()>>,
        intent: Intent,
    ) -> StoreGuard {
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
            stale: gate.is_some(),
            writing: intent == Intent::Write,
            gate,
        }
    }

    /// Whether this store's in-memory state matches the directory's current epoch, for
    /// an acquisition joining a flock this process already holds.
    ///
    /// A store that adopted the current generation is current without looking at the
    /// epoch: nothing outside this process can write while the flock is held, so only a
    /// new generation can have left it behind. One that adopted an earlier generation is
    /// current if the epoch it observed still matches, and records that by taking the
    /// generation — so a store compares epochs at most once per generation.
    ///
    /// Called under the directory's state lock, which keeps the generation this
    /// records the one it compared against.
    fn is_current(&self, state: &LockState) -> bool {
        if self.observed_generation.load(Ordering::Relaxed) == state.generation {
            return true;
        }
        if is_stale(*self.observed.lock(), state.current) {
            return false;
        }
        self.observed_generation
            .store(state.generation, Ordering::Relaxed);
        true
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
    fn try_join(self: &Arc<Self>, intent: Intent) -> Option<StoreGuard> {
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
        state.count_in(intent);
        Some(StoreGuard {
            lock: Arc::clone(self),
            stale: false,
            writing: intent == Intent::Write,
            gate: None,
        })
    }

    /// Records that the store has modifications not yet written to disk.
    ///
    /// Keeps the flock past the last operation until [`Self::clear_dirty`], so no
    /// other process can read a store this one has changed and not yet flushed.
    pub fn mark_dirty(&self) {
        self.shared.state.lock().dirty = true;
    }

    /// Where a scan for dirty buckets starts, or `None` when no scan could clear
    /// anything.
    ///
    /// `None` when the store is not marked dirty, so there is no claim for a scan to
    /// release, and while a writer is in flight, which a scan cannot see past. Asked
    /// before scanning, so a flush that cannot clear anything does not pay for visiting
    /// every bucket flag.
    #[must_use]
    pub fn clean_scan_start(&self) -> Option<CleanScan> {
        let state = self.shared.state.lock();
        (state.dirty && state.writing == 0).then_some(CleanScan(state.write_generation))
    }

    /// Records that a scan started at `scan` found every bucket on disk, releasing the
    /// flock if nothing else is holding it.
    ///
    /// Refused while a writer is in flight, and refused if any write claim ended after
    /// the scan started: either may have marked a bucket the scan had already passed,
    /// so a scan that found nothing is not evidence that there is nothing.
    pub fn clear_dirty(&self, scan: CleanScan) {
        let mut state = self.shared.state.lock();
        if state.writing > 0 || state.write_generation != scan.0 {
            return;
        }
        state.dirty = false;
        Self::release_if_idle(&mut state);
    }

    /// Whether this process is holding the flock right now. For tests and
    /// diagnostics; callers should hold a [`StoreGuard`] instead of asking.
    #[must_use]
    pub fn is_held(&self) -> bool {
        self.shared.state.lock().held.is_some()
    }

    /// Drops the flock once nothing is in flight and nothing is unflushed.
    fn release_if_idle(state: &mut LockState) {
        if state.active == 0 && !state.dirty {
            state.held = None;
            state.advanced = false;
        }
    }
}

/// Where a scan for dirty buckets started, from [`StoreLock::clean_scan_start`].
///
/// Opaque, so a store's dirty mark can only be cleared by a scan that began against
/// the lock's own count of finished writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanScan(u64);

impl PathLock {
    /// Writes `stamp` as the store directory's epoch, returning it.
    ///
    /// Staged and renamed into place, so what is replaced is the directory entry at the
    /// epoch path — never a file a link there names, and never a FIFO opened and
    /// waited on while the flock and the gate are held. Not for atomicity: the epoch is
    /// only ever read or written by a process holding the flock, so there is no
    /// concurrent reader to tear, and a torn file reads as [`Epoch::Unreadable`], which
    /// is the safe direction.
    ///
    /// A staging file left by an advance that did not finish is replaced, and one this
    /// call leaves behind by failing is removed where it can be.
    ///
    /// # Errors
    ///
    /// [`std::io::Error`] if the staging file cannot be written, or cannot be renamed
    /// over the epoch path.
    fn write_epoch(&self, stamp: Stamp) -> std::io::Result<Stamp> {
        #[cfg(feature = "test-util")]
        epoch_write_barrier::wait_if_installed(&self.epoch_path);
        let written = self
            .stage_epoch(stamp)
            .and_then(|()| std::fs::rename(&self.epoch_staging_path, &self.epoch_path));
        if written.is_err() {
            // Best effort: the next advance replaces a staging file it finds anyway.
            let _ = std::fs::remove_file(&self.epoch_staging_path);
        }
        written.map(|()| stamp)
    }

    /// Writes `stamp` to a staging file created for it.
    ///
    /// # Errors
    ///
    /// [`std::io::Error`] if the staging file cannot be created, or written.
    fn stage_epoch(&self, stamp: Stamp) -> std::io::Result<()> {
        use std::io::Write;

        let create = || {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&self.epoch_staging_path)
        };
        let mut file = match create() {
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                std::fs::remove_file(&self.epoch_staging_path)?;
                create()?
            }
            created => created?,
        };
        file.write_all(&stamp.to_bytes())
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
/// Opened rather than probed, so absence is learned from the open itself: one fewer
/// syscall, and no window between asking whether the file exists and reading it.
/// **Only `NotFound` is absence.** Any other failure to open or read proves nothing
/// and reads as [`Epoch::Unreadable`], so a transient error is never mistaken for a
/// store with no epoch — which would compare fresh against a store that observed
/// none. The buffer is one byte longer than an epoch, so a file of any other length
/// is caught without allocating.
///
/// On Unix the open follows no link and does not block: a link at the path reads as
/// unreadable rather than as whatever file it names, and a FIFO reads as unreadable
/// rather than waiting for a writer while the flock and the gate are held.
#[lore_macro::test_pub]
fn read_epoch(path: &Path) -> Epoch {
    use std::io::Read;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Epoch::Absent,
        Err(_) => return Epoch::Unreadable,
    };
    let mut buffer = [0_u8; EPOCH_BYTES + 1];
    let mut filled = 0;
    while let Some(unfilled) = buffer.get_mut(filled..).filter(|rest| !rest.is_empty()) {
        match file.read(unfilled) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Epoch::Unreadable,
        }
    }
    match buffer.first_chunk::<EPOCH_BYTES>() {
        Some(bytes) if filled == EPOCH_BYTES => Epoch::At(Stamp::from_bytes(*bytes)),
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
    /// Whether this claim was taken to write, so the writer count falls with it.
    writing: bool,
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
        drop(gate);
    }

    /// The lock this guard holds, for marking the store dirty while it is held.
    #[must_use]
    pub fn lock(&self) -> &Arc<StoreLock> {
        &self.lock
    }
}

impl Drop for StoreGuard {
    fn drop(&mut self) {
        // A stale guard dropped without `note_refreshed` is a reload that failed or
        // was cancelled. Its gate goes with it, so the directory is never wedged, and
        // the observed epoch is left behind, so the next acquisition retries.
        let mut state = self.lock.shared.state.lock();
        state.active = state.active.saturating_sub(1);
        if self.writing {
            state.writing = state.writing.saturating_sub(1);
            state.write_generation = state.write_generation.wrapping_add(1);
        }
        StoreLock::release_if_idle(&mut state);
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
