// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Streaming content-defined chunking.
//!
//! Cuts a file into content-defined fragments without ever holding it whole. The
//! chunker keeps a window of `2 * FRAGMENT_SIZE_THRESHOLD` bytes and only cuts while
//! at least one maximum-sized fragment is buffered, so every cut decision sees the
//! same lookahead it would with the entire file resident. The emitted boundaries are
//! therefore identical to running [`fastcdc::v2020::FastCDC`] over the whole buffer,
//! which the tests below assert directly — a boundary change would rewrite every
//! fragment hash in every repository.
//!
//! A window is half read target, half headroom for the bytes the previous cut could
//! not decide. Any file bigger than one window gets two windows: the next read runs
//! in one while chunks are cut and handed out from the other, so disk latency is off
//! the critical path, and the undecided remainder travels into the incoming window's
//! headroom so the cut logic never sees the seam. A file that fits in a single read
//! gets one window, no headroom and no read-ahead.
//!
//! A cut queues boundaries, not bytes; the chunk is copied out of the window when it
//! is handed over, which is also when the caller charges it to the fragment budget.
//! Peak residency is therefore the windows themselves, which is exactly what the
//! reservation in [`FileChunker::open`] covers.
//!
//! Each read is one `lore-io` scatter into the window it fills, so the whole scan goes
//! through the file I/O engine and no thread is held across it on a completion backend.
//! The window travels into the operation and comes back with it, which is what the
//! owned-buffer contract requires; the windowing, pipelining and cut logic above it is
//! unchanged by that, as is every cut boundary.

use std::collections::VecDeque;
use std::path::Path;

use bytes::Bytes;
use bytes::BytesMut;
use lore_io::IoDriver;
use lore_io::IoFile;
use lore_io::OpenOptions;
use tokio::task::JoinHandle;

use crate::compress::FRAGMENT_SIZE_THRESHOLD;
use crate::concurrency::FRAGMENT_SIZE_EXPECTED;
use crate::concurrency::FRAGMENT_SIZE_MINIMUM;
use crate::error::StorageError;

/// Window capacity: one maximum fragment of headroom for the undecided remainder,
/// one to read into.
const WINDOW_SIZE: usize = 2 * FRAGMENT_SIZE_THRESHOLD;

/// A content-defined chunk: where it starts in the source, and its bytes.
pub struct Chunk {
    pub offset: u64,
    pub data: Bytes,
}

/// Open `path` for reading, returning the shared handle and its size.
pub async fn open_read(path: &Path) -> std::io::Result<(IoFile, u64)> {
    let file = IoDriver::global()
        .open(path, &OpenOptions::new().read(true))
        .await?;
    let size = file.metadata().await?.len();
    Ok((file, size))
}

/// How the chunker picks cut points.
enum CutMode {
    /// Cut where the content says to, matching whole-file `FastCDC`.
    ContentDefined,
    /// Cut every N bytes. Never exceeds [`FRAGMENT_SIZE_THRESHOLD`], so the window
    /// always holds at least one whole chunk.
    FixedSize(usize),
}

/// The window a read fills, owned by the operation for its whole flight and handed back
/// with it. A single segment: the read lands in `buffer[start..start + want]`, leaving
/// anything in front of it as headroom for bytes the caller carries over itself.
pub(crate) struct WindowRead {
    pub(crate) buffer: BytesMut,
    pub(crate) start: usize,
    pub(crate) want: usize,
}

impl lore_io::StableBufListMut for WindowRead {
    fn byte_segments_mut(&mut self) -> impl Iterator<Item = &mut [u8]> {
        std::iter::once(&mut self.buffer[self.start..self.start + self.want])
    }
}

/// A read in flight into the window that is not being cut.
struct PendingRead {
    task: JoinHandle<std::io::Result<BytesMut>>,
    /// The exact count the read fills, from the clamp in `start_read`. The driver reports
    /// no count of its own: an exact read either fills the request or fails.
    want: usize,
}

