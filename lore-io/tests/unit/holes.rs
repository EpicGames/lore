// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ops::Range;

use lore_base::test_util::TempDir;
use lore_io::DataCursor;
use lore_io::IoDriver;
use lore_io::IoFile;
use lore_io::OpenOptions;

const MIB: u64 = 1 << 20;

/// A file of `size` bytes with `0xAB` at `extents` and a hole everywhere else, open to read and
/// write. On Apple platforms the holes are punched once the data is synced, as APFS fills a hole
/// of under 16 MiB beside data it flushes.
async fn sparse_file(dir: &TempDir, size: u64, extents: &[Range<u64>]) -> IoFile {
    let file = IoDriver::global()
        .open(
            dir.path().join("sparse"),
            &OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true),
        )
        .await
        .expect("open");
    file.set_len_sparse(size).await.expect("size");
    for extent in extents {
        let data = vec![0xABu8; (extent.end - extent.start) as usize];
        file.write_all_at(data, extent.start).await.expect("write");
    }
    file.sync_all().await.expect("sync");
    #[cfg(target_vendor = "apple")]
    file.punch_holes(gaps(size, extents))
        .await
        .expect("punch the holes");
    file
}

/// The ranges of a file of `size` bytes outside `extents`, which are sorted and apart.
#[cfg(target_vendor = "apple")]
fn gaps(size: u64, extents: &[Range<u64>]) -> Vec<Range<u64>> {
    let mut gaps = Vec::new();
    let mut at = 0;
    for extent in extents {
        if extent.start > at {
            gaps.push(at..extent.start);
        }
        at = extent.end;
    }
    if at < size {
        gaps.push(at..size);
    }
    gaps
}

/// Whether `dir` is on NTFS, the one file system whose valid data length a walk reads. A test of
/// that rule returns early elsewhere, saying why.
#[cfg(target_family = "windows")]
fn on_ntfs(dir: &TempDir) -> bool {
    let ntfs =
        lore_io::clone::volume_of(dir.path()).is_ok_and(|volume| volume.filesystem == "NTFS");
    if !ntfs {
        eprintln!(
            "the tests do not run on NTFS, the one file system whose valid data length is read"
        );
    }
    ntfs
}

/// The block a walk of `file` reads on its first lookup.
async fn block(file: &IoFile) -> u64 {
    let (found, cursor) = file.next_data(0, DataCursor::default()).await;
    found.expect("find data");
    cursor.block().expect("the block, after the first lookup")
}

/// Every range of data the file reports from the start, in one walk with one cursor.
async fn data(file: &IoFile) -> Vec<Range<u64>> {
    let mut ranges = Vec::new();
    let mut offset = 0;
    let mut cursor = DataCursor::default();
    loop {
        let (found, walked) = file.next_data(offset, cursor).await;
        cursor = walked;
        let Some(range) = found.expect("find data") else {
            break;
        };
        offset = range.end;
        ranges.push(range);
    }
    ranges
}

/// The first range of data at or after `offset`, from a walk of its own.
async fn next_data(file: &IoFile, offset: u64) -> Option<Range<u64>> {
    file.next_data(offset, DataCursor::default())
        .await
        .0
        .expect("find data")
}

