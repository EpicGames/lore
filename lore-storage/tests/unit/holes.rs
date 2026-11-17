// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Content stored skipping the holes of a file, and read back with them.
//!
//! The files are made sparse on the file system the tests run on, which must report holes.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use lore_base::test_util::TempDir;
use lore_storage::Address;
use lore_storage::CallerBuffer;
use lore_storage::ContentSource;
use lore_storage::Context;
use lore_storage::FragmentReference;
use lore_storage::ImmutableStore;
use lore_storage::ImmutableStoreSettings;
use lore_storage::LocalImmutableStore;
use lore_storage::Partition;
use lore_storage::ReadOptions;
use lore_storage::TypedBytes;
use lore_storage::WriteContext;
use lore_storage::WriteOptions;
use lore_storage::compress::FRAGMENT_SIZE_THRESHOLD;
use lore_storage::fragment_flags::FragmentFlags;
use lore_storage::validate_fragment_list;

const MIB: usize = 1 << 20;

fn random_buffer(size: usize) -> Vec<u8> {
    use rand::Rng;
    let mut data = vec![0u8; size];
    rand::rng().fill(&mut data[..]);
    data
}

/// A file of `size` bytes holding random data at `extents`, `(offset, length)` each, and a hole
/// everywhere else, with the content it reads as.
pub(crate) async fn sparse_file(path: &Path, size: usize, extents: &[(usize, usize)]) -> Vec<u8> {
    let file = lore_io::IoDriver::global()
        .open(
            path,
            &lore_io::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true),
        )
        .await
        .expect("create test file");
    file.set_len_sparse(size as u64)
        .await
        .expect("size test file");
    let mut content = vec![0u8; size];
    for &(offset, length) in extents {
        let data = random_buffer(length);
        content[offset..offset + length].copy_from_slice(&data);
        file.write_all_at(data, offset as u64)
            .await
            .expect("write extent");
    }
    file.sync_all().await.expect("sync test file");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open test file");
    punch_gaps(&file, size, extents);
    content
}

/// Writes `bytes` at `offset` of the file at `path`, keeping its length where they end before it.
pub(crate) async fn write_at(path: &Path, offset: u64, bytes: &[u8]) {
    lore_io::IoDriver::global()
        .open(path, &lore_io::OpenOptions::new().write(true))
        .await
        .expect("open the file")
        .write_all_at(bytes.to_vec(), offset)
        .await
        .expect("write into the file");
}

/// The bytes of storage the file at `path` holds. On Windows the file is flushed first, as NTFS
/// allocates a write into a sparse file when it flushes the write.
fn allocated(path: &Path) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        std::fs::metadata(path).expect("stat").blocks() * 512
    }

    #[cfg(target_family = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;

        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .and_then(|file| file.sync_all())
            .expect("flush the file");
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let mut high = 0u32;
        // SAFETY: `wide` is a path ending in a null, and `high` outlives the call.
        let low = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW(
                wide.as_ptr(),
                &mut high,
            )
        };
        (u64::from(high) << 32) | u64::from(low)
    }
}

/// Punches the whole blocks of the ranges of `file`, `size` bytes, outside `extents`, which are
/// `(offset, length)` each, sorted and apart, once its data is synced. Only on Apple platforms,
/// where APFS fills a hole of under 16 MiB beside data it flushes; elsewhere a range left unwritten
/// stays a hole.
pub(crate) fn punch_gaps(file: &std::fs::File, size: usize, extents: &[(usize, usize)]) {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;

        let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: `fstatfs` fills `stats`, which outlives the call.
        assert_eq!(
            unsafe { libc::fstatfs(file.as_raw_fd(), stats.as_mut_ptr()) },
            0,
            "statfs"
        );
        // SAFETY: `fstatfs` succeeded, so it filled `stats`.
        let block = unsafe { stats.assume_init() }.f_bsize as usize;
        let mut at = 0;
        for gap in extents
            .iter()
            .map(|&(offset, length)| {
                let gap = at..offset;
                at = offset + length;
                gap
            })
            .collect::<Vec<_>>()
            .into_iter()
            .chain(std::iter::once(at..size))
        {
            let start = gap.start.div_ceil(block) * block;
            let end = gap.end / block * block;
            if start >= end {
                continue;
            }
            let punch = libc::fpunchhole_t {
                fp_flags: 0,
                reserved: 0,
                fp_offset: start as libc::off_t,
                fp_length: (end - start) as libc::off_t,
            };
            // SAFETY: `punch` outlives the call, which only reads it.
            assert_eq!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &punch) },
                0,
                "punch {start}..{end}: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    #[cfg(not(target_vendor = "apple"))]
    let _ = (file, size, extents);
}

/// The data ranges the file system reports for `path`.
pub(crate) async fn data_ranges(path: &Path) -> Vec<std::ops::Range<u64>> {
    let (handle, size) = ContentSource::file(path).open().await.expect("open file");
    let mut ranges = Vec::new();
    let mut offset = 0;
    let mut cursor = lore_io::DataCursor::default();
    while offset < size {
        let (found, walked) = handle.next_data(offset, cursor).await;
        cursor = walked;
        let Some(range) = found.expect("find data") else {
            break;
        };
        offset = range.end;
        ranges.push(range.start..range.end.min(size));
    }
    ranges
}

