// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use lore_base::fs::lock::FSLock;
use lore_base::test_util::TempDir;
use lore_storage::local::store_lock::*;

/// What the epoch file in the store directory at `path` records.
fn epoch(path: &Path) -> Epoch {
    read_epoch(&path.join(EPOCH_FILE))
}

/// **A guard always covers a flock this process actually holds.**
///
/// `release_if_idle` fires at `active == 0`, so an acquisition that observes the
/// flock in one critical section and counts itself in in another leaves a window
/// where the last outstanding guard can drop and release it. The acquirer then
/// writes the epoch holding nothing, and hands its caller a guard for a store this
/// process has no claim on — silently, since its own drop finds the flock already
/// gone.
///
/// The gate does not close that window: it serialises acquirers, and a guard is
/// released from `Drop`, which is synchronous and never takes it.
///
/// Concurrent by necessity: a sequential test cannot reach this at all. Mixed
/// intents, because the window is widest for the first writer of a hold, whose slow
/// path spans a file write. And no acquisition after the opening reload may report
/// stale: nothing but this store writes the directory, so one that did would be an
/// acquisition that passed the gate before the one ahead of it adopted its epoch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_guard_always_covers_a_flock_this_process_holds() {
    let dir = dir("concurrent");
    let lock = store_lock(dir.path());

    let mut opening = lock.acquire(Intent::Read).await.expect("acquires");
    opening.note_refreshed();
    drop(opening);

    let mut tasks = tokio::task::JoinSet::new();
    // Readers hold briefly rather than instantly: the window under test opens only
    // when a writer observes the flock held by someone who then releases it while
    // the writer is still in its slow path. A reader whose guard lives for
    // nanoseconds is almost never still holding at the instant of observation, so
    // it never sets the window up.
    for _ in 0..12u32 {
        let lock = Arc::clone(&lock);
        lore_base::lore_spawn!(tasks, async move {
            for _ in 0..400u32 {
                let guard = lock.acquire(Intent::Read).await.expect("acquires");
                assert!(!guard.is_stale(), "nothing else wrote the directory");
                tokio::task::yield_now().await;
                drop(guard);
                tokio::task::yield_now().await;
            }
        });
    }
    // Writers reach the gate whenever a read hold exists that has not yet advanced
    // the epoch, which is the branch that counts itself in before a file write.
    for _ in 0..4u32 {
        let lock = Arc::clone(&lock);
        lore_base::lore_spawn!(tasks, async move {
            for _ in 0..400u32 {
                let mut guard = lock.acquire(Intent::Write).await.expect("acquires");
                assert!(
                    lock.is_held(),
                    "a live guard must mean this process holds the flock"
                );
                assert!(!guard.is_stale(), "nothing else wrote the directory");
                guard.note_refreshed();
                drop(guard);
                tokio::task::yield_now().await;
            }
        });
    }
    while let Some(joined) = tasks.join_next().await {
        joined.expect("no task saw an unheld flock or a stale store");
    }
}

/// **A store is not declared clean while a writer is still in flight.**
///
/// A write claim marks the store dirty as it is taken; the operation sets its
/// bucket's own flag later. Anything scanning buckets in that gap sees a clean
/// store — and a flush that acts on it clears the flag, so the flock is released
/// the moment the writer's guard drops, over a bucket marked in the meantime.
/// Another process then takes the store and writes, and the reload that follows
/// discards the acknowledged write.
///
/// The background eviction and compaction passes are what make this routine: they
/// call `note_flushed` on a timer, with no idea what else is mid-write.
#[tokio::test]
async fn a_clean_scan_cannot_release_a_store_with_a_writer_in_flight() {
    let dir = dir("writer-in-flight");
    let lock = store_lock(dir.path());

    let mut first = lock.acquire(Intent::Write).await.expect("acquires");
    first.note_refreshed();
    lock.mark_dirty();
    drop(first);

    // A flush starts scanning a dirty store with nothing in flight; a writer joins
    // before it reports, and has not yet marked the bucket it is about to write.
    let scan = lock
        .clean_scan_start()
        .expect("a dirty store with nothing in flight can be scanned");
    let writer = lock.acquire(Intent::Write).await.expect("joins");
    assert_eq!(
        lock.clean_scan_start(),
        None,
        "no scan starts while a writer is in flight"
    );
    lock.clear_dirty(scan);

    drop(writer);
    assert!(
        lock.is_held(),
        "the store must stay claimed: a scan that ran before the writer marked \
         anything is no evidence that there is nothing to flush"
    );

    // A scan started once the writer is gone is evidence, and the claim goes.
    let scan = lock.clean_scan_start().expect("nothing in flight");
    lock.clear_dirty(scan);
    assert!(!lock.is_held(), "and with no writer in flight it releases");
}

/// **A reload that never happened is never adopted.** The epoch is taken only by
/// [`StoreGuard::note_refreshed`], so a caller whose reload failed — or whose
/// future was dropped part way through one — leaves the store still knowing it is
/// behind, and the next acquisition retries.
#[tokio::test]
async fn a_staleness_left_unacknowledged_is_reported_again() {
    let dir = dir("unacked");
    let lock = store_lock(dir.path());

    // A write, so the directory has an epoch at all: with no epoch file there is
    // nothing to observe, and every acquisition is correctly stale forever.
    let first = lock.acquire(Intent::Write).await.expect("acquires");
    assert!(first.is_stale(), "nothing observed yet");
    // Dropped without acknowledging, as a failed or cancelled reload would be.
    drop(first);

    let second = lock.acquire(Intent::Read).await.expect("acquires");
    assert!(
        second.is_stale(),
        "the reload never happened, so the store still knows it is behind"
    );
    drop(second);

    let mut third = lock.acquire(Intent::Read).await.expect("acquires");
    assert!(third.is_stale());
    third.note_refreshed();
    drop(third);

    let fourth = lock.acquire(Intent::Read).await.expect("acquires");
    assert!(
        !fourth.is_stale(),
        "and once acknowledged, it stops being reported"
    );
}

