// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::io::Write;

use lore_base::test_util::TempDir;
use lore_storage::chunker::*;
use lore_storage::compress::FRAGMENT_SIZE_THRESHOLD;
use lore_storage::concurrency::FRAGMENT_SIZE_EXPECTED;
use lore_storage::concurrency::FRAGMENT_SIZE_MINIMUM;

/// Boundaries the current whole-buffer implementation produces, which the
/// streaming chunker must reproduce byte for byte.
fn reference_boundaries(buffer: &[u8]) -> Vec<(u64, usize)> {
    fastcdc::v2020::FastCDC::with_level(
        buffer,
        FRAGMENT_SIZE_MINIMUM as u32,
        FRAGMENT_SIZE_EXPECTED as u32,
        FRAGMENT_SIZE_THRESHOLD as u32,
        fastcdc::v2020::Normalization::Level1,
    )
    .map(|c| (c.offset as u64, c.length))
    .collect()
}

/// Boundaries the current fixed-size implementation produces.
fn reference_fixed_boundaries(size: usize, step: usize) -> Vec<(u64, usize)> {
    (0..size)
        .step_by(step)
        .map(|offset| (offset as u64, (offset + step).min(size) - offset))
        .collect()
}

/// Where a chunk of data starts, and its bytes. A chunker reading every range hands out no hole.
fn chunk_data(chunk: &Chunk) -> (u64, &[u8]) {
    match chunk {
        Chunk::Data { offset, data } => (*offset, data.as_ref()),
        Chunk::Hole { offset, .. } => {
            panic!("a hole at {offset} from a chunker reading every range")
        }
    }
}

async fn streamed_chunks(
    dir: &TempDir,
    name: &str,
    content: &[u8],
    fixed_size: usize,
) -> Vec<Chunk> {
    let path = dir.path().join(name);
    let mut file = std::fs::File::create(&path).expect("create test file");
    file.write_all(content).expect("write test file");
    file.sync_all().expect("sync test file");
    drop(file);

    let (file, file_size) = lore_storage::content::ContentSource::file(&path)
        .open()
        .await
        .expect("open test file");
    let mut chunker = if fixed_size > 0 {
        FileChunker::fixed_size(file, file_size, fixed_size).await
    } else {
        FileChunker::content_defined(file, file_size).await
    };
    let mut chunks = Vec::new();
    while let Some(chunk) = chunker.next_chunk().await.expect("chunking succeeds") {
        chunks.push(chunk);
    }
    chunks
}

/// The property the whole change rests on: streaming must not move a boundary.
async fn assert_matches_reference(name: &str, content: &[u8]) {
    let dir = TempDir::new("lore-storage-chunker-test-");
    let chunks = streamed_chunks(&dir, name, content, 0).await;

    let actual: Vec<(u64, usize)> = chunks
        .iter()
        .map(|c| (chunk_data(c).0, chunk_data(c).1.len()))
        .collect();
    assert_eq!(
        actual,
        reference_boundaries(content),
        "streaming boundaries diverged from whole-buffer FastCDC for {name} ({} bytes)",
        content.len()
    );

    assert_covers(&chunks, content);
}

async fn assert_matches_fixed_reference(name: &str, content: &[u8], step: usize) {
    let dir = TempDir::new("lore-storage-chunker-test-");
    let chunks = streamed_chunks(&dir, name, content, step).await;

    let actual: Vec<(u64, usize)> = chunks
        .iter()
        .map(|c| (chunk_data(c).0, chunk_data(c).1.len()))
        .collect();
    assert_eq!(
        actual,
        reference_fixed_boundaries(content.len(), step),
        "streaming fixed-size boundaries diverged for {name} ({} bytes, step {step})",
        content.len()
    );

    assert_covers(&chunks, content);
}

fn assert_covers(chunks: &[Chunk], content: &[u8]) {
    for chunk in chunks {
        let (offset, data) = chunk_data(chunk);
        let start = offset as usize;
        assert_eq!(
            data,
            &content[start..start + data.len()],
            "chunk data at offset {start} does not match the source"
        );
    }

    let covered: usize = chunks.iter().map(|c| chunk_data(c).1.len()).sum();
    assert_eq!(covered, content.len(), "chunks do not cover the whole file");
}

fn random_buffer(size: usize) -> Vec<u8> {
    use rand::Rng;
    let mut data = vec![0u8; size];
    rand::rng().fill(&mut data[..]);
    data
}

