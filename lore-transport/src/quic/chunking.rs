// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::mpsc::Sender;
use std::time::Duration;
use std::time::Instant;

use bytes::Bytes;
use bytes::BytesMut;
use lore_base::lore_trace;
use quinn::Chunk;
use smallvec::SmallVec;
use zerocopy::IntoBytes;

use crate::quic::command_header::COMMAND_HEADER_SIZE_V4;
use crate::quic::command_header::CommandHeader;

#[derive(Debug)]
pub struct QuicMessage {
    pub header: CommandHeader,
    pub payload: Option<Bytes>,
    /// When the chunk completing the message's header arrived, where the caller supplied arrival
    /// times.
    pub header_at: Option<Instant>,
}

pub enum ChunkingMetric {
    Stall(Duration),
    PendingChunks(usize),
}

pub struct SerialChunking {
    header_size: usize,
    max_chunk_size: u32,

    request: CommandHeader,
    payload: Option<bytes::BytesMut>,
    request_bytes: [u8; COMMAND_HEADER_SIZE_V4],
    request_bytes_read: usize,
    /// Where the message currently being assembled begins. Advances as each one is framed.
    start_offset: u64,
    current_offset: u64,
    pending_chunks: Vec<Chunk>,

    metrics: Option<Sender<ChunkingMetric>>,
    stall_start: Instant,
}

impl SerialChunking {
    pub fn new(
        header_size: usize,
        max_chunk_size: usize,
        metrics: Option<Sender<ChunkingMetric>>,
    ) -> Self {
        debug_assert!(header_size <= COMMAND_HEADER_SIZE_V4);

        SerialChunking {
            header_size,
            max_chunk_size: max_chunk_size as u32,
            request: CommandHeader::default(),
            payload: None,
            request_bytes: [0u8; COMMAND_HEADER_SIZE_V4],
            request_bytes_read: 0,
            start_offset: 0,
            current_offset: 0,
            pending_chunks: vec![],
            metrics,
            stall_start: Instant::now(),
        }
    }

    /// The header of the message being assembled, once enough bytes have arrived to read it.
    fn command_header(&self) -> Option<&CommandHeader> {
        if self.request_bytes_read >= self.header_size {
            return Some(&self.request);
        }
        None
    }