/// Whether the file system the tests run on reports holes, probed once with a file holding a hole
/// between two ranges of data. A test that needs holes reported returns early where they are not,
/// saying why. On Windows the tests need a temporary directory on a file system that makes files
/// sparse, as NTFS does, and fail elsewhere, so that a run there never skips them unseen.
pub(crate) async fn holes_reported() -> bool {
    static REPORTED: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();
    let reported = *REPORTED
        .get_or_init(|| async {
            let dir = TempDir::new("lore-storage-holes-probe-");
            let path = dir.path().join("probe");
            sparse_file(&path, 4 * MIB, &[(0, 4096), (2 * MIB, 4096)]).await;
            data_ranges(&path).await.len() == 2
        })
        .await;
    assert!(
        reported || !cfg!(target_family = "windows"),
        "the file system reports no hole"
    );
    if !reported {
        eprintln!("the file system the test runs on reports no hole");
    }
    reported
}

async fn make_store() -> (TempDir, Arc<dyn ImmutableStore>) {
    let dir = TempDir::new("lore-storage-holes-test-");
    let store = LocalImmutableStore::new(
        Some(PathBuf::from(dir.as_ref())),
        ImmutableStoreSettings::default(),
    )
    .await
    .expect("create test store");
    (dir, store)
}

fn partition() -> Partition {
    Partition::from([0x5B; 16])
}

async fn put(store: &Arc<dyn ImmutableStore>, path: &Path, options: WriteOptions) -> Address {
    lore_storage::write::write_from_file(
        store.clone(),
        partition(),
        &ContentSource::file(path),
        Context::from([0x5C; 16]),
        options,
        None,
        WriteContext::none(),
    )
    .await
    .expect("put the file")
    .address
}

/// Every list under `address`, each checked as the server checks one it is sent, and the number
/// of hole references and of leaves they hold.
async fn walk_lists(store: &Arc<dyn ImmutableStore>, address: Address) -> (usize, usize, usize) {
    let (mut lists, mut holes, mut leaves) = (0, 0, 0);
    let mut level = vec![address];
    while !level.is_empty() {
        let mut next = Vec::new();
        for list in level {
            let (fragment, payload) = lore_storage::read::load_fragment(
                store.clone(),
                partition(),
                list,
                ReadOptions::default().no_remote(),
                None,
            )
            .await
            .expect("load list");
            assert_ne!(
                fragment.flags & FragmentFlags::PayloadFragmented,
                0,
                "a list"
            );
            validate_fragment_list(&fragment, &payload).expect("a list a server takes");
            lists += 1;
            let payload = payload.to_aligned::<FragmentReference>();
            for reference in payload.as_type_slice::<FragmentReference>() {
                if reference.is_hole() {
                    holes += 1;
                    continue;
                }
                let child = Address {
                    hash: reference.hash,
                    context: list.context,
                };
                let described = store
                    .clone()
                    .get_metadata(partition(), child)
                    .await
                    .expect("describe child");
                if described.fragment.flags & FragmentFlags::PayloadFragmented != 0 {
                    next.push(child);
                } else {
                    leaves += 1;
                }
            }
        }
        level = next;
    }
    (lists, holes, leaves)
}

fn holes_options(step: usize) -> WriteOptions {
    WriteOptions::default()
        .with_fixed_size_chunk(step)
        .with_skip_holes()
}

/// A file stored skipping its holes reads back as the file, through every reader, and a file read
/// back from it holds the same holes. No hole is stored: the store holds the leaves and lists
/// alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_stored_skipping_holes_reads_back_with_its_holes() {
    if !holes_reported().await {
        return;
    }
    let dir = TempDir::new("lore-storage-holes-roundtrip-");
    let source = dir.path().join("image");
    let size = 24 * MIB;
    let extents = [
        (MIB + 3 * 4096, 2 * MIB + 4096),
        (8 * MIB, 4096),
        (16 * MIB - 4 * 4096, 3 * MIB),
    ];
    let content = sparse_file(&source, size, &extents).await;
    let source_data = data_ranges(&source).await;

    let (_store_dir, store) = make_store().await;
    let address = put(&store, &source, holes_options(FRAGMENT_SIZE_THRESHOLD)).await;
    let (lists, holes, leaves) = walk_lists(&store, address).await;
    assert_eq!(lists, 1);
    assert_eq!(
        holes,
        source_data.len() + 1,
        "one hole between and around the data"
    );
    let fragments = store
        .clone()
        .fragment_count()
        .await
        .expect("a fragment count");
    assert_eq!(
        fragments,
        lists + leaves,
        "the store holds a fragment for no hole"
    );

    let out = dir.path().join("out");
    lore_storage::read::read_into_file(
        store.clone(),
        partition(),
        address,
        &out,
        ".tmp",
        None,
        ReadOptions::default().sparse_writing(),
        None,
    )
    .await
    .expect("get the file");
    assert_eq!(std::fs::read(&out).expect("read the output"), content);
    assert_eq!(
        data_ranges(&out).await,
        source_data,
        "the holes are holes again"
    );
    assert!(
        allocated(&out) <= allocated(&source),
        "the output is allocated beyond its source"
    );

    let (_, whole) = lore_storage::read::read(
        store.clone(),
        partition(),
        address,
        None,
        ReadOptions::default(),
        None,
    )
    .await
    .expect("read into memory");
    assert_eq!(whole.as_ref(), content.as_slice());

    let range = 2 * MIB..9 * MIB;
    let mut memory = vec![0xEEu8; range.len()];
    // SAFETY: `memory` outlives the read, which returns once every handle to it is gone.
    let mut buffer = unsafe { CallerBuffer::new(memory.as_mut_ptr(), memory.len()) };
    lore_storage::read::read_into_buffer(
        store.clone(),
        partition(),
        address,
        Some(range.clone()),
        &mut buffer,
        ReadOptions::default(),
        None,
    )
    .await
    .expect("read a range into a buffer");
    drop(buffer);
    assert_eq!(memory.as_slice(), &content[range], "a hole reads as zeros");

    let (sender, mut receiver) = tokio::sync::mpsc::channel::<Result<Bytes, _>>(8);
    lore_storage::read::read_stream(
        store.clone(),
        partition(),
        address,
        None,
        ReadOptions::default(),
        sender,
        None,
    )
    .await
    .expect("stream the content");
    let mut streamed = Vec::with_capacity(size);
    while let Some(piece) = receiver.recv().await {
        let piece = piece.expect("a piece of the stream");
        assert!(
            piece.len() <= FRAGMENT_SIZE_THRESHOLD,
            "a piece larger than a fragment"
        );
        streamed.extend_from_slice(&piece);
    }
    assert_eq!(streamed, content);
}