/// Whether the file system the tests run on reports holes, probed once with a file holding a hole
/// between two ranges of data. A test that needs holes reported returns early where they are not,
/// saying why. On Windows the tests need a temporary directory on a file system that makes files
/// sparse, as NTFS does, and fail elsewhere, so that a run there never skips them unseen.
async fn holes_reported() -> bool {
    static REPORTED: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();
    let reported = *REPORTED
        .get_or_init(|| async {
            let dir = TempDir::new("lore-io-holes-probe-");
            let file = sparse_file(&dir, 4 * MIB, &[0..4096, 2 * MIB..2 * MIB + 4096]).await;
            data(&file).await.len() == 2
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

/// Data written on whole megabytes is reported where it was written, with the holes around it
/// left out, and past the last data there is none.
#[tokio::test]
async fn next_data_reports_written_ranges_and_skips_holes() {
    if !holes_reported().await {
        return;
    }
    let dir = TempDir::new("lore-io-holes-");
    let extents = [MIB..2 * MIB, 5 * MIB..7 * MIB];
    let file = sparse_file(&dir, 16 * MIB, &extents).await;
    assert_eq!(data(&file).await, extents);
    assert_eq!(next_data(&file, 7 * MIB).await, None, "past the data");
    assert_eq!(next_data(&file, 64 * MIB).await, None, "past the end");
    assert_eq!(
        next_data(&file, MIB + 4096).await,
        Some(MIB + 4096..2 * MIB),
        "inside data"
    );
}

/// The valid data length is the end of the valid regions where the regions tile the file from
/// offset 0 with every valid region first, as NTFS reports them, and zero where none is valid.
#[test]
fn the_valid_data_length_is_read_from_regions_in_the_shape_ntfs_reports() {
    use lore_io::holes::valid_end;

    assert_eq!(
        valid_end([(0..MIB, true), (MIB..4 * MIB, false)], 4 * MIB),
        Some(MIB)
    );
    assert_eq!(valid_end([(0..4 * MIB, true)], 4 * MIB), Some(4 * MIB));
    assert_eq!(valid_end([(0..4 * MIB, false)], 4 * MIB), Some(0));
    assert_eq!(
        valid_end(
            [
                (0..MIB, true),
                (MIB..2 * MIB, true),
                (2 * MIB..4 * MIB, false)
            ],
            4 * MIB
        ),
        Some(2 * MIB),
        "adjacent valid regions"
    );
}

/// Any other answer is not trusted: no region, a gap or an overlap, a first region past offset 0,
/// a valid region after one that is not, an empty region, or regions that stop short of the file.
#[test]
fn the_valid_data_length_is_not_read_from_regions_in_another_shape() {
    use lore_io::holes::valid_end;

    let none: [(std::ops::Range<u64>, bool); 0] = [];
    assert_eq!(valid_end(none, 4 * MIB), None, "no region");
    assert_eq!(
        valid_end([(0..MIB, true), (2 * MIB..4 * MIB, false)], 4 * MIB),
        None,
        "a gap"
    );
    assert_eq!(
        valid_end([(0..2 * MIB, true), (MIB..4 * MIB, false)], 4 * MIB),
        None,
        "an overlap"
    );
    assert_eq!(
        valid_end([(MIB..4 * MIB, true)], 4 * MIB),
        None,
        "past offset 0"
    );
    assert_eq!(
        valid_end([(0..MIB, false), (MIB..4 * MIB, true)], 4 * MIB),
        None,
        "valid after unflagged"
    );
    assert_eq!(
        valid_end([(0..0, true), (0..4 * MIB, false)], 4 * MIB),
        None,
        "an empty region"
    );
    assert_eq!(
        valid_end([(0..MIB, true)], 4 * MIB),
        None,
        "short of the file"
    );
}

/// Allocated ranges of one map fold into the range of data a lookup builds: adjacent and
/// overlapping ranges are one, clipped to the lookup's offset and the valid data length, and the
/// range is complete where a gap or the valid data length ends it.
#[test]
fn allocated_ranges_fold_into_one_range_of_data() {
    use lore_io::holes::fold_ranges;

    let mut found = None;
    assert!(fold_ranges(
        &mut found,
        [0..MIB, MIB..2 * MIB, 3 * MIB..4 * MIB],
        0,
        u64::MAX
    ));
    assert_eq!(
        found,
        Some(0..2 * MIB),
        "adjacent ranges are one, a gap ends it"
    );

    let mut found = None;
    assert!(fold_ranges(
        &mut found,
        [0..2 * MIB, MIB..3 * MIB, 4 * MIB..5 * MIB],
        0,
        u64::MAX
    ));
    assert_eq!(found, Some(0..3 * MIB), "overlapping ranges are one");

    let mut found = None;
    assert!(!fold_ranges(
        &mut found,
        std::iter::once(0..2 * MIB),
        MIB,
        u64::MAX
    ));
    assert_eq!(
        found,
        Some(MIB..2 * MIB),
        "clipped to the offset, and open at the batch's end"
    );

    let mut found = None;
    assert!(fold_ranges(
        &mut found,
        [0..2 * MIB, 3 * MIB..4 * MIB],
        0,
        MIB
    ));
    assert_eq!(
        found,
        Some(0..MIB),
        "clipped to, and ended by, the valid data length"
    );
}

/// A range of data goes on across two maps where one ends exactly where the next begins, and ends
/// at a gap between them; a map that holds no range leaves it as it was.
#[test]
fn a_range_of_data_folds_across_two_maps() {
    use lore_io::holes::fold_ranges;

    let mut found = None;
    assert!(!fold_ranges(
        &mut found,
        [0..MIB, MIB..2 * MIB],
        0,
        u64::MAX
    ));
    assert!(!fold_ranges(
        &mut found,
        std::iter::once(2 * MIB..3 * MIB),
        0,
        u64::MAX
    ));
    assert_eq!(
        found,
        Some(0..3 * MIB),
        "a range ending where the next map begins"
    );

    let mut found = None;
    assert!(!fold_ranges(
        &mut found,
        std::iter::once(0..MIB),
        0,
        u64::MAX
    ));
    assert!(fold_ranges(
        &mut found,
        std::iter::once(2 * MIB..3 * MIB),
        0,
        u64::MAX
    ));
    assert_eq!(found, Some(0..MIB), "a gap between maps");

    let mut found = None;
    assert!(!fold_ranges(
        &mut found,
        std::iter::once(0..MIB),
        0,
        u64::MAX
    ));
    let none: [std::ops::Range<u64>; 0] = [];
    assert!(!fold_ranges(&mut found, none, 0, u64::MAX));
    assert_eq!(found, Some(0..MIB), "an empty map");
}

/// The valid data length is read on NTFS alone: a file system's name, as Windows reports it ending
/// in a null, names NTFS only where it is exactly `NTFS`.
#[test]
fn only_ntfs_is_named_ntfs() {
    use lore_io::holes::names_ntfs;

    let name = |text: &str| -> Vec<u16> { text.encode_utf16().chain([0, 0, 0]).collect() };
    assert!(names_ntfs(&name("NTFS")), "NTFS");
    for other in ["ReFS", "FAT32", "exFAT", "NTFSX", "ntfs", "NTF", ""] {
        assert!(!names_ntfs(&name(other)), "{other}");
    }
}

/// A walk reads the file system's block on its first lookup and keeps it for the rest of the walk.
#[tokio::test]
async fn a_walk_keeps_the_file_systems_block_from_its_first_lookup() {
    let dir = TempDir::new("lore-io-holes-block-");
    let file = sparse_file(&dir, MIB, std::slice::from_ref(&(0..4096))).await;
    let cursor = DataCursor::default();
    assert_eq!(cursor.block(), None, "before the first lookup");

    let (found, cursor) = file.next_data(0, cursor).await;
    found.expect("find data");

    let block = cursor.block().expect("the block, after the first lookup");
    assert!(
        block >= 512 && block.is_power_of_two(),
        "a block of {block} bytes"
    );
}

/// A write of one byte into a block of a sparse file is reported as data of that whole block, the
/// unit the file system allocates, and the blocks around it as holes. The file's last byte is
/// written too, so all of it was written to on Windows, and only its allocation decides.
#[tokio::test]
async fn a_byte_written_is_reported_as_data_of_its_whole_block() {
    if !holes_reported().await {
        return;
    }
    let dir = TempDir::new("lore-io-holes-unit-");
    let probe = sparse_file(&dir, MIB, &[]).await;
    let block = block(&probe).await;
    drop(probe);
    let file = sparse_file(
        &dir,
        8 * block,
        &[3 * block + 7..3 * block + 8, 8 * block - 1..8 * block],
    )
    .await;
    assert_eq!(
        data(&file).await,
        [3 * block..4 * block, 7 * block..8 * block]
    );
}

/// NTFS allocates a sparse file in units of 16 clusters, which a walk reports as its block.
#[cfg(target_family = "windows")]
#[tokio::test]
async fn a_walk_on_ntfs_takes_16_clusters_for_its_block() {
    let dir = TempDir::new("lore-io-holes-ntfs-");
    let file = sparse_file(&dir, MIB, std::slice::from_ref(&(0..1))).await;
    let (Ok(volume), Ok(space)) = (
        lore_io::clone::volume_of(dir.path()),
        lore_io::clone::space_of(dir.path()),
    ) else {
        eprintln!("the tests do not run on a volume this test can measure");
        return;
    };
    if volume.filesystem != "NTFS" {
        eprintln!("the tests do not run on an NTFS volume");
        return;
    }
    assert_eq!(block(&file).await, 16 * space.block);
}

/// A file that is not sparse is one range of data up to its valid data length, its ranges never
/// written below that included, and a hole past it.
#[cfg(target_family = "windows")]
#[tokio::test]
async fn a_file_that_is_not_sparse_is_data_to_its_valid_data_length() {
    let dir = TempDir::new("lore-io-holes-dense-");
    if !on_ntfs(&dir) {
        return;
    }
    let path = dir.path().join("dense");
    let file = std::fs::File::create(&path).expect("create");
    file.set_len(4 * MIB).expect("size");
    drop(file);
    let file = IoDriver::global()
        .open(&path, &OpenOptions::new().read(true).write(true))
        .await
        .expect("open");
    file.write_all_at(vec![0xABu8; 4096], MIB)
        .await
        .expect("write");
    assert_eq!(data(&file).await, std::slice::from_ref(&(0..MIB + 4096)));
}

/// A walk that cannot flush a sparse file, which another handle holds open sharing no write
/// access, takes the whole file as one range of data, its holes included: data written through a
/// mapping may be in neither the valid data length nor the allocation. Once that handle is closed,
/// the walk reports the written range alone.
#[cfg(target_family = "windows")]
#[tokio::test]
async fn a_walk_that_cannot_flush_a_sparse_file_takes_it_as_data_whole() {
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

    let dir = TempDir::new("lore-io-holes-unflushed-");
    drop(sparse_file(&dir, 4 * MIB, std::slice::from_ref(&(MIB..MIB + 4096))).await);
    let path = dir.path().join("sparse");
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ)
        .open(&path)
        .expect("hold the file, sharing no write access");
    let file = IoDriver::global()
        .open(&path, &OpenOptions::new().read(true))
        .await
        .expect("open");
    assert_eq!(data(&file).await, std::slice::from_ref(&(0..4 * MIB)));
    drop(holder);
    assert_eq!(data(&file).await, std::slice::from_ref(&(MIB..MIB + 4096)));
}

/// A walk of a sparse file that carries the read-only attribute, which it cannot flush, takes the
/// whole file as one range of data, its holes included: a handle opened to write it before the
/// attribute was set still writes, so its allocation may not hold all it was written.
#[cfg(target_family = "windows")]
#[tokio::test]
async fn a_walk_of_a_read_only_sparse_file_takes_it_as_data_whole() {
    let dir = TempDir::new("lore-io-holes-read-only-");
    drop(sparse_file(&dir, 4 * MIB, std::slice::from_ref(&(MIB..2 * MIB))).await);
    let path = dir.path().join("sparse");
    let mut permissions = std::fs::metadata(&path).expect("stat").permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&path, permissions.clone()).expect("make the file read-only");
    let file = IoDriver::global()
        .open(&path, &OpenOptions::new().read(true))
        .await
        .expect("open");
    let walked = data(&file).await;
    drop(file);
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(&path, permissions).expect("make the file writable to remove it");
    assert_eq!(walked, std::slice::from_ref(&(0..4 * MIB)));
}

/// A walk of a sparse file that carries the offline attribute, whose data its file system may keep
/// elsewhere until it is read, takes the whole file as one range of data, its holes included; the
/// same file without the attribute keeps its hole.
#[cfg(target_family = "windows")]
#[tokio::test]
async fn a_walk_of_an_offline_sparse_file_takes_it_as_data_whole() {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_OFFLINE;
    use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;

    let dir = TempDir::new("lore-io-holes-offline-");
    drop(sparse_file(&dir, 4 * MIB, std::slice::from_ref(&(MIB..2 * MIB))).await);
    let path = dir.path().join("sparse");
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let options = OpenOptions::new().read(true);
    let open = || IoDriver::global().open(&path, &options);
    let online = data(&open().await.expect("open")).await;
    // SAFETY: `wide` is a path ending in a null.
    let set = unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_OFFLINE) };
    assert_ne!(set, 0, "mark the file offline");
    let offline = data(&open().await.expect("open")).await;
    // SAFETY: as above.
    unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_NORMAL) };
    assert_eq!(online, std::slice::from_ref(&(MIB..2 * MIB)), "online");
    assert_eq!(offline, std::slice::from_ref(&(0..4 * MIB)), "offline");
}