    /// Frames whatever messages the chunk completes, appending them to `messages`.
    ///
    /// The buffer belongs to the caller so a stream can keep one across its read loop: a chunk
    /// carrying several messages then costs no allocation in steady state.
    ///
    /// Returns the offending header if one declares a payload larger than the chunk size cap,
    /// which is fatal to the stream.
    pub fn resolve_into(
        &mut self,
        new_chunk: Chunk,
        messages: &mut Vec<QuicMessage>,
    ) -> Result<(), CommandHeader> {
        let mut pending_chunk_change = None;

        let mut next_chunk = Some(new_chunk);
        while let Some(mut chunk) = next_chunk.take() {
            if chunk.offset == self.current_offset {
                while !chunk.bytes.is_empty() {
                    if self.request_bytes_read < self.header_size {
                        // Read the request header
                        if chunk.bytes.len() + self.request_bytes_read < self.header_size {
                            let got_count = chunk.bytes.len();
                            self.request_bytes
                                [self.request_bytes_read..(self.request_bytes_read + got_count)]
                                .copy_from_slice(chunk.bytes.as_ref());

                            self.request_bytes_read += got_count;
                            self.current_offset += got_count as u64;
                            chunk.bytes.clear();
                        } else {
                            let remain_count = self.header_size - self.request_bytes_read;
                            let remain_bytes = chunk.bytes.split_to(remain_count);

                            self.request_bytes[self.request_bytes_read..self.header_size]
                                .copy_from_slice(remain_bytes.as_ref());

                            self.request = if self.header_size == COMMAND_HEADER_SIZE_V4 {
                                CommandHeader::from_bytes_v4(self.request_bytes.as_bytes())
                            } else {
                                CommandHeader::from_bytes(self.request_bytes.as_bytes())
                            };
                            // The cap bounds a payload allocation. An error header's size field
                            // is a status code, so there is no payload for it to bound and a
                            // large status is not oversized.
                            if !self.request.error
                                && self.request.size_or_status > self.max_chunk_size
                            {
                                return Err(self.request);
                            }

                            self.request_bytes_read = self.header_size;
                            self.current_offset += remain_count as u64;

                            lore_trace!("QUIC stream read request header {:?}", self.request);

                            // If error there is no more data, otherwise allocate buffer for response payload
                            if !self.request.error && self.request.size_or_status > 0 {
                                if chunk.bytes.len() >= self.request.size_or_status as usize {
                                    // Happy path, we can directly use buffer as it contains the full request
                                    let size = self.request.size_or_status as usize;
                                    let current_payload = chunk.bytes.split_to(size);

                                    self.current_offset += size as u64;

                                    lore_trace!(
                                        "QUIC stream read {} bytes complete payload from single chunk",
                                        size
                                    );

                                    self.request_bytes_read = 0;
                                    self.start_offset = self.current_offset;
                                    messages.push(QuicMessage {
                                        header: self.request,
                                        payload: Some(current_payload),
                                        header_at: None,
                                    });
                                } else {
                                    // Allocate buffer for request payload
                                    self.payload = Some(bytes::BytesMut::with_capacity(
                                        self.request.size_or_status as usize,
                                    ));
                                }
                            } else {
                                self.request_bytes_read = 0;
                                self.start_offset = self.current_offset;
                                messages.push(QuicMessage {
                                    header: self.request,
                                    payload: None,
                                    header_at: None,
                                });
                            }
                        }
                    }
                    if let Some(mut current_payload) = self.payload.take() {
                        let size = std::cmp::min(
                            current_payload.capacity() - current_payload.len(),
                            chunk.bytes.len(),
                        );

                        let this_chunk = chunk.bytes.split_to(size);
                        current_payload.extend_from_slice(this_chunk.as_bytes());

                        self.current_offset += size as u64;

                        lore_trace!(
                            "QUIC stream read {} bytes for a total of {} / {} bytes of payload",
                            size,
                            current_payload.len(),
                            current_payload.capacity()
                        );

                        if current_payload.capacity() == current_payload.len() {
                            self.request_bytes_read = 0;

                            self.start_offset = self.current_offset;
                            messages.push(QuicMessage {
                                header: self.request,
                                payload: Some(current_payload.freeze()),
                                header_at: None,
                            });
                        } else {
                            self.payload = Some(current_payload);
                        }
                    }
                }
            } else {
                // Queue for later processing
                lore_trace!(
                    "Got out of order chunk @ offset {}, current offset is {}",
                    chunk.offset,
                    self.current_offset
                );
                if self.pending_chunks.is_empty() {
                    self.stall_start = Instant::now();
                }
                self.pending_chunks.push(chunk);
                pending_chunk_change = Some(self.pending_chunks.len());
            }

            for (ichunk, chunk) in self.pending_chunks.iter().enumerate() {
                if chunk.offset == self.current_offset {
                    lore_trace!(
                        "Grab out of order chunk @ current offset {} - {} ooo chunks remaining",
                        self.current_offset,
                        self.pending_chunks.len() - 1
                    );
                    next_chunk = Some(self.pending_chunks.swap_remove(ichunk));
                    pending_chunk_change = Some(self.pending_chunks.len());
                    if self.pending_chunks.is_empty()
                        && let Some(metrics) = self.metrics.as_mut()
                    {
                        let _ = metrics.send(ChunkingMetric::Stall(self.stall_start.elapsed()));
                    }
                    break;
                }
            }
        }

        if let Some(pending_chunk_change) = pending_chunk_change
            && let Some(metrics) = self.metrics.as_mut()
        {
            let _ = metrics.send(ChunkingMetric::PendingChunks(pending_chunk_change));
        }

        Ok(())
    }
}

/// One serial run over a contiguous region of the stream.
///
/// A run frames messages strictly in order from where it starts, so it is only useful alongside
/// others: splitting a stalled run in two lets the bytes piling up behind its gap be framed by a
/// second run rather than waiting on the first.
struct SerialBoundary {
    assembler: SerialChunking,

    /// Whether `pending_chunks` is in offset order. Arrivals leave it stale, and the reading paths
    /// restore it on demand, so a backlog costs one sort per read rather than one per arrival.
    chunks_sorted: bool,

    /// Where the run stops, once it has been split and a successor owns the bytes beyond. `None`
    /// while this is the last run, which owns everything the peer has yet to send.
    end_exclusive: Option<u64>,
}

impl SerialBoundary {
    pub fn resolve_into(
        &mut self,
        new_chunk: Chunk,
        messages: &mut Vec<QuicMessage>,
    ) -> Result<(), CommandHeader> {
        self.assembler.resolve_into(new_chunk, messages)?;
        if !self.assembler.pending_chunks.is_empty() {
            self.chunks_sorted = false;
        }
        Ok(())
    }