/// **Two stores on one directory share the flock but not the epoch.**
///
/// `flock` excludes by open file description, so a second description in the same
/// process would block against the first — forever, since the wait is unbounded.
/// Sharing the lock is what makes a second store on the same directory join
/// rather than hang. Their in-memory state is still their own, so one writing
/// must leave the other knowing it is behind.
#[tokio::test]
async fn two_stores_on_one_directory_share_the_flock_but_not_the_epoch() {
    let dir = dir("shared");
    let first = store_lock(dir.path());
    let second = store_lock(dir.path());

    // A write first, so the directory has an epoch for either store to observe.
    let mut opening = second.acquire(Intent::Write).await.expect("acquires");
    opening.note_refreshed();
    drop(opening);

    // Taken while the first store's guard is alive: this is the acquisition that
    // would deadlock against a second flock on the same directory.
    let mut held = first.acquire(Intent::Write).await.expect("acquires");
    held.note_refreshed();
    let joined = tokio::time::timeout(Duration::from_secs(5), second.acquire(Intent::Read))
        .await
        .expect("does not block on itself")
        .expect("acquires");
    assert!(
        joined.is_stale(),
        "the other store advanced the epoch under this one"
    );
    drop(joined);
    drop(held);

    assert!(
        !first.is_held(),
        "and the flock goes when the last of them leaves"
    );
}

