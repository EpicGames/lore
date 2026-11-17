// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! File reads that keep their temporary file when they fail and resume from it
//! ([`ReadOptions::resume_write`]): what a resumed read loads again, and what it leaves on disk.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;

use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
use lore_storage::FlushOptions;
use lore_storage::FragmentReference;
use lore_storage::options::RESUME_WRITE_MIN_LEN;
use lore_storage::test_util::HookedImmutableStore;
use lore_storage::test_util::ImmutableCall;
use lore_storage::test_util::hold_rename_of;
use lore_storage::test_util::moving_temporary_files;
use lore_storage::test_util::noting_lock_waits;
use lore_storage::test_util::refusing_locks;
use tokio::sync::Notify;
use zerocopy::IntoBytes;

use super::*;

/// The leaves of each content here: enough for a range of [`RESUME_WRITE_MIN_LEN`] that clips a
/// leaf at each end.
const LEAVES: usize = RESUME_WRITE_MIN_LEN / LEAF + 1;

/// The size of each leaf, the most a fragment holds.
const LEAF: usize = FRAGMENT_SIZE_THRESHOLD;

/// The leaf a first read fails at, unless a test names another.
const FAILING: usize = 8;

/// The time a wait that should end is given before the test fails.
const PATIENCE: Duration = Duration::from_secs(10);

fn partition() -> Partition {
    Partition::from([0xC7; 16])
}

fn context() -> Context {
    Context::from([0xC7; 16])
}

/// The bytes of leaf `index` stored with `seed`, distinct for each pair: the index and the seed,
/// then a fill.
fn leaf_bytes(index: usize, seed: u8) -> Vec<u8> {
    let mut bytes = vec![seed.wrapping_add(0x5A); LEAF];
    bytes[..8].copy_from_slice(&(index as u64).to_le_bytes());
    bytes[8] = seed;
    bytes
}

/// Content of [`LEAVES`] leaves, leaf `index` stored with `seeds[index]`: the list's address, the
/// leaves' addresses in content order, and the content.
async fn put_leaves(
    store: &Arc<dyn ImmutableStore>,
    seeds: &[u8; LEAVES],
) -> (Address, Vec<Address>, Vec<u8>) {
    let leaves = seeds
        .iter()
        .enumerate()
        .map(|(index, seed)| leaf_bytes(index, *seed));
    put_list_of(store, leaves).await
}

/// Content of the leaves `leaf_contents`, in order: the list's address, the leaves' addresses in
/// content order, and the content.
async fn put_list_of(
    store: &Arc<dyn ImmutableStore>,
    leaf_contents: impl Iterator<Item = Vec<u8>>,
) -> (Address, Vec<Address>, Vec<u8>) {
    let (root, leaves, content) = put_entries(store, leaf_contents.map(Some)).await;
    let leaves = leaves
        .into_iter()
        .map(|leaf| leaf.expect("a leaf"))
        .collect();
    (root, leaves, content)
}

/// Content of `entries`, in order, each a leaf or, where `None`, a hole of a leaf's length: the
/// list's address, each entry's leaf address in content order, and the content.
async fn put_entries(
    store: &Arc<dyn ImmutableStore>,
    entries: impl Iterator<Item = Option<Vec<u8>>>,
) -> (Address, Vec<Option<Address>>, Vec<u8>) {
    let mut leaves = Vec::with_capacity(LEAVES);
    let mut refs = Vec::with_capacity(LEAVES);
    let mut content = Vec::with_capacity(LEAVES * LEAF);
    for entry in entries {
        let offset_content = content.len() as u64;
        let Some(leaf) = entry else {
            refs.push(FragmentReference {
                hash: Hash::default(),
                offset_content,
            });
            leaves.push(None);
            content.resize(content.len() + LEAF, 0);
            continue;
        };
        let bytes = Bytes::from(leaf);
        let address = put_whole(store, partition(), context(), &bytes).await;
        refs.push(FragmentReference {
            hash: address.hash,
            offset_content,
        });
        leaves.push(Some(address));
        content.extend_from_slice(&bytes);
    }
    let list = Bytes::copy_from_slice(refs.as_bytes());
    let root = Address {
        hash: hash::hash_slice(list.as_ref()),
        context: context(),
    };
    store
        .clone()
        .put(
            partition(),
            root,
            Fragment {
                flags: FragmentFlags::PayloadFragmented.bits(),
                size_payload: list.len() as u32,
                size_content: content.len() as u64,
            },
            Some(list),
            false,
        )
        .await
        .expect("put the fragment list");
    (root, leaves, content)
}

/// `store`, failing every load of `failing`, with the address of every load it forwards kept in
/// the returned list.
fn hooked(
    store: &Arc<dyn ImmutableStore>,
    failing: Option<Address>,
) -> (Arc<dyn ImmutableStore>, Arc<Mutex<Vec<Address>>>) {
    let loads = Arc::new(Mutex::new(Vec::new()));
    let kept = loads.clone();
    let hooked = HookedImmutableStore::wrap(store.clone(), move |call, _partition, address| {
        if call != ImmutableCall::Get {
            return Ok(());
        }
        if Some(address) == failing {
            return Err(StoreError::from(Disconnected));
        }
        kept.lock().unwrap().push(address);
        Ok(())
    });
    (hooked, loads)
}

/// A load held at the store, and what releases it.
struct Park {
    /// Notified once the load is held.
    reached: Arc<Notify>,
    release: mpsc::Sender<()>,
}

/// `store`, holding the first load of `parked` until the returned [`Park`] releases it. The hold
/// blocks the thread the load runs on, so the runtime of a test using it needs more than one.
fn parking(store: &Arc<dyn ImmutableStore>, parked: Address) -> (Arc<dyn ImmutableStore>, Park) {
    let reached = Arc::new(Notify::new());
    let (release, released) = mpsc::channel::<()>();
    let released = Mutex::new(released);
    let taken = AtomicBool::new(false);
    let notify = reached.clone();
    let hooked = HookedImmutableStore::wrap(store.clone(), move |call, _partition, address| {
        if call == ImmutableCall::Get && address == parked && !taken.swap(true, Ordering::Relaxed) {
            notify.notify_one();
            let _ = released.lock().unwrap().recv();
        }
        Ok(())
    });
    (hooked, Park { reached, release })
}