    /// Whether this run is part-way through a message: bytes of one have arrived, and it has not
    /// been framed yet. A run assembles one message at a time, so this is its pending count.
    fn has_pending_message(&self) -> bool {
        self.assembler.request_bytes_read > 0
            || self.assembler.payload.is_some()
            || !self.assembler.pending_chunks.is_empty()
    }

    /// Restores the offset order the reading paths rely on. A no-op unless a chunk has arrived
    /// since the last call, so the sort is paid once per assembly rather than once per arrival.
    fn sort_chunks(&mut self) {
        if !self.chunks_sorted {
            self.assembler
                .pending_chunks
                .sort_by_key(|chunk| chunk.offset);
            self.chunks_sorted = true;
        }
    }

    /// Removes every byte at or past `end_exclusive`, for the boundary that owns them.
    /// Returns in reverse offset order
    fn split_off_surplus(&mut self, end_exclusive: u64) -> Vec<Chunk> {
        if self.assembler.pending_chunks.is_empty() {
            return vec![];
        }
        self.sort_chunks();

        let mut surplus = vec![];

        while let Some(last) = self.assembler.pending_chunks.last_mut() {
            if last.offset >= end_exclusive {
                let chunk = self
                    .assembler
                    .pending_chunks
                    .pop()
                    .expect("a chunk was just observed at the back");
                surplus.push(chunk);
                continue;
            }

            if last.offset + last.bytes.len() as u64 > end_exclusive {
                let keep = (end_exclusive - last.offset) as usize;
                let remainder = last.bytes.split_off(keep);
                surplus.push(Chunk {
                    offset: end_exclusive,
                    bytes: remainder,
                });
            }
            break;
        }

        surplus
    }
}

/// Frames messages serially until a run stalls, then splits it so later messages need not wait on
/// an earlier gap.
///
/// A stream that arrives in order never splits and costs what [`SerialChunking`] costs. Splitting
/// only begins once a run has queued `split_chunk_threshold` chunks behind a gap, which bounds what
/// the reordering tolerance costs a stream that does not need it. The threshold is the whole
/// tuning surface: too low and streams split when the gap would have filled on its own, too high
/// and a stalled run blocks messages that were ready.
pub struct ThresholdSerialChunking {
    header_size: usize,
    max_chunk_size: usize,
    split_chunk_threshold: usize,

    /// In order of start offset. The last one is always open-ended.
    boundaries: Vec<SerialBoundary>,

    metrics: Option<Sender<ChunkingMetric>>,
    /// When chunks first became unconsumable behind a gap, while that is still the case.
    stall_start: Option<Instant>,
    reported_pending_chunks: usize,
}

impl ThresholdSerialChunking {
    pub fn new(
        header_size: usize,
        max_chunk_size: usize,
        split_chunk_threshold: usize,
        metrics: Option<Sender<ChunkingMetric>>,
    ) -> Self {
        debug_assert!(header_size <= COMMAND_HEADER_SIZE_V4);

        let mut return_value = ThresholdSerialChunking {
            header_size,
            max_chunk_size,
            split_chunk_threshold,
            boundaries: vec![],

            metrics,
            stall_start: None,
            reported_pending_chunks: 0,
        };
        let first_boundary = return_value.open_at(0);
        return_value.boundaries.push(first_boundary);
        return_value
    }

    /// How many runs are currently open. A run per in-flight message is the cost of splitting, so
    /// a test can hold it to what the stream actually needs.
    #[cfg(feature = "test-util")]
    pub fn open_run_count(&self) -> usize {
        self.boundaries.len()
    }

    /// Opens a run beginning at `offset`, with no end until it is split.
    fn open_at(&self, offset: u64) -> SerialBoundary {
        let mut assembler = SerialChunking::new(
            self.header_size,
            self.max_chunk_size,
            None, /* no metrics, we do them higher up */
        );
        assembler.current_offset = offset;
        // The run begins here too, and `boundary_for_offset` reads it to decide ownership.
        assembler.start_offset = offset;
        SerialBoundary {
            assembler,
            chunks_sorted: true,
            end_exclusive: None,
        }
    }

    /// The boundary owning `offset`: the last one starting at or before it that has not already
    /// been framed past it.
    fn boundary_for_offset(&self, offset: u64) -> Option<usize> {
        self.boundaries
            .iter()
            .enumerate()
            .rev()
            .find(|(_, boundary)| {
                if boundary.assembler.start_offset <= offset {
                    boundary.end_exclusive.is_none_or(|end| end > offset)
                } else {
                    false
                }
            })
            .map(|(index, _)| index)
    }