/// A lock for a directory known to exist, so a test that means to check the lock
/// does not silently check path resolution instead.
fn store_lock(path: &std::path::Path) -> Arc<StoreLock> {
    StoreLock::new(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

/// A store directory to lock, removed with the test.
fn dir(name: &str) -> TempDir {
    TempDir::new(&format!("lore-store-lock-{name}-"))
}

/// Waits for `condition` for up to five seconds, answering whether it came to hold.
async fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !condition() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    true
}

/// **The flock is held only while there is work, and released when there is
/// not**, so an idle process that keeps a store alive holds nothing.
#[tokio::test]
async fn the_flock_is_held_only_while_an_operation_is_in_flight() {
    let dir = dir("in-flight");
    let lock = store_lock(dir.path());
    assert!(
        !lock.is_held(),
        "nothing is held before the first operation"
    );

    // Acknowledged: a guard reporting stale holds the directory's gate until its
    // caller has reloaded, so a second acquisition would wait behind it.
    let mut first = lock.acquire(Intent::Read).await.expect("acquires");
    first.note_refreshed();
    assert!(lock.is_held());
    let second = lock.acquire(Intent::Read).await.expect("joins the hold");
    assert!(lock.is_held());

    drop(second);
    assert!(
        lock.is_held(),
        "the last operation out releases, not the first"
    );
    drop(first);
    assert!(!lock.is_held(), "an idle store holds nothing");
}

/// **A store with unflushed modifications keeps the flock past its last
/// operation**, because another process must not read a store this one has
/// changed and not yet written.
#[tokio::test]
async fn dirty_state_keeps_the_flock_until_it_is_flushed() {
    let dir = dir("dirty");
    let lock = store_lock(dir.path());

    let guard = lock.acquire(Intent::Write).await.expect("acquires");
    lock.mark_dirty();
    drop(guard);
    assert!(lock.is_held(), "unflushed modifications hold the flock");

    let scan = lock
        .clean_scan_start()
        .expect("dirty, with nothing in flight");
    lock.clear_dirty(scan);
    assert!(!lock.is_held(), "and a flush releases it");
}

/// The count the directory's epoch records, or `None` where it records no
/// readable epoch. Tests assert on the count because the nonce beside it is
/// drawn at random and has no value to predict.
fn epoch_count(path: &Path) -> Option<u64> {
    match epoch(path) {
        Epoch::At(stamp) => Some(stamp.count),
        Epoch::Absent | Epoch::Unreadable => None,
    }
}

/// Writes an epoch straight to disk, standing in for another process that holds
/// the flock. Goes through [`Stamp::to_bytes`] so a test cannot agree with a
/// mistaken encoding.
fn put_epoch(path: &Path, nonce: u64, count: u64) {
    std::fs::write(path.join(EPOCH_FILE), Stamp { nonce, count }.to_bytes())
        .expect("writes an epoch");
}

/// **A writer advances the epoch, a reader does not.** Two readers must not
/// invalidate each other, and a writer must be visible to everyone.
#[tokio::test]
async fn only_a_writer_advances_the_epoch() {
    let dir = dir("advance");
    let lock = store_lock(dir.path());

    drop(lock.acquire(Intent::Read).await.expect("acquires"));
    assert_eq!(epoch(dir.path()), Epoch::Absent, "a reader writes no epoch");

    drop(lock.acquire(Intent::Write).await.expect("acquires"));
    assert_eq!(epoch_count(dir.path()), Some(1), "a writer advances it");

    // The whole epoch, so a reader that rewrote only the nonce is caught too.
    let written = epoch(dir.path());
    drop(lock.acquire(Intent::Read).await.expect("acquires"));
    assert_eq!(
        epoch(dir.path()),
        written,
        "and a later reader leaves it alone"
    );
}

/// **The epoch advances before the write, not after**, so a process killed
/// mid-write has already invalidated everyone else.
#[tokio::test]
async fn the_epoch_advances_before_the_caller_writes() {
    let dir = dir("before");
    let lock = store_lock(dir.path());

    let guard = lock.acquire(Intent::Write).await.expect("acquires");
    assert_eq!(
        epoch_count(dir.path()),
        Some(1),
        "advanced while the guard is still held"
    );
    drop(guard);
}

/// **A writer joining a hold a reader started still advances the epoch.**
/// The cheap path must not let a write go unannounced because a reader
/// happened to take the flock first.
#[tokio::test]
async fn a_writer_joining_a_readers_hold_still_advances() {
    let dir = dir("join");
    let lock = store_lock(dir.path());

    // Acknowledged, because a stale guard holds the directory's gate until its
    // caller has reloaded — which is what keeps an operation off a store being
    // rebuilt, and which every real caller does inside `hold`.
    let mut reader = lock.acquire(Intent::Read).await.expect("acquires");
    reader.note_refreshed();
    assert_eq!(epoch(dir.path()), Epoch::Absent);

    let writer = lock.acquire(Intent::Write).await.expect("joins");
    assert_eq!(
        epoch_count(dir.path()),
        Some(1),
        "the writer announced itself"
    );
    let announced = epoch(dir.path());

    // A second writer in the same hold does not pay another file write.
    let again = lock.acquire(Intent::Write).await.expect("joins");
    assert_eq!(
        epoch(dir.path()),
        announced,
        "one advance per hold is enough"
    );
    drop((reader, writer, again));
}

/// **Nothing this process did to itself reads as staleness.** A store that
/// reacquires after its own writes must keep its in-memory state, or the
/// reuse this exists for never happens.
#[tokio::test]
async fn a_store_is_not_stale_against_its_own_writes() {
    let dir = dir("self");
    let lock = store_lock(dir.path());

    // The first acquisition of a store that has observed nothing is stale, and
    // acknowledging it is what a caller does after loading from disk.
    let mut first = lock.acquire(Intent::Write).await.expect("acquires");
    first.note_refreshed();
    drop(first);

    let again = lock.acquire(Intent::Read).await.expect("acquires");
    assert!(
        !again.is_stale(),
        "this process wrote it; it knows the epoch"
    );
    drop(again);

    let third = lock.acquire(Intent::Write).await.expect("acquires");
    assert!(!third.is_stale(), "nor does its own advance make it stale");
}

/// **Another process's write is stale, and so is anything unprovable.**
/// The other process is simulated by writing the epoch file directly, which
/// is exactly what it would do.
#[tokio::test]
async fn another_process_writing_makes_the_state_stale() {
    let dir = dir("other");
    let lock = store_lock(dir.path());

    let mut opening = lock.acquire(Intent::Read).await.expect("acquires");
    opening.note_refreshed();
    drop(opening);

    put_epoch(dir.path(), 42, 7);
    let mut after = lock.acquire(Intent::Read).await.expect("acquires");
    assert!(after.is_stale(), "the epoch moved under us");
    after.note_refreshed();
    drop(after);

    // Having observed that epoch, an unchanged store is fresh again.
    let unchanged = lock.acquire(Intent::Read).await.expect("acquires");
    assert!(!unchanged.is_stale());
    drop(unchanged);

    // And every unprovable answer is stale: a missing file, and a short one.
    std::fs::remove_file(dir.path().join(EPOCH_FILE)).expect("removes");
    let mut absent = lock.acquire(Intent::Read).await.expect("acquires");
    assert!(absent.is_stale(), "no epoch proves nothing");
    absent.note_refreshed();
    drop(absent);

    std::fs::write(dir.path().join(EPOCH_FILE), [1u8, 2, 3]).expect("writes");
    let short = lock.acquire(Intent::Read).await.expect("acquires");
    assert!(short.is_stale(), "an unreadable epoch proves nothing");
    drop(short);

    // A count with no nonce beside it is not an epoch this build can read, so a
    // store carrying only a count cannot be mistaken for one this process knows.
    std::fs::write(dir.path().join(EPOCH_FILE), 7u64.to_le_bytes()).expect("writes");
    let bare = lock.acquire(Intent::Read).await.expect("acquires");
    assert!(bare.is_stale(), "a count alone proves nothing");
}

/// **The first acquisition of all is stale**, because a store that has
/// observed nothing cannot claim its state matches anything.
#[tokio::test]
async fn the_first_acquisition_is_stale() {
    let dir = dir("first");
    let lock = store_lock(dir.path());
    let first = lock.acquire(Intent::Read).await.expect("acquires");
    assert!(first.is_stale());
}

/// **A hold releases every store it covers, together.** The repository lock
/// will own one of these, so the moment it stops holding them is the moment
/// this drops — which is what keeps the repository flock inside the store
/// locks on the way out as well as the way in.
#[tokio::test]
async fn a_hold_releases_every_store_it_covers() {
    let one = dir("hold-one");
    let two = dir("hold-two");
    let first = store_lock(one.path());
    let second = store_lock(two.path());

    let hold = StoreHold::new(vec![
        first.acquire(Intent::Read).await.expect("acquires"),
        second.acquire(Intent::Write).await.expect("acquires"),
    ]);
    assert_eq!(hold.len(), 2);
    assert!(first.is_held() && second.is_held());
    assert!(hold.is_stale(), "neither store has observed anything yet");

    drop(hold);
    assert!(!first.is_held(), "released with the hold");
    assert!(!second.is_held(), "and so is the other");
}

/// An in-memory store has no flock, so a hold over nothing is legitimate
/// rather than a bug to guard against.
#[test]
fn a_hold_over_no_on_disk_store_is_empty_and_fresh() {
    let hold = StoreHold::default();
    assert!(hold.is_empty());
    assert!(!hold.is_stale(), "nothing on disk cannot have gone stale");
}

/// A recreated store directory reads as changed rather than as older, which
/// is why the comparison is equality and never ordering.
#[tokio::test]
async fn a_reset_epoch_reads_as_changed() {
    let dir = dir("reset");
    let lock = store_lock(dir.path());

    put_epoch(dir.path(), 42, 9);
    let mut observed = lock.acquire(Intent::Read).await.expect("acquires");
    observed.note_refreshed();
    drop(observed);

    put_epoch(dir.path(), 42, 0);
    let after = lock.acquire(Intent::Read).await.expect("acquires");
    assert!(after.is_stale(), "backwards is still different");
}

/// **A store deleted and recreated is stale even at the same count.** This is
/// the case a bare counter cannot express: counting restarts from one, so the
/// new store's first write lands on the same count as the old store's, and a
/// process still holding the old state would otherwise reuse it against a
/// directory sharing nothing with it but a path.
#[tokio::test]
async fn a_recreated_store_is_stale_at_the_same_count() {
    let dir = dir("recreated");
    let lock = store_lock(dir.path());

    // This process writes, and observes what it wrote.
    let mut mine = lock.acquire(Intent::Write).await.expect("acquires");
    mine.note_refreshed();
    drop(mine);
    assert_eq!(
        epoch_count(dir.path()),
        Some(1),
        "the first write counts one"
    );

    // Another process removes the directory's contents and writes it afresh,
    // reaching the same count by the same route.
    std::fs::remove_file(dir.path().join(EPOCH_FILE)).expect("removes");
    let elsewhere = StoreLock::new(dir.path()).expect("locks");
    drop(elsewhere.acquire(Intent::Write).await.expect("acquires"));
    assert_eq!(
        epoch_count(dir.path()),
        Some(1),
        "and so does the next store's"
    );

    let after = lock.acquire(Intent::Read).await.expect("acquires");
    assert!(
        after.is_stale(),
        "the same count in a different store is not the state this one holds"
    );
}

/// **Every write in one store's lifetime keeps its nonce**, so the count is
/// what distinguishes them and staleness does not depend on the nonce changing.
#[tokio::test]
async fn writes_within_one_lifetime_share_a_nonce() {
    let dir = dir("lifetime");
    let lock = store_lock(dir.path());

    drop(lock.acquire(Intent::Write).await.expect("acquires"));
    let Epoch::At(first) = epoch(dir.path()) else {
        panic!("a writer records an epoch");
    };

    drop(lock.acquire(Intent::Write).await.expect("acquires"));
    let Epoch::At(second) = epoch(dir.path()) else {
        panic!("a writer records an epoch");
    };

    assert_eq!(first.nonce, second.nonce, "one lifetime, one nonce");
    assert_eq!(second.count, first.count.wrapping_add(1), "the count moves");
}

/// **A write that ends while a scan runs keeps the store claimed.** The writer is no
/// longer in flight when the scan reports, so only the count of finished writes can
/// tell the scan it may have passed the bucket that writer marked.
#[tokio::test]
async fn a_write_that_ends_during_a_scan_keeps_the_store_claimed() {
    let dir = dir("ends-during-scan");
    let lock = store_lock(dir.path());

    let mut first = lock.acquire(Intent::Write).await.expect("acquires");
    first.note_refreshed();
    lock.mark_dirty();
    drop(first);

    let scan = lock
        .clean_scan_start()
        .expect("dirty, with nothing in flight");
    // Starts and ends inside the scan, marking a bucket the scan had already passed.
    drop(lock.acquire(Intent::Write).await.expect("joins"));
    lock.clear_dirty(scan);
    assert!(
        lock.is_held(),
        "a write that ended during the scan may have marked a bucket it passed"
    );

    let scan = lock.clean_scan_start().expect("nothing in flight");
    lock.clear_dirty(scan);
    assert!(!lock.is_held(), "a scan started after it is evidence");
}

/// A store that is not marked dirty offers no scan, so a flush over a clean store
/// does not visit a single bucket flag.
#[tokio::test]
async fn a_clean_store_offers_no_scan() {
    let dir = dir("clean-scan");
    let lock = store_lock(dir.path());

    let mut reader = lock.acquire(Intent::Read).await.expect("acquires");
    reader.note_refreshed();
    assert_eq!(lock.clean_scan_start(), None, "nothing is marked dirty");
    drop(reader);
    assert_eq!(lock.clean_scan_start(), None, "nor once released");
}

/// **A writer that joins without the gate is counted until it drops**, so a scan
/// cannot start while it may still be about to mark a bucket.
#[tokio::test]
async fn a_writer_that_joins_is_counted_until_it_drops() {
    let dir = dir("joined-writer");
    let lock = store_lock(dir.path());

    let mut first = lock.acquire(Intent::Write).await.expect("acquires");
    first.note_refreshed();
    lock.mark_dirty();
    let second = lock
        .acquire(Intent::Write)
        .await
        .expect("joins without the gate");
    drop(first);
    assert_eq!(
        lock.clean_scan_start(),
        None,
        "the writer that joined is still in flight"
    );

    drop(second);
    let scan = lock.clean_scan_start().expect("nothing in flight");
    lock.clear_dirty(scan);
    assert!(!lock.is_held());
}

/// **A stale guard keeps every other acquisition out until its caller has
/// reloaded**, and the acquisition it held back then joins the state the reload
/// established.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_guard_keeps_other_acquisitions_out_until_it_has_reloaded() {
    let dir = dir("gate");
    let lock = store_lock(dir.path());

    let mut reloading = lock.acquire(Intent::Read).await.expect("acquires");
    assert!(reloading.is_stale(), "nothing observed yet");

    let waiting = {
        let lock = Arc::clone(&lock);
        lore_base::lore_spawn!(async move {
            lock.acquire(Intent::Read)
                .await
                .map(|guard| guard.is_stale())
        })
    };
    // The gate's own reference, the stale guard's, and the waiter's while it waits.
    assert!(
        eventually(|| Arc::strong_count(&lock.shared.gate) == 3).await,
        "the acquisition reaches the gate"
    );
    assert!(
        !waiting.is_finished(),
        "an acquisition must wait while the store is being reloaded"
    );

    reloading.note_refreshed();
    let stale = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the reload releases the acquisition it held back")
        .expect("the task completes")
        .expect("acquires");
    assert!(!stale, "and it joins the state the reload established");
    drop(reloading);
}