#[tokio::test]
async fn matches_reference_on_random_data() {
    for size in [
        1024,
        FRAGMENT_SIZE_MINIMUM - 1,
        FRAGMENT_SIZE_EXPECTED + 13,
        5 * WINDOW_SIZE + 4097,
    ] {
        assert_matches_reference(&format!("random-{size}"), &random_buffer(size)).await;
    }
}

/// All zeroes never trips the rolling hash, so every cut is a forced maximum-size
/// cut — the case where an undersized window would cut early instead.
#[tokio::test]
async fn matches_reference_on_all_zero_data() {
    assert_matches_reference("zeros", &vec![0u8; 5 * WINDOW_SIZE + 977]).await;
}

/// A period far below the minimum fragment size: the hash repeats constantly, so
/// candidate cuts cluster and the minimum-size skip does the work.
#[tokio::test]
async fn matches_reference_on_highly_repetitive_data() {
    let pattern: Vec<u8> = (0u8..=63).collect();
    let content: Vec<u8> = pattern
        .iter()
        .copied()
        .cycle()
        .take(4 * WINDOW_SIZE + 31)
        .collect();
    assert_matches_reference("repetitive", &content).await;
}

/// Sizes landing exactly on the window and fragment limits, where an off-by-one in
/// the refill or lookahead check would show up.
#[tokio::test]
async fn matches_reference_on_boundary_sizes() {
    for size in [
        FRAGMENT_SIZE_THRESHOLD - 1,
        FRAGMENT_SIZE_THRESHOLD,
        FRAGMENT_SIZE_THRESHOLD + 1,
        WINDOW_SIZE - 1,
        WINDOW_SIZE,
        WINDOW_SIZE + 1,
        2 * WINDOW_SIZE,
    ] {
        assert_matches_reference(&format!("boundary-{size}"), &random_buffer(size)).await;
    }
}

/// Fixed-size chunking is what `WriteOptions::with_fixed_size_chunk` selects, and
/// it reaches the streaming path through `write_from_file`.
#[tokio::test]
async fn matches_reference_on_fixed_size_chunking() {
    for (size, step) in [
        (10 * 1024 + 17, 1024),
        (100, 1024),
        (WINDOW_SIZE + 1, FRAGMENT_SIZE_THRESHOLD),
        (3 * FRAGMENT_SIZE_THRESHOLD, FRAGMENT_SIZE_THRESHOLD),
        (5 * WINDOW_SIZE + 331, FRAGMENT_SIZE_EXPECTED),
        // A step that does not divide the read size, so every window carries a
        // remainder over into the next one.
        (5 * WINDOW_SIZE + 331, 100_003),
    ] {
        assert_matches_fixed_reference(&format!("fixed-{size}-{step}"), &random_buffer(size), step)
            .await;
    }
}

async fn open_chunker(dir: &TempDir, name: &str, size: usize) -> FileChunker {
    let path = dir.path().join(name);
    std::fs::write(&path, random_buffer(size)).expect("write test file");
    let (file, _) = lore_storage::content::ContentSource::file(&path)
        .open()
        .await
        .expect("open test file");

    FileChunker::content_defined(file, size as u64).await
}

fn reserved_permits(chunker: &FileChunker) -> usize {
    chunker
        ._reservation
        .as_ref()
        .expect("window budget reserved")
        .num_permits()
}

/// The reservation has to cover every byte the chunker can hold, which since a cut
/// queues boundaries rather than buffers is exactly its windows plus the chunk it
/// pre-pays for the caller.
#[tokio::test]
async fn the_reservation_covers_every_window() {
    let dir = TempDir::new("lore-storage-chunker-test-");

    let read_ahead = open_chunker(&dir, "read-ahead-budget", 4 * WINDOW_SIZE).await;
    assert_eq!(
        reserved_permits(&read_ahead),
        (2 * WINDOW_SIZE + FRAGMENT_SIZE_THRESHOLD) / 1024,
        "two windows and one chunk"
    );

    let single_read = open_chunker(&dir, "single-read-budget", WINDOW_SIZE).await;
    assert_eq!(
        reserved_permits(&single_read),
        (WINDOW_SIZE + FRAGMENT_SIZE_THRESHOLD) / 1024,
        "one window and one chunk"
    );
    assert_eq!(single_read.headroom, 0, "nothing to carry over");
    assert!(single_read.spare.is_none(), "no second window to read into");
}

