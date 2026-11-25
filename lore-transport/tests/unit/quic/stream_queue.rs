// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use lore_transport::quic::QuicClientError;
use lore_transport::quic::client::QUEUE_FRAMES_KEPT;
use lore_transport::quic::client::QueuedFrame;
use lore_transport::quic::client::StreamQueue;
use lore_transport::quic::response_reader::PendingCommands;
use tokio::sync::oneshot;

type Written = oneshot::Receiver<Result<Bytes, QuicClientError>>;

/// Queues on `queue` the frame of the command `command_id`, a header of four bytes and then
/// `payload`, for a command that waits only for its frame to be written. Answers the channel that
/// says so, and the frame's bytes.
fn queue_frame(
    queue: &mut StreamQueue,
    pending: &PendingCommands,
    command_id: u32,
    payload: &[&[u8]],
) -> (Written, Vec<u8>) {
    let header = Bytes::from(vec![command_id as u8; 4]);
    let mut body: Vec<Bytes> = payload
        .iter()
        .map(|chunk| Bytes::copy_from_slice(chunk))
        .collect();
    let bytes = std::iter::once(&header)
        .chain(&body)
        .flatten()
        .copied()
        .collect();
    let (reply, written) = oneshot::channel();
    queue.push(
        QueuedFrame::new(header, &mut body, command_id, reply, false, None),
        pending,
        None,
    );
    (written, bytes)
}

/// Takes up to `budget` bytes from the front of `chunks`, emptying or shortening in place the
/// chunks it takes, as a write to a stream does, and appends them to `stream`.
fn write(chunks: &mut [Bytes], mut budget: usize, stream: &mut Vec<u8>) {
    for chunk in chunks {
        let taken = chunk.len().min(budget);
        stream.extend_from_slice(&chunk[..taken]);
        *chunk = chunk.slice(taken..);
        budget -= taken;
        if budget == 0 {
            return;
        }
    }
}

/// Writes every frame `queue` holds to `stream`, `budget` bytes a write, as the stream's writer
/// does.
fn write_all(
    queue: &mut StreamQueue,
    pending: &PendingCommands,
    budget: usize,
    inflight: &AtomicU64,
    stream: &mut Vec<u8>,
) {
    for _ in 0..10_000 {
        if queue.frames.is_empty() {
            return;
        }
        write(queue.frames.unwritten(), budget, stream);
        queue.pop_written(pending, inflight);
    }
    panic!("the queue was never written in full");
}

/// Queues three frames of three chunks, with the first written and taken out before the third is
/// queued, so that the third starts at the end of the queue's buffer and ends at its start.
fn queue_across_the_wrap(
    queue: &mut StreamQueue,
    pending: &PendingCommands,
    inflight: &AtomicU64,
) -> (Vec<Written>, Vec<u8>) {
    let mut stream = Vec::new();
    let (first, _) = queue_frame(queue, pending, 1, &[b"first", b"one"]);
    let (second, second_bytes) = queue_frame(queue, pending, 2, &[b"second", b"two"]);
    let first_len = queue.frames.chunks.range(..3).map(Bytes::len).sum();
    write(queue.frames.unwritten(), first_len, &mut stream);
    queue.pop_written(pending, inflight);
    assert_eq!(
        queue.frames.len(),
        1,
        "the first frame was not written whole"
    );
    let (third, third_bytes) = queue_frame(queue, pending, 3, &[b"third", b"three"]);
    assert!(
        !queue.frames.chunks.as_slices().1.is_empty()
            && queue.frames.chunks.as_slices().0.len() > 3,
        "the third frame does not span the end of the buffer: {:?}",
        queue.frames.chunks.as_slices()
    );
    (
        vec![first, second, third],
        [second_bytes, third_bytes].concat(),
    )
}

