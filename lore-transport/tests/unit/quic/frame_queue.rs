// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use bytes::Bytes;
use lore_transport::quic::frame_queue::FRAME_CHUNKS;
use lore_transport::quic::frame_queue::FrameChunks;
use lore_transport::quic::frame_queue::FrameQueue;
use lore_transport::quic::frame_queue::TakenBack;

/// The frames the queues of these tests keep room for.
const FRAMES_KEPT: usize = 16;

/// Queues on `queue` a frame with a header of four bytes of `id` and then `body`, with `id` as its
/// entry. Answers the frame's bytes.
fn queue_frame(queue: &mut FrameQueue<u32>, id: u32, body: &[&[u8]]) -> Vec<u8> {
    let header = Bytes::from(vec![id as u8; 4]);
    let mut body: Vec<Bytes> = body
        .iter()
        .map(|chunk| Bytes::copy_from_slice(chunk))
        .collect();
    let bytes = std::iter::once(&header)
        .chain(&body)
        .flatten()
        .copied()
        .collect();
    queue.push(FrameChunks::new(header, &mut body), id);
    bytes
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

/// Writes every frame `queue` holds to `stream`, `budget` bytes a write, as a stream's writer
/// does. Answers the entries of the frames written, in the order they were written in full.
fn write_all(queue: &mut FrameQueue<u32>, budget: usize, stream: &mut Vec<u8>) -> Vec<u32> {
    let mut written = Vec::new();
    for _ in 0..10_000 {
        if queue.is_empty() {
            return written;
        }
        write(queue.unwritten(), budget, stream);
        queue.pop_written(|id| written.push(id));
    }
    panic!("the queue was never written in full");
}

/// Queues three frames of three chunks, with the first written and taken out before the third is
/// queued, so that the third starts at the end of the queue's buffer and ends at its start.
/// Answers the bytes of the second and third.
fn queue_across_the_wrap(queue: &mut FrameQueue<u32>) -> Vec<u8> {
    let mut stream = Vec::new();
    queue_frame(queue, 1, &[b"first", b"one"]);
    let second = queue_frame(queue, 2, &[b"second", b"two"]);
    let first_len = queue.chunks.range(..3).map(Bytes::len).sum();
    write(queue.unwritten(), first_len, &mut stream);
    let mut popped = Vec::new();
    queue.pop_written(|id| popped.push(id));
    assert_eq!(popped, [1], "the first frame was not written whole");
    let third = queue_frame(queue, 3, &[b"third", b"three"]);
    assert!(
        !queue.chunks.as_slices().1.is_empty() && queue.chunks.as_slices().0.len() > 3,
        "the third frame does not span the end of the buffer: {:?}",
        queue.chunks.as_slices()
    );
    [second, third].concat()
}

mod frame_chunks {
    use super::*;

    /// A frame of a header and `FRAME_CHUNKS - 1` body chunks fills a queued frame without a copy.
    #[test]
    fn a_frame_of_exactly_the_chunks_it_holds_keeps_each_chunk() {
        let mut queue = FrameQueue::new(FRAMES_KEPT);
        let body: Vec<Vec<u8>> = (0..FRAME_CHUNKS - 1)
            .map(|index| vec![index as u8; 3])
            .collect();
        let body: Vec<&[u8]> = body.iter().map(Vec::as_slice).collect();
        let expected = queue_frame(&mut queue, 9, &body);

        assert_eq!(queue.chunks.len(), FRAME_CHUNKS);
        assert!(queue.chunks.iter().skip(1).all(|chunk| chunk.len() == 3));
        let mut stream = Vec::new();
        assert_eq!(write_all(&mut queue, usize::MAX, &mut stream), [9]);
        assert_eq!(stream, expected);
    }

    /// A frame of one body chunk more than a queued frame holds has its last two joined.
    #[test]
    fn a_frame_of_one_chunk_more_than_it_holds_joins_its_last_two() {
        let mut queue = FrameQueue::new(FRAMES_KEPT);
        let body: Vec<Vec<u8>> = (0..FRAME_CHUNKS)
            .map(|index| vec![index as u8; 3])
            .collect();
        let body: Vec<&[u8]> = body.iter().map(Vec::as_slice).collect();
        let expected = queue_frame(&mut queue, 9, &body);

        assert_eq!(queue.chunks.len(), FRAME_CHUNKS);
        assert_eq!(queue.chunks[FRAME_CHUNKS - 1].len(), 6);
        let mut stream = Vec::new();
        assert_eq!(write_all(&mut queue, usize::MAX, &mut stream), [9]);
        assert_eq!(stream, expected);
    }

    /// A frame of more chunks than a queued frame holds keeps every byte, in order.
    #[test]
    fn a_frame_of_more_chunks_than_it_holds_keeps_every_byte_in_order() {
        let mut queue = FrameQueue::new(FRAMES_KEPT);
        let body: Vec<Vec<u8>> = (0..FRAME_CHUNKS + 3)
            .map(|index| vec![index as u8; 3])
            .collect();
        let body: Vec<&[u8]> = body.iter().map(Vec::as_slice).collect();
        let expected = queue_frame(&mut queue, 9, &body);

        assert_eq!(queue.chunks.len(), FRAME_CHUNKS);
        let mut stream = Vec::new();
        assert_eq!(write_all(&mut queue, usize::MAX, &mut stream), [9]);
        assert_eq!(stream, expected);
    }
}

mod pop_written {
    use super::*;

    /// Frames queued across the end of the queue's buffer are written whole and in order, a few
    /// bytes a write, and each frame's entry is given back once it is written in full.
    #[test]
    fn frames_across_the_end_of_the_buffer_are_written_whole_and_in_order() {
        let mut queue = FrameQueue::new(FRAMES_KEPT);
        let expected = queue_across_the_wrap(&mut queue);

        let mut stream = Vec::new();
        let written = write_all(&mut queue, 3, &mut stream);

        assert_eq!(stream, expected);
        assert_eq!(written, [2, 3]);
    }

    /// A frame written in part is not given back.
    #[test]
    fn a_frame_written_in_part_stays_queued() {
        let mut queue = FrameQueue::new(FRAMES_KEPT);
        queue_frame(&mut queue, 1, &[b"payload"]);

        write(queue.unwritten(), 6, &mut Vec::new());
        let mut written = Vec::new();
        queue.pop_written(|id| written.push(id));

        assert!(written.is_empty());
        assert_eq!(queue.len(), 1);
    }

    /// A queue that grew past twice the frames it keeps room for gives back the rest once it
    /// empties.
    #[test]
    fn an_emptied_queue_gives_back_its_room_beyond_the_frames_it_keeps() {
        let mut queue = FrameQueue::new(FRAMES_KEPT);
        let body = [b"payload".as_slice(); FRAME_CHUNKS - 1];
        for id in 0..2 * FRAMES_KEPT as u32 + 1 {
            queue_frame(&mut queue, id, &body);
        }
        assert!(
            queue.frames.capacity() > 2 * FRAMES_KEPT
                && queue.chunks.capacity() > 2 * FRAMES_KEPT * FRAME_CHUNKS,
            "the queue did not grow past twice its room"
        );

        write_all(&mut queue, usize::MAX, &mut Vec::new());

        assert_eq!(queue.frames.capacity(), FRAMES_KEPT);
        assert_eq!(queue.chunks.capacity(), FRAMES_KEPT * FRAME_CHUNKS);
    }
}

mod fail {
    use super::*;

    /// Every frame queued, the one written in part included, is given back in order, and the
    /// queue is left empty.
    #[test]
    fn every_queued_frame_is_given_back_in_order() {
        let mut queue = FrameQueue::new(FRAMES_KEPT);
        for id in 1..=3 {
            queue_frame(&mut queue, id, &[b"payload"]);
        }
        write(queue.unwritten(), 2, &mut Vec::new());

        let mut failed = Vec::new();
        queue.fail(|id| failed.push(id));

        assert_eq!(failed, [1, 2, 3]);
        assert!(queue.is_empty());
        assert!(queue.chunks.is_empty());
    }
}

mod take_back {
    use super::*;

    /// A frame across the end of the buffer none of which is written is taken out whole, and the
    /// frames before and after it are written whole and in order.
    #[test]
    fn an_unwritten_frame_across_the_end_of_the_buffer_is_removed() {
        let mut queue = FrameQueue::new(FRAMES_KEPT);
        queue_across_the_wrap(&mut queue);
        let fourth = queue_frame(&mut queue, 4, &[b"fourth"]);
        let second: Vec<u8> = queue.chunks.range(..3).flatten().copied().collect();

        let removed = queue.take_back(|id| *id == 3);

        assert!(matches!(removed, TakenBack::Removed(3)));
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.chunks.len(), 3 + 2);
        let mut stream = Vec::new();
        assert_eq!(write_all(&mut queue, 3, &mut stream), [2, 4]);
        assert_eq!(stream, [second, fourth].concat());
    }

    /// A frame across the end of the buffer written in part is kept and finished, and the frames
    /// before and after it stay whole and in order.
    #[test]
    fn a_frame_written_in_part_across_the_end_of_the_buffer_is_finished() {
        let mut queue = FrameQueue::new(FRAMES_KEPT);
        let expected = queue_across_the_wrap(&mut queue);
        let fourth = queue_frame(&mut queue, 4, &[b"fourth"]);
        let second_len = queue.chunks.range(..3).map(Bytes::len).sum::<usize>();

        let mut stream = Vec::new();
        write(queue.unwritten(), second_len + 2, &mut stream);
        let mut written = Vec::new();
        queue.pop_written(|id| written.push(id));
        let started = queue.take_back(|id| *id == 3);

        assert!(matches!(started, TakenBack::Started(&mut 3)));
        written.extend(write_all(&mut queue, 3, &mut stream));
        assert_eq!(written, [2, 3, 4]);
        assert_eq!(stream, [expected, fourth].concat());
    }

    /// A frame the queue does not hold leaves the queue as it was.
    #[test]
    fn a_frame_not_queued_is_absent() {
        let mut queue = FrameQueue::new(FRAMES_KEPT);
        queue_frame(&mut queue, 1, &[b"payload"]);

        assert!(matches!(queue.take_back(|id| *id == 2), TakenBack::Absent));
        assert_eq!(queue.len(), 1);
    }
}