/// Waits until nothing but the caller holds `store`: every task of the reads through it has ended.
async fn until_released(store: &Arc<dyn ImmutableStore>) {
    tokio::time::timeout(PATIENCE, async {
        while Arc::strong_count(store) > 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the reads through the store end");
}

/// The indices of the `leaves` loaded to be written, in order and once each. The walk of the list
/// loads its first leaf once to learn that the list holds leaves, so that leaf counts only from
/// its second load.
fn leaves_loaded(loads: &Mutex<Vec<Address>>, leaves: &[Address]) -> Vec<usize> {
    let loads = loads.lock().unwrap();
    (0..leaves.len())
        .filter(|index| {
            let count = loads.iter().filter(|load| **load == leaves[*index]).count();
            count > usize::from(*index == 0)
        })
        .collect()
}

/// A read of `range` of `address` from `store` into `target` that resumes.
async fn read_resuming(
    store: Arc<dyn ImmutableStore>,
    address: Address,
    target: &Path,
    range: Option<Range<usize>>,
) -> Result<(Fragment, Option<std::fs::Metadata>), StorageError> {
    read_into_file(
        store,
        partition(),
        address,
        target,
        ".loretmp",
        range,
        no_remote().resuming_write(),
        None,
    )
    .await
}

/// A read of `address` into `target` that fails at its leaf `failing`, leaving the leaves before
/// it in the temporary file.
async fn read_failing_at(
    store: &Arc<dyn ImmutableStore>,
    address: Address,
    failing: Address,
    target: &Path,
    range: Option<Range<usize>>,
) {
    let (failing, _loads) = hooked(store, Some(failing));
    read_resuming(failing, address, target, range)
        .await
        .expect_err("the read fails at the failing leaf");
}

/// How many references to its store a load of `leaf` holds while it is held at the store, as a
/// read's fetch of a leaf holds them: the load is made as the fetch makes it, through
/// [`load_fragment`], held, counted and let go.
async fn references_of_a_held_load(store: &Arc<dyn ImmutableStore>, leaf: Address) -> usize {
    let (parked, park) = parking(store, leaf);
    let load = lore_base::lore_spawn!(load_fragment(
        parked.clone(),
        partition(),
        leaf,
        no_remote().with_decompress(),
        None,
    ));
    park.reached.notified().await;
    let references = Arc::strong_count(&parked) - 1;
    let _ = park.release.send(());
    load.await
        .expect("the load's task ends")
        .expect("the load completes");
    references
}

/// The temporary file of `target`, a `.bin` file.
fn temporary_file(target: &Path) -> PathBuf {
    target.with_extension("bin.loretmp")
}

/// The names in `directory`, sorted.
fn names(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .expect("list the directory")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// A read that fails keeps its temporary file and the resume record beside it, and a read of the
/// same content resumes from them: it loads none of the leaves the failed read wrote, and leaves
/// the content at the target and nothing beside it.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_read_resumes_and_loads_only_the_rest() {
    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-rest-");
    let target = target_dir.path().join("content.bin");
    let (root, leaves, content) = put_leaves(&store, &[0; LEAVES]).await;

    read_failing_at(&store, root, leaves[FAILING], &target, None).await;
    assert_eq!(
        names(target_dir.path()),
        ["content.bin.loretmp", "content.bin.loretmp.rec"],
        "the failed read keeps its temporary file and its resume record, and writes no target"
    );

    let (resumed, loads) = hooked(&store, None);
    read_resuming(resumed, root, &target, None)
        .await
        .expect("the resumed read completes");

    let loaded = leaves_loaded(&loads, &leaves);
    assert!(
        loaded.contains(&FAILING),
        "the leaf the read failed at is loaded: {loaded:?}"
    );
    assert!(
        loaded.iter().all(|index| *index >= FAILING),
        "a leaf the failed read wrote is loaded again: {loaded:?}"
    );
    assert_eq!(std::fs::read(&target).expect("read the target"), content);
    assert_eq!(names(target_dir.path()), ["content.bin"]);
}

/// A leaf of zeros the failed read wrote is loaded again, on every file system: bytes of zeros
/// cannot tell written zeros from a range no read wrote, so a leaf of zeros is never held.
#[tokio::test(flavor = "multi_thread")]
async fn a_written_leaf_of_zeros_is_loaded_again() {
    const ZEROED: usize = 3;

    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-written-zeros-");
    let target = target_dir.path().join("content.bin");
    let leaf_contents = (0..LEAVES).map(|index| {
        if index == ZEROED {
            vec![0; LEAF]
        } else {
            leaf_bytes(index, 0)
        }
    });
    let (root, leaves, content) = put_list_of(&store, leaf_contents).await;

    read_failing_at(&store, root, leaves[FAILING], &target, None).await;
    let (resumed, loads) = hooked(&store, None);
    read_resuming(resumed, root, &target, None)
        .await
        .expect("the resumed read completes");

    assert!(
        loads.lock().unwrap().contains(&leaves[ZEROED]),
        "the leaf of zeros is loaded again"
    );
    assert_eq!(std::fs::read(&target).expect("read the target"), content);
}

/// A leaf whose bytes in the temporary file were changed after the failed read wrote them is loaded
/// again, and only that one of the leaves it wrote.
#[tokio::test(flavor = "multi_thread")]
async fn a_corrupted_range_of_the_temporary_file_is_loaded_again() {
    const CORRUPTED: usize = 2;

    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-corrupt-");
    let target = target_dir.path().join("content.bin");
    let (root, leaves, content) = put_leaves(&store, &[0; LEAVES]).await;

    read_failing_at(&store, root, leaves[FAILING], &target, None).await;
    let temporary = temporary_file(&target);
    let mut held = std::fs::read(&temporary).expect("read the temporary file");
    held[CORRUPTED * LEAF + LEAF / 2] ^= 0xFF;
    std::fs::write(&temporary, held).expect("corrupt the temporary file");

    let (resumed, loads) = hooked(&store, None);
    read_resuming(resumed, root, &target, None)
        .await
        .expect("the resumed read completes");

    let loaded = leaves_loaded(&loads, &leaves);
    assert!(
        loaded.contains(&CORRUPTED) && loaded.contains(&FAILING),
        "the corrupted leaf and the failing one are loaded: {loaded:?}"
    );
    assert!(
        loaded
            .iter()
            .all(|index| *index == CORRUPTED || *index >= FAILING),
        "an intact leaf the failed read wrote is loaded again: {loaded:?}"
    );
    assert_eq!(std::fs::read(&target).expect("read the target"), content);
}

/// A temporary file cut short after the failed read is loaded again from the leaf the cut falls in.
#[tokio::test(flavor = "multi_thread")]
async fn a_truncated_temporary_file_is_loaded_again_from_the_cut() {
    const CUT: usize = 5;
    let last = LEAVES - 1;

    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-truncated-");
    let target = target_dir.path().join("content.bin");
    let (root, leaves, content) = put_leaves(&store, &[0; LEAVES]).await;

    read_failing_at(&store, root, leaves[last], &target, None).await;
    let temporary = temporary_file(&target);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&temporary)
        .expect("open the temporary file")
        .set_len((CUT * LEAF + LEAF / 2) as u64)
        .expect("cut the temporary file short");

    let (resumed, loads) = hooked(&store, None);
    read_resuming(resumed, root, &target, None)
        .await
        .expect("the resumed read completes");

    assert_eq!(
        leaves_loaded(&loads, &leaves),
        (CUT..LEAVES).collect::<Vec<_>>(),
        "the leaves from the cut on are loaded, and only those"
    );
    assert_eq!(std::fs::read(&target).expect("read the target"), content);
}

/// A temporary file cut inside a leaf whose second half is zeros is written again from the cut: the
/// bytes there read as the whole leaf, but its second half is a hole of the file, so the leaf is
/// loaded again and written whole. The file is written out before the cut, so the cut leaves a hole
/// past it on every file system: version 3.14 of the Resilient File System records the ranges past
/// a cut of data not yet written out as allocated once the file grows again, and the resumed read
/// then counts the leaf as held, which still reads back as the content.
#[tokio::test(flavor = "multi_thread")]
async fn a_leaf_cut_where_its_zeros_begin_is_written_again() {
    const CUT: usize = 5;
    let last = LEAVES - 1;

    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-cut-zeros-");
    if !crate::holes::holes_reported().await {
        return;
    }
    let target = target_dir.path().join("content.bin");
    let leaf_contents = (0..LEAVES).map(|index| {
        let mut bytes = leaf_bytes(index, 0);
        if index == CUT {
            bytes[LEAF / 2..].fill(0);
        }
        bytes
    });
    let (root, leaves, content) = put_list_of(&store, leaf_contents).await;

    read_failing_at(&store, root, leaves[last], &target, None).await;
    let temporary = std::fs::OpenOptions::new()
        .write(true)
        .open(temporary_file(&target))
        .expect("open the temporary file");
    temporary.sync_all().expect("write the temporary file out");
    temporary
        .set_len((CUT * LEAF + LEAF / 2) as u64)
        .expect("cut the temporary file where the zeros begin");
    drop(temporary);

    let (resumed, loads) = hooked(&store, None);
    read_resuming(resumed, root, &target, None)
        .await
        .expect("the resumed read completes");

    assert!(
        leaves_loaded(&loads, &leaves).contains(&CUT),
        "the cut leaf is loaded again"
    );
    assert_eq!(std::fs::read(&target).expect("read the target"), content);
    let whole = 0..content.len() as u64;
    assert_eq!(
        crate::holes::data_ranges(&target).await,
        vec![whole],
        "the cut leaf is written whole"
    );
}

/// A temporary file a read of other content left is discarded rather than resumed from, even where
/// that content shares leaves at the same offsets.
#[tokio::test(flavor = "multi_thread")]
async fn a_temporary_file_of_other_content_is_discarded() {
    let last = LEAVES - 1;
    let mut other_seeds = [0; LEAVES];
    other_seeds[last] = 1;

    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-other-");
    let target = target_dir.path().join("content.bin");
    let (first, first_leaves, _) = put_leaves(&store, &[0; LEAVES]).await;
    let (other, other_leaves, other_content) = put_leaves(&store, &other_seeds).await;
    assert_eq!(
        first_leaves[..last],
        other_leaves[..last],
        "the two contents share every leaf but the last"
    );

    read_failing_at(&store, first, first_leaves[last], &target, None).await;

    let (read, loads) = hooked(&store, None);
    read_resuming(read, other, &target, None)
        .await
        .expect("the read of the other content completes");

    assert_eq!(
        leaves_loaded(&loads, &other_leaves),
        (0..LEAVES).collect::<Vec<_>>(),
        "every leaf of the other content is loaded"
    );
    assert_eq!(
        std::fs::read(&target).expect("read the target"),
        other_content
    );
}

/// A ranged read, here of the shortest range that resumes, resumes to the same bytes: the leaves
/// the range clips are loaded again, since only a whole leaf can be checked against its address,
/// and the whole leaves the failed read wrote are not.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_ranged_read_resumes_to_the_same_bytes() {
    let range = LEAF / 2..LEAF / 2 + RESUME_WRITE_MIN_LEN;
    let last = range.end / LEAF;

    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-ranged-");
    let target = target_dir.path().join("content.bin");
    let (root, leaves, content) = put_leaves(&store, &[0; LEAVES]).await;

    read_failing_at(&store, root, leaves[FAILING], &target, Some(range.clone())).await;

    let (resumed, loads) = hooked(&store, None);
    read_resuming(resumed, root, &target, Some(range.clone()))
        .await
        .expect("the resumed read completes");

    let loaded = leaves_loaded(&loads, &leaves);
    assert!(
        loaded.contains(&0) && loaded.contains(&FAILING),
        "the clipped first leaf and the failing one are loaded: {loaded:?}"
    );
    assert!(
        loaded
            .iter()
            .all(|index| *index == 0 || (FAILING..=last).contains(index)),
        "a whole leaf the failed read wrote is loaded again, or one outside the range: {loaded:?}"
    );
    assert_eq!(
        std::fs::read(&target).expect("read the target"),
        content[range]
    );
}