/// A file preallocated and then marked sparse is allocated over the whole preallocation, and is
/// data up to its valid data length and a hole past it.
#[cfg(target_family = "windows")]
#[tokio::test]
async fn a_file_preallocated_and_then_marked_sparse_is_data_to_its_valid_data_length() {
    use std::io::Write;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Storage::FileSystem::FILE_ALLOCATION_INFO;
    use windows_sys::Win32::Storage::FileSystem::FileAllocationInfo;
    use windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle;

    let dir = TempDir::new("lore-io-holes-preallocated-");
    if !on_ntfs(&dir) {
        return;
    }
    let path = dir.path().join("preallocated");
    let mut file = std::fs::File::create(&path).expect("create");
    let allocation = FILE_ALLOCATION_INFO {
        AllocationSize: 4 * MIB as i64,
    };
    // SAFETY: the handle is open, and `allocation` outlives the call.
    let allocated = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileAllocationInfo,
            (&raw const allocation).cast(),
            size_of::<FILE_ALLOCATION_INFO>() as u32,
        )
    };
    assert_ne!(allocated, 0, "preallocate");
    file.write_all(&vec![0xABu8; 256 * 1024]).expect("write");
    file.set_len(4 * MIB).expect("size");
    drop(file);
    let file = IoDriver::global()
        .open(&path, &OpenOptions::new().read(true).write(true))
        .await
        .expect("open");
    file.set_len_sparse(4 * MIB).await.expect("mark sparse");
    assert_eq!(data(&file).await, std::slice::from_ref(&(0..256 * 1024)));
}