/// A file with no hole stores exactly as it does without skipping holes.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_without_holes_is_addressed_as_without_skipping_holes() {
    let dir = TempDir::new("lore-storage-holes-none-");
    let source = dir.path().join("full");
    std::fs::write(&source, random_buffer(3 * MIB + 17)).expect("write the file");
    let (_store_dir, store) = make_store().await;
    for step in [0, FRAGMENT_SIZE_THRESHOLD] {
        let plain = WriteOptions::default().with_fixed_size_chunk(step);
        let with = put(&store, &source, plain.with_skip_holes()).await;
        let without = put(&store, &source, plain).await;
        assert_eq!(with, without, "chunking every {step} bytes");
    }
}

/// A file put before its data is written out gets the address it gets once it is: the put syncs
/// the file before it looks for holes, so a hole the file system fills with zeros when it writes
/// the data beside it out, as APFS does with one of under 16 MiB, is the same range either way. The
/// source is opened read-only.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_put_before_its_data_is_written_out_gets_the_address_it_gets_after() {
    use std::io::Seek;
    use std::io::Write;

    let dir = TempDir::new("lore-storage-holes-unsynced-");
    let source = dir.path().join("fresh");
    let mut file = std::fs::File::create(&source).expect("create test file");
    file.set_len(8 * MIB as u64).expect("size test file");
    for offset in [0, 2 * MIB, 6 * MIB] {
        file.seek(std::io::SeekFrom::Start(offset as u64))
            .and_then(|_| file.write_all(&random_buffer(MIB)))
            .expect("write extent");
    }
    let (_store_dir, store) = make_store().await;
    let options = holes_options(256 << 10);

    let before = put(&store, &source, options).await;
    file.sync_all().expect("sync test file");
    let after = put(&store, &source, options).await;

    assert_eq!(before, after);
}

/// Holes in a list long enough to be split into sublists: every sublist holding a hole is a list
/// of leaves, which a ranged read across a sublist boundary walks without a peek.
#[tokio::test(flavor = "multi_thread")]
async fn holes_in_a_tree_of_lists_read_back() {
    if !holes_reported().await {
        return;
    }
    let dir = TempDir::new("lore-storage-holes-tree-");
    let source = dir.path().join("image");
    let size = 32 * MIB;
    let extents = [
        (4096, 3 * MIB),
        (5 * MIB, 2 * MIB + 3 * 4096),
        (12 * MIB, 4096),
        (20 * MIB, 3 * MIB),
    ];
    let content = sparse_file(&source, size, &extents).await;

    let (_store_dir, store) = make_store().await;
    let address = put(&store, &source, holes_options(1024)).await;
    let (lists, holes, _) = walk_lists(&store, address).await;
    assert!(lists > 2, "a tree of lists, not {lists}");
    assert!(holes > 0);

    for range in [0..size, 3 * MIB..21 * MIB, 12 * MIB - 10..12 * MIB + 10] {
        let (_, read) = lore_storage::read::read(
            store.clone(),
            partition(),
            address,
            Some(range.clone()),
            ReadOptions::default(),
            None,
        )
        .await
        .expect("read a range");
        assert_eq!(read.as_ref(), &content[range.clone()], "range {range:?}");
    }

    let out = dir.path().join("out");
    lore_storage::read::read_into_file(
        store.clone(),
        partition(),
        address,
        &out,
        ".tmp",
        None,
        ReadOptions::default().sparse_writing(),
        None,
    )
    .await
    .expect("get the file");
    assert_eq!(std::fs::read(&out).expect("read the output"), content);
    assert_eq!(data_ranges(&out).await, data_ranges(&source).await);
}

/// Reserves `range` of the file at `path` as an extent allocated and never written, an unwritten
/// extent, or `false` where its file system reserves none or has no extent map to report one by,
/// as tmpfs.
#[cfg(target_os = "linux")]
fn reserve(path: &Path, range: std::ops::Range<usize>) -> bool {
    use std::os::fd::AsRawFd;

    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open to reserve");
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `fstatfs` fills `stats`, which outlives the call.
    assert_eq!(
        unsafe { libc::fstatfs(file.as_raw_fd(), stats.as_mut_ptr()) },
        0,
        "statfs"
    );
    // SAFETY: `fstatfs` succeeded, so it filled `stats`.
    if unsafe { stats.assume_init() }.f_type as u64 == libc::TMPFS_MAGIC as u64 {
        return false;
    }
    // SAFETY: `fallocate` reads no memory of ours.
    let reserved = unsafe {
        libc::fallocate(
            file.as_raw_fd(),
            0,
            range.start as libc::off_t,
            range.len() as libc::off_t,
        )
    };
    if reserved != 0 {
        return false;
    }
    file.sync_all().expect("sync the reservation");
    true
}