    /// As [`Self::resolve`], appending to a caller-supplied buffer.
    pub fn resolve_into(
        &mut self,
        new_chunk: Chunk,
        messages: &mut Vec<QuicMessage>,
    ) -> Result<(), CommandHeader> {
        let result = self.frame_chunk(new_chunk, messages);
        self.report_metrics();
        result
    }

    /// Reports what this chunk cost.
    ///
    /// A stall runs for as long as any run is holding chunks it cannot consume because an earlier
    /// byte has not arrived. A message spanning many chunks delivered in order queues nothing and
    /// so does not stall. The pending count is how many runs are part-way through a message.
    fn report_metrics(&mut self) {
        let Some(metrics) = self.metrics.as_ref() else {
            return;
        };

        let queued: usize = self
            .boundaries
            .iter()
            .map(|boundary| boundary.assembler.pending_chunks.len())
            .sum();
        match (self.stall_start, queued) {
            (None, 1..) => self.stall_start = Some(Instant::now()),
            (Some(stall_start), 0) => {
                let _ = metrics.send(ChunkingMetric::Stall(stall_start.elapsed()));
                self.stall_start = None;
            }
            _ => {}
        }

        let pending = self
            .boundaries
            .iter()
            .filter(|boundary| boundary.has_pending_message())
            .count();
        if pending != self.reported_pending_chunks {
            let _ = metrics.send(ChunkingMetric::PendingChunks(pending));
            self.reported_pending_chunks = pending;
        }
    }

    /// Frames a chunk, recursing when splitting a run hands bytes to its successor.
    fn frame_chunk(
        &mut self,
        mut new_chunk: Chunk,
        messages: &mut Vec<QuicMessage>,
    ) -> Result<(), CommandHeader> {
        while !new_chunk.bytes.is_empty() {
            let Some(boundary_index) = self.boundary_for_offset(new_chunk.offset) else {
                // the client has sent a chunk for a boundary that has already been closed
                return Err(CommandHeader::default());
            };

            let is_intermediate_boundary = boundary_index != self.boundaries.len() - 1;
            let chunk_for_boundary = if is_intermediate_boundary && let Some(boundary_end) =
                    self.boundaries[boundary_index].end_exclusive
                // the boundary might only want a portion of the chunk
                && boundary_end < new_chunk.offset + new_chunk.bytes.len() as u64
            {
                let trimmed_chunk = Chunk {
                    offset: new_chunk.offset,
                    bytes: new_chunk
                        .bytes
                        .split_to((boundary_end - new_chunk.offset) as usize),
                };
                new_chunk.offset = boundary_end;

                trimmed_chunk
            } else {
                Chunk {
                    offset: new_chunk.offset,
                    bytes: std::mem::take(&mut new_chunk.bytes),
                }
            };

            let were_messages_added = {
                let pre_size = messages.len();
                self.boundaries[boundary_index].resolve_into(chunk_for_boundary, messages)?;
                messages.len() != pre_size
            };

            // An intermediate boundary covers exactly the message it was split at, so emitting a
            // message with nothing left queued means its run is done.
            if were_messages_added
                && is_intermediate_boundary
                && self.boundaries[boundary_index]
                    .assembler
                    .pending_chunks
                    .is_empty()
            {
                self.boundaries.remove(boundary_index);
            }
            // A boundary queuing this much has stalled behind a gap, so split the message it is
            // waiting on away from the bytes piling up after it. Whether this chunk happened to
            // complete a message is beside the point - a boundary that completes nothing is the
            // one most in need of splitting.
            else if self.boundaries[boundary_index]
                .assembler
                .pending_chunks
                .len()
                >= self.split_chunk_threshold
                && let Some(header) = self.boundaries[boundary_index].assembler.command_header()
            {
                let new_boundary_start_offset =
                    self.boundaries[boundary_index].assembler.start_offset
                        + self.header_size as u64
                        + payload_size(header);
                let mut new_boundary_chunks =
                    self.boundaries[boundary_index].split_off_surplus(new_boundary_start_offset);

                // Everything queued belongs to the message this run is already waiting on, so the
                // gap is inside that message and a second run would have nothing to frame. Without
                // this the run splits again on every later chunk, stranding an empty boundary each
                // time.
                // Everything queued belongs to the message this run is already waiting on, so the
                // gap is inside that message and a second run would have nothing to frame. Without
                // this the run splits again on every later chunk, stranding an empty boundary each
                // time.
                if new_boundary_chunks.is_empty() {
                    continue;
                }

                let new_boundary = self.open_at(new_boundary_start_offset);
                self.boundaries[boundary_index].end_exclusive = Some(new_boundary_start_offset);
                self.boundaries.insert(boundary_index + 1, new_boundary);

                // `split_off_surplus` hands these back highest first; feeding them in ascending
                // order lets the contiguous run resolve as it arrives instead of queueing again.
                for chunk in new_boundary_chunks.drain(..).rev() {
                    self.frame_chunk(chunk, messages)?;
                }
            }
        }

        Ok(())
    }
}