/// A file sized by `set_len_sparse` reports no data and holds no storage on NTFS, and at most one
/// cluster elsewhere, as version 3.14 of the Resilient File System holds one for a file created,
/// marked sparse and sized through one handle. One sized by `set_len` is not sparse, so its size on
/// disk is its length.
#[cfg(target_family = "windows")]
#[tokio::test]
async fn a_file_sized_sparse_holds_at_most_one_cluster() {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;

    let dir = TempDir::new("lore-io-holes-sized-");
    let held = |name: &str| {
        let wide: Vec<u16> = dir
            .path()
            .join(name)
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect();
        let mut high = 0u32;
        // SAFETY: `wide` is a path ending in a null, and `high` outlives the call.
        let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &mut high) };
        (u64::from(high) << 32) | u64::from(low)
    };
    let sparse = sparse_file(&dir, 8 * MIB, &[]).await;
    let walked = data(&sparse).await;
    let dense = IoDriver::global()
        .open(
            dir.path().join("dense"),
            &OpenOptions::new().read(true).write(true).create(true),
        )
        .await
        .expect("open");
    dense.set_len(8 * MIB, ()).await.expect("size");
    drop((sparse, dense));
    let ntfs = lore_io::clone::volume_of(dir.path())
        .expect("the volume")
        .filesystem
        == "NTFS";
    let most = if ntfs {
        0
    } else {
        lore_io::clone::space_of(dir.path())
            .expect("the volume's cluster")
            .block
    };
    assert!(walked.is_empty(), "a walk reports data at {walked:?}");
    let sparse_held = held("sparse");
    assert!(
        sparse_held <= most,
        "the sparse file holds {sparse_held} bytes, past {most}"
    );
    assert_eq!(held("dense"), 8 * MIB);
}