/// An extent allocated and never written is a hole whatever the page cache holds: a put after the
/// whole file was read, which leaves the extent's pages cached, has the address of the put before,
/// and stores nothing new. Read back, the extent is a hole that holds no storage.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn an_unwritten_extent_is_a_hole_whatever_the_cache_holds() {
    if !holes_reported().await {
        return;
    }
    let dir = TempDir::new("lore-storage-holes-unwritten-");
    let source = dir.path().join("image");
    let size = 16 * MIB;
    let extents = [(MIB + 3 * 4096, MIB), (10 * MIB, 512 * 1024 + 4096)];
    let content = sparse_file(&source, size, &extents).await;
    if !reserve(&source, 4 * MIB..8 * MIB) {
        eprintln!("the file system the test runs on reports no unwritten extent");
        return;
    }
    let source_data = data_ranges(&source).await;
    assert_eq!(
        source_data,
        [
            MIB as u64 + 3 * 4096..2 * MIB as u64 + 3 * 4096,
            10 * MIB as u64..10 * MIB as u64 + 512 * 1024 + 4096,
        ],
        "the reserved extent is a hole"
    );

    let (_store_dir, store) = make_store().await;
    let options = holes_options(FRAGMENT_SIZE_THRESHOLD);
    let address = put(&store, &source, options).await;
    let (_, holes, _) = walk_lists(&store, address).await;
    assert_eq!(holes, source_data.len() + 1);
    let fragments = store
        .clone()
        .fragment_count()
        .await
        .expect("a fragment count");

    std::fs::read(&source).expect("read the whole file, the reserved extent included");
    assert_eq!(
        put(&store, &source, options).await,
        address,
        "the address does not depend on the page cache"
    );
    assert_eq!(
        store
            .clone()
            .fragment_count()
            .await
            .expect("a fragment count"),
        fragments,
        "nothing new stored"
    );

    let out = dir.path().join("out");
    lore_storage::read::read_into_file(
        store.clone(),
        partition(),
        address,
        &out,
        ".tmp",
        None,
        ReadOptions::default().sparse_writing(),
        None,
    )
    .await
    .expect("get the file");
    assert_eq!(std::fs::read(&out).expect("read the output"), content);
    assert_eq!(data_ranges(&out).await, source_data);
    assert!(
        allocated(&out) < allocated(&source),
        "the reserved extent holds storage in the output"
    );
}

/// A hole and written zeros read the same, but are different content: a range with no data, and
/// data. The two files get different addresses, and each reads back as it was put, the one with
/// a hole there and the other with zeros written there.
#[tokio::test(flavor = "multi_thread")]
async fn a_hole_and_written_zeros_are_different_content() {
    if !holes_reported().await {
        return;
    }
    let dir = TempDir::new("lore-storage-holes-zeros-");
    let size = 4 * MIB;
    let with_hole = dir.path().join("hole");
    let content = sparse_file(&with_hole, size, &[(0, MIB), (3 * MIB, MIB)]).await;
    let with_zeros = dir.path().join("zeros");
    std::fs::write(&with_zeros, &content).expect("write the file with zeros");
    let hole_data = data_ranges(&with_hole).await;
    assert_eq!(hole_data, [0..MIB as u64, 3 * MIB as u64..size as u64]);
    let whole = 0..size as u64;
    assert_eq!(data_ranges(&with_zeros).await, std::slice::from_ref(&whole));

    let (_store_dir, store) = make_store().await;
    let options = holes_options(FRAGMENT_SIZE_THRESHOLD);
    let hole_address = put(&store, &with_hole, options).await;
    let zeros_address = put(&store, &with_zeros, options).await;
    assert_ne!(hole_address, zeros_address);
    assert_eq!(walk_lists(&store, hole_address).await.1, 1);
    assert_eq!(walk_lists(&store, zeros_address).await.1, 0);

    for (address, name, data) in [
        (hole_address, "out-hole", hole_data),
        (zeros_address, "out-zeros", vec![whole]),
    ] {
        let out = dir.path().join(name);
        lore_storage::read::read_into_file(
            store.clone(),
            partition(),
            address,
            &out,
            ".tmp",
            None,
            ReadOptions::default().sparse_writing(),
            None,
        )
        .await
        .expect("get the file");
        assert_eq!(std::fs::read(&out).expect("read the output"), content);
        assert_eq!(data_ranges(&out).await, data, "{name}");
    }
}

/// A file that is one hole is a list of that hole, and reads back as a file with no data.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_that_is_one_hole_reads_back_as_one_hole() {
    if !holes_reported().await {
        return;
    }
    let dir = TempDir::new("lore-storage-holes-one-");
    let source = dir.path().join("hole");
    let size = 64 * MIB;
    sparse_file(&source, size, &[]).await;

    let (_store_dir, store) = make_store().await;
    let address = put(&store, &source, holes_options(FRAGMENT_SIZE_THRESHOLD)).await;
    assert_eq!(walk_lists(&store, address).await, (1, 1, 0));

    let out = dir.path().join("out");
    lore_storage::read::read_into_file(
        store.clone(),
        partition(),
        address,
        &out,
        ".tmp",
        None,
        ReadOptions::default().sparse_writing(),
        None,
    )
    .await
    .expect("get the file");
    assert_eq!(std::fs::metadata(&out).expect("stat").len(), size as u64);
    assert!(
        data_ranges(&out).await.is_empty(),
        "the output holds no data"
    );
}

/// A file of 40 MiB with random data at [`IMAGE_DATA`], zeros written at [`IMAGE_ZEROS`], and a
/// hole everywhere else, each on whole leaves of 256 KiB. Returns the content it reads as.
async fn image_with_zeros(path: &Path) -> Vec<u8> {
    let content = sparse_file(path, IMAGE_SIZE, &IMAGE_DATA).await;
    write_at(
        path,
        IMAGE_ZEROS.start as u64,
        &vec![0u8; IMAGE_ZEROS.len()],
    )
    .await;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open to punch the holes");
    file.sync_all().expect("sync the zeros");
    punch_gaps(
        &file,
        IMAGE_SIZE,
        &[
            IMAGE_DATA[0],
            (IMAGE_ZEROS.start, IMAGE_ZEROS.len()),
            IMAGE_DATA[1],
        ],
    );
    content
}