/// A leaf the range clips is loaded again even where the temporary file holds bytes at its offset
/// that hash to the whole leaf: there, the first leaf and the one after it repeat one byte that is
/// not zero, so the bytes from the start of the file read as the whole first leaf, though only part
/// of it is there.
#[tokio::test(flavor = "multi_thread")]
async fn a_clipped_leaf_is_loaded_again_where_the_bytes_there_hash_to_it() {
    let range = LEAF / 2..LEAF / 2 + RESUME_WRITE_MIN_LEN;

    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-clipped-");
    let target = target_dir.path().join("content.bin");
    let leaf_contents = (0..LEAVES).map(|index| {
        if index < 2 {
            vec![0x11; LEAF]
        } else {
            leaf_bytes(index, 0)
        }
    });
    let (root, leaves, content) = put_list_of(&store, leaf_contents).await;

    read_failing_at(&store, root, leaves[FAILING], &target, Some(range.clone())).await;
    read_resuming(store, root, &target, Some(range.clone()))
        .await
        .expect("the resumed read completes");
    assert_eq!(
        std::fs::read(&target).expect("read the target"),
        content[range]
    );
}

/// A file already at the target stays as it was through reads that fail, and is replaced only by
/// the content, whole and checked, never by what the temporary file holds.
#[tokio::test(flavor = "multi_thread")]
async fn the_target_holds_its_old_bytes_or_the_whole_content() {
    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-target-");
    let target = target_dir.path().join("content.bin");
    std::fs::write(&target, b"previous").expect("write the old target");
    let (root, leaves, content) = put_leaves(&store, &[0; LEAVES]).await;

    read_failing_at(&store, root, leaves[FAILING], &target, None).await;
    read_failing_at(&store, root, leaves[FAILING], &target, None).await;
    assert_eq!(
        std::fs::read(&target).expect("read the target"),
        b"previous",
        "a failed read leaves the target as it was"
    );

    let temporary = temporary_file(&target);
    let mut held = std::fs::read(&temporary).expect("read the temporary file");
    held[..LEAF].fill(0);
    std::fs::write(&temporary, held).expect("corrupt the temporary file");

    read_resuming(store, root, &target, None)
        .await
        .expect("the resumed read completes");
    assert_eq!(std::fs::read(&target).expect("read the target"), content);
}