/// **An advance that cannot be written fails the acquisition and leaves nothing
/// counted**, on the path that takes the flock and on the path that joins it. A
/// count left behind would pin the flock forever.
#[tokio::test]
async fn a_failed_epoch_write_leaves_nothing_counted() {
    let dir = dir("failed-advance");
    let lock = store_lock(dir.path());
    // A directory where the epoch file belongs: it cannot be written, and a read of
    // it fails without being absent.
    std::fs::create_dir(dir.path().join(EPOCH_FILE)).expect("a directory in the file's place");
    assert_eq!(
        epoch(dir.path()),
        Epoch::Unreadable,
        "a failure to read is never mistaken for absence"
    );

    assert!(
        lock.acquire(Intent::Write).await.is_err(),
        "an advance that cannot be written fails the acquisition"
    );
    assert!(!lock.is_held(), "and takes nothing with it");

    let mut reader = lock
        .acquire(Intent::Read)
        .await
        .expect("a reader writes nothing");
    reader.note_refreshed();
    assert!(
        lock.acquire(Intent::Write).await.is_err(),
        "joining the hold, the advance still fails"
    );
    // A scan is offered only with no writer in flight, so a failed advance that
    // left its writer counted would refuse it.
    lock.mark_dirty();
    let scan = lock
        .clean_scan_start()
        .expect("and counts no writer in flight");
    lock.clear_dirty(scan);
    drop(reader);
    assert!(
        !lock.is_held(),
        "so the flock goes with the last real guard"
    );
}