/// The next read must already be running while chunks are handed out, or disk
/// latency is back on the critical path.
#[tokio::test]
async fn a_read_runs_while_chunks_are_handed_out() {
    let dir = TempDir::new("lore-storage-chunker-test-");
    let mut chunker = open_chunker(&dir, "read-ahead", 4 * WINDOW_SIZE).await;

    for index in 0..4 {
        let chunk = chunker.next_chunk().await.expect("chunking succeeds");
        assert!(chunk.is_some(), "chunk {index} missing");
        assert!(
            chunker.pending.is_some(),
            "no read running behind chunk {index}"
        );
    }
}

/// A file appended to after it was sized must still yield exactly the recorded size:
/// the caller stores that size in the root fragment, and a longer chunk list than the
/// recorded content underflows the offset arithmetic in `hash_file`.
#[tokio::test]
async fn never_reads_past_the_opened_size() {
    let dir = TempDir::new("lore-storage-chunker-test-");
    let path = dir.path().join("growing");
    let declared = 3 * WINDOW_SIZE + 517;
    std::fs::write(&path, random_buffer(declared)).expect("write test file");

    let (file, _) = lore_storage::content::ContentSource::file(&path)
        .open()
        .await
        .expect("open test file");
    let mut chunker = FileChunker::content_defined(file, declared as u64).await;

    // Grow the file behind the chunker, as an appender would.
    let mut appended = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("reopen for append");
    appended
        .write_all(&random_buffer(2 * WINDOW_SIZE))
        .expect("append");
    appended.sync_all().expect("sync append");

    let mut covered = 0usize;
    while let Some(chunk) = chunker.next_chunk().await.expect("chunking succeeds") {
        covered += chunk_data(&chunk).1.len();
    }
    assert_eq!(
        covered, declared,
        "chunker emitted bytes beyond the size it was opened at"
    );
}

/// A file holding less than the size it was measured at is rejected at the read that
/// crosses the end, part-way through a scan that was succeeding: the chunk list would
/// otherwise cover fewer bytes than the root fragment records, and readers trust that
/// count.
///
/// The short file stands in for one truncated behind the chunker, which cannot be staged
/// portably — truncation needs a second, writing handle, and the reader may hold a share
/// mode that denies one.
#[tokio::test]
async fn a_file_shorter_than_its_measured_size_fails() {
    let dir = TempDir::new("lore-storage-chunker-shrink-");
    let path = dir.path().join("short");
    let declared = 4 * WINDOW_SIZE;
    std::fs::write(&path, random_buffer(2 * WINDOW_SIZE)).expect("write test file");

    let (file, _) = lore_storage::content::ContentSource::file(&path)
        .open()
        .await
        .expect("open test file");
    let mut chunker = FileChunker::content_defined(file, declared as u64).await;
    chunker
        .next_chunk()
        .await
        .expect("first chunk")
        .expect("a chunk");

    let err = loop {
        match chunker.next_chunk().await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("a shrunk file must not report a complete scan"),
            Err(err) => break err,
        }
    };
    assert!(
        format!("{err}").contains("read file for chunking"),
        "expected the read itself to fail: {err}"
    );

    let Err(_) = chunker.next_chunk().await else {
        panic!("a failed chunker stays failed");
    };
}

#[tokio::test]
async fn empty_file_yields_no_chunks() {
    let dir = TempDir::new("lore-storage-chunker-test-");
    assert!(streamed_chunks(&dir, "empty", &[], 0).await.is_empty());
    assert!(
        streamed_chunks(&dir, "empty-fixed", &[], FRAGMENT_SIZE_EXPECTED)
            .await
            .is_empty()
    );
}
/// A read failure leaves no pending read and no window: the buffer went with the failed
/// task. A second call must not treat that as the single-read regime and steal the live
/// window.
#[tokio::test]
async fn a_failed_read_does_not_leave_the_chunker_usable() {
    let dir = TempDir::new("lore-storage-chunker-poison-");
    let path = dir.path().join("write-only");
    std::fs::write(&path, vec![7u8; 4 * FRAGMENT_SIZE_THRESHOLD]).expect("seed file");
    // Write-only handle: every positional read fails.
    let file = lore_io::IoDriver::global()
        .open(&path, &lore_io::OpenOptions::new().write(true))
        .await
        .expect("open write-only");

    let mut chunker = FileChunker::content_defined(
        lore_storage::content::ContentHandle::File(file),
        4 * FRAGMENT_SIZE_THRESHOLD as u64,
    )
    .await;
    let Err(first) = chunker.next_chunk().await else {
        panic!("read must fail");
    };
    assert!(
        format!("{first}").contains("read file for chunking"),
        "expected the read itself to fail: {first}"
    );

    // Distinguishes the fuse from the read simply failing again.
    let Err(second) = chunker.next_chunk().await else {
        panic!("must not resume after a failure");
    };
    assert!(
        format!("{second}").contains("used again after a read failure"),
        "expected the fuse, got: {second}"
    );
}