/// A read dropped part-way writes nothing into the file a later read of other content to the same
/// path renames into place: its tasks end with it, and the later read waits for none of them. Here
/// the dropped read's last leaf is held at the store until the later read has completed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_read_writes_nothing_into_a_later_read_of_its_path() {
    let last = LEAVES - 1;

    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-dropped-");
    let target = target_dir.path().join("content.bin");
    let (first, first_leaves, _) = put_leaves(&store, &[0; LEAVES]).await;
    let (other, _, other_content) = put_leaves(&store, &[1; LEAVES]).await;

    let (parked, park) = parking(&store, first_leaves[last]);
    let dropped = {
        let parked = parked.clone();
        let target = target.clone();
        lore_base::lore_spawn!(async move { read_resuming(parked, first, &target, None).await })
    };
    park.reached.notified().await;
    dropped.abort();
    let _cancelled = dropped.await;

    tokio::time::timeout(PATIENCE, read_resuming(store.clone(), other, &target, None))
        .await
        .expect("the later read waits for no task of the dropped one")
        .expect("the later read completes");
    let _ = park.release.send(());
    until_released(&parked).await;

    assert_eq!(
        std::fs::read(&target).expect("read the target"),
        other_content
    );
}

/// A read to a path whose temporary file another read is writing waits for that read, and then
/// writes its own content, whichever content that read wrote.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_waits_for_another_writing_its_temporary_file() {
    let last = LEAVES - 1;

    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-concurrent-");
    let target = target_dir.path().join("content.bin");
    let (first, first_leaves, _) = put_leaves(&store, &[0; LEAVES]).await;
    let (other, _, other_content) = put_leaves(&store, &[1; LEAVES]).await;

    let (parked, park) = parking(&store, first_leaves[last]);
    let first_read = {
        let target = target.clone();
        lore_base::lore_spawn!(async move { read_resuming(parked, first, &target, None).await })
    };
    park.reached.notified().await;
    let waiting = Arc::new(Notify::new());
    let later = {
        let store = store.clone();
        let target = target.clone();
        let waiting = waiting.clone();
        lore_base::lore_spawn!(async move {
            noting_lock_waits(waiting, read_resuming(store, other, &target, None)).await
        })
    };
    tokio::time::timeout(PATIENCE, waiting.notified())
        .await
        .expect("the later read waits for the lock the first holds");
    let _ = park.release.send(());

    first_read
        .await
        .expect("the first read's task ends")
        .expect("the first read completes");
    later
        .await
        .expect("the later read's task ends")
        .expect("the later read completes");
    assert_eq!(
        std::fs::read(&target).expect("read the target"),
        other_content
    );
}

/// A read dropped while it waits for the lock another read holds on its temporary file leaves the
/// file to that read, which completes with its content at the target.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_dropped_while_it_waits_for_the_lock_leaves_the_file_to_its_holder() {
    let last = LEAVES - 1;

    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-waiter-");
    let target = target_dir.path().join("content.bin");
    let (root, leaves, content) = put_leaves(&store, &[0; LEAVES]).await;

    let (parked, park) = parking(&store, leaves[last]);
    let holder = {
        let target = target.clone();
        lore_base::lore_spawn!(async move { read_resuming(parked, root, &target, None).await })
    };
    park.reached.notified().await;
    let waiting = Arc::new(Notify::new());
    let waiter = {
        let store = store.clone();
        let target = target.clone();
        let waiting = waiting.clone();
        lore_base::lore_spawn!(async move {
            noting_lock_waits(waiting, read_resuming(store, root, &target, None)).await
        })
    };
    tokio::time::timeout(PATIENCE, waiting.notified())
        .await
        .expect("the second read waits for the lock the first holds");
    waiter.abort();
    let _cancelled = waiter.await;
    let _ = park.release.send(());

    holder
        .await
        .expect("the first read's task ends")
        .expect("the first read completes");
    assert_eq!(std::fs::read(&target).expect("read the target"), content);
}

/// What entry `index` of [`put_holed`]'s content is: a hole where it is 1 past a multiple of 4, a
/// leaf of written zeros where it is 3 past one, and a leaf of data otherwise.
fn holed_entry(index: usize) -> Option<Vec<u8>> {
    match index % 4 {
        1 => None,
        3 => Some(vec![0; LEAF]),
        _ => Some(leaf_bytes(index, 0)),
    }
}

/// Content of [`LEAVES`] entries as [`holed_entry`] makes them, holes and written zeros between its
/// data: the list's address, each entry's leaf address, and the content.
async fn put_holed(store: &Arc<dyn ImmutableStore>) -> (Address, Vec<Option<Address>>, Vec<u8>) {
    put_entries(store, (0..LEAVES).map(holed_entry)).await
}

/// The ranges a file of [`put_holed`]'s content holds data for: every leaf, of zeros included, and
/// no hole.
fn holed_data() -> Vec<Range<u64>> {
    let mut ranges: Vec<Range<u64>> = Vec::new();
    for index in (0..LEAVES).filter(|index| holed_entry(*index).is_some()) {
        let leaf = (index * LEAF) as u64..((index + 1) * LEAF) as u64;
        match ranges.last_mut() {
            Some(last) if last.end == leaf.start => last.end = leaf.end,
            _ => ranges.push(leaf),
        }
    }
    ranges
}