/// **A writer joining a hold adopts the epoch it wrote**, not the one it replaced,
/// so its next operation in the same hold is not stale against its own write.
#[tokio::test]
async fn a_writer_joining_a_hold_adopts_the_epoch_it_wrote() {
    let dir = dir("joined-adopt");
    let lock = store_lock(dir.path());

    let mut reader = lock.acquire(Intent::Read).await.expect("acquires");
    reader.note_refreshed();
    let writer = lock
        .acquire(Intent::Write)
        .await
        .expect("joins and advances");
    assert!(!writer.is_stale(), "its own advance does not make it stale");
    let after = lock.acquire(Intent::Read).await.expect("joins");
    assert!(
        !after.is_stale(),
        "nor does it make the next operation stale"
    );
    drop((reader, writer, after));
}

/// **Two spellings of one directory share one flock.** A second flock on the same
/// file would exclude the first inside this process and wait on itself forever.
#[tokio::test]
async fn two_spellings_of_one_directory_share_one_flock() {
    let dir = dir("spelling");
    std::fs::create_dir(dir.path().join("sub")).expect("a subdirectory");
    let plain = store_lock(dir.path());
    let roundabout = store_lock(&dir.path().join("sub").join(".."));
    assert!(
        Arc::ptr_eq(&plain.shared, &roundabout.shared),
        "one directory, one flock"
    );

    let mut held = plain.acquire(Intent::Read).await.expect("acquires");
    held.note_refreshed();
    let joined = tokio::time::timeout(Duration::from_secs(5), roundabout.acquire(Intent::Read))
        .await
        .expect("does not wait on itself")
        .expect("acquires");
    drop((joined, held));
}

/// An unreadable epoch never matches, not even itself, so a store that reloaded over
/// one is still stale on its next acquisition.
#[tokio::test]
async fn an_unreadable_epoch_stays_stale_after_a_reload() {
    let dir = dir("unreadable-stays");
    let lock = store_lock(dir.path());
    std::fs::write(dir.path().join(EPOCH_FILE), [1u8, 2, 3]).expect("writes");

    let mut first = lock.acquire(Intent::Read).await.expect("acquires");
    assert!(first.is_stale());
    first.note_refreshed();
    drop(first);

    let second = lock.acquire(Intent::Read).await.expect("acquires");
    assert!(
        second.is_stale(),
        "an unreadable epoch proves nothing twice"
    );
}

/// **An absent epoch is reusable.** Two looks that both find no epoch file agree, so
/// a store nobody has written does not reload on every acquisition.
#[tokio::test]
async fn an_absent_epoch_is_reusable() {
    let dir = dir("absent-reuse");
    let lock = store_lock(dir.path());

    let mut first = lock.acquire(Intent::Read).await.expect("acquires");
    assert!(first.is_stale(), "nothing observed yet");
    first.note_refreshed();
    drop(first);
    assert!(!lock.is_held());

    let second = lock.acquire(Intent::Read).await.expect("acquires");
    assert!(
        !second.is_stale(),
        "two looks that both found no epoch agree"
    );
}