const IMAGE_SIZE: usize = 40 * MIB;
const IMAGE_DATA: [(usize, usize); 2] = [(MIB, MIB), (20 * MIB, MIB)];
const IMAGE_ZEROS: std::ops::Range<usize> = 8 * MIB..12 * MIB;

/// The ranges of [`image_with_zeros`] holding random data.
fn image_data() -> Vec<std::ops::Range<u64>> {
    IMAGE_DATA
        .iter()
        .map(|&(offset, length)| offset as u64..(offset + length) as u64)
        .collect()
}

/// Gets `address` into `out`, with `options`, writing it sparse as the storage API's file get
/// does.
async fn get(store: &Arc<dyn ImmutableStore>, address: Address, out: &Path, options: ReadOptions) {
    lore_storage::read::read_into_file(
        store.clone(),
        partition(),
        address,
        out,
        ".loretmp",
        None,
        options.sparse_writing(),
        None,
    )
    .await
    .expect("get the file");
}

/// Written zeros are data, whatever the size of the get: a file of 40 MiB stored skipping its holes
/// reads back with its holes as holes and its written zeros written and allocated, and one stored
/// without reads back written whole, as its holes were stored as data.
#[tokio::test(flavor = "multi_thread")]
async fn a_large_get_writes_its_written_zeros_as_data() {
    if !holes_reported().await {
        return;
    }
    let dir = TempDir::new("lore-storage-holes-written-zeros-");
    let source = dir.path().join("image.img");
    let content = image_with_zeros(&source).await;
    let (_store_dir, store) = make_store().await;
    let step = 256 * 1024;
    let mut with_zeros = image_data();
    with_zeros.insert(1, IMAGE_ZEROS.start as u64..IMAGE_ZEROS.end as u64);
    for (name, options, written) in [
        ("skipping holes", holes_options(step), with_zeros),
        (
            "whole",
            WriteOptions::default().with_fixed_size_chunk(step),
            std::iter::once(0..IMAGE_SIZE as u64).collect(),
        ),
    ] {
        let address = put(&store, &source, options).await;
        let out = dir.path().join("out.img");
        get(&store, address, &out, ReadOptions::default()).await;
        assert_eq!(
            std::fs::read(&out).expect("read the output"),
            content,
            "{name}"
        );
        let held = allocated(&out);
        let written_len: u64 = written.iter().map(|range| range.end - range.start).sum();
        assert!(
            held >= written_len,
            "{name}: every range written, zeros included, holds storage, by the storage the file \
             holds"
        );
        assert_eq!(data_ranges(&out).await, written, "{name}");
    }
}

/// A file that is not sparse at `path`, of `size` bytes, holding random data in its first MiB and
/// zeros written in its second, extended past them and preallocated to its size where
/// `preallocated`, then marked sparse there. Returns the content it reads as.
#[cfg(target_family = "windows")]
async fn written_short_of_its_size(path: &Path, size: usize, preallocated: bool) -> Vec<u8> {
    use std::io::Write;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Storage::FileSystem::FILE_ALLOCATION_INFO;
    use windows_sys::Win32::Storage::FileSystem::FileAllocationInfo;
    use windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle;

    let mut content = random_buffer(MIB);
    content.resize(size, 0);
    let mut file = std::fs::File::create(path).expect("create the file");
    if preallocated {
        let allocation = FILE_ALLOCATION_INFO {
            AllocationSize: size as i64,
        };
        // SAFETY: the handle is open, and `allocation` outlives the call.
        let done = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileAllocationInfo,
                (&raw const allocation).cast(),
                size_of::<FILE_ALLOCATION_INFO>() as u32,
            )
        };
        assert_ne!(done, 0, "preallocate the file");
    }
    file.write_all(&content[..2 * MIB])
        .expect("write the data and the zeros");
    file.set_len(size as u64).expect("extend the file");
    drop(file);
    if preallocated {
        lore_io::IoDriver::global()
            .open(path, &lore_io::OpenOptions::new().read(true).write(true))
            .await
            .expect("open to mark sparse")
            .set_len_sparse(size as u64)
            .await
            .expect("mark the file sparse");
    }
    content
}

/// The tail of a file past its valid data length was never written, and is a hole: a file that is
/// not sparse extended past what was written, and one preallocated and then marked sparse, store
/// it as a hole and read back as the file, with the tail unallocated and the zeros written below
/// it allocated, as data.
#[cfg(target_family = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn a_tail_past_the_valid_data_length_is_a_hole() {
    let dir = TempDir::new("lore-storage-holes-valid-");
    if !lore_io::clone::volume_of(dir.path()).is_ok_and(|volume| volume.filesystem == "NTFS") {
        eprintln!(
            "the tests do not run on NTFS, the one file system whose valid data length is read"
        );
        return;
    }
    let (_store_dir, store) = make_store().await;
    let size = 8 * MIB;
    let written = 0..2 * MIB as u64;
    for (name, preallocated) in [("extended", false), ("preallocated", true)] {
        let source = dir.path().join(format!("{name}.img"));
        let content = written_short_of_its_size(&source, size, preallocated).await;
        assert_eq!(
            data_ranges(&source).await,
            std::slice::from_ref(&written),
            "{name}"
        );

        let address = put(&store, &source, holes_options(FRAGMENT_SIZE_THRESHOLD)).await;
        assert_eq!(walk_lists(&store, address).await.1, 1, "{name}: the tail");
        let out = dir.path().join(format!("{name}-out.img"));
        get(&store, address, &out, ReadOptions::default()).await;
        assert_eq!(
            std::fs::read(&out).expect("read the output"),
            content,
            "{name}"
        );
        assert_eq!(
            data_ranges(&out).await,
            std::slice::from_ref(&written),
            "{name}"
        );
        assert_eq!(
            allocated(&out),
            written.end,
            "{name}: the zeros are allocated"
        );
        assert!(
            allocated(&source) > written.end,
            "{name}: the source holds its tail"
        );
    }
}