/// The indices of the leaves of data in `leaves` loaded through `loads`, in order and once each.
/// A list holding holes is known to hold leaves without a load of its first, so every load counts.
fn data_leaves_loaded(loads: &Mutex<Vec<Address>>, leaves: &[Option<Address>]) -> Vec<usize> {
    let loads = loads.lock().unwrap();
    (0..leaves.len())
        .filter(|index| index % 4 != 1 && index % 4 != 3)
        .filter(|index| loads.iter().any(|load| Some(*load) == leaves[*index]))
        .collect()
}

/// The entry of [`put_holed`]'s content that the first leaf of zeros is: every leaf of zeros has
/// its address.
const ZEROS: usize = 3;

/// A resumed read of content with holes and written zeros writes the content's bytes, loads none of
/// the leaves of data the failed read wrote, so the temporary file it resumes is not emptied, and
/// leaves each hole unallocated and each leaf of zeros written. The failed read fails at the
/// address of every leaf of zeros, so it writes none of them, and every range of zeros reads as
/// zeros in the temporary file when the read resumes.
#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_read_leaves_holes_unwritten_and_writes_zeros() {
    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-holes-");
    if !crate::holes::holes_reported().await {
        return;
    }
    let target = target_dir.path().join("content.bin");
    let (root, leaves, content) = put_holed(&store).await;
    let zeros = leaves[ZEROS].expect("a leaf of zeros");

    read_failing_at(&store, root, zeros, &target, None).await;
    let (resumed, loads) = hooked(&store, None);
    read_resuming(resumed, root, &target, None)
        .await
        .expect("the resumed read completes");

    let loaded = data_leaves_loaded(&loads, &leaves);
    assert!(
        !loaded.contains(&0) && !loaded.contains(&2),
        "a leaf the failed read wrote is loaded again, the temporary file emptied: {loaded:?}"
    );
    assert!(
        loads.lock().unwrap().contains(&zeros),
        "the leaves of zeros are loaded"
    );
    assert_eq!(std::fs::read(&target).expect("read the target"), content);
    assert_eq!(
        crate::holes::data_ranges(&target).await,
        holed_data(),
        "each hole is unallocated, and each leaf of zeros written"
    );
}

/// A resumed read whose temporary file holds data in a hole of the content, written there after
/// the failed read, empties the file and writes it as a fresh one: the target is the content, and
/// each hole is a hole again. On Apple platforms the write sink frees the hole instead, as
/// [`a_resumed_read_on_apfs_loads_only_the_leaves_it_lacks_and_keeps_every_hole`] tests.
#[cfg(not(target_vendor = "apple"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_hole_of_the_temporary_file_holding_data_empties_it() {
    const HOLE: usize = 1;

    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-filled-hole-");
    if !crate::holes::holes_reported().await {
        return;
    }
    let target = target_dir.path().join("content.bin");
    let (root, leaves, content) = put_holed(&store).await;
    assert!(leaves[HOLE].is_none(), "the entry is a hole");

    read_failing_at(
        &store,
        root,
        leaves[ZEROS].expect("a leaf of zeros"),
        &target,
        None,
    )
    .await;
    crate::holes::write_at(
        &temporary_file(&target),
        (HOLE * LEAF + LEAF / 4) as u64,
        &[0xFF; 4096],
    )
    .await;
    read_resuming(store, root, &target, None)
        .await
        .expect("the resumed read completes");

    assert_eq!(std::fs::read(&target).expect("read the target"), content);
    assert_eq!(crate::holes::data_ranges(&target).await, holed_data());
}

/// On APFS, which fills a hole of under 16 MiB beside data it writes out, a resumed read of content
/// with such holes resumes whatever the whole blocks of its holes hold in the temporary file, zeros
/// the file system filled in or other bytes written there after the failed read: it loads only the
/// leaves of data the temporary file lacks, and the target is the content with every hole a hole
/// and every leaf of zeros written.
#[cfg(target_vendor = "apple")]
#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_read_on_apfs_loads_only_the_leaves_it_lacks_and_keeps_every_hole() {
    const HOLE: usize = 1;

    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-apfs-");
    if !crate::holes::holes_reported().await {
        return;
    }
    let target = target_dir.path().join("content.bin");
    let (root, leaves, content) = put_holed(&store).await;
    read_failing_at(
        &store,
        root,
        leaves[ZEROS].expect("a leaf of zeros"),
        &target,
        None,
    )
    .await;
    let temporary = temporary_file(&target);
    let held = std::fs::read(&temporary).expect("read the temporary file");
    let holds = |index: usize| {
        held[index * LEAF..(index + 1) * LEAF] == content[index * LEAF..(index + 1) * LEAF]
    };
    let lacking: Vec<usize> = (0..LEAVES)
        .filter(|index| index % 4 != 1 && index % 4 != 3)
        .filter(|index| !holds(*index))
        .collect();
    let filled = crate::holes::data_ranges(&temporary).await;
    assert!(
        (HOLE..LEAVES - 1).step_by(4).any(|hole| {
            let range = (hole * LEAF) as u64..((hole + 1) * LEAF) as u64;
            filled
                .iter()
                .any(|data| data.start <= range.start && range.end <= data.end)
        }),
        "the file system filled a hole of the temporary file: {filled:?}"
    );
    crate::holes::write_at(&temporary, (HOLE * LEAF + LEAF / 4) as u64, &[0xFF; 4096]).await;

    let (resumed, loads) = hooked(&store, None);
    read_resuming(resumed, root, &target, None)
        .await
        .expect("the resumed read completes");

    assert_eq!(
        data_leaves_loaded(&loads, &leaves),
        lacking,
        "only the leaves the temporary file lacks are loaded"
    );
    assert_eq!(std::fs::read(&target).expect("read the target"), content);
    assert_eq!(
        crate::holes::data_ranges(&target).await,
        holed_data(),
        "each hole is unallocated, and each leaf of zeros written"
    );
}

/// A resumed read of content with holes writes the content's bytes on every file system, whether it
/// reports holes or not. Where it reports holes, the read resumes, and does not load the first
/// leaf, which the failed read wrote. Where it reports none, the whole temporary file is data, so
/// the read finds data in a hole, each of which holds whole blocks, and writes the file again as a
/// fresh one, loading the first leaf again.
#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_read_of_content_with_holes_writes_its_bytes_wherever_it_runs() {
    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-holes-anywhere-");
    let target = target_dir.path().join("content.bin");
    let (root, leaves, content) = put_holed(&store).await;

    read_failing_at(
        &store,
        root,
        leaves[ZEROS].expect("a leaf of zeros"),
        &target,
        None,
    )
    .await;
    let (resumed, loads) = hooked(&store, None);
    read_resuming(resumed, root, &target, None)
        .await
        .expect("the resumed read completes");

    assert_eq!(std::fs::read(&target).expect("read the target"), content);
    let reported = crate::holes::holes_reported().await;
    assert_eq!(
        data_leaves_loaded(&loads, &leaves).contains(&0),
        !reported,
        "the first leaf is loaded again exactly where the file system reports no holes"
    );
}

