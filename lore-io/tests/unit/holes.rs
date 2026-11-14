// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ops::Range;
use std::os::unix::fs::FileExt;

use lore_base::test_util::TempDir;
use lore_io::DataCursor;
use lore_io::IoDriver;
use lore_io::IoFile;
use lore_io::OpenOptions;

const MIB: u64 = 1 << 20;

/// A file of `size` bytes with `0xAB` at `extents` and a hole everywhere else, open to read and
/// write.
async fn sparse_file(dir: &TempDir, size: u64, extents: &[Range<u64>]) -> IoFile {
    let path = dir.path().join("sparse");
    let file = std::fs::File::create(&path).expect("create");
    file.set_len(size).expect("size");
    for extent in extents {
        let data = vec![0xABu8; (extent.end - extent.start) as usize];
        file.write_all_at(&data, extent.start).expect("write");
    }
    file.sync_all().expect("sync");
    IoDriver::global()
        .open(&path, &OpenOptions::new().read(true).write(true))
        .await
        .expect("open")
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
/// saying why.
async fn holes_reported() -> bool {
    static REPORTED: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();
    let reported = *REPORTED
        .get_or_init(|| async {
            let dir = TempDir::new("lore-io-holes-probe-");
            let file = sparse_file(&dir, 4 * MIB, &[0..4096, 2 * MIB..2 * MIB + 4096]).await;
            data(&file).await.len() == 2
        })
        .await;
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

/// One walk reports every range of a file holding more extents than one map of the file takes,
/// across the batches it maps them in, as walks of their own do one range at a time.
#[tokio::test]
async fn one_walk_reports_more_extents_than_one_map_takes() {
    if !holes_reported().await {
        return;
    }
    let dir = TempDir::new("lore-io-many-extents-");
    let step = 64 * 1024;
    let extents: Vec<Range<u64>> = (0..200)
        .map(|index| {
            let start = index * step + 4096 * (index % 3);
            start..start + 8192
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