/// A view of the whole of a file, mapped to write and unmapped when dropped, so that what is
/// written through it stays unflushed until then.
#[cfg(target_family = "windows")]
struct MappedView {
    mapping: windows_sys::Win32::Foundation::HANDLE,
    view: windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS,
}

#[cfg(target_family = "windows")]
impl MappedView {
    /// Maps the `size` bytes of `file` and writes `bytes` at `offset` through the view.
    fn write(file: &std::fs::File, size: usize, offset: usize, bytes: &[u8]) -> Self {
        use std::os::windows::io::AsRawHandle;

        use windows_sys::Win32::System::Memory::CreateFileMappingW;
        use windows_sys::Win32::System::Memory::FILE_MAP_WRITE;
        use windows_sys::Win32::System::Memory::MapViewOfFile;
        use windows_sys::Win32::System::Memory::PAGE_READWRITE;

        assert!(offset + bytes.len() <= size, "the bytes fit the view");
        // SAFETY: the handle is open, the view holds `size` bytes, of which the copy writes
        // `bytes.len()` from `offset`, and `Drop` closes the view and the mapping.
        unsafe {
            let mapping = CreateFileMappingW(
                file.as_raw_handle(),
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                0,
                std::ptr::null(),
            );
            assert!(!mapping.is_null(), "map the file");
            let view = MapViewOfFile(mapping, FILE_MAP_WRITE, 0, 0, size);
            assert!(!view.Value.is_null(), "map a view of the file");
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                view.Value.cast::<u8>().add(offset),
                bytes.len(),
            );
            Self { mapping, view }
        }
    }
}

#[cfg(target_family = "windows")]
impl Drop for MappedView {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::UnmapViewOfFile;

        // SAFETY: the view and the mapping are this value's, and nothing uses them after.
        unsafe {
            UnmapViewOfFile(self.view);
            CloseHandle(self.mapping);
        }
    }
}

/// Data written through a memory mapping, with the view still mapped and not flushed, is data: a
/// file written whole that way stores no hole, and reads back as written.
#[cfg(target_family = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn data_written_through_a_mapping_is_not_a_hole() {
    let dir = TempDir::new("lore-storage-holes-mapped-");
    let source = dir.path().join("mapped.img");
    let size = 4 * MIB;
    let content = random_buffer(size);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&source)
        .expect("create the file");
    file.set_len(size as u64).expect("size the file");
    let view = MappedView::write(&file, size, 0, &content);

    let (_store_dir, store) = make_store().await;
    let address = put(&store, &source, holes_options(FRAGMENT_SIZE_THRESHOLD)).await;
    drop(view);
    drop(file);
    assert_eq!(walk_lists(&store, address).await.1, 0, "no hole");
    let out = dir.path().join("mapped-out.img");
    get(&store, address, &out, ReadOptions::default()).await;
    assert_eq!(std::fs::read(&out).expect("read the output"), content);
}

/// Data written through a memory mapping into a hole of a sparse file that another handle holds
/// open, sharing no write access, so that the put cannot flush it, is data: the put stores no hole,
/// and the file reads back as written.
#[cfg(target_family = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn data_mapped_into_a_file_the_put_cannot_flush_is_not_a_hole() {
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

    let dir = TempDir::new("lore-storage-holes-mapped-held-");
    let source = dir.path().join("held.img");
    let size = 4 * MIB;
    let mut content = sparse_file(&source, size, &[(0, MIB)]).await;
    let mapped = random_buffer(MIB);
    content[2 * MIB..3 * MIB].copy_from_slice(&mapped);
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ)
        .open(&source)
        .expect("hold the file, sharing no write access");
    let view = MappedView::write(&holder, size, 2 * MIB, &mapped);

    let (_store_dir, store) = make_store().await;
    let address = put(&store, &source, holes_options(FRAGMENT_SIZE_THRESHOLD)).await;
    drop(view);
    drop(holder);
    assert_eq!(walk_lists(&store, address).await.1, 0, "no hole");
    let out = dir.path().join("held-out.img");
    get(&store, address, &out, ReadOptions::default()).await;
    assert_eq!(std::fs::read(&out).expect("read the output"), content);
}