/// Only a file of exactly an epoch's length is an epoch: a bare count is too short,
/// and anything longer is not an epoch this build wrote.
#[test]
fn only_an_epoch_length_file_is_an_epoch() {
    let dir = dir("lengths");
    std::fs::write(dir.path().join(EPOCH_FILE), 7u64.to_le_bytes()).expect("writes");
    assert_eq!(epoch(dir.path()), Epoch::Unreadable, "a count alone");
    std::fs::write(dir.path().join(EPOCH_FILE), [0u8; EPOCH_BYTES + 1]).expect("writes");
    assert_eq!(epoch(dir.path()), Epoch::Unreadable, "one byte too many");
    std::fs::write(dir.path().join(EPOCH_FILE), [0u8; EPOCH_BYTES]).expect("writes");
    assert!(
        matches!(epoch(dir.path()), Epoch::At(_)),
        "exactly an epoch"
    );
}

/// The on-disk layout is the nonce then the count, each little-endian, and reading
/// it back is the exact inverse — pinned byte for byte, so a round trip through a
/// matching pair of mistakes cannot pass.
#[test]
fn the_epoch_file_is_nonce_then_count_little_endian() {
    let stamp = Stamp {
        nonce: 0x0102_0304_0506_0708,
        count: 0x1112_1314_1516_1718,
    };
    let bytes = stamp.to_bytes();
    assert_eq!(
        bytes,
        [
            0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, 0x18, 0x17, 0x16, 0x15, 0x14, 0x13,
            0x12, 0x11,
        ]
    );
    assert_eq!(Stamp::from_bytes(bytes), stamp);
}

/// A count that reaches the end of its range wraps within the same lifetime.
#[tokio::test]
async fn a_count_wraps_within_its_lifetime() {
    let dir = dir("wrap");
    let lock = store_lock(dir.path());
    put_epoch(dir.path(), 42, u64::MAX);

    drop(lock.acquire(Intent::Write).await.expect("acquires"));
    let Epoch::At(stamp) = epoch(dir.path()) else {
        panic!("a writer records an epoch");
    };
    assert_eq!((stamp.nonce, stamp.count), (42, 0));
}

/// A write over an unreadable epoch begins a lifetime rather than continuing one it
/// cannot read.
#[tokio::test]
async fn an_unreadable_epoch_begins_a_new_lifetime() {
    let dir = dir("unreadable-lifetime");
    let lock = store_lock(dir.path());
    std::fs::write(dir.path().join(EPOCH_FILE), [1u8, 2, 3]).expect("writes");

    drop(lock.acquire(Intent::Write).await.expect("acquires"));
    assert_eq!(epoch_count(dir.path()), Some(1));
}

/// **A writer is counted in before its advance, a second writer waits for that
/// advance, and then joins the epoch the first adopted.** The first writer is
/// stopped inside its epoch write, with the flock observed and its advance not yet
/// committed — the window all three properties are about:
///
/// - the reader leaving must not release the flock under the writer, which it
///   would if the writer counted itself in only after the write;
/// - the second writer must not pass the gate and advance the same epoch again,
///   which it would if the gate were released before the advance is committed;
/// - and once through, it must find the store current, which it would not if the
///   first writer adopted its epoch only after letting the gate go.
///
/// The second writer is polled by this test rather than spawned. A spawned waiter is
/// woken onto the worker that released the gate and runs once the task releasing it
/// yields — after whatever that task does next, which is what the third property is
/// about. Polled here, it runs on this thread as soon as it is handed the gate.
///
/// What is seen inside the window is recorded, and asserted only once the first
/// writer has been let go, so a regression fails this test rather than leaving a
/// runtime thread stopped at the barrier.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_writer_is_counted_before_its_advance_and_a_second_waits_for_it() {
    let dir = dir("advance-window");
    let lock = store_lock(dir.path());

    let mut reader = lock.acquire(Intent::Read).await.expect("acquires");
    reader.note_refreshed();

    let barrier = epoch_write_barrier::install(&lock.shared.epoch_path);
    let active = |lock: &StoreLock| lock.shared.state.lock().active;
    let first = {
        let lock = Arc::clone(&lock);
        lore_base::lore_spawn!(async move {
            lock.acquire(Intent::Write)
                .await
                .map(|guard| guard.is_stale())
        })
    };
    let first_counted = eventually(|| barrier.reached() == 1).await && active(&lock) == 2;

    let second = lock.acquire(Intent::Write);
    tokio::pin!(second);
    let mut second_early = None;
    // The gate's own reference, the first writer's, and the second writer's while it
    // waits for it.
    let second_waited = tokio::select! {
        biased;
        early = &mut second => {
            second_early = Some(early);
            false
        }
        parked = eventually(|| Arc::strong_count(&lock.shared.gate) == 3) => parked,
    } && barrier.reached() == 1
        && active(&lock) == 2;

    drop(reader);
    let held_after_reader = lock.is_held();
    // Kept until both writers are done, so the flock outlives the first writer's own
    // guard. Released instead, the second writer would take the flock afresh and
    // advance again, which is a different path from the one under test.
    let keeper = lock.acquire(Intent::Read).await.expect("joins the hold");

    barrier.release();
    // The second writer first: it is handed the gate the moment the first lets go.
    let second = match second_early {
        Some(early) => Ok(early),
        None => tokio::time::timeout(Duration::from_secs(5), &mut second).await,
    };
    let first = tokio::time::timeout(Duration::from_secs(5), first).await;
    let advances = barrier.reached();
    drop(keeper);
    drop(barrier);

    assert!(
        first_counted,
        "the first writer counted itself in before its advance"
    );
    assert!(
        second_waited,
        "the second writer waits at the gate rather than advancing alongside the first"
    );
    assert!(
        held_after_reader,
        "a writer counted in before its advance keeps the flock when the reader leaves"
    );
    let first_stale = first
        .expect("the first writer finishes")
        .expect("the task completes")
        .expect("acquires");
    assert!(
        !first_stale,
        "the reader's reload stands for the first writer"
    );
    let second_stale = second
        .expect("the second writer finishes")
        .expect("acquires")
        .is_stale();
    assert!(
        !second_stale,
        "and the second finds the epoch the first adopted before letting the gate go"
    );
    assert_eq!(advances, 1, "exactly one advance was written");
    assert_eq!(epoch_count(dir.path()), Some(1));
}