/// Cuts a file into chunks a window at a time.
pub struct FileChunker {
    file: IoFile,
    /// Unconsumed bytes live in `window[head..head + length]`. Everything outside that
    /// range is uninitialised until a read or a carry-over writes it.
    window: BytesMut,
    head: usize,
    length: usize,
    /// Where a read lands in a window, and so how much room the undecided remainder
    /// has in front of it: one maximum fragment, or nothing for a single-read file.
    headroom: usize,
    /// Bytes of the window the last cut decided. Their boundaries are queued in
    /// `ready`, so they stay put until it drains and are then carried over.
    decided: usize,
    /// Boundaries cut but not yet handed out, as `(offset in window, length)`. The
    /// bytes are copied out at hand-out, so a queued boundary holds no memory.
    ready: VecDeque<(usize, usize)>,
    /// Set once a call has failed. A failed read takes its buffer with it, so the window
    /// bookkeeping no longer describes the window: resuming would cut boundaries from the
    /// wrong bytes and silently hash them as the file's.
    failed: bool,
    /// The read filling the other window while this one is handed out.
    pending: Option<PendingRead>,
    /// The other window, parked here whenever no read is in flight.
    spare: Option<BytesMut>,
    /// Absolute file offset of `window[head]`.
    base: u64,
    /// Absolute file offset the next read starts at.
    read_offset: u64,
    /// Size the file was opened at, used to spot the end when the window is sized to
    /// the file and a completely full buffer therefore also means "nothing follows".
    file_size: u64,
    eof: bool,
    mode: CutMode,
    /// Budget for every window and one chunk, released when the chunker is dropped.
    _reservation: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl FileChunker {
    /// Cut on content, matching [`fastcdc::v2020::FastCDC`] over the whole file.
    pub async fn content_defined(file: IoFile, file_size: u64) -> Self {
        Self::open(file, file_size, CutMode::ContentDefined).await
    }

    /// Cut every `chunk_size` bytes, clamped to a whole fragment and at least one byte
    /// so a caller passing zero cannot stall the cut loop.
    pub async fn fixed_size(file: IoFile, file_size: u64, chunk_size: usize) -> Self {
        let mode = CutMode::FixedSize(chunk_size.clamp(1, FRAGMENT_SIZE_THRESHOLD));
        Self::open(file, file_size, mode).await
    }

    /// Reserves every window this chunker will allocate plus one maximum-size chunk
    /// from the fragment budget in a single acquire, and holds it for as long as the
    /// chunker lives.
    ///
    /// The reservation is what lets the caller finish a file it has started: the
    /// windows are held across every chunk permit the caller takes, so charging them
    /// separately would let concurrent files fill the budget with windows and then all
    /// wait on chunk permits only they could release. Because one chunk is already
    /// reserved here, a caller that cannot get a fresh permit may process a single
    /// chunk at a time without one.
    ///
    /// Every term is bounded by the file: nothing larger than the file is ever
    /// buffered or cut from it, and a file that fits in one read gets a single window
    /// with no headroom and no read-ahead.
    async fn open(file: IoFile, file_size: u64, mode: CutMode) -> Self {
        let capacity = file_size.min(WINDOW_SIZE as u64) as usize;
        let single_read = file_size <= WINDOW_SIZE as u64;
        let headroom = if single_read {
            0
        } else {
            FRAGMENT_SIZE_THRESHOLD
        };
        let windows = if single_read { 1 } else { 2 };
        let reserved_chunk = file_size.min(FRAGMENT_SIZE_THRESHOLD as u64) as usize;
        let reservation =
            crate::concurrency::acquire_fragment_memory_permit(windows * capacity + reserved_chunk)
                .await;

        Self {
            file,
            // SAFETY: no byte is read before it is written. The cut only ever looks at
            // `window[head..head + length]`, which is the region the read filled plus the
            // remainder `swap_in` carries into the headroom in front of it.
            window: unsafe { lore_io::uninit_buffer(capacity) },
            head: headroom,
            length: 0,
            headroom,
            decided: 0,
            ready: VecDeque::new(),
            pending: None,
            failed: false,
            // SAFETY: as above; the two windows are used interchangeably.
            spare: (!single_read).then(|| unsafe { lore_io::uninit_buffer(capacity) }),
            base: 0,
            read_offset: 0,
            file_size,
            eof: false,
            mode,
            _reservation: reservation,
        }
    }

    /// The next chunk, or `None` once the file is exhausted.
    ///
    /// A chunker that has returned an error returns one for every later call. It cannot be
    /// resumed, and answering `None` instead would report a truncated file as complete.
    pub async fn next_chunk(&mut self) -> Result<Option<Chunk>, StorageError> {
        if self.failed {
            return Err(StorageError::internal(
                "chunker was used again after a read failure",
            ));
        }
        loop {
            if let Some((offset, length)) = self.ready.pop_front() {
                return Ok(Some(Chunk {
                    offset: self.base + (offset - self.head) as u64,
                    data: Bytes::copy_from_slice(&self.window[offset..offset + length]),
                }));
            }
            if self.eof {
                return Ok(None);
            }
            if let Err(err) = self.advance().await {
                self.failed = true;
                return Err(err);
            }
        }
    }

    /// Take the read that ran while the last batch was handed out, carry the undecided
    /// remainder in front of it, and cut again.
    async fn advance(&mut self) -> Result<(), StorageError> {
        let (buffer, read) = self.take_read().await?;
        self.swap_in(buffer, read);
        self.cut();
        self.issue_read();

        Ok(())
    }

    /// Make `buffer` the window, moving the undecided remainder into the headroom in
    /// front of the bytes just read so the two are contiguous — the cut logic never
    /// sees the seam between one read and the next.
    fn swap_in(&mut self, mut buffer: BytesMut, read: usize) {
        let remainder = self.length - self.decided;
        debug_assert!(
            remainder <= self.headroom,
            "{remainder} undecided bytes do not fit the {} byte headroom",
            self.headroom
        );

        let head = self.headroom - remainder;
        buffer[head..self.headroom]
            .copy_from_slice(&self.window[self.head + self.decided..self.head + self.length]);

        self.spare = Some(std::mem::replace(&mut self.window, buffer));
        self.base += self.decided as u64;
        self.head = head;
        self.length = remainder + read;
        self.decided = 0;
    }

    /// The bytes of the next read: from the task started while the last batch was
    /// handed out, or from one started and awaited here — which is the first read of a
    /// file, and any round where the previous cut decided nothing.
    async fn take_read(&mut self) -> Result<(BytesMut, usize), StorageError> {
        let pending = if let Some(pending) = self.pending.take() {
            pending
        } else {
            // There is no spare only in the single-read regime, where the file fits
            // in the one window and nothing has been read into it yet.
            let target = self
                .spare
                .take()
                .unwrap_or_else(|| std::mem::take(&mut self.window));
            self.start_read(target)
        };

        let buffer = pending
            .task
            .await
            .map_err(|e| StorageError::internal_with_context(e, "chunker read task failure"))?
            .map_err(|e| StorageError::internal_with_context(e, "read file for chunking"))?;

        self.read_offset += pending.want as u64;
        if self.read_offset >= self.file_size {
            self.eof = true;
        }

        Ok((buffer, pending.want))
    }

    /// Start filling the other window, so the read overlaps cutting and handing out the
    /// batch just cut. Nothing to start once the file is exhausted.
    fn issue_read(&mut self) {
        if self.pending.is_some() || self.read_offset >= self.file_size {
            return;
        }
        if let Some(target) = self.spare.take() {
            self.pending = Some(self.start_read(target));
        }
    }

    /// One scatter into `buffer[headroom..]`, spawned so it overlaps the cutting and
    /// handing out of the batch already in the other window.
    ///
    /// The length is clamped to what the file held when it was opened, so the read is an
    /// exact one: a file that shrank underneath is an error rather than a silent early
    /// end, and one appended to mid-write cannot yield a chunk list covering more bytes
    /// than the root fragment records, which readers trust.
    fn start_read(&self, buffer: BytesMut) -> PendingRead {
        let file = self.file.clone();
        let start = self.headroom;
        let offset = self.read_offset;
        let remaining = self.file_size.saturating_sub(offset);
        let want = (buffer.len() - start).min(remaining as usize);

        let task = lore_base::lore_spawn!(async move {
            let window = file
                .read_exact_vectored_at(
                    WindowRead {
                        buffer,
                        start,
                        want,
                    },
                    offset,
                )
                .await?;
            Ok(window.buffer)
        });

        PendingRead { task, want }
    }

    /// Cut every chunk the current window can decide. Only boundaries are queued; the
    /// bytes stay in the window until each chunk is handed out.
    fn cut(&mut self) {
        match self.mode {
            CutMode::ContentDefined => self.cut_on_content(),
            CutMode::FixedSize(size) => self.cut_fixed(size),
        }
        debug_assert!(
            self.length - self.decided <= self.headroom,
            "undecided remainder does not fit the headroom it must be carried in"
        );
    }

    /// Queue whole fixed-size chunks, and the short tail once the file has ended.
    fn cut_fixed(&mut self, size: usize) {
        let mut decided = 0;
        while decided < self.length {
            let remaining = self.length - decided;
            let take = if remaining >= size {
                size
            } else if self.eof {
                remaining
            } else {
                break;
            };
            self.ready.push_back((self.head + decided, take));
            decided += take;
        }
        self.decided = decided;
    }

    /// Queue every content-defined cut the window can decide without more lookahead.
    fn cut_on_content(&mut self) {
        let mut chunker = fastcdc::v2020::FastCDC::with_level(
            &self.window[self.head..self.head + self.length],
            FRAGMENT_SIZE_MINIMUM as u32,
            FRAGMENT_SIZE_EXPECTED as u32,
            FRAGMENT_SIZE_THRESHOLD as u32,
            fastcdc::v2020::Normalization::Level1,
        );

        let mut decided = 0;
        loop {
            // Without a whole maximum fragment of lookahead the chunker would force a
            // cut at the window edge where the whole file would have kept scanning.
            // Tested before pulling the cut, so its scan is not computed and discarded.
            if !self.eof && decided + FRAGMENT_SIZE_THRESHOLD > self.length {
                break;
            }
            let Some(chunk) = chunker.next() else {
                break;
            };
            self.ready
                .push_back((self.head + chunk.offset, chunk.length));
            decided = chunk.offset + chunk.length;
        }
        self.decided = decided;
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::test_util::TempDir;

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

        let (file, file_size) = open_read(&path).await.expect("open test file");
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

        let actual: Vec<(u64, usize)> = chunks.iter().map(|c| (c.offset, c.data.len())).collect();
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

        let actual: Vec<(u64, usize)> = chunks.iter().map(|c| (c.offset, c.data.len())).collect();
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
            let start = chunk.offset as usize;
            assert_eq!(
                chunk.data.as_ref(),
                &content[start..start + chunk.data.len()],
                "chunk data at offset {start} does not match the source"
            );
        }

        let covered: usize = chunks.iter().map(|c| c.data.len()).sum();
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
            assert_matches_fixed_reference(
                &format!("fixed-{size}-{step}"),
                &random_buffer(size),
                step,
            )
            .await;
        }
    }

    async fn open_chunker(dir: &TempDir, name: &str, size: usize) -> FileChunker {
        let path = dir.path().join(name);
        std::fs::write(&path, random_buffer(size)).expect("write test file");
        let (file, _) = open_read(&path).await.expect("open test file");

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

        let (file, _) = open_read(&path).await.expect("open test file");
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
            covered += chunk.data.len();
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

        let (file, _) = open_read(&path).await.expect("open test file");
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
            .open(&path, &OpenOptions::new().write(true))
            .await
            .expect("open write-only");

        let mut chunker =
            FileChunker::content_defined(file, 4 * FRAGMENT_SIZE_THRESHOLD as u64).await;
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
}