/// The entry at `index` of content whose holes start and end inside blocks of the file system: the
/// first leaf is 512 bytes short of the others, and the rest are as [`holed_entry`] makes them.
fn unaligned_entry(index: usize) -> Option<Vec<u8>> {
    if index == 0 {
        let mut bytes = leaf_bytes(index, 2);
        bytes.truncate(LEAF - 512);
        return Some(bytes);
    }
    holed_entry(index)
}

/// A resumed read of content whose holes start and end inside blocks of the file system resumes,
/// rather than taking the blocks its holes share with its leaves for data in a hole: it loads none
/// of the leaves of data the failed read wrote, and writes the content's bytes, with its data where
/// a read that does not resume puts it.
#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_read_of_holes_off_the_blocks_resumes() {
    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-unaligned-");
    if !crate::holes::holes_reported().await {
        return;
    }
    let target = target_dir.path().join("content.bin");
    let fresh = target_dir.path().join("fresh.bin");
    let (root, leaves, content) = put_entries(&store, (0..LEAVES).map(unaligned_entry)).await;
    let failing = leaves[FAILING].expect("a leaf of data where the read fails");

    read_failing_at(&store, root, failing, &target, None).await;
    let (resumed, loads) = hooked(&store, None);
    read_resuming(resumed, root, &target, None)
        .await
        .expect("the resumed read completes");
    read_resuming(store, root, &fresh, None)
        .await
        .expect("the fresh read completes");

    let loaded = data_leaves_loaded(&loads, &leaves);
    assert!(
        loaded.iter().all(|index| *index >= FAILING),
        "a leaf the failed read wrote is loaded again, the file written as a fresh one: {loaded:?}"
    );
    assert_eq!(std::fs::read(&target).expect("read the target"), content);
    assert_eq!(
        crate::holes::data_ranges(&target).await,
        crate::holes::data_ranges(&fresh).await
    );
}

/// A resumed read of content whose holes start and end inside blocks of the file system writes the
/// file again as a fresh one where bytes other than zeros were written, after the failed read, into
/// the part of a hole in a block it shares with a leaf beside it: at the hole's start, and at its
/// end.
#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_read_finds_bytes_written_into_the_edge_of_a_hole() {
    const HOLE: usize = 1;

    let (_store_dir, store) = make_test_store().await;
    if !crate::holes::holes_reported().await {
        return;
    }
    let (root, leaves, content) = put_entries(&store, (0..LEAVES).map(unaligned_entry)).await;
    assert!(leaves[HOLE].is_none(), "the entry is a hole");
    let failing = leaves[FAILING].expect("a leaf of data where the read fails");
    let hole = (HOLE * LEAF - 512) as u64..((HOLE + 1) * LEAF - 512) as u64;

    for stray in [hole.start + 256, hole.end - 256] {
        let target_dir = TempDir::new("lore-storage-resume-hole-edge-");
        let target = target_dir.path().join("content.bin");
        read_failing_at(&store, root, failing, &target, None).await;
        crate::holes::write_at(&temporary_file(&target), stray, &[0xFF; 16]).await;
        read_resuming(store.clone(), root, &target, None)
            .await
            .expect("the resumed read completes");

        assert_eq!(
            std::fs::read(&target).expect("read the target"),
            content,
            "bytes written into the hole at {stray}"
        );
    }
}

/// The entry at `index` of content ending in a hole that ends inside a block of the file system:
/// as [`unaligned_entry`] makes them, but for the last entry, a hole.
fn ending_entry(index: usize) -> Option<Vec<u8>> {
    if index == LEAVES - 1 {
        return None;
    }
    unaligned_entry(index)
}

/// A resumed read of content ending in a hole writes the file again as a fresh one where zeros were
/// written, after the failed read, into the last block of the file, part of the hole: no leaf
/// shares that block, so data there is data in the hole, even zeros. On Apple platforms the read
/// resumes instead, and the write sink frees the block. The target holds the content's bytes, with
/// its data where a read that does not resume puts it, so that block is a hole again.
#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_read_finds_bytes_written_into_a_hole_ending_the_content() {
    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-ending-hole-");
    if !crate::holes::holes_reported().await {
        return;
    }
    let target = target_dir.path().join("content.bin");
    let fresh = target_dir.path().join("fresh.bin");
    let (root, leaves, content) = put_entries(&store, (0..LEAVES).map(ending_entry)).await;
    assert!(leaves[LEAVES - 1].is_none(), "the last entry is a hole");
    let failing = leaves[FAILING].expect("a leaf of data where the read fails");

    read_failing_at(&store, root, failing, &target, None).await;
    crate::holes::write_at(
        &temporary_file(&target),
        content.len() as u64 - 64,
        &[0; 16],
    )
    .await;
    let (resumed, loads) = hooked(&store, None);
    read_resuming(resumed, root, &target, None)
        .await
        .expect("the resumed read completes");
    read_resuming(store, root, &fresh, None)
        .await
        .expect("the fresh read completes");

    let loaded = data_leaves_loaded(&loads, &leaves);
    assert_eq!(
        loaded.iter().all(|index| *index >= FAILING),
        cfg!(target_vendor = "apple"),
        "the read resumes exactly where the write sink frees the block: {loaded:?}"
    );
    assert_eq!(std::fs::read(&target).expect("read the target"), content);
    assert_eq!(
        crate::holes::data_ranges(&target).await,
        crate::holes::data_ranges(&fresh).await
    );
}

/// The entry at `index` of content whose first and last entries are holes: a hole where the index
/// is a multiple of 4, a leaf of written zeros where it is 2 past one, and a leaf of data
/// otherwise.
fn edged_entry(index: usize) -> Option<Vec<u8>> {
    match index % 4 {
        0 => None,
        2 => Some(vec![0; LEAF]),
        _ => Some(leaf_bytes(index, 1)),
    }
}