/// An assembled command header. A header is never wider than this, so framing one never
/// allocates.
type AssembledHeader = SmallVec<[u8; COMMAND_HEADER_SIZE_V4]>;

/// One message's region of the stream: its header, and the payload that follows.
///
/// A region's extent is unknown until the header naming its length arrives, so the last boundary
/// is always open and owns every byte past the last framed message.
struct PendingBoundary {
    header: Option<CommandHeader>,
    start_offset: u64,

    /// This message's header bytes received so far. Bytes continuing the header land here
    /// directly, so they never enter the chunk queue and never have to be drained out of it.
    header_bytes: AssembledHeader,

    /// Non-overlapping, none starting before `header_cursor`, and only ordered by offset once
    /// `sort_chunks` has been called - arrivals are appended and leave the order stale.
    pending_chunks: Vec<Chunk>,

    /// Bytes held in `pending_chunks`. Since the chunks cannot overlap and are clipped to this
    /// message's region, this reaching the payload size proves every payload byte has arrived,
    /// with no ordering or contiguity check needed.
    pending_bytes: u64,
    chunks_sorted: bool,

    /// Lowest offset held in `pending_chunks`, or `u64::MAX` while it is empty. Cheap to keep on
    /// arrival and enough to tell whether the next byte the header needs is even here, so an
    /// unreachable header costs no sort.
    lowest_pending_offset: u64,

    /// When the chunk completing this message's header arrived, once it has and arrival times are
    /// supplied.
    header_at: Option<Instant>,

    is_dirty: bool,
}

impl PendingBoundary {
    fn open_at(start_offset: u64) -> Self {
        PendingBoundary {
            header: None,
            start_offset,
            header_bytes: AssembledHeader::new(),
            pending_chunks: vec![],
            pending_bytes: 0,
            chunks_sorted: true,
            lowest_pending_offset: u64::MAX,
            header_at: None,
            is_dirty: false,
        }
    }

    /// The offset one past this message's last byte, known once its header has been framed.
    fn end_offset_exclusive(&self, header_size: usize) -> Option<u64> {
        let header = self.header.as_ref()?;
        Some(self.start_offset + header_size as u64 + payload_size(header))
    }

    /// The offset the next header byte would occupy. Once the header is whole this is where the
    /// payload starts.
    fn header_cursor(&self) -> u64 {
        self.start_offset + self.header_bytes.len() as u64
    }

    /// Takes as much of `bytes` as the header still needs, returning how much it took.
    fn absorb_header_bytes(&mut self, header_size: usize, bytes: &mut Bytes) -> usize {
        let wanted = (header_size - self.header_bytes.len()).min(bytes.len());
        self.header_bytes.extend_from_slice(&bytes.split_to(wanted));
        wanted
    }

    /// Moves header bytes already waiting in the chunk queue into the buffer, for the arrival
    /// order that queued a later part of the header before an earlier one.
    fn absorb_queued_header_bytes(&mut self, header_size: usize) {
        // Nothing held starts before the cursor, so unless the lowest offset meets it the next
        // header byte has yet to arrive and there is nothing to order.
        if self.lowest_pending_offset != self.header_cursor() {
            return;
        }
        self.sort_chunks();

        while self.header_bytes.len() < header_size {
            let cursor = self.header_cursor();
            let Some(first) = self.pending_chunks.first_mut() else {
                break;
            };
            if first.offset != cursor {
                break;
            }

            let wanted = header_size - self.header_bytes.len();
            if first.bytes.len() <= wanted {
                let chunk = self.pending_chunks.remove(0);
                self.pending_bytes -= chunk.bytes.len() as u64;
                self.header_bytes.extend_from_slice(&chunk.bytes);
            } else {
                let head = first.bytes.split_to(wanted);
                first.offset += wanted as u64;
                self.pending_bytes -= wanted as u64;
                self.header_bytes.extend_from_slice(&head);
            }
        }

        self.refresh_lowest_pending_offset();
    }