/// A file emptied through the handle a resumed read keeps holds no data, and reads as zeros to its
/// new length, through that handle and through one opened after it: a file written through other
/// handles, then opened, locked, sized, mapped and read back, as a resumed read does before it
/// empties its file.
#[tokio::test]
async fn an_emptied_file_holds_no_data_through_a_handle_kept_open() {
    if !holes_reported().await {
        return;
    }
    let dir = TempDir::new("lore-io-holes-emptied-");
    drop(sparse_file(&dir, 4 * MIB, std::slice::from_ref(&(0..4096))).await);
    let other = IoDriver::global()
        .open(dir.path().join("sparse"), &OpenOptions::new().write(true))
        .await
        .expect("open another handle");
    other
        .write_all_at(vec![0xABu8; 4096], MIB + 4096)
        .await
        .expect("write through the other handle");
    drop(other);
    let file = IoDriver::global()
        .open(
            dir.path().join("sparse"),
            &OpenOptions::new().read(true).write(true),
        )
        .await
        .expect("open the file");
    assert_eq!(
        file.try_lock().expect("lock"),
        lore_io::LockAttempt::Acquired
    );
    file.set_len_sparse(4 * MIB).await.expect("size the file");
    // Each write lies inside a range of data. APFS writes out the small hole between them as
    // zeros, so they may be one range there.
    let ranges = data(&file).await;
    for written in [0..4096, MIB + 4096..MIB + 8192] {
        assert!(
            ranges
                .iter()
                .any(|data| data.start <= written.start && written.end <= data.end),
            "the write at {written:?} is data: {ranges:?}"
        );
    }
    file.read_exact_at(4096, MIB + 4096)
        .await
        .expect("read back the write");

    file.empty(4 * MIB).await.expect("empty the file");
    assert_eq!(data(&file).await, [], "through the handle kept open");
    assert_eq!(file.metadata().await.expect("stat").len(), 4 * MIB);
    let read = file.read_exact_at(4 * MIB as usize, 0).await.expect("read");
    assert!(
        read.iter().all(|byte| *byte == 0),
        "the file reads as zeros"
    );
    drop(file);
    let reopened = IoDriver::global()
        .open(dir.path().join("sparse"), &OpenOptions::new().read(true))
        .await
        .expect("open again");
    assert_eq!(
        data(&reopened).await,
        [],
        "through a handle opened after it"
    );
}