/// A get writes its file sparse only where its options ask, as the storage API's file get does:
/// without them, as the revision layer reads, the file it writes is not sparse, and holds the
/// holes of the content as zeros. A direct write without them over a file a get made sparse keeps
/// that file's sparse attribute, as before Windows support for a file made sparse by anything.
#[cfg(target_family = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn a_get_writes_its_file_sparse_only_where_asked() {
    use std::os::windows::fs::MetadataExt;

    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_SPARSE_FILE;

    let dir = TempDir::new("lore-storage-holes-sparse-write-");
    let source = dir.path().join("image.img");
    let content = sparse_file(&source, 8 * MIB, &[(MIB, MIB), (5 * MIB, 4096)]).await;
    let (_store_dir, store) = make_store().await;
    let address = put(&store, &source, holes_options(FRAGMENT_SIZE_THRESHOLD)).await;
    let get_sparse = |out: PathBuf, sparse_write: bool, direct_write: bool| {
        let store = store.clone();
        let content = &content;
        async move {
            let options = ReadOptions {
                sparse_write,
                direct_write,
                ..ReadOptions::default()
            };
            lore_storage::read::read_into_file(
                store,
                partition(),
                address,
                &out,
                ".loretmp",
                None,
                options,
                None,
            )
            .await
            .expect("get the file");
            assert_eq!(&std::fs::read(&out).expect("read the output"), content);
            std::fs::metadata(&out).expect("stat").file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE
                != 0
        }
    };
    for sparse in [false, true] {
        let out = dir.path().join(format!("out-{sparse}.img"));
        assert_eq!(
            get_sparse(out, sparse, false).await,
            sparse,
            "sparse_write {sparse}"
        );
    }
    let direct = dir.path().join("direct.img");
    assert!(
        get_sparse(direct.clone(), true, true).await,
        "a direct write asked to be sparse"
    );
    assert!(
        get_sparse(direct, false, true).await,
        "a direct write not asked to be sparse keeps the attribute of the file it replaces"
    );
}

/// A get over a temporary file an interrupted get left behind leaves none of its bytes in a hole.
#[tokio::test(flavor = "multi_thread")]
async fn a_get_over_a_leftover_temporary_file_reads_zeros_in_its_holes() {
    if !holes_reported().await {
        return;
    }
    let dir = TempDir::new("lore-storage-holes-leftover-");
    let source = dir.path().join("image.img");
    let content = sparse_file(&source, 8 * MIB, &[(MIB, MIB), (5 * MIB, 4096)]).await;
    let (_store_dir, store) = make_store().await;
    let address = put(&store, &source, holes_options(FRAGMENT_SIZE_THRESHOLD)).await;

    let out = dir.path().join("out.img");
    std::fs::write(dir.path().join("out.img.loretmp"), vec![0xAB; 8 * MIB])
        .expect("leave a temporary file behind");
    get(&store, address, &out, ReadOptions::default()).await;
    assert_eq!(std::fs::read(&out).expect("read the output"), content);
}

/// A direct write over a larger file holding other bytes replaces it whole: its holes read as
/// zeros, and its leaves of zeros are written, whatever its size.
#[tokio::test(flavor = "multi_thread")]
async fn a_direct_write_over_a_file_replaces_it_whole() {
    if !holes_reported().await {
        return;
    }
    let dir = TempDir::new("lore-storage-holes-direct-");
    let source = dir.path().join("image.img");
    let content = image_with_zeros(&source).await;
    let (_store_dir, store) = make_store().await;
    let address = put(&store, &source, holes_options(256 * 1024)).await;

    let out = dir.path().join("out.img");
    std::fs::write(&out, vec![0xAB; IMAGE_SIZE + MIB]).expect("write the earlier file");
    let options = ReadOptions {
        direct_write: true,
        ..ReadOptions::default()
    };
    get(&store, address, &out, options).await;
    assert_eq!(std::fs::read(&out).expect("read the output"), content);
    let mut written = image_data();
    written.insert(1, IMAGE_ZEROS.start as u64..IMAGE_ZEROS.end as u64);
    assert_eq!(data_ranges(&out).await, written);
}

/// A direct write over a sparse file whose data is not yet written out, as a get without a sync
/// leaves its file, leaves the holes of the content it writes as holes: none of the ranges the
/// earlier file held is allocated in them. Measured on version 3.14 of the Resilient File System, a
/// sparse file truncated before its data is written out and grown again records those ranges as
/// allocated.
#[tokio::test(flavor = "multi_thread")]
async fn a_direct_write_over_a_sparse_file_not_yet_written_out_leaves_its_holes() {
    if !holes_reported().await {
        return;
    }
    let dir = TempDir::new("lore-storage-holes-direct-unsynced-");
    let size = 8 * MIB;
    let whole = dir.path().join("whole.img");
    sparse_file(&whole, size, &[(0, size)]).await;
    let holed = dir.path().join("holed.img");
    let content = sparse_file(&holed, size, &[(MIB, MIB), (5 * MIB, 4096)]).await;
    let holed_data = data_ranges(&holed).await;
    let (_store_dir, store) = make_store().await;
    let options = holes_options(FRAGMENT_SIZE_THRESHOLD);
    let whole_address = put(&store, &whole, options).await;
    let holed_address = put(&store, &holed, options).await;

    let out = dir.path().join("out.img");
    let direct = ReadOptions {
        direct_write: true,
        ..ReadOptions::default()
    };
    get(&store, whole_address, &out, direct).await;
    get(&store, holed_address, &out, direct).await;
    assert_eq!(std::fs::read(&out).expect("read the output"), content);
    assert_eq!(data_ranges(&out).await, holed_data);
}

/// A get of a range that starts and ends inside holes writes that range alone: the parts of the
/// holes it holds are holes of the file it writes, and the data between them is written.
#[tokio::test(flavor = "multi_thread")]
async fn a_ranged_get_from_inside_a_hole_to_inside_a_hole() {
    if !holes_reported().await {
        return;
    }
    let dir = TempDir::new("lore-storage-holes-range-");
    let source = dir.path().join("image.img");
    let content = sparse_file(&source, 8 * MIB, &[(MIB, MIB), (5 * MIB, MIB)]).await;
    let (_store_dir, store) = make_store().await;
    let address = put(&store, &source, holes_options(FRAGMENT_SIZE_THRESHOLD)).await;

    let half = MIB as u64 / 2;
    for (range, data) in [
        (
            MIB / 2..4 * MIB,
            std::iter::once(half..3 * half).collect::<Vec<_>>(),
        ),
        (5 * MIB / 2..9 * MIB / 2, Vec::new()),
    ] {
        let out = dir.path().join(format!("out-{}.img", range.start));
        lore_storage::read::read_into_file(
            store.clone(),
            partition(),
            address,
            &out,
            ".loretmp",
            Some(range.clone()),
            ReadOptions::default().sparse_writing(),
            None,
        )
        .await
        .expect("get the range");
        assert_eq!(
            std::fs::read(&out).expect("read the output"),
            &content[range.clone()],
            "range {range:?}"
        );
        assert_eq!(data_ranges(&out).await, data, "range {range:?}");
    }
}

