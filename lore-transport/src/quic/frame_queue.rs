// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The frames a QUIC stream's writer task has taken and not yet written in full, kept so that
//! every frame queued at once goes out in as few `write_chunks` calls as flow control allows.

use std::collections::VecDeque;

use bytes::Bytes;

/// The most chunks a queued frame holds, its header included. A frame of more has the rest joined
/// into its last chunk.
#[lore_macro::test_pub]
const FRAME_CHUNKS: usize = 6;

/// A frame's chunks, its header first, in the first `count`.
#[lore_macro::test_pub]
pub struct FrameChunks {
    chunks: [Bytes; FRAME_CHUNKS],
    count: u8,
}

impl FrameChunks {
    /// The frame of `header` and then `body`, taking the chunks of `body`.
    pub fn new(header: Bytes, body: &mut [Bytes]) -> Self {
        let mut frame: [Bytes; FRAME_CHUNKS] = Default::default();
        frame[0] = header;
        let count = (1 + body.len()).min(FRAME_CHUNKS);
        for (slot, chunk) in frame[1..].iter_mut().zip(body.iter_mut()) {
            *slot = std::mem::take(chunk);
        }
        if 1 + body.len() > FRAME_CHUNKS {
            frame[FRAME_CHUNKS - 1] =
                joined(std::iter::once(&frame[FRAME_CHUNKS - 1]).chain(&body[FRAME_CHUNKS - 1..]));
        }
        Self {
            chunks: frame,
            count: count as u8,
        }
    }
}

/// A copy of `chunks`, one after another, in one chunk.
fn joined<'a>(chunks: impl Iterator<Item = &'a Bytes> + Clone) -> Bytes {
    let mut joined = Vec::with_capacity(chunks.clone().map(Bytes::len).sum());
    for chunk in chunks {
        joined.extend_from_slice(chunk);
    }
    Bytes::from(joined)
}

/// Frames waiting to be written to one stream, in the order they are written, each with an entry
/// `E` its writer gets back once the frame is written in full or failed. Each frame is written
/// whole before the next, so frames never interleave on the stream.
#[lore_macro::test_pub]
pub struct FrameQueue<E> {
    /// The chunks of `frames`, one frame's after another's. A write empties or shortens in place
    /// the chunks it takes.
    chunks: VecDeque<Bytes>,
    frames: VecDeque<QueuedEntry<E>>,
    /// The frames the queue keeps room for once it empties. A queue that grew past twice as many
    /// in a burst gives the rest back.
    frames_kept: usize,
}

/// A frame in a [`FrameQueue`].
#[lore_macro::test_pub]
struct QueuedEntry<E> {
    chunk_count: u8,
    /// The length of the frame's header, its first chunk, which keeps that length until a write
    /// takes some of the frame.
    header_size: u32,
    entry: E,
}

/// What [`FrameQueue::take_back`] found of a frame.
pub enum TakenBack<'a, E> {
    /// No frame in the queue matched.
    Absent,
    /// The frame had none of its bytes written, and was taken out.
    Removed(E),
    /// The frame was written in part. The queue keeps a copy of its rest to finish it, so that the
    /// next frame's header does not land inside its payload.
    Started(&'a mut E),
}

impl<E> FrameQueue<E> {
    /// An empty queue that keeps room for `frames_kept` frames once it empties after a burst.
    pub fn new(frames_kept: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            frames: VecDeque::new(),
            frames_kept,
        }
    }

    /// How many frames are not yet written in full.
    #[cfg(feature = "test-util")]
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Whether every frame queued is written in full.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Puts `frame` at the end of the queue, with `entry` to give back once it is written.
    pub fn push(&mut self, frame: FrameChunks, entry: E) {
        let FrameChunks { chunks, count } = frame;
        let header_size = chunks[0].len() as u32;
        self.chunks.extend(chunks.into_iter().take(count as usize));
        self.frames.push_back(QueuedEntry {
            chunk_count: count,
            header_size,
            entry,
        });
    }

    /// The chunks for the next write, those written included: the front part of the queue's
    /// buffer until all of it is written, then the part after it. A frame the buffer holds in both
    /// parts is written in two writes, which costs less than moving every chunk queued to make the
    /// buffer one part.
    pub fn unwritten(&mut self) -> &mut [Bytes] {
        let (front, back) = self.chunks.as_mut_slices();
        if front.iter().all(Bytes::is_empty) {
            back
        } else {
            front
        }
    }

    /// Takes out the frames at the front written in full, in order, handing each one's entry to
    /// `written`.
    pub fn pop_written(&mut self, mut written: impl FnMut(E)) {
        while let Some(frame) = self.frames.front()
            && self
                .chunks
                .range(..frame.chunk_count as usize)
                .all(Bytes::is_empty)
        {
            self.chunks.drain(..frame.chunk_count as usize);
            let Some(frame) = self.frames.pop_front() else {
                break;
            };
            written(frame.entry);
        }
        self.shrink_when_empty();
    }

    /// Takes out every frame, in order, handing each one's entry to `failed`, as when a write to
    /// the stream fails.
    pub fn fail(&mut self, mut failed: impl FnMut(E)) {
        for frame in self.frames.drain(..) {
            failed(frame.entry);
        }
        self.chunks.clear();
        self.shrink_when_empty();
    }

    /// Takes back the first frame whose entry `matches`, so that the queue holds none of the
    /// chunks it was given for it. A frame none of which is written is taken out whole; a frame
    /// written in part has its rest copied, so the queue still finishes it.
    pub fn take_back(&mut self, matches: impl Fn(&E) -> bool) -> TakenBack<'_, E> {
        let mut start = 0;
        let Some(index) = self.frames.iter().position(|frame| {
            let found = matches(&frame.entry);
            if !found {
                start += frame.chunk_count as usize;
            }
            found
        }) else {
            return TakenBack::Absent;
        };
        let frame = &self.frames[index];
        let end = start + frame.chunk_count as usize;
        if self.chunks[start].len() == frame.header_size as usize {
            self.chunks.drain(start..end);
            let Some(frame) = self.frames.remove(index) else {
                return TakenBack::Absent;
            };
            return TakenBack::Removed(frame.entry);
        }
        let rest = joined(self.chunks.range(start..end));
        for chunk in self.chunks.range_mut(start..end) {
            *chunk = Bytes::new();
        }
        self.chunks[start] = rest;
        TakenBack::Started(&mut self.frames[index].entry)
    }

    /// Gives back the room of an empty queue beyond `frames_kept` frames.
    fn shrink_when_empty(&mut self) {
        if !self.frames.is_empty() {
            return;
        }
        if self.frames.capacity() > 2 * self.frames_kept {
            self.frames.shrink_to(self.frames_kept);
        }
        if self.chunks.capacity() > 2 * self.frames_kept * FRAME_CHUNKS {
            self.chunks.shrink_to(self.frames_kept * FRAME_CHUNKS);
        }
    }
}