/// One walk reports every range of a file holding more extents than one map of the file takes,
/// across the batches it maps them in, as walks of their own do one range at a time. Each extent is
/// two blocks of the file system, on a grid of 16, off it by up to two.
#[tokio::test]
async fn one_walk_reports_more_extents_than_one_map_takes() {
    if !holes_reported().await {
        return;
    }
    let dir = TempDir::new("lore-io-many-extents-");
    let probe = sparse_file(&dir, MIB, &[]).await;
    let block = block(&probe).await;
    drop(probe);
    let step = 16 * block;
    let extents: Vec<Range<u64>> = (0..200)
        .map(|index| {
            let start = index * step + block * (index % 3);
            start..start + 2 * block
        })
        .collect();
    let file = sparse_file(&dir, 200 * step, &extents).await;
    assert_eq!(data(&file).await, extents, "one walk");

    let mut separate = Vec::new();
    let mut offset = 0;
    while let Some(range) = next_data(&file, offset).await {
        offset = range.end;
        separate.push(range);
    }
    assert_eq!(separate, extents, "a walk for each range");
    assert_eq!(
        next_data(&file, extents[130].start + 4096).await,
        Some(extents[130].start + 4096..extents[130].end),
        "inside a range of a later batch"
    );
}

/// Reserves `range` of the file at `path` as an extent allocated and never written, an unwritten
/// extent, or `false` where its file system reserves none or has no extent map to report one by,
/// as tmpfs.
#[cfg(target_os = "linux")]
fn reserve(path: &std::path::Path, range: Range<u64>) -> bool {
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
            (range.end - range.start) as libc::off_t,
        )
    };
    if reserved != 0 {
        return false;
    }
    file.sync_all().expect("sync the reservation");
    true
}

/// An extent allocated and never written is a hole, also once the page cache holds its pages,
/// where `SEEK_DATA` takes it for data.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn next_data_takes_an_unwritten_extent_as_a_hole_whatever_the_cache_holds() {
    if !holes_reported().await {
        return;
    }
    let dir = TempDir::new("lore-io-unwritten-");
    let extents = [0..MIB, 6 * MIB..7 * MIB];
    let file = sparse_file(&dir, 8 * MIB, &extents).await;
    let path = dir.path().join("sparse");
    if !reserve(&path, 2 * MIB..4 * MIB) {
        eprintln!("the file system the test runs on reports no unwritten extent");
        return;
    }
    // Every page of the file, the reserved ones included, now in the page cache.
    std::fs::read(&path).expect("read the whole file");
    assert_eq!(data(&file).await, extents);
}