    /// Appends a chunk, leaving the offset order stale. Ordering costs a sort, and nothing needs
    /// it until the message is ready to assemble.
    fn push_chunk(&mut self, chunk: Chunk) {
        self.pending_bytes += chunk.bytes.len() as u64;
        self.lowest_pending_offset = self.lowest_pending_offset.min(chunk.offset);
        self.pending_chunks.push(chunk);
        self.chunks_sorted = false;
    }

    /// Re-reads the lowest offset from an ordered queue, after chunks have been taken out of it.
    fn refresh_lowest_pending_offset(&mut self) {
        debug_assert!(self.chunks_sorted);
        self.lowest_pending_offset = self
            .pending_chunks
            .first()
            .map_or(u64::MAX, |chunk| chunk.offset);
    }

    /// Restores the offset order the reading paths rely on. A no-op unless a chunk has arrived
    /// since the last call, so the sort is paid once per assembly rather than once per arrival.
    fn sort_chunks(&mut self) {
        if !self.chunks_sorted {
            self.pending_chunks.sort_by_key(|chunk| chunk.offset);
            self.chunks_sorted = true;
        }
    }

    /// Whether every byte of a payload of `payload_size` is held.
    fn holds_whole_payload(&self, payload_size: u64) -> bool {
        debug_assert!(
            self.pending_bytes <= payload_size,
            "chunks outran the message region"
        );
        self.pending_bytes == payload_size
    }

    /// Removes and returns the payload, which `holds_whole_payload` has shown to be complete.
    fn take_payload(&mut self, payload_size: usize) -> Bytes {
        self.sort_chunks();
        debug_assert_eq!(self.pending_bytes as usize, payload_size);
        debug_assert!(
            self.pending_chunks
                .iter()
                .scan(self.header_cursor(), |expected, chunk| {
                    let meets = chunk.offset == *expected;
                    *expected += chunk.bytes.len() as u64;
                    Some(meets)
                })
                .all(|meets| meets),
            "a complete byte count must mean contiguous chunks"
        );

        self.pending_bytes = 0;
        self.lowest_pending_offset = u64::MAX;

        // A payload delivered in one chunk is handed on as it stands, without copying it.
        if self.pending_chunks.len() == 1 {
            return self
                .pending_chunks
                .pop()
                .expect("a single chunk was just observed")
                .bytes;
        }

        let mut payload = BytesMut::with_capacity(payload_size);
        for chunk in self.pending_chunks.drain(..) {
            payload.extend_from_slice(&chunk.bytes);
        }
        payload.freeze()
    }

    /// Chunks held behind a gap, unable to be framed until an earlier byte of this message
    /// arrives. Chunks forming the contiguous run from the next byte due are not stalled.
    fn stalled_chunks(&self) -> usize {
        // Reads the order as it stands: a stale order only ever over-reports, which is the safe
        // direction for a metric and not worth a sort on the reporting path.
        let mut expected = self.header_cursor();

        let mut contiguous = 0;
        for chunk in &self.pending_chunks {
            if chunk.offset != expected {
                break;
            }
            expected += chunk.bytes.len() as u64;
            contiguous += 1;
        }

        self.pending_chunks.len() - contiguous
    }

    /// Removes every byte at or past `end_exclusive`, for the boundary that owns them.
    fn split_off_surplus(&mut self, end_exclusive: u64) -> Vec<Chunk> {
        if self.pending_chunks.is_empty() {
            return vec![];
        }
        self.sort_chunks();

        let mut surplus = vec![];

        while let Some(last) = self.pending_chunks.last_mut() {
            if last.offset >= end_exclusive {
                let chunk = self
                    .pending_chunks
                    .pop()
                    .expect("a chunk was just observed at the back");
                self.pending_bytes -= chunk.bytes.len() as u64;
                surplus.push(chunk);
                continue;
            }

            if last.offset + last.bytes.len() as u64 > end_exclusive {
                let keep = (end_exclusive - last.offset) as usize;
                let remainder = last.bytes.split_off(keep);
                self.pending_bytes -= remainder.len() as u64;
                surplus.push(Chunk {
                    offset: end_exclusive,
                    bytes: remainder,
                });
            }
            break;
        }

        self.refresh_lowest_pending_offset();

        surplus
    }
}

pub struct ParallelChunking {
    header_size: usize,
    max_chunk_size: u32,