/// Chunking that skips holes, which needs a file system reporting them.
mod skipping_holes {
    use super::*;
    use crate::holes::holes_reported;

    /// A file `name` in `dir` of `size` bytes holding random data at `extents`, `(offset, length)`
    /// each, and a hole everywhere else, with the content it reads as.
    async fn sparse_file(
        dir: &TempDir,
        name: &str,
        size: usize,
        extents: &[(usize, usize)],
    ) -> Vec<u8> {
        crate::holes::sparse_file(&dir.path().join(name), size, extents).await
    }

    /// The data ranges of `name` up to `size`, as `ContentHandle::next_data` reports them.
    async fn reported_data(dir: &TempDir, name: &str, size: u64) -> Vec<std::ops::Range<u64>> {
        let (handle, _) = lore_storage::content::ContentSource::file(&dir.path().join(name))
            .open()
            .await
            .expect("open test file");
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

    /// Every chunk of `name`, skipping holes, cut every `fixed_size` bytes, or by content when
    /// `fixed_size` is zero.
    async fn chunks_skipping_holes(dir: &TempDir, name: &str, fixed_size: usize) -> Vec<Chunk> {
        let (file, file_size) = lore_storage::content::ContentSource::file(&dir.path().join(name))
            .open()
            .await
            .expect("open test file");
        let chunker = if fixed_size > 0 {
            FileChunker::fixed_size(file, file_size, fixed_size).await
        } else {
            FileChunker::content_defined(file, file_size).await
        };
        let mut chunker = chunker.skipping_holes();
        let mut chunks = Vec::new();
        while let Some(chunk) = chunker.next_chunk().await.expect("chunking succeeds") {
            chunks.push(chunk);
        }
        chunks
    }

    /// The chunks tile the file in order, holes exactly where the file system reports no data, and
    /// each chunk of data is the content at its offset.
    fn assert_tiles_with_holes(chunks: &[Chunk], content: &[u8], data: &[std::ops::Range<u64>]) {
        let mut at = 0u64;
        let mut holes = Vec::new();
        for chunk in chunks {
            match chunk {
                Chunk::Hole { offset, len } => {
                    assert_eq!(*offset, at, "chunks leave a gap or overlap at {at}");
                    holes.push(*offset..offset + len);
                    at += len;
                }
                Chunk::Data {
                    offset,
                    data: bytes,
                } => {
                    assert_eq!(*offset, at, "chunks leave a gap or overlap at {at}");
                    let start = *offset as usize;
                    assert_eq!(
                        bytes.as_ref(),
                        &content[start..start + bytes.len()],
                        "chunk data at offset {start} does not match the source"
                    );
                    assert!(
                        data.iter().any(|range| range.start <= *offset
                            && offset + bytes.len() as u64 <= range.end),
                        "chunk at {start} spans more than one range of data"
                    );
                    at += bytes.len() as u64;
                }
            }
        }
        assert_eq!(
            at,
            content.len() as u64,
            "chunks do not cover the whole file"
        );

        let mut expected_holes = Vec::new();
        let mut end = 0;
        for range in data {
            if range.start > end {
                expected_holes.push(end..range.start);
            }
            end = range.end;
        }
        if end < content.len() as u64 {
            expected_holes.push(end..content.len() as u64);
        }
        assert_eq!(
            holes, expected_holes,
            "holes differ from what the file system reports"
        );
    }

    /// Data at offsets off the 256 KiB grid, holes between, before and after: no hole is read, and
    /// every fixed-size cut lands on the grid from the start of the file or where data starts or
    /// ends, so a chunk wholly inside data is the chunk the file cut without holes has there.
    #[tokio::test]
    async fn skipping_holes_hands_out_holes_and_keeps_the_fixed_grid() {
        if !holes_reported().await {
            return;
        }
        const MIB: usize = 1 << 20;
        let dir = TempDir::new("lore-storage-chunker-holes-");
        let size = 16 * MIB;
        let extents = [
            (MIB + 12 * 4096, 3 * MIB + 5 * 4096),
            (6 * MIB, MIB),
            (9 * MIB + 4096, 4096),
            (12 * MIB - 8 * 4096, 2 * MIB),
        ];
        let content = sparse_file(&dir, "sparse", size, &extents).await;
        let data = reported_data(&dir, "sparse", size as u64).await;
        let data_bytes: u64 = data.iter().map(|range| range.end - range.start).sum();

        let step = FRAGMENT_SIZE_THRESHOLD;
        let chunks = chunks_skipping_holes(&dir, "sparse", step).await;
        assert_tiles_with_holes(&chunks, &content, &data);

        let data_chunks = || {
            chunks.iter().filter_map(|chunk| match chunk {
                Chunk::Data { offset, data } => Some((*offset, data.len() as u64)),
                Chunk::Hole { .. } => None,
            })
        };
        for (offset, len) in data_chunks() {
            let end = offset + len;
            let on_grid = |at: u64| {
                at.is_multiple_of(step as u64) || data.iter().any(|r| r.start == at || r.end == at)
            };
            assert!(
                on_grid(offset) && on_grid(end),
                "chunk {offset}..{end} is cut off the grid"
            );
        }

        let read: u64 = data_chunks().map(|(_, len)| len).sum();
        assert_eq!(
            read, data_bytes,
            "bytes handed out differ from the data the file holds"
        );
    }

    /// Content-defined chunking skipping holes cuts each range of data as a file of its own.
    #[tokio::test]
    async fn skipping_holes_cuts_each_range_on_its_own_content() {
        if !holes_reported().await {
            return;
        }
        const MIB: usize = 1 << 20;
        let dir = TempDir::new("lore-storage-chunker-holes-cdc-");
        let size = 8 * MIB;
        let extents = [(MIB, MIB + 4096), (4 * MIB + 4096, 2 * MIB)];
        let content = sparse_file(&dir, "sparse", size, &extents).await;
        let data = reported_data(&dir, "sparse", size as u64).await;

        let chunks = chunks_skipping_holes(&dir, "sparse", 0).await;
        assert_tiles_with_holes(&chunks, &content, &data);

        for range in &data {
            let expected: Vec<(u64, usize)> =
                reference_boundaries(&content[range.start as usize..range.end as usize])
                    .into_iter()
                    .map(|(offset, length)| (offset + range.start, length))
                    .collect();
            let actual: Vec<(u64, usize)> = chunks
                .iter()
                .filter_map(|chunk| match chunk {
                    Chunk::Data { offset, data } if range.contains(offset) => {
                        Some((*offset, data.len()))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                actual, expected,
                "range {range:?} is not cut as a file of its own"
            );
        }
    }

    /// A file one window holds is read in one window, which every range of data is read into in
    /// turn, and a file with no hole is cut exactly as it is without skipping holes.
    #[tokio::test]
    async fn skipping_holes_reads_each_range_of_a_file_one_window_holds() {
        if !holes_reported().await {
            return;
        }
        let dir = TempDir::new("lore-storage-chunker-holes-small-");
        let size = WINDOW_SIZE;
        let extents = [(4096, 64 * 1024), (256 * 1024, 8192), (400 * 1024, 4096)];
        let content = sparse_file(&dir, "small", size, &extents).await;
        let data = reported_data(&dir, "small", size as u64).await;
        for step in [0, FRAGMENT_SIZE_EXPECTED] {
            let chunks = chunks_skipping_holes(&dir, "small", step).await;
            assert_tiles_with_holes(&chunks, &content, &data);
        }

        let full = random_buffer(3 * FRAGMENT_SIZE_THRESHOLD + 17);
        for step in [0, FRAGMENT_SIZE_THRESHOLD] {
            let without = streamed_chunks(&dir, "full", &full, step).await;
            let with = chunks_skipping_holes(&dir, "full", step).await;
            let cuts = |chunks: &[Chunk]| -> Vec<(u64, usize, u64)> {
                chunks
                    .iter()
                    .map(|c| match c {
                        Chunk::Data { offset, data } => (*offset, data.len(), 0),
                        Chunk::Hole { offset, len } => (*offset, 0, *len),
                    })
                    .collect()
            };
            assert_eq!(
                cuts(&with),
                cuts(&without),
                "a file without holes is cut differently"
            );
        }
    }

    /// A file that is one hole is handed out as that one hole, a chunk that carries no data.
    #[tokio::test]
    async fn skipping_holes_hands_out_a_file_that_is_one_hole_as_that_hole() {
        if !holes_reported().await {
            return;
        }
        let dir = TempDir::new("lore-storage-chunker-holes-empty-");
        let size = 4 * FRAGMENT_SIZE_THRESHOLD;
        let content = sparse_file(&dir, "hole", size, &[]).await;
        let data = reported_data(&dir, "hole", size as u64).await;
        let chunks = chunks_skipping_holes(&dir, "hole", FRAGMENT_SIZE_THRESHOLD).await;
        assert_tiles_with_holes(&chunks, &content, &data);
        assert!(data.is_empty(), "the file holds no data");
        assert_eq!(chunks.len(), 1, "one hole is one chunk");
    }

    /// Whether a walk of the file at `path` takes `SEEK_DATA`, which keeps nothing between lookups:
    /// on Linux where the file lives on tmpfs, which has no extent map, and everywhere but Windows,
    /// where a walk keeps its batch of allocated ranges.
    fn walked_with_seek(path: &std::path::Path) -> bool {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
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
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let _ = path;
            !cfg!(target_family = "windows")
        }
    }

    /// Chunks `name` skipping holes, cut every fragment: takes one chunk, which maps the file,
    /// shrinks the file to `shrunk` through a handle of its own, then takes the rest.
    async fn chunks_of_a_file_shrunk_after_its_first_chunk(
        dir: &TempDir,
        name: &str,
        shrunk: u64,
    ) -> Result<Vec<Chunk>, lore_storage::error::StorageError> {
        let path = dir.path().join(name);
        let (file, file_size) = lore_storage::content::ContentSource::file(&path)
            .open()
            .await
            .expect("open test file");
        let mut chunker = FileChunker::fixed_size(file, file_size, FRAGMENT_SIZE_THRESHOLD)
            .await
            .skipping_holes();
        let mut chunks = vec![
            chunker
                .next_chunk()
                .await
                .expect("the first chunk")
                .expect("a chunk"),
        ];
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open to shrink")
            .set_len(shrunk)
            .expect("shrink the file");
        while let Some(chunk) = chunker.next_chunk().await? {
            chunks.push(chunk);
        }
        Ok(chunks)
    }

    /// A file that shrinks below a range of data after the walk mapped it fails the chunking where
    /// the walk keeps an extent map, as the read of that range meets the end of the file. With
    /// `SEEK_DATA`, which keeps nothing, the lost tail is a hole to the size at open.
    #[tokio::test]
    async fn skipping_holes_fails_where_a_shrunk_file_lost_mapped_data() {
        if !holes_reported().await {
            return;
        }
        const MIB: usize = 1 << 20;
        let dir = TempDir::new("lore-storage-chunker-holes-shrunk-data-");
        sparse_file(&dir, "shrinks", 8 * MIB, &[(MIB, MIB), (5 * MIB, MIB)]).await;
        let seek = walked_with_seek(&dir.path().join("shrinks"));
        let chunked = chunks_of_a_file_shrunk_after_its_first_chunk(&dir, "shrinks", 3 << 20).await;
        if seek {
            let chunks = chunked.expect("the lost tail is a hole");
            assert!(matches!(
                chunks.last(),
                Some(Chunk::Hole { offset, len }) if *offset == 2 << 20 && *len == 6 << 20
            ));
        } else {
            assert!(chunked.is_err(), "the read of the lost range of data fails");
        }
    }

    /// A file that shrinks after the walk mapped it, over a tail that held no data, is chunked at
    /// its size at open, with the lost tail as a hole.
    #[tokio::test]
    async fn skipping_holes_keeps_a_lost_tail_that_held_no_data_as_a_hole() {
        if !holes_reported().await {
            return;
        }
        const MIB: usize = 1 << 20;
        let dir = TempDir::new("lore-storage-chunker-holes-shrunk-hole-");
        sparse_file(&dir, "shrinks", 8 * MIB, &[(MIB, MIB)]).await;
        let chunks = chunks_of_a_file_shrunk_after_its_first_chunk(&dir, "shrinks", 4 << 20)
            .await
            .expect("the chunking succeeds");
        assert!(matches!(
            chunks.last(),
            Some(Chunk::Hole { offset, len }) if *offset == 2 << 20 && *len == 6 << 20
        ));
    }
}