/// A resumed read of a range that starts and ends inside holes, the first and the last entries of
/// the content, writes the bytes of the range, with each hole of it unallocated and each leaf of
/// zeros written.
#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_range_starting_and_ending_in_holes_keeps_them() {
    let range = LEAF / 2..LEAF / 2 + RESUME_WRITE_MIN_LEN;
    assert!(edged_entry(0).is_none() && edged_entry(range.end / LEAF).is_none());

    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-edged-");
    if !crate::holes::holes_reported().await {
        return;
    }
    let target = target_dir.path().join("content.bin");
    let (root, leaves, content) =
        put_entries(&store, (0..range.end / LEAF + 1).map(edged_entry)).await;
    let failing = leaves[FAILING + 1].expect("a leaf of data where the read fails");

    read_failing_at(&store, root, failing, &target, Some(range.clone())).await;
    read_resuming(store, root, &target, Some(range.clone()))
        .await
        .expect("the resumed read completes");

    assert_eq!(
        std::fs::read(&target).expect("read the target"),
        content[range.clone()]
    );
    let mut data: Vec<Range<u64>> = Vec::new();
    for index in (0..leaves.len()).filter(|index| edged_entry(*index).is_some()) {
        let leaf = (index * LEAF - range.start) as u64..((index + 1) * LEAF - range.start) as u64;
        match data.last_mut() {
            Some(last) if last.end == leaf.start => last.end = leaf.end,
            _ => data.push(leaf),
        }
    }
    assert_eq!(crate::holes::data_ranges(&target).await, data);
}

/// A resuming read over the temporary file a failed read of other content left at its path, of the
/// same length, empties it before writing: each hole of its content is unallocated, as in a read
/// that does not resume, wherever the other content held data.
#[tokio::test(flavor = "multi_thread")]
async fn a_temporary_file_of_other_content_is_emptied_of_its_storage() {
    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-other-emptied-");
    if !crate::holes::holes_reported().await {
        return;
    }
    let target = target_dir.path().join("content.bin");
    let (other, other_leaves, _) = put_leaves(&store, &[3; LEAVES]).await;
    read_failing_at(&store, other, other_leaves[LEAVES - 1], &target, None).await;
    let (root, _, content) = put_holed(&store).await;

    read_resuming(store, root, &target, None)
        .await
        .expect("the read completes");

    assert_eq!(std::fs::read(&target).expect("read the target"), content);
    assert_eq!(crate::holes::data_ranges(&target).await, holed_data());
}

/// A resuming read over a temporary file that names no content, left with other bytes, empties it
/// before writing: each hole reads as zeros and is unallocated, as in a read that does not resume.
#[tokio::test(flavor = "multi_thread")]
async fn a_resuming_read_empties_a_temporary_file_it_does_not_resume() {
    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-leftover-");
    if !crate::holes::holes_reported().await {
        return;
    }
    let target = target_dir.path().join("content.bin");
    let (root, _, content) = put_holed(&store).await;
    std::fs::write(temporary_file(&target), vec![0xAB; LEAVES * LEAF])
        .expect("leave a temporary file behind");

    read_resuming(store, root, &target, None)
        .await
        .expect("the read completes");

    assert_eq!(std::fs::read(&target).expect("read the target"), content);
    assert_eq!(crate::holes::data_ranges(&target).await, holed_data());
}

/// A read dropped while its temporary file is renamed into place keeps the file's lock until the
/// rename ends, so a later read of other content to the same path waits for it rather than emptying
/// the file the rename moves, and then writes its own content. The resume record is gone before the
/// rename. Here the rename is held until the later read waits for the lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_dropped_while_its_file_is_renamed_keeps_the_lock_until_the_rename_ends() {
    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-rename-");
    let target = target_dir.path().join("content.bin");
    let (first, _, _) = put_leaves(&store, &[0; LEAVES]).await;
    let (other, _, other_content) = put_leaves(&store, &[1; LEAVES]).await;

    let rename = hold_rename_of(temporary_file(&target));
    let first_read = {
        let store = store.clone();
        let target = target.clone();
        lore_base::lore_spawn!(async move { read_resuming(store, first, &target, None).await })
    };
    tokio::time::timeout(PATIENCE, rename.reached.notified())
        .await
        .expect("the first read reaches its rename");
    assert_eq!(
        names(target_dir.path()),
        ["content.bin.loretmp"],
        "the resume record is gone before the rename"
    );
    first_read.abort();
    let _cancelled = first_read.await;

    let waiting = Arc::new(Notify::new());
    let later = {
        let store = store.clone();
        let target = target.clone();
        let waiting = waiting.clone();
        lore_base::lore_spawn!(async move {
            noting_lock_waits(waiting, read_resuming(store, other, &target, None)).await
        })
    };
    tokio::time::timeout(PATIENCE, waiting.notified())
        .await
        .expect("the later read waits for the lock the dropped read's rename holds");
    rename.resume.notify_one();

    later
        .await
        .expect("the later read's task ends")
        .expect("the later read completes");
    assert_eq!(
        std::fs::read(&target).expect("read the target"),
        other_content
    );
    assert_eq!(names(target_dir.path()), ["content.bin"]);
}

/// A read resuming from the temporary file a failed read left, dropped part-way, writes nothing
/// into the file a later read of other content to the same path renames into place: the leaves it
/// checks hold the file only while they read it, and its other tasks end with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_resumed_read_writes_nothing_into_a_later_read_of_its_path() {
    let last = LEAVES - 1;

    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-dropped-resumed-");
    let target = target_dir.path().join("content.bin");
    let (first, first_leaves, _) = put_leaves(&store, &[0; LEAVES]).await;
    let (other, _, other_content) = put_leaves(&store, &[1; LEAVES]).await;

    read_failing_at(&store, first, first_leaves[last], &target, None).await;
    let (parked, park) = parking(&store, first_leaves[last]);
    let dropped = {
        let parked = parked.clone();
        let target = target.clone();
        lore_base::lore_spawn!(async move { read_resuming(parked, first, &target, None).await })
    };
    park.reached.notified().await;
    dropped.abort();
    let _cancelled = dropped.await;

    tokio::time::timeout(PATIENCE, read_resuming(store.clone(), other, &target, None))
        .await
        .expect("the later read waits for no task of the dropped one")
        .expect("the later read completes");
    let _ = park.release.send(());
    until_released(&parked).await;

    assert_eq!(
        std::fs::read(&target).expect("read the target"),
        other_content
    );
}