    metrics: Option<Sender<ChunkingMetric>>,
    reported_pending_chunks: usize,
    stall_start: Instant,

    /// When the chunk being resolved arrived, if the caller supplied it.
    arrived: Option<Instant>,

    /// Ordered by start offset. The last boundary is always open, so every byte the peer can
    /// still send belongs to one of these.
    pending_boundaries: Vec<PendingBoundary>,
}

impl ParallelChunking {
    pub fn new(
        header_size: usize,
        max_chunk_size: usize,
        metrics: Option<Sender<ChunkingMetric>>,
    ) -> Self {
        debug_assert!(header_size <= COMMAND_HEADER_SIZE_V4);

        ParallelChunking {
            header_size,
            max_chunk_size: max_chunk_size as u32,
            metrics,
            reported_pending_chunks: 0,
            stall_start: Instant::now(),
            arrived: None,
            pending_boundaries: vec![PendingBoundary::open_at(0)],
        }
    }

    /// Frames whatever messages the chunk completes, appending them to `messages`.
    ///
    /// The buffer belongs to the caller so a stream can keep one across its read loop.
    pub fn resolve_into(
        &mut self,
        new_chunk: Chunk,
        messages: &mut Vec<QuicMessage>,
    ) -> Result<(), CommandHeader> {
        self.resolve_arrived_into(new_chunk, None, messages)
    }

    /// Frames whatever messages the chunk completes, appending them to `messages`, and dates each
    /// header the chunk completes from `arrived`, when the chunk arrived.
    pub fn resolve_arrived_into(
        &mut self,
        new_chunk: Chunk,
        arrived: Option<Instant>,
        messages: &mut Vec<QuicMessage>,
    ) -> Result<(), CommandHeader> {
        self.arrived = arrived;
        self.distribute_chunk(new_chunk)?;

        // Oldest boundary first: framing one header hands the bytes past its message end to the
        // boundary after it, which then needs its own turn.
        let mut index = 0;
        while index < self.pending_boundaries.len() {
            if !self.pending_boundaries[index].is_dirty {
                index += 1;
                continue;
            }
            self.pending_boundaries[index].is_dirty = false;

            if self.pending_boundaries[index].header.is_none() {
                let header_size = self.header_size;
                self.pending_boundaries[index].absorb_queued_header_bytes(header_size);

                if self.pending_boundaries[index].header_bytes.len() < header_size {
                    index += 1;
                    continue;
                }
                self.frame_header(index)?;
            }

            match self.take_completed_message(index) {
                // Removing the boundary shifted the one after it into this index, so leaving the
                // index alone visits it next.
                Some(message) => messages.push(message),
                None => index += 1,
            }
        }

        self.report_metrics();

        Ok(())
    }

    /// Hands a chunk's bytes to the boundaries that own them, splitting it where it crosses a
    /// framed message's end.
    fn distribute_chunk(&mut self, mut new_chunk: Chunk) -> Result<(), CommandHeader> {
        let header_size = self.header_size;

        while !new_chunk.bytes.is_empty() {
            let Some(index) = self.boundary_for_offset(new_chunk.offset) else {
                // Bytes at an offset every boundary has already framed past. A conforming stream
                // never re-delivers consumed bytes, so the peer is fabricating offsets and nothing
                // further can be framed. There is no header to name, but the caller has to tear
                // the stream down rather than loop.
                return Err(CommandHeader::default());
            };

            self.pending_boundaries[index].is_dirty = true;

            // Bytes continuing this message's header go straight into its buffer, never entering
            // the chunk queue. The header is framed the moment it is whole, so the bytes after it
            // find the boundary they belong to on the next pass rather than having to be moved on.
            if self.pending_boundaries[index].header.is_none()
                && self.pending_boundaries[index].header_cursor() == new_chunk.offset
            {
                let taken = self.pending_boundaries[index]
                    .absorb_header_bytes(header_size, &mut new_chunk.bytes);
                new_chunk.offset += taken as u64;

                if self.pending_boundaries[index].header_bytes.len() == header_size {
                    self.frame_header(index)?;
                }
                continue;
            }

            let chunk_end = new_chunk.offset + new_chunk.bytes.len() as u64;
            let boundary = &mut self.pending_boundaries[index];

            match boundary.end_offset_exclusive(header_size) {
                Some(end) if end < chunk_end => {
                    // `boundary_for_offset` only yields a boundary ending past the offset, so this
                    // takes at least one byte and the remainder starts strictly later.
                    let head = new_chunk.bytes.split_to((end - new_chunk.offset) as usize);
                    boundary.push_chunk(Chunk {
                        offset: new_chunk.offset,
                        bytes: head,
                    });
                    new_chunk.offset = end;
                }
                _ => {
                    boundary.push_chunk(new_chunk);
                    break;
                }
            }
        }

        Ok(())
    }