/// **A writer that takes the flock adopts its epoch before it lets the gate go.** An
/// acquisition waiting at the gate is handed it the moment it is released, and one that
/// found the store not yet adopted would report stale — and its caller would reload the
/// store underneath the writer that had just announced a write to it.
///
/// The waiting acquisition is polled by this test rather than spawned, for the reason
/// the test above gives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_writer_that_takes_the_flock_adopts_before_letting_the_gate_go() {
    let dir = dir("slow-path-adopt");
    let lock = store_lock(dir.path());

    let mut opening = lock.acquire(Intent::Read).await.expect("acquires");
    opening.note_refreshed();
    drop(opening);
    assert!(!lock.is_held(), "the writer below takes the flock afresh");

    let barrier = epoch_write_barrier::install(&lock.shared.epoch_path);
    let writer = {
        let lock = Arc::clone(&lock);
        lore_base::lore_spawn!(async move {
            lock.acquire(Intent::Write)
                .await
                .map(|guard| guard.is_stale())
        })
    };
    let writer_advancing = eventually(|| barrier.reached() == 1).await;

    let reader = lock.acquire(Intent::Read);
    tokio::pin!(reader);
    let mut reader_early = None;
    // The gate's own reference, the writer's, and the reader's while it waits for it.
    let reader_waited = tokio::select! {
        biased;
        early = &mut reader => {
            reader_early = Some(early);
            false
        }
        parked = eventually(|| Arc::strong_count(&lock.shared.gate) == 3) => parked,
    };

    barrier.release();
    let reader = match reader_early {
        Some(early) => Ok(early),
        None => tokio::time::timeout(Duration::from_secs(5), &mut reader).await,
    };
    let writer = tokio::time::timeout(Duration::from_secs(5), writer).await;
    drop(barrier);

    assert!(
        writer_advancing,
        "the writer took the flock and stopped in its advance"
    );
    assert!(
        reader_waited,
        "the reader waits at the gate the writer holds"
    );
    let writer_stale = writer
        .expect("the writer finishes")
        .expect("the task completes")
        .expect("acquires");
    assert!(!writer_stale, "the opening reload stands for the writer");
    let reader_stale = reader
        .expect("the reader finishes")
        .expect("acquires")
        .is_stale();
    assert!(
        !reader_stale,
        "and the reader is handed a store the writer has already adopted"
    );
}

/// **A lock file replaced while the store is idle is the one locked next.** A store
/// directory deleted and recreated has a new lock file, which another process locks;
/// an acquisition that locked a descriptor kept for the old file would exclude nobody.
#[tokio::test]
async fn an_acquisition_locks_the_lock_file_the_path_names_now() {
    let dir = dir("lock-replaced");
    let lock = store_lock(dir.path());
    let mut first = lock.acquire(Intent::Read).await.expect("acquires");
    first.note_refreshed();
    drop(first);
    assert!(!lock.is_held());

    std::fs::remove_file(dir.path().join("lock")).expect("removes the lock file");
    let elsewhere = FSLock::acquire_exact_path(&dir.path().join("lock"))
        .await
        .expect("another process locks the lock file that replaces it");

    let waited = tokio::time::timeout(Duration::from_millis(50), lock.acquire(Intent::Read)).await;
    assert!(
        waited.is_err(),
        "the acquisition waits for the process holding the file the path names"
    );
    drop(elsewhere);
    tokio::time::timeout(Duration::from_secs(5), lock.acquire(Intent::Read))
        .await
        .expect("and takes it once that process lets go")
        .expect("acquires");
}

/// **Trying to acquire leaves a store for later rather than waiting for it**, whether
/// this process has it mid-transition or another process holds it, and otherwise
/// takes or joins the lock as an acquisition does.
#[tokio::test]
async fn trying_to_acquire_leaves_a_store_in_transition_or_held_elsewhere() {
    let dir = dir("try");
    let lock = store_lock(dir.path());

    // Mid-transition here: a stale guard holds the gate while its caller reloads.
    let reloading = lock.acquire(Intent::Read).await.expect("acquires");
    assert!(reloading.is_stale());
    let during = tokio::time::timeout(Duration::from_secs(5), lock.try_acquire(Intent::Read))
        .await
        .expect("does not wait for the gate")
        .expect("attempts");
    assert!(during.is_none(), "a store being reloaded is left for later");
    drop(reloading);

    let elsewhere = FSLock::acquire_exact_path(&dir.path().join("lock"))
        .await
        .expect("another process locks the store");
    let refused = tokio::time::timeout(Duration::from_secs(5), lock.try_acquire(Intent::Read))
        .await
        .expect("does not wait for the flock")
        .expect("attempts");
    assert!(
        refused.is_none(),
        "a store another process holds is left for later"
    );
    assert!(!lock.is_held(), "and nothing is taken");
    drop(elsewhere);

    let mut taken = lock
        .try_acquire(Intent::Read)
        .await
        .expect("attempts")
        .expect("a free store is taken");
    assert!(taken.is_stale(), "reporting what any acquisition would");
    taken.note_refreshed();
    let joined = lock
        .try_acquire(Intent::Read)
        .await
        .expect("attempts")
        .expect("a store this process holds is joined");
    assert!(!joined.is_stale());
    drop((joined, taken));
    assert!(!lock.is_held());
}