/// Whether the file at `path` lives on tmpfs, which has no extent map to give.
#[cfg(target_os = "linux")]
fn on_tmpfs(path: &std::path::Path) -> bool {
    use std::os::fd::AsRawFd;

    let file = std::fs::File::open(path).expect("open to statfs");
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `fstatfs` fills `stats`, which outlives the call.
    assert_eq!(
        unsafe { libc::fstatfs(file.as_raw_fd(), stats.as_mut_ptr()) },
        0,
        "statfs"
    );
    // SAFETY: `fstatfs` succeeded, so it filled `stats`.
    unsafe { stats.assume_init() }.f_type as u64 == libc::TMPFS_MAGIC as u64
}

/// On a file system with no extent map, a walk is refused `FIEMAP` once, takes `SEEK_DATA` and
/// `SEEK_HOLE` from then on, and reports the written ranges with one cursor.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_walk_without_an_extent_map_reports_the_written_ranges() {
    let shm = std::path::Path::new("/dev/shm");
    if !shm.is_dir() {
        eprintln!("no /dev/shm to make a file without an extent map on");
        return;
    }
    let dir = TempDir::new_in(shm, "lore-io-holes-tmpfs-");
    let extents = [MIB..2 * MIB, 5 * MIB..5 * MIB + 4096, 7 * MIB..8 * MIB];
    let file = sparse_file(&dir, 8 * MIB, &extents).await;
    if !on_tmpfs(&dir.path().join("sparse")) {
        eprintln!("/dev/shm is not tmpfs, so the walk would not take SEEK_DATA");
        return;
    }
    assert_eq!(data(&file).await, extents);
}

/// On a file system that holds holes, punching holes deallocates the whole blocks of each hole and
/// nothing else: the data around them, written zeros included, and the parts of a hole in the
/// blocks it shares with data stay allocated, and the file reads as before.
#[cfg(target_vendor = "apple")]
#[tokio::test]
async fn punching_holes_deallocates_their_whole_blocks_alone() {
    let dir = TempDir::new("lore-io-punch-");
    let path = dir.path().join("file");
    let hole = 2 * MIB + 100..3 * MIB - 100;
    let mut content = vec![0xABu8; 4 * MIB as usize];
    content[MIB as usize..2 * MIB as usize].fill(0);
    content[hole.start as usize..hole.end as usize].fill(0);
    std::fs::write(&path, &content).expect("write the file");
    let file = IoDriver::global()
        .open(&path, &OpenOptions::new().read(true).write(true))
        .await
        .expect("open");
    let (found, cursor) = file.next_data(0, DataCursor::default()).await;
    found.expect("find data");
    let block = cursor.block().expect("the block");
    assert!(file.holds_holes().await, "APFS holds holes");

    file.punch_holes(vec![hole.clone()])
        .await
        .expect("punch the hole");

    let start = hole.start.div_ceil(block) * block;
    let end = hole.end / block * block;
    assert_eq!(data(&file).await, [0..start, end..4 * MIB]);
    assert_eq!(std::fs::read(&path).expect("read the file"), content);
}

/// A punched hole that ends the file takes the file's last block whole, also where the file ends
/// inside it, and a hole inside one block deallocates nothing.
#[cfg(target_vendor = "apple")]
#[tokio::test]
async fn punching_a_hole_that_ends_the_file_takes_its_last_block_and_one_inside_a_block_none() {
    let dir = TempDir::new("lore-io-punch-end-");
    let path = dir.path().join("file");
    let len = 4 * MIB + 1000;
    let inside = MIB + 100..MIB + 200;
    let ending = 3 * MIB + 100..len;
    let mut content = vec![0xABu8; len as usize];
    content[inside.start as usize..inside.end as usize].fill(0);
    content[ending.start as usize..].fill(0);
    std::fs::write(&path, &content).expect("write the file");
    let file = IoDriver::global()
        .open(&path, &OpenOptions::new().read(true).write(true))
        .await
        .expect("open");
    let (found, cursor) = file.next_data(0, DataCursor::default()).await;
    found.expect("find data");
    let block = cursor.block().expect("the block");

    file.punch_holes(vec![inside, ending.clone()])
        .await
        .expect("punch the holes");

    let start = ending.start.div_ceil(block) * block;
    assert_eq!(data(&file).await, std::slice::from_ref(&(0..start)));
    assert_eq!(std::fs::read(&path).expect("read the file"), content);
}