    /// The boundary owning `offset`: the last one starting at or before it that has not already
    /// been framed past it.
    fn boundary_for_offset(&self, offset: u64) -> Option<usize> {
        let header_size = self.header_size;
        self.pending_boundaries
            .iter()
            .enumerate()
            .rev()
            .find(|(_, boundary)| {
                boundary.start_offset <= offset
                    && boundary
                        .end_offset_exclusive(header_size)
                        .is_none_or(|end| end > offset)
            })
            .map(|(index, _)| index)
    }

    /// Parses a boundary's completed header buffer and opens the boundary for the message after
    /// it, handing on any bytes already held past this message's end.
    fn frame_header(&mut self, index: usize) -> Result<(), CommandHeader> {
        let raw_header = &self.pending_boundaries[index].header_bytes;
        debug_assert_eq!(raw_header.len(), self.header_size);

        let header = if self.header_size == COMMAND_HEADER_SIZE_V4 {
            CommandHeader::from_bytes_v4(raw_header.as_slice())
        } else {
            CommandHeader::from_bytes(raw_header.as_slice())
        };
        // The cap bounds a payload allocation. An error header's size field is a status code, so
        // there is no payload for it to bound and a large status is not oversized.
        if !header.error && header.size_or_status > self.max_chunk_size {
            return Err(header);
        }

        lore_trace!("QUIC stream framed request header {:?}", header);

        self.pending_boundaries[index].header = Some(header);
        self.pending_boundaries[index].header_at = self.arrived;

        // Only the open boundary has an unframed header, and it is always the last, so the next
        // message starts where this one ends.
        debug_assert_eq!(index + 1, self.pending_boundaries.len());
        let end = self.pending_boundaries[index]
            .end_offset_exclusive(self.header_size)
            .expect("the header was just framed");
        let surplus = self.pending_boundaries[index].split_off_surplus(end);

        self.pending_boundaries
            .insert(index + 1, PendingBoundary::open_at(end));
        let next = &mut self.pending_boundaries[index + 1];
        for chunk in surplus {
            next.is_dirty = true;
            next.push_chunk(chunk);
        }

        Ok(())
    }

    /// Takes the message out of a boundary whose bytes have all arrived, or leaves the boundary in
    /// place while any are still missing.
    fn take_completed_message(&mut self, index: usize) -> Option<QuicMessage> {
        let header = self.pending_boundaries[index]
            .header
            .expect("the header is framed before a message is taken");
        let payload_size = payload_size(&header);

        if payload_size == 0 {
            let boundary = self.pending_boundaries.remove(index);
            return Some(QuicMessage {
                header,
                payload: None,
                header_at: boundary.header_at,
            });
        }

        if !self.pending_boundaries[index].holds_whole_payload(payload_size) {
            return None;
        }
        let payload = self.pending_boundaries[index].take_payload(payload_size as usize);
        let boundary = self.pending_boundaries.remove(index);

        lore_trace!("QUIC stream framed {} bytes of payload", payload_size);

        Some(QuicMessage {
            header,
            payload: Some(payload),
            header_at: boundary.header_at,
        })
    }

    /// Reports the chunks held behind a gap, matching what the serial assembler queues: bytes
    /// arriving in order are consumed as they land and are never counted, however many chunks a
    /// message is split across.
    fn report_metrics(&mut self) {
        let Some(metrics) = self.metrics.as_ref() else {
            return;
        };

        let pending: usize = self
            .pending_boundaries
            .iter()
            .map(PendingBoundary::stalled_chunks)
            .sum();
        if pending == self.reported_pending_chunks {
            return;
        }

        if self.reported_pending_chunks == 0 {
            self.stall_start = Instant::now();
        } else if pending == 0 {
            let _ = metrics.send(ChunkingMetric::Stall(self.stall_start.elapsed()));
        }

        let _ = metrics.send(ChunkingMetric::PendingChunks(pending));
        self.reported_pending_chunks = pending;
    }
}

/// An error header's size field carries a status code rather than a length, so no payload
/// follows it on the wire.
fn payload_size(header: &CommandHeader) -> u64 {
    if header.error {
        0
    } else {
        header.size_or_status as u64
    }
}