/// Frames queued across the end of the queue's buffer are written whole and in order, a few bytes
/// a write, and each command is completed once its frame is written.
#[test]
fn frames_across_the_end_of_the_queue_buffer_are_written_whole_and_in_order() {
    let pending = PendingCommands::new(Default::default());
    let inflight = AtomicU64::new(0);
    let mut queue = StreamQueue::default();
    let (mut written, expected) = queue_across_the_wrap(&mut queue, &pending, &inflight);

    let mut stream = Vec::new();
    write_all(&mut queue, &pending, 3, &inflight, &mut stream);

    assert_eq!(stream, expected);
    for written in &mut written {
        assert!(matches!(written.try_recv(), Ok(Ok(_))));
    }
}

/// A frame across the end of the queue's buffer whose command ends part way through its write has
/// its rest written, and the frames before and after it stay whole and in order.
#[test]
fn a_frame_taken_back_across_the_end_of_the_queue_buffer_has_its_rest_written() {
    let pending = PendingCommands::new(Default::default());
    let inflight = AtomicU64::new(0);
    let mut queue = StreamQueue::default();
    let (_written, expected_before) = queue_across_the_wrap(&mut queue, &pending, &inflight);
    let (_fourth, fourth_bytes) = queue_frame(&mut queue, &pending, 4, &[b"fourth"]);
    let second_len = queue
        .frames
        .chunks
        .range(..3)
        .map(Bytes::len)
        .sum::<usize>();

    let mut stream = Vec::new();
    write(queue.frames.unwritten(), second_len + 2, &mut stream);
    queue.pop_written(&pending, &inflight);
    queue.take_back(3, &pending, &inflight);
    assert_eq!(inflight.load(Ordering::Relaxed), 1);
    write_all(&mut queue, &pending, 3, &inflight, &mut stream);

    assert_eq!(stream, [expected_before, fourth_bytes].concat());
    assert_eq!(inflight.load(Ordering::Relaxed), 0);
}

/// A frame across the end of the queue's buffer whose command ends before any of it is written is
/// removed whole, and the frames before and after it are written whole and in order.
#[test]
fn a_frame_taken_back_unwritten_across_the_end_of_the_queue_buffer_is_removed() {
    let pending = PendingCommands::new(Default::default());
    let inflight = AtomicU64::new(0);
    let mut queue = StreamQueue::default();
    let (mut written, _) = queue_across_the_wrap(&mut queue, &pending, &inflight);
    let (_fourth, fourth_bytes) = queue_frame(&mut queue, &pending, 4, &[b"fourth"]);
    let second_bytes: Vec<u8> = queue.frames.chunks.range(..3).flatten().copied().collect();

    queue.take_back(3, &pending, &inflight);

    assert_eq!(queue.frames.len(), 2);
    assert_eq!(queue.frames.chunks.len(), 3 + 2);
    assert_eq!(inflight.load(Ordering::Relaxed), 0);
    let mut stream = Vec::new();
    write_all(&mut queue, &pending, 3, &inflight, &mut stream);
    assert_eq!(stream, [second_bytes, fourth_bytes].concat());
    assert!(!matches!(written[2].try_recv(), Ok(Ok(_))));
}

/// A client stream's queue that grew past twice the frames it keeps room for gives back the rest
/// once it empties, keeping room for [`QUEUE_FRAMES_KEPT`].
#[test]
fn an_emptied_queue_keeps_room_for_the_client_frames_kept() {
    let pending = PendingCommands::new(Default::default());
    let inflight = AtomicU64::new(0);
    let mut queue = StreamQueue::default();
    let _written: Vec<_> = (0..2 * QUEUE_FRAMES_KEPT as u32 + 1)
        .map(|command_id| queue_frame(&mut queue, &pending, command_id, &[b"payload"]))
        .collect();
    assert!(
        queue.frames.frames.capacity() > 2 * QUEUE_FRAMES_KEPT,
        "the queue did not grow past twice its room"
    );

    write_all(&mut queue, &pending, usize::MAX, &inflight, &mut Vec::new());

    assert_eq!(queue.frames.frames.capacity(), QUEUE_FRAMES_KEPT);
}