/// **A hold stays current over an epoch it cannot read.** The comparison that fails
/// safe belongs to the start of a hold; inside one, nothing outside this process can
/// write, so the reload that began it stands. Otherwise every operation joining the
/// hold would reload the store — underneath the work already running on it.
#[tokio::test]
async fn a_hold_stays_current_over_an_epoch_it_cannot_read() {
    let dir = dir("unreadable-hold");
    let lock = store_lock(dir.path());
    std::fs::write(dir.path().join(EPOCH_FILE), [1u8, 2, 3]).expect("writes");

    let mut span = lock.acquire(Intent::Read).await.expect("acquires");
    assert!(span.is_stale(), "an unreadable epoch proves nothing");
    span.note_refreshed();

    let joined = lock.acquire(Intent::Read).await.expect("joins");
    assert!(
        !joined.is_stale(),
        "an operation joining the hold keeps the reload that began it"
    );
    let writer = lock
        .acquire(Intent::Write)
        .await
        .expect("joins and advances");
    assert!(
        !writer.is_stale(),
        "and so does a writer, whose advance replaces the epoch nobody could read"
    );
    assert_eq!(epoch_count(dir.path()), Some(1));
    drop((joined, writer, span));

    let again = lock.acquire(Intent::Read).await.expect("acquires");
    assert!(
        !again.is_stale(),
        "and the next hold finds the epoch this store wrote"
    );
}

/// Creates a FIFO at `path`.
#[cfg(unix)]
fn make_fifo(path: &Path) {
    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("a C path");
    // SAFETY: `name` is a valid NUL-terminated path that outlives the call.
    assert_eq!(
        unsafe { libc::mkfifo(name.as_ptr(), 0o600) },
        0,
        "creates the FIFO"
    );
}

/// **A FIFO where the epoch belongs reads as unreadable, without waiting.** Opened
/// the ordinary way, a FIFO blocks until a writer comes, and the read happens with
/// the flock and the gate held — so a process would stop, and hold every other one
/// out, for as long as nobody wrote to it.
#[cfg(unix)]
#[tokio::test]
async fn an_epoch_that_is_a_fifo_reads_as_unreadable_without_blocking() {
    use std::os::unix::fs::OpenOptionsExt;

    let dir = dir("fifo-read");
    let fifo = dir.path().join(EPOCH_FILE);
    make_fifo(&fifo);

    let reading = {
        let fifo = fifo.clone();
        lore_base::lore_spawn_blocking!(move || read_epoch(&fifo))
    };
    let read = tokio::time::timeout(Duration::from_secs(5), reading).await;
    if read.is_err() {
        // A writer lets a read stopped in the open through, so a regression fails
        // here rather than leaving a thread in an open that never returns.
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo);
    }
    assert_eq!(
        read.expect("a FIFO is read without waiting for a writer")
            .expect("the read completes"),
        Epoch::Unreadable
    );
}

/// **A link where the epoch belongs is neither read through nor written through.**
/// Read, it would present whatever file it names as this store's epoch; written, an
/// advance would overwrite that file.
#[cfg(unix)]
#[tokio::test]
async fn an_epoch_link_is_neither_read_through_nor_written_through() {
    let dir = dir("link");
    let lock = store_lock(dir.path());
    let target = dir.path().join("elsewhere");
    let planted = Stamp {
        nonce: 42,
        count: 7,
    }
    .to_bytes();
    std::fs::write(&target, planted).expect("writes the file the link names");
    std::os::unix::fs::symlink(&target, dir.path().join(EPOCH_FILE)).expect("links");

    assert_eq!(
        epoch(dir.path()),
        Epoch::Unreadable,
        "a link is not an epoch, whatever it names"
    );

    drop(lock.acquire(Intent::Write).await.expect("acquires"));
    assert_eq!(
        std::fs::read(&target).expect("reads the file the link named"),
        planted,
        "the advance did not write through the link"
    );
    assert!(
        std::fs::symlink_metadata(dir.path().join(EPOCH_FILE))
            .expect("an epoch")
            .file_type()
            .is_file(),
        "it replaced the link with an epoch of its own"
    );
    assert_eq!(
        epoch_count(dir.path()),
        Some(1),
        "beginning a lifetime, since the link proved nothing"
    );
}

/// **An advance replaces what an unfinished one left staged**, rather than failing
/// every write claim on the store from then on.
#[tokio::test]
async fn a_leftover_staging_file_does_not_block_an_advance() {
    let dir = dir("staging-leftover");
    let lock = store_lock(dir.path());
    std::fs::write(
        dir.path().join(EPOCH_STAGING_FILE),
        b"left by an advance that never finished",
    )
    .expect("writes");

    drop(lock.acquire(Intent::Write).await.expect("acquires"));
    assert_eq!(epoch_count(dir.path()), Some(1));
    assert!(
        !dir.path().join(EPOCH_STAGING_FILE).exists(),
        "and nothing is left staged"
    );
}