/// A file whose size is not a whole number of blocks, with data in its last, partial block, reads
/// back whole: the range of data the file system reports for that block ends at the end of the
/// file, not of the block.
#[tokio::test(flavor = "multi_thread")]
async fn data_in_the_last_partial_block_reads_back() {
    if !holes_reported().await {
        return;
    }
    let dir = TempDir::new("lore-storage-holes-tail-");
    let source = dir.path().join("image.img");
    let size = 3 * MIB + 1234;
    let content = sparse_file(&source, size, &[(MIB, 4096), (3 * MIB - 4096, 4096 + 1234)]).await;
    let source_data = data_ranges(&source).await;
    assert_eq!(
        source_data.last().map(|range| range.end),
        Some(size as u64),
        "data at the end of the file"
    );
    let (_store_dir, store) = make_store().await;
    for step in [0, 64 * 1024] {
        let address = put(&store, &source, holes_options(step)).await;
        assert_eq!(
            walk_lists(&store, address).await.1,
            2,
            "chunking every {step} bytes"
        );
        let out = dir.path().join(format!("out-{step}.img"));
        get(&store, address, &out, ReadOptions::default()).await;
        assert_eq!(
            std::fs::read(&out).expect("read the output"),
            content,
            "chunking every {step} bytes"
        );
        assert_eq!(
            data_ranges(&out).await,
            source_data,
            "chunking every {step} bytes"
        );
    }
}

/// Obliterating content stored skipping holes passes over its holes, which name no fragment, and
/// removes every list and leaf it holds.
#[tokio::test(flavor = "multi_thread")]
async fn obliterating_content_with_holes_removes_its_fragments() {
    if !holes_reported().await {
        return;
    }
    let dir = TempDir::new("lore-storage-holes-obliterate-");
    let source = dir.path().join("image.img");
    sparse_file(&source, 8 * MIB, &[(MIB, MIB), (5 * MIB, 4096)]).await;
    let (_store_dir, store) = make_store().await;
    let address = put(&store, &source, holes_options(FRAGMENT_SIZE_THRESHOLD)).await;
    let (lists, holes, leaves) = walk_lists(&store, address).await;
    assert!(holes > 0, "a list holding holes");

    let stats = Arc::new(lore_storage::StoreObliterateStats::default());
    store
        .clone()
        .obliterate(partition(), address, stats.clone())
        .await
        .expect("obliterate the content");
    assert_eq!(
        stats
            .num_fragments
            .load(std::sync::atomic::Ordering::Relaxed),
        lists + leaves,
        "every list and leaf is obliterated"
    );
}

/// Stores `payload` under the hash of its bytes, as a fragment with `flags` standing for
/// `size_content` bytes of content.
async fn store_fragment(
    store: &Arc<dyn ImmutableStore>,
    payload: &[u8],
    flags: u32,
    size_content: u64,
) -> Address {
    let address = Address {
        hash: lore_storage::hash::hash_slice(payload),
        context: Context::from([0x5C; 16]),
    };
    store
        .clone()
        .put(
            partition(),
            address,
            lore_storage::Fragment {
                flags,
                size_payload: payload.len() as u32,
                size_content,
            },
            Some(Bytes::copy_from_slice(payload)),
            false,
        )
        .await
        .expect("store the fragment");
    address
}

/// A list holding a hole is a list of leaves for a read into memory or into a caller's buffer, as
/// for every other walk: an entry beside the hole that names a fragment list is refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_into_memory_refuses_a_list_named_beside_a_hole() {
    use zerocopy::IntoBytes;

    let (_store_dir, store) = make_store().await;
    let first = store_fragment(&store, &random_buffer(100), 0, 100).await;
    let second = store_fragment(&store, &random_buffer(100), 0, 100).await;
    let sublist = [
        FragmentReference {
            hash: first.hash,
            offset_content: 0,
        },
        FragmentReference {
            hash: second.hash,
            offset_content: 100,
        },
    ];
    let fragmented = FragmentFlags::PayloadFragmented.bits();
    let sublist = store_fragment(&store, sublist.as_bytes(), fragmented, 200).await;
    let list = [
        FragmentReference {
            hash: lore_storage::Hash::default(),
            offset_content: 0,
        },
        FragmentReference {
            hash: sublist.hash,
            offset_content: 4096,
        },
    ];
    let address = store_fragment(&store, list.as_bytes(), fragmented, 4096 + 200).await;

    let mut memory = vec![0u8; 4096 + 200];
    // SAFETY: `memory` outlives the read, which returns once every handle to it is gone.
    let mut buffer = unsafe { CallerBuffer::new(memory.as_mut_ptr(), memory.len()) };
    lore_storage::read::read_into_buffer(
        store.clone(),
        partition(),
        address,
        None,
        &mut buffer,
        ReadOptions::default(),
        None,
    )
    .await
    .expect_err("a list beside a hole is refused in a buffer read");
    drop(buffer);
    lore_storage::read::read(
        store.clone(),
        partition(),
        address,
        None,
        ReadOptions::default(),
        None,
    )
    .await
    .expect_err("a list beside a hole is refused in a read into memory");
}