/// A read dropped part-way keeps its store claimed until the loads it detached have ended: once
/// every other task of the read has ended, the load of its last leaf, held at the store, still
/// keeps the store's lock, which goes with that load.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_read_keeps_its_store_claimed_until_its_detached_loads_end() {
    let last = LEAVES - 1;

    let (_store_dir, store) = make_test_store().await;
    let lock = store.store_lock().expect("a store on disk has a lock");
    let target_dir = TempDir::new("lore-storage-resume-claim-");
    let target = target_dir.path().join("content.bin");
    let (root, leaves, _) = put_leaves(&store, &[0; LEAVES]).await;
    store
        .clone()
        .flush(FlushOptions::default())
        .await
        .expect("flush the puts");
    let held = references_of_a_held_load(&store, leaves[0]).await;
    assert!(!lock.is_held(), "nothing holds the store before the read");

    let (parked, park) = parking(&store, leaves[last]);
    let dropped = {
        let parked = parked.clone();
        let target = target.clone();
        lore_base::lore_spawn!(async move { read_resuming(parked, root, &target, None).await })
    };
    park.reached.notified().await;
    dropped.abort();
    let _cancelled = dropped.await;
    tokio::time::timeout(PATIENCE, async {
        while Arc::strong_count(&parked) > 1 + held {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("every task of the read but the held load ends");

    assert!(lock.is_held(), "the held load keeps the store claimed");
    let _ = park.release.send(());
    until_released(&parked).await;
    tokio::time::timeout(PATIENCE, async {
        while lock.is_held() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the claim goes with the load");
}

/// A read whose file system refuses the lock of its temporary file writes as one that does not
/// resume: it keeps nothing when it fails, and completes.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_whose_file_system_refuses_locks_writes_without_resuming() {
    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-refused-");
    let target = target_dir.path().join("content.bin");
    let (root, leaves, content) = put_leaves(&store, &[0; LEAVES]).await;

    let (failing, _loads) = hooked(&store, Some(leaves[FAILING]));
    refusing_locks(read_resuming(failing, root, &target, None))
        .await
        .expect_err("the read fails at the failing leaf");
    assert!(
        names(target_dir.path()).is_empty(),
        "the failed read keeps no temporary file and no resume record"
    );

    refusing_locks(read_resuming(store, root, &target, None))
        .await
        .expect("the read completes");
    assert_eq!(std::fs::read(&target).expect("read the target"), content);
}

/// A read whose locked temporary file has moved from its path each time, other reads renaming
/// theirs into place, gives up after a bounded number of opens and fails as one to retry later.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_whose_temporary_file_keeps_moving_fails_to_be_retried_later() {
    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-moving-");
    let target = target_dir.path().join("content.bin");
    let (root, _, _) = put_leaves(&store, &[0; LEAVES]).await;

    let failed = tokio::time::timeout(
        PATIENCE,
        moving_temporary_files(read_resuming(store, root, &target, None)),
    )
    .await
    .expect("the read gives up")
    .expect_err("the read fails");
    assert!(
        matches!(failed, StorageError::SlowDown(_)),
        "the read fails as one to retry later: {failed:?}"
    );
}

/// The temporary file of a target without an extension is the target's name with `.loretmp`
/// appended, as is the temporary file of any other target.
#[tokio::test(flavor = "multi_thread")]
async fn a_target_without_an_extension_has_its_temporary_file_at_its_name_with_loretmp() {
    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-no-extension-");
    let target = target_dir.path().join("content");
    let (root, leaves, _) = put_leaves(&store, &[0; LEAVES]).await;

    read_failing_at(&store, root, leaves[FAILING], &target, None).await;
    assert_eq!(
        names(target_dir.path()),
        ["content.loretmp", "content.loretmp.rec"]
    );
}

/// A temporary file without the resume record naming what it holds is emptied, and every leaf is
/// loaded.
#[tokio::test(flavor = "multi_thread")]
async fn a_temporary_file_without_its_resume_record_is_emptied() {
    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-no-record-");
    let target = target_dir.path().join("content.bin");
    let (root, leaves, content) = put_leaves(&store, &[0; LEAVES]).await;

    read_failing_at(&store, root, leaves[FAILING], &target, None).await;
    std::fs::remove_file(target.with_extension("bin.loretmp.rec"))
        .expect("remove the resume record");

    let (read, loads) = hooked(&store, None);
    read_resuming(read, root, &target, None)
        .await
        .expect("the read completes");
    assert_eq!(
        leaves_loaded(&loads, &leaves),
        (0..LEAVES).collect::<Vec<_>>(),
        "every leaf is loaded"
    );
    assert_eq!(std::fs::read(&target).expect("read the target"), content);
}

/// A temporary file of another range of the same content is discarded, even where the two ranges
/// start at the same offset and so place each leaf alike.
#[tokio::test(flavor = "multi_thread")]
async fn a_temporary_file_of_another_range_is_discarded() {
    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-other-range-");
    let target = target_dir.path().join("content.bin");
    let (root, leaves, content) = put_leaves(&store, &[0; LEAVES]).await;

    read_failing_at(
        &store,
        root,
        leaves[FAILING],
        &target,
        Some(0..RESUME_WRITE_MIN_LEN),
    )
    .await;

    let (read, loads) = hooked(&store, None);
    read_resuming(read, root, &target, None)
        .await
        .expect("the read of the whole content completes");
    assert_eq!(
        leaves_loaded(&loads, &leaves),
        (0..LEAVES).collect::<Vec<_>>(),
        "every leaf is loaded"
    );
    assert_eq!(std::fs::read(&target).expect("read the target"), content);
}

/// A read of a range one byte shorter than [`RESUME_WRITE_MIN_LEN`] writes as one that does not
/// resume: it keeps nothing when it fails, and a later read of the range loads every leaf in it.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_of_a_range_shorter_than_the_resume_length_keeps_nothing() {
    let range = LEAF / 2..LEAF / 2 + RESUME_WRITE_MIN_LEN - 1;
    let last = (range.end - 1) / LEAF;

    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-resume-short-");
    let target = target_dir.path().join("content.bin");
    let (root, leaves, content) = put_leaves(&store, &[0; LEAVES]).await;

    read_failing_at(&store, root, leaves[FAILING], &target, Some(range.clone())).await;
    assert!(
        names(target_dir.path()).is_empty(),
        "the failed read keeps no temporary file and no resume record"
    );

    let (read, loads) = hooked(&store, None);
    read_resuming(read, root, &target, Some(range.clone()))
        .await
        .expect("the read of the range completes");
    assert_eq!(
        leaves_loaded(&loads, &leaves),
        (0..=last).collect::<Vec<_>>(),
        "every leaf in the range is loaded"
    );
    assert_eq!(
        std::fs::read(&target).expect("read the target"),
        content[range]
    );
}
