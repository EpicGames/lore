// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use std::sync::mpsc::Sender;

use bytes::Bytes;
use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
use lore_transport::quic::QuicServiceError;
use lore_transport::quic::RESERVED_ERROR_CODE_START;
use lore_transport::quic::chunking::ChunkingMetric;
use lore_transport::quic::chunking::ParallelChunking;
use lore_transport::quic::chunking::QuicMessage;
use lore_transport::quic::chunking::SerialChunking;
use lore_transport::quic::chunking::ThresholdSerialChunking;
use lore_transport::quic::command_header::COMMAND_HEADER_SIZE;
use lore_transport::quic::command_header::COMMAND_HEADER_SIZE_V4;
use lore_transport::quic::command_header::CommandHeader;
use quinn::Chunk;

const MAX_CHUNK: usize = 64 * 1024;

fn header_size(v4: bool) -> usize {
    if v4 {
        COMMAND_HEADER_SIZE_V4
    } else {
        COMMAND_HEADER_SIZE
    }
}

/// One message as it is written onto the wire, paired with the payload the chunker owes back.
#[derive(Clone, Debug)]
struct WireMessage {
    header: CommandHeader,
    payload: Vec<u8>,
}

impl WireMessage {
    fn success(cmd: u8, command_id: u32, session_id: u32, v4: bool, payload: Vec<u8>) -> Self {
        WireMessage {
            header: CommandHeader {
                cmd,
                error: false,
                size_or_status: payload.len() as u32,
                command_id,
                session_id,
                v4,
            },
            payload,
        }
    }

    fn failure(cmd: u8, command_id: u32, session_id: u32, v4: bool, status: u32) -> Self {
        WireMessage {
            header: CommandHeader {
                cmd,
                error: true,
                size_or_status: status,
                command_id,
                session_id,
                v4,
            },
            payload: vec![],
        }
    }

    fn encode(&self, into: &mut Vec<u8>) {
        if self.header.v4 {
            into.extend_from_slice(&self.header.to_bytes_v4());
        } else {
            into.extend_from_slice(&self.header.to_bytes());
        }
        // On an error header the size field is a status code, so no payload follows it.
        if !self.header.error {
            into.extend_from_slice(&self.payload);
        }
    }
}

/// Message flattened for comparison. Derives `Ord` so a set of messages can be compared
/// without depending on the order the chunker emitted them in.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Flat {
    command_id: u32,
    cmd: u8,
    error: bool,
    size_or_status: u32,
    session_id: u32,
    v4: bool,
    payload: Option<Vec<u8>>,
}

fn flatten_expected(message: &WireMessage) -> Flat {
    let carries_payload = !message.header.error && message.header.size_or_status > 0;
    Flat {
        command_id: message.header.command_id,
        cmd: message.header.cmd,
        error: message.header.error,
        size_or_status: message.header.size_or_status,
        session_id: message.header.session_id,
        v4: message.header.v4,
        payload: carries_payload.then(|| message.payload.clone()),
    }
}

fn flatten_actual(message: &QuicMessage) -> Flat {
    Flat {
        command_id: message.header.command_id,
        cmd: message.header.cmd,
        error: message.header.error,
        size_or_status: message.header.size_or_status,
        session_id: message.header.session_id,
        v4: message.header.v4,
        payload: message.payload.as_ref().map(|payload| payload.to_vec()),
    }
}

fn into_chunks(stream: &[u8], chunk_size: usize) -> Vec<Chunk> {
    stream
        .chunks(chunk_size)
        .enumerate()
        .map(|(index, bytes)| Chunk {
            offset: (index * chunk_size) as u64,
            bytes: Bytes::copy_from_slice(bytes),
        })
        .collect()
}

#[derive(Clone, Copy, Debug)]
enum Arrival {
    InOrder,
    Reversed,
    OddsThenEvens,
    Shuffled,
}

const ARRIVALS: [Arrival; 4] = [
    Arrival::InOrder,
    Arrival::Reversed,
    Arrival::OddsThenEvens,
    Arrival::Shuffled,
];

/// Seed for the one permutation `Arrival::Shuffled` samples. Fixed, so the exhaustive sweep
/// stays reproducible.
const SHUFFLE_SEED: u64 = 0x2545_f491_4f6c_dd1d;

fn reorder(chunks: &mut Vec<Chunk>, arrival: Arrival) {
    match arrival {
        Arrival::InOrder => {}
        Arrival::Reversed => chunks.reverse(),
        Arrival::OddsThenEvens => {
            let mut slots: Vec<Option<Chunk>> = chunks.drain(..).map(Some).collect();
            for start in [1, 0] {
                let mut index = start;
                while index < slots.len() {
                    if let Some(chunk) = slots[index].take() {
                        chunks.push(chunk);
                    }
                    index += 2;
                }
            }
        }
        Arrival::Shuffled => {
            shuffle(&mut Chaos::new(SHUFFLE_SEED), chunks);
        }
    }
}

/// Lets one test drive both assemblers over the same input, rather than each assembler
/// carrying its own copy of the test.
trait ChunkAssembler {
    const NAME: &'static str;

    fn build(header_size: usize, max_chunk_size: usize) -> Self;

    fn build_with_metrics(
        header_size: usize,
        max_chunk_size: usize,
        metrics: Sender<ChunkingMetric>,
    ) -> Self;

    fn resolve_chunk(
        &mut self,
        chunk: Chunk,
        messages: &mut Vec<QuicMessage>,
    ) -> Result<(), CommandHeader>;
}

impl ChunkAssembler for SerialChunking {
    const NAME: &'static str = "serial";

    fn build(header_size: usize, max_chunk_size: usize) -> Self {
        SerialChunking::new(header_size, max_chunk_size, None)
    }

    fn build_with_metrics(
        header_size: usize,
        max_chunk_size: usize,
        metrics: Sender<ChunkingMetric>,
    ) -> Self {
        SerialChunking::new(header_size, max_chunk_size, Some(metrics))
    }

    fn resolve_chunk(
        &mut self,
        chunk: Chunk,
        messages: &mut Vec<QuicMessage>,
    ) -> Result<(), CommandHeader> {
        self.resolve_into(chunk, messages)
    }
}

impl ChunkAssembler for ParallelChunking {
    const NAME: &'static str = "parallel";

    fn build(header_size: usize, max_chunk_size: usize) -> Self {
        ParallelChunking::new(header_size, max_chunk_size, None)
    }

    fn build_with_metrics(
        header_size: usize,
        max_chunk_size: usize,
        metrics: Sender<ChunkingMetric>,
    ) -> Self {
        ParallelChunking::new(header_size, max_chunk_size, Some(metrics))
    }

    fn resolve_chunk(
        &mut self,
        chunk: Chunk,
        messages: &mut Vec<QuicMessage>,
    ) -> Result<(), CommandHeader> {
        self.resolve_into(chunk, messages)
    }
}

/// Chunks a boundary queues before it is split. Arbitrary - low enough that the test streams
/// actually cross it, so the split path is exercised rather than skipped.
const TEST_SPLIT_THRESHOLD: usize = 4;

impl ChunkAssembler for ThresholdSerialChunking {
    const NAME: &'static str = "threshold";

    fn build(header_size: usize, max_chunk_size: usize) -> Self {
        ThresholdSerialChunking::new(header_size, max_chunk_size, TEST_SPLIT_THRESHOLD, None)
    }

    fn build_with_metrics(
        header_size: usize,
        max_chunk_size: usize,
        metrics: Sender<ChunkingMetric>,
    ) -> Self {
        ThresholdSerialChunking::new(
            header_size,
            max_chunk_size,
            TEST_SPLIT_THRESHOLD,
            Some(metrics),
        )
    }

    fn resolve_chunk(
        &mut self,
        chunk: Chunk,
        messages: &mut Vec<QuicMessage>,
    ) -> Result<(), CommandHeader> {
        self.resolve_into(chunk, messages)
    }
}

/// `quinn::Chunk` is not `Clone`, and a `Bytes` clone is a refcount bump, so handing the same
/// input to a second assembler copies no payload bytes.
fn duplicate(chunks: &[Chunk]) -> Vec<Chunk> {
    chunks
        .iter()
        .map(|chunk| Chunk {
            offset: chunk.offset,
            bytes: chunk.bytes.clone(),
        })
        .collect()
}

/// One chunk's worth of output, for the tests that step chunk by chunk.
fn resolve_one<A: ChunkAssembler>(
    assembler: &mut A,
    chunk: Chunk,
) -> Result<Vec<QuicMessage>, CommandHeader> {
    let mut messages = Vec::new();
    assembler.resolve_chunk(chunk, &mut messages)?;
    Ok(messages)
}

fn feed<A: ChunkAssembler>(assembler: &mut A, chunks: Vec<Chunk>) -> Vec<QuicMessage> {
    let mut messages = vec![];
    for chunk in chunks {
        assembler
            .resolve_chunk(chunk, &mut messages)
            .expect("chunk accepted");
    }
    messages
}

fn assemble<A: ChunkAssembler>(
    header_size: usize,
    max_chunk_size: usize,
    chunks: Vec<Chunk>,
) -> Vec<Flat> {
    let mut assembler = A::build(header_size, max_chunk_size);
    let mut framed: Vec<Flat> = feed(&mut assembler, chunks)
        .iter()
        .map(flatten_actual)
        .collect();
    framed.sort();
    framed
}

/// Holds every assembler to the same set of messages for the same chunks. Emission order
/// differs between them, so each side is sorted.
fn assert_assemblers_agree(
    header_size: usize,
    max_chunk_size: usize,
    chunks: &[Chunk],
    expected: &[Flat],
    case: &str,
) {
    let serial = assemble::<SerialChunking>(header_size, max_chunk_size, duplicate(chunks));
    assert_same_messages(&serial, expected, &format!("{case}: serial"));

    let parallel = assemble::<ParallelChunking>(header_size, max_chunk_size, duplicate(chunks));
    assert_same_messages(&parallel, expected, &format!("{case}: parallel"));

    let threshold =
        assemble::<ThresholdSerialChunking>(header_size, max_chunk_size, duplicate(chunks));
    assert_same_messages(&threshold, expected, &format!("{case}: threshold"));
}

/// The expected set for one message, for the many tests that assemble exactly one.
fn one_message(message: &WireMessage) -> Vec<Flat> {
    vec![flatten_expected(message)]
}

fn encode_all(messages: &[WireMessage]) -> Vec<u8> {
    let mut stream = vec![];
    for message in messages {
        message.encode(&mut stream);
    }
    stream
}

/// Payload shapes that between them cover every branch the chunker takes: absent, a single
/// byte, exactly one header wide, wider than any single chunk under test, and repeated
/// payload-free messages back to back.
fn mixed_stream(v4: bool) -> Vec<WireMessage> {
    let session = |id: u32| if v4 { id } else { 0 };
    vec![
        WireMessage::success(1, 1, session(7), v4, vec![]),
        WireMessage::success(2, 2, session(7), v4, vec![0xaa]),
        WireMessage::failure(3, 3, session(7), v4, QuicServiceError::NotFound as u32),
        WireMessage::success(4, 4, session(9), v4, (0..=255u8).collect()),
        WireMessage::failure(5, 5, session(9), v4, QuicServiceError::SlowDown as u32),
        WireMessage::failure(6, 6, session(9), v4, RESERVED_ERROR_CODE_START),
        WireMessage::success(7, 7, session(0), v4, vec![0x5a; COMMAND_HEADER_SIZE_V4]),
        WireMessage::success(8, 8, session(1), v4, vec![]),
        WireMessage::success(9, 9, session(2), v4, vec![]),
        WireMessage::success(10, 10, session(3), v4, vec![0xff; 700]),
        WireMessage::success(11, 11, session(4), v4, vec![0x01, 0x02, 0x03]),
    ]
}

/// Compares two sets of messages already sorted by command id, reporting the first field that
/// differs rather than dumping whole payloads.
fn assert_same_messages(actual: &[Flat], expected: &[Flat], case: &str) {
    if actual.len() != expected.len() {
        let got: std::collections::HashSet<u32> =
            actual.iter().map(|flat| flat.command_id).collect();
        let missing: Vec<u32> = expected
            .iter()
            .map(|flat| flat.command_id)
            .filter(|id| !got.contains(id))
            .collect();
        panic!(
            "{case}: got {} of {} messages; missing ids {:?}",
            actual.len(),
            expected.len(),
            &missing[..missing.len().min(40)]
        );
    }

    for (got, want) in actual.iter().zip(expected) {
        assert_eq!(got.command_id, want.command_id, "{case}: command id");

        let command_id = want.command_id;
        assert_eq!(got.cmd, want.cmd, "{case}: command {command_id} opcode");
        assert_eq!(got.error, want.error, "{case}: command {command_id} error");
        assert_eq!(
            got.size_or_status, want.size_or_status,
            "{case}: command {command_id} size or status"
        );
        assert_eq!(
            got.session_id, want.session_id,
            "{case}: command {command_id} session id"
        );
        assert_eq!(got.v4, want.v4, "{case}: command {command_id} header width");
        assert_eq!(
            got.payload.is_some(),
            want.payload.is_some(),
            "{case}: command {command_id} payload presence"
        );

        if let (Some(got_payload), Some(want_payload)) = (&got.payload, &want.payload) {
            assert_eq!(
                got_payload.len(),
                want_payload.len(),
                "{case}: command {command_id} payload length"
            );

            let first_difference = got_payload
                .iter()
                .zip(want_payload)
                .position(|(got_byte, want_byte)| got_byte != want_byte);
            assert!(
                first_difference.is_none(),
                "{case}: command {command_id} payload differs at byte {first_difference:?}"
            );
        }
    }
}

/// xorshift64*, so a chaos case reproduces exactly from the seed the failure reports.
struct Chaos(u64);

impl Chaos {
    fn new(seed: u64) -> Self {
        // A zero state is a fixed point for xorshift and would emit nothing but zeroes.
        Chaos(if seed == 0 { 1 } else { seed })
    }

    fn next(&mut self) -> u64 {
        let mut state = self.0;
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        self.0 = state;
        state.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() >> 33) as usize % bound
    }
}

/// Fisher-Yates. Shared so a fix reaches both the fixed-seed arrival and the chaos one.
fn shuffle(chaos: &mut Chaos, chunks: &mut [Chunk]) {
    for index in (1..chunks.len()).rev() {
        chunks.swap(index, chaos.below(index + 1));
    }
}

/// Randomized message mixture. Command ids are sequential and unique so the sorted comparison
/// pairs each emitted message with the one it was built from.
fn chaos_messages(
    chaos: &mut Chaos,
    count: usize,
    v4: bool,
    max_payload: usize,
) -> Vec<WireMessage> {
    (0..count)
        .map(|index| {
            let command_id = index as u32 + 1;
            let cmd = chaos.below(256) as u8;
            let session_id = if v4 { chaos.next() as u32 } else { 0 };

            if chaos.below(4) == 0 {
                let status = chaos.below(300) as u32;
                return WireMessage::failure(cmd, command_id, session_id, v4, status);
            }

            // Weighted towards the small sizes, where the header and payload boundaries
            // interleave most densely, while still reaching payloads spanning many chunks.
            let size = match chaos.below(8) {
                0 => 0,
                1 => 1,
                2 => chaos.below(16),
                _ => chaos.below(max_payload),
            };
            let payload = (0..size).map(|_| chaos.next() as u8).collect();
            WireMessage::success(cmd, command_id, session_id, v4, payload)
        })
        .collect()
}

/// Cuts the stream at random boundaries, each chunk between one byte and `read_cap` - the
/// bound production reads under.
fn into_random_chunks(chaos: &mut Chaos, stream: &[u8], read_cap: usize) -> Vec<Chunk> {
    let mut chunks = vec![];
    let mut offset = 0;

    while offset < stream.len() {
        let size = 1 + chaos.below(read_cap.min(stream.len() - offset));
        let end = offset + size;
        chunks.push(Chunk {
            offset: offset as u64,
            bytes: Bytes::copy_from_slice(&stream[offset..end]),
        });
        offset = end;
    }

    chunks
}

#[derive(Clone, Copy, Debug)]
enum ChaosArrival {
    InOrder,
    /// Swaps only within a short window, the reordering a real network produces.
    NearbySwaps,
    FullShuffle,
}

const CHAOS_SEEDS: [u64; 8] = [
    0x0000_0000_0000_0001,
    0xdead_beef_cafe_f00d,
    0x1234_5678_9abc_def0,
    0xffff_ffff_ffff_ffff,
    0x5555_aaaa_5555_aaaa,
    0x0f1e_2d3c_4b5a_6978,
    0x8000_0000_0000_0000,
    0x0123_4567_89ab_cdef,
];

fn deliver(chaos: &mut Chaos, chunks: &mut [Chunk], arrival: ChaosArrival) {
    match arrival {
        ChaosArrival::InOrder => {}
        ChaosArrival::NearbySwaps => {
            for index in 0..chunks.len() {
                let reach = (chunks.len() - index).min(4);
                let target = index + chaos.below(reach);
                chunks.swap(index, target);
            }
        }
        ChaosArrival::FullShuffle => shuffle(chaos, chunks),
    }
}

mod serial_chunking_new {
    use super::*;

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "header_size <= COMMAND_HEADER_SIZE_V4")]
    fn header_wider_than_the_staging_buffer_is_rejected() {
        SerialChunking::new(COMMAND_HEADER_SIZE_V4 + 1, MAX_CHUNK, None);
    }

    #[test]
    fn widest_supported_header_is_accepted() {
        let mut chunker = SerialChunking::new(COMMAND_HEADER_SIZE_V4, MAX_CHUNK, None);
        let message = WireMessage::success(1, 1, 5, true, vec![0x01]);
        let mut stream = vec![];
        message.encode(&mut stream);

        let messages = feed(&mut chunker, into_chunks(&stream, stream.len()));

        assert_eq!(
            messages.iter().map(flatten_actual).collect::<Vec<_>>(),
            vec![flatten_expected(&message)]
        );
    }
}

mod resolve {
    use super::*;

    /// The headline property: both assemblers reproduce exactly the set of messages written
    /// onto the wire, for every chunk size from one byte upwards and every arrival order.
    #[test]
    fn mixed_stream_round_trips_under_every_split_and_arrival_order() {
        for v4 in [false, true] {
            let messages = mixed_stream(v4);
            let stream = encode_all(&messages);

            let mut expected: Vec<Flat> = messages.iter().map(flatten_expected).collect();
            expected.sort();

            for chunk_size in 1..=stream.len() {
                for arrival in ARRIVALS {
                    let mut chunks = into_chunks(&stream, chunk_size);
                    reorder(&mut chunks, arrival);

                    assert_assemblers_agree(
                        header_size(v4),
                        MAX_CHUNK,
                        &chunks,
                        &expected,
                        &format!("v4 {v4}, chunk size {chunk_size}, {arrival:?}"),
                    );
                }
            }
        }
    }

    /// Chaos at volume: a few thousand messages of randomized shape per case, cut at random
    /// chunk boundaries and delivered with the local reordering a real network produces. The
    /// seed is reported on failure so a case reproduces.
    #[test]
    fn chaos_large_mixture_reconstructs_every_message() {
        const MESSAGE_COUNT: usize = 3000;
        const MAX_PAYLOAD: usize = 8192;
        const READ_CAP: usize = 1400;
        const CHUNK_CAP: usize = 16384;

        for seed in CHAOS_SEEDS {
            for v4 in [false, true] {
                let mut chaos = Chaos::new(seed);
                let messages = chaos_messages(&mut chaos, MESSAGE_COUNT, v4, MAX_PAYLOAD);
                let stream = encode_all(&messages);

                let mut expected: Vec<Flat> = messages.iter().map(flatten_expected).collect();
                expected.sort();

                for arrival in [ChaosArrival::InOrder, ChaosArrival::NearbySwaps] {
                    let mut chunks = into_random_chunks(&mut chaos, &stream, READ_CAP);
                    deliver(&mut chaos, &mut chunks, arrival);

                    assert_assemblers_agree(
                        header_size(v4),
                        CHUNK_CAP,
                        &chunks,
                        &expected,
                        &format!(
                            "seed {seed:#018x}, v4 {v4}, {arrival:?}, {} bytes",
                            stream.len()
                        ),
                    );
                }
            }
        }
    }

    /// The same property under total reordering, where every chunk but the first has to be
    /// queued before anything can be assembled. Smaller than the volume case above because
    /// both assemblers rescan what they are holding as each chunk arrives.
    #[test]
    fn chaos_full_shuffle_reconstructs_every_message() {
        const MESSAGE_COUNT: usize = 1200;
        const MAX_PAYLOAD: usize = 2048;
        const READ_CAP: usize = 256;
        const CHUNK_CAP: usize = 4096;

        for seed in CHAOS_SEEDS {
            for v4 in [false, true] {
                let mut chaos = Chaos::new(seed);
                let messages = chaos_messages(&mut chaos, MESSAGE_COUNT, v4, MAX_PAYLOAD);
                let stream = encode_all(&messages);

                let mut chunks = into_random_chunks(&mut chaos, &stream, READ_CAP);
                deliver(&mut chaos, &mut chunks, ChaosArrival::FullShuffle);

                let mut expected: Vec<Flat> = messages.iter().map(flatten_expected).collect();
                expected.sort();

                assert_assemblers_agree(
                    header_size(v4),
                    CHUNK_CAP,
                    &chunks,
                    &expected,
                    &format!("seed {seed:#018x}, v4 {v4}, {} bytes", stream.len()),
                );
            }
        }
    }

    /// Payload sizes that land exactly on a chunk boundary, and payloads large enough to span
    /// many chunks - up to the fragment maximum a real payload can reach. Delivered out of
    /// order, so a long run of chunks has to be held before any of it can be framed.
    #[test]
    fn boundary_and_large_payloads_reassemble_out_of_order() {
        const READ_CAP: usize = 1400;

        let sizes = [
            0,
            1,
            COMMAND_HEADER_SIZE,
            255,
            256,
            READ_CAP - 1,
            READ_CAP,
            READ_CAP + 1,
            2 * READ_CAP,
            2 * READ_CAP + 1,
            65_535,
            65_536,
            65_537,
            FRAGMENT_SIZE_THRESHOLD - 1,
            FRAGMENT_SIZE_THRESHOLD,
        ];

        for v4 in [false, true] {
            let messages: Vec<WireMessage> = sizes
                .iter()
                .enumerate()
                .map(|(index, &size)| {
                    WireMessage::success(
                        index as u8,
                        index as u32 + 1,
                        if v4 { 7 } else { 0 },
                        v4,
                        vec![0xa5; size],
                    )
                })
                .collect();
            let stream = encode_all(&messages);

            let mut expected: Vec<Flat> = messages.iter().map(flatten_expected).collect();
            expected.sort();

            for arrival in [
                ChaosArrival::InOrder,
                ChaosArrival::NearbySwaps,
                ChaosArrival::FullShuffle,
            ] {
                for seed in [CHAOS_SEEDS[0], CHAOS_SEEDS[3]] {
                    let mut chaos = Chaos::new(seed);
                    let mut chunks = into_random_chunks(&mut chaos, &stream, READ_CAP);
                    deliver(&mut chaos, &mut chunks, arrival);

                    assert_assemblers_agree(
                        header_size(v4),
                        FRAGMENT_SIZE_THRESHOLD,
                        &chunks,
                        &expected,
                        &format!(
                            "v4 {v4}, {arrival:?}, seed {seed:#018x}, {} chunks",
                            chunks.len()
                        ),
                    );
                }
            }
        }
    }

    #[test]
    fn message_and_payload_arriving_in_one_chunk() {
        let message = WireMessage::success(9, 4, 0, false, vec![0xde, 0xad, 0xbe, 0xef]);
        let stream = encode_all(std::slice::from_ref(&message));

        assert_assemblers_agree(
            COMMAND_HEADER_SIZE,
            MAX_CHUNK,
            &into_chunks(&stream, stream.len()),
            &one_message(&message),
            "whole message in one chunk",
        );
    }

    #[test]
    fn zero_length_payload_yields_no_payload() {
        let message = WireMessage::success(3, 1, 0, false, vec![]);
        let stream = encode_all(std::slice::from_ref(&message));

        assert_assemblers_agree(
            COMMAND_HEADER_SIZE,
            MAX_CHUNK,
            &into_chunks(&stream, stream.len()),
            &one_message(&message),
            "zero length payload",
        );
    }

    /// An error header's size field is a status code, so the assembler must not read payload
    /// bytes for it - the next bytes on the wire belong to the following message.
    #[test]
    fn error_header_yields_its_status_and_consumes_no_payload() {
        let failed = WireMessage::failure(2, 1, 0, false, QuicServiceError::NotFound as u32);
        let followed = WireMessage::success(3, 2, 0, false, vec![0x77; 5]);
        let messages = [failed, followed];
        let stream = encode_all(&messages);

        let expected: Vec<Flat> = messages.iter().map(flatten_expected).collect();

        assert_assemblers_agree(
            COMMAND_HEADER_SIZE,
            MAX_CHUNK,
            &into_chunks(&stream, stream.len()),
            &expected,
            "error header followed by a message",
        );
    }

    #[test]
    fn header_split_one_byte_per_chunk() {
        fn check<A: ChunkAssembler>(message: &WireMessage, stream: &[u8]) {
            let mut assembler = A::build(COMMAND_HEADER_SIZE, MAX_CHUNK);
            let chunks = into_chunks(stream, 1);
            let (last, leading) = chunks.split_last().expect("header splits into chunks");

            for chunk in leading {
                let partial = resolve_one(
                    &mut assembler,
                    Chunk {
                        offset: chunk.offset,
                        bytes: chunk.bytes.clone(),
                    },
                )
                .expect("partial header accepted");
                assert!(
                    partial.is_empty(),
                    "{}: no message before the header completes",
                    A::NAME
                );
            }

            let framed = resolve_one(
                &mut assembler,
                Chunk {
                    offset: last.offset,
                    bytes: last.bytes.clone(),
                },
            )
            .expect("final header byte accepted");

            assert_eq!(
                framed.iter().map(flatten_actual).collect::<Vec<_>>(),
                one_message(message),
                "{}",
                A::NAME
            );
        }

        let message = WireMessage::success(5, 3, 0, false, vec![]);
        let stream = encode_all(std::slice::from_ref(&message));

        check::<SerialChunking>(&message, &stream);
        check::<ParallelChunking>(&message, &stream);
        check::<ThresholdSerialChunking>(&message, &stream);
    }

    /// Payload bytes are concatenated in stream order even though message emission order is
    /// not part of either assembler's contract.
    #[test]
    fn payload_split_across_chunks_keeps_its_byte_order() {
        let message = WireMessage::success(6, 1, 0, false, (0..=255u8).collect());
        let stream = encode_all(std::slice::from_ref(&message));

        assert_assemblers_agree(
            COMMAND_HEADER_SIZE,
            MAX_CHUNK,
            &into_chunks(&stream, 7),
            &one_message(&message),
            "payload split across chunks",
        );
    }

    #[test]
    fn several_messages_in_one_chunk_are_all_emitted() {
        let messages = [
            WireMessage::success(1, 1, 0, false, vec![0x01]),
            WireMessage::success(2, 2, 0, false, vec![]),
            WireMessage::failure(3, 3, 0, false, QuicServiceError::Failed as u32),
            WireMessage::success(4, 4, 0, false, vec![0x02, 0x03]),
        ];
        let stream = encode_all(&messages);

        let expected: Vec<Flat> = messages.iter().map(flatten_expected).collect();

        assert_assemblers_agree(
            COMMAND_HEADER_SIZE,
            MAX_CHUNK,
            &into_chunks(&stream, stream.len()),
            &expected,
            "four messages in one chunk",
        );
    }

    /// With the message's own header missing, neither assembler can frame anything, however
    /// much of the payload has arrived.
    #[test]
    fn a_chunk_past_the_gap_is_held_until_the_gap_is_filled() {
        fn check<A: ChunkAssembler>(message: &WireMessage, stream: &[u8]) {
            let mut assembler = A::build(COMMAND_HEADER_SIZE, MAX_CHUNK);
            let chunks = into_chunks(stream, 8);
            let (first, rest) = chunks.split_first().expect("stream splits into chunks");

            for chunk in rest {
                let held = resolve_one(
                    &mut assembler,
                    Chunk {
                        offset: chunk.offset,
                        bytes: chunk.bytes.clone(),
                    },
                )
                .expect("out of order chunk accepted");
                assert!(
                    held.is_empty(),
                    "{}: nothing emitted while offset 0 is missing",
                    A::NAME
                );
            }

            let framed = resolve_one(
                &mut assembler,
                Chunk {
                    offset: first.offset,
                    bytes: first.bytes.clone(),
                },
            )
            .expect("gap-filling chunk accepted");

            assert_eq!(
                framed.iter().map(flatten_actual).collect::<Vec<_>>(),
                one_message(message),
                "{}",
                A::NAME
            );
        }

        let message = WireMessage::success(1, 1, 0, false, vec![0x42; 24]);
        let stream = encode_all(std::slice::from_ref(&message));

        check::<SerialChunking>(&message, &stream);
        check::<ParallelChunking>(&message, &stream);
        check::<ThresholdSerialChunking>(&message, &stream);
    }

    /// The cap bounds a payload allocation, and an error header carries a status code in place of
    /// a length, so a status above the cap does not make the message oversized.
    #[test]
    fn an_error_status_above_the_chunk_cap_is_accepted() {
        const CAP: usize = 64;
        let message = WireMessage::failure(3, 5, 0, false, RESERVED_ERROR_CODE_START);
        let stream = encode_all(std::slice::from_ref(&message));

        assert_assemblers_agree(
            COMMAND_HEADER_SIZE,
            CAP,
            &into_chunks(&stream, stream.len()),
            &one_message(&message),
            "error status above the cap",
        );
    }

    #[test]
    fn payload_larger_than_the_chunk_cap_is_rejected() {
        fn check<A: ChunkAssembler>(stream: &[u8], cap: usize) {
            let mut assembler = A::build(COMMAND_HEADER_SIZE, cap);
            let rejected = resolve_one(
                &mut assembler,
                Chunk {
                    offset: 0,
                    bytes: Bytes::copy_from_slice(stream),
                },
            )
            .expect_err("payload over the cap is rejected");

            assert_eq!(rejected.command_id, 12, "{}", A::NAME);
            assert_eq!(rejected.cmd, 7, "{}", A::NAME);
            assert_eq!(rejected.size_or_status, cap as u32 + 1, "{}", A::NAME);
        }

        const CAP: usize = 64;
        let stream = encode_all(&[WireMessage::success(7, 12, 0, false, vec![0x00; CAP + 1])]);

        check::<SerialChunking>(&stream, CAP);
        check::<ParallelChunking>(&stream, CAP);
        check::<ThresholdSerialChunking>(&stream, CAP);
    }

    #[test]
    fn payload_exactly_at_the_chunk_cap_is_accepted() {
        const CAP: usize = 64;
        let message = WireMessage::success(7, 12, 0, false, vec![0x31; CAP]);
        let stream = encode_all(std::slice::from_ref(&message));

        assert_assemblers_agree(
            COMMAND_HEADER_SIZE,
            CAP,
            &into_chunks(&stream, 16),
            &one_message(&message),
            "payload exactly at the cap",
        );
    }

    #[test]
    fn wide_header_carries_the_session_id() {
        let message = WireMessage::success(1, 1, 0xabcd, true, vec![0x09]);
        let stream = encode_all(std::slice::from_ref(&message));

        assert_assemblers_agree(
            COMMAND_HEADER_SIZE_V4,
            MAX_CHUNK,
            &into_chunks(&stream, stream.len()),
            &one_message(&message),
            "wide header",
        );
    }

    #[test]
    fn narrow_header_reports_no_session_id() {
        let message = WireMessage::success(1, 1, 0, false, vec![0x09]);
        let stream = encode_all(std::slice::from_ref(&message));

        assert_assemblers_agree(
            COMMAND_HEADER_SIZE,
            MAX_CHUNK,
            &into_chunks(&stream, stream.len()),
            &one_message(&message),
            "narrow header",
        );
    }

    #[test]
    fn out_of_order_arrival_reports_the_queue_depth_and_one_stall() {
        fn check<A: ChunkAssembler>(stream: &[u8]) {
            let (sender, receiver) = std::sync::mpsc::channel();
            let mut assembler = A::build_with_metrics(COMMAND_HEADER_SIZE, MAX_CHUNK, sender);

            let chunks = into_chunks(stream, 8);
            let (first, rest) = chunks.split_first().expect("stream splits into chunks");
            for chunk in rest {
                resolve_one(
                    &mut assembler,
                    Chunk {
                        offset: chunk.offset,
                        bytes: chunk.bytes.clone(),
                    },
                )
                .expect("out of order chunk accepted");
            }
            resolve_one(
                &mut assembler,
                Chunk {
                    offset: first.offset,
                    bytes: first.bytes.clone(),
                },
            )
            .expect("gap-filling chunk accepted");

            let mut depths = vec![];
            let mut stalls = 0;
            while let Ok(metric) = receiver.try_recv() {
                match metric {
                    ChunkingMetric::PendingChunks(depth) => depths.push(depth),
                    ChunkingMetric::Stall(_) => stalls += 1,
                }
            }

            assert_eq!(depths, vec![1, 2, 3, 0], "{}", A::NAME);
            assert_eq!(
                stalls,
                1,
                "{}: one stall spanning the whole queued period",
                A::NAME
            );
        }

        let stream = encode_all(&[WireMessage::success(1, 1, 0, false, vec![0x11; 24])]);

        check::<SerialChunking>(&stream);
        check::<ParallelChunking>(&stream);
    }

    #[test]
    fn in_order_arrival_reports_no_metrics() {
        fn check<A: ChunkAssembler>(stream: &[u8]) {
            let (sender, receiver) = std::sync::mpsc::channel();
            let mut assembler = A::build_with_metrics(COMMAND_HEADER_SIZE, MAX_CHUNK, sender);

            feed(&mut assembler, into_chunks(stream, 8));

            assert!(
                receiver.try_recv().is_err(),
                "{}: no stall or queue to report",
                A::NAME
            );
        }

        let stream = encode_all(&[WireMessage::success(1, 1, 0, false, vec![0x11; 24])]);

        check::<SerialChunking>(&stream);
        check::<ParallelChunking>(&stream);
    }

    /// Behaviour that only appears once a run has split, so it has no counterpart in the
    /// serial assembler it is built from.
    mod threshold_only {
        use super::*;

        /// A gap inside the message a run is already waiting on gives a second run nothing to
        /// frame, so no split happens however many chunks queue up behind it.
        #[test]
        fn a_gap_within_the_waiting_message_opens_no_further_runs() {
            const CHUNK: usize = 8;

            // One message long enough that every chunk after the gap still falls inside it.
            let message = WireMessage::success(1, 1, 0, false, vec![0xaa; CHUNK * 40]);
            let stream = encode_all(std::slice::from_ref(&message));

            let mut chunker = ThresholdSerialChunking::new(
                COMMAND_HEADER_SIZE,
                MAX_CHUNK,
                TEST_SPLIT_THRESHOLD,
                None,
            );

            let chunks = into_chunks(&stream, CHUNK);
            let (header_chunk, rest) = chunks.split_first().expect("stream splits into chunks");
            let (gap_chunk, after_gap) = rest.split_first().expect("a chunk follows the header");

            resolve_one(
                &mut chunker,
                Chunk {
                    offset: header_chunk.offset,
                    bytes: header_chunk.bytes.clone(),
                },
            )
            .expect("header chunk accepted");
            for chunk in after_gap {
                resolve_one(
                    &mut chunker,
                    Chunk {
                        offset: chunk.offset,
                        bytes: chunk.bytes.clone(),
                    },
                )
                .expect("out of order chunk accepted");
            }

            assert_eq!(
                chunker.open_run_count(),
                1,
                "{} chunks queued behind a gap inside one message opened extra runs",
                after_gap.len()
            );

            let framed = resolve_one(
                &mut chunker,
                Chunk {
                    offset: gap_chunk.offset,
                    bytes: gap_chunk.bytes.clone(),
                },
            )
            .expect("gap-filling chunk accepted");
            assert_eq!(
                framed.iter().map(flatten_actual).collect::<Vec<_>>(),
                vec![flatten_expected(&message)]
            );
        }

        /// Once enough chunks pile up behind a gap the run splits, and the messages after the
        /// stalled one are framed without waiting for it. Below the threshold the assembler is
        /// serial and blocks exactly as the serial one does.
        #[test]
        fn a_split_run_frames_messages_past_an_incomplete_earlier_one() {
            const CHUNK: usize = 8;

            let blocked = WireMessage::success(1, 1, 0, false, vec![0xaa; 32]);
            let following: Vec<WireMessage> = (2..=6)
                .map(|id| WireMessage::success(2, id, 0, false, vec![]))
                .collect();

            let mut messages = vec![blocked.clone()];
            messages.extend(following.iter().cloned());
            let stream = encode_all(&messages);

            let chunks = into_chunks(&stream, CHUNK);
            let (header_chunk, rest) = chunks.split_first().expect("stream splits into chunks");
            let (gap_chunk, after_gap) = rest.split_first().expect("a chunk follows the header");

            let mut chunker = ThresholdSerialChunking::new(
                COMMAND_HEADER_SIZE,
                MAX_CHUNK,
                TEST_SPLIT_THRESHOLD,
                None,
            );
            let mut serial = SerialChunking::new(COMMAND_HEADER_SIZE, MAX_CHUNK, None);

            let mut deliver = vec![Chunk {
                offset: header_chunk.offset,
                bytes: header_chunk.bytes.clone(),
            }];
            deliver.extend(after_gap.iter().map(|chunk| Chunk {
                offset: chunk.offset,
                bytes: chunk.bytes.clone(),
            }));

            let framed = feed(&mut chunker, duplicate(&deliver));
            let mut framed: Vec<Flat> = framed.iter().map(flatten_actual).collect();
            framed.sort();
            let mut expected: Vec<Flat> = following.iter().map(flatten_expected).collect();
            expected.sort();
            assert_same_messages(&framed, &expected, "past the gap");

            assert!(
                feed(&mut serial, duplicate(&deliver)).is_empty(),
                "the serial assembler blocks on the missing chunk"
            );

            // The blocked message is framed once its missing bytes arrive.
            let completed = resolve_one(
                &mut chunker,
                Chunk {
                    offset: gap_chunk.offset,
                    bytes: gap_chunk.bytes.clone(),
                },
            )
            .expect("gap-filling chunk accepted");
            assert_eq!(
                completed.iter().map(flatten_actual).collect::<Vec<_>>(),
                vec![flatten_expected(&blocked)]
            );
        }
    }

    /// `ThresholdSerialChunking` defines a stall as a run of chunks that frame nothing, so its
    /// metrics are not comparable with the other assemblers' and are held on their own.
    mod threshold_metrics {
        use super::*;

        fn drain(receiver: &std::sync::mpsc::Receiver<ChunkingMetric>) -> (Vec<usize>, usize) {
            let mut pending = vec![];
            let mut stalls = 0;
            while let Ok(metric) = receiver.try_recv() {
                match metric {
                    ChunkingMetric::PendingChunks(count) => pending.push(count),
                    ChunkingMetric::Stall(_) => stalls += 1,
                }
            }
            (pending, stalls)
        }

        /// Chunks arriving in order are consumed as they land, so a message spanning many of
        /// them never stalls.
        #[test]
        fn an_in_order_multi_chunk_message_reports_no_stall() {
            let (sender, receiver) = std::sync::mpsc::channel();
            let mut chunker = ThresholdSerialChunking::new(
                COMMAND_HEADER_SIZE,
                MAX_CHUNK,
                TEST_SPLIT_THRESHOLD,
                Some(sender),
            );
            let stream = encode_all(std::slice::from_ref(&WireMessage::success(
                1,
                1,
                0,
                false,
                vec![0x11; 24],
            )));

            let framed = feed(&mut chunker, into_chunks(&stream, 8));

            assert_eq!(framed.len(), 1);
            let (pending, stalls) = drain(&receiver);
            assert_eq!(stalls, 0, "nothing was ever held behind a gap");
            assert_eq!(
                pending.last(),
                Some(&0),
                "nothing is part-way through once the message is framed"
            );
        }

        /// A chunk carrying whole messages frames on every arrival, so nothing stalls.
        #[test]
        fn whole_messages_per_chunk_report_no_stall() {
            let (sender, receiver) = std::sync::mpsc::channel();
            let mut chunker = ThresholdSerialChunking::new(
                COMMAND_HEADER_SIZE,
                MAX_CHUNK,
                TEST_SPLIT_THRESHOLD,
                Some(sender),
            );
            let messages: Vec<WireMessage> = (0..4)
                .map(|index| WireMessage::success(1, index + 1, 0, false, vec![0x22; 4]))
                .collect();
            let stream = encode_all(&messages);

            let framed = feed(&mut chunker, into_chunks(&stream, COMMAND_HEADER_SIZE + 4));

            assert_eq!(framed.len(), 4);
            let (_, stalls) = drain(&receiver);
            assert_eq!(stalls, 0);
        }

        /// Each run holding a part-assembled message counts once.
        #[test]
        fn pending_counts_runs_part_way_through_a_message() {
            let (sender, receiver) = std::sync::mpsc::channel();
            let mut chunker = ThresholdSerialChunking::new(
                COMMAND_HEADER_SIZE,
                MAX_CHUNK,
                TEST_SPLIT_THRESHOLD,
                Some(sender),
            );
            let stream = encode_all(std::slice::from_ref(&WireMessage::success(
                1,
                1,
                0,
                false,
                vec![0x33; 64],
            )));

            let chunks = into_chunks(&stream, 8);
            let (first, rest) = chunks.split_first().expect("stream splits into chunks");
            for chunk in rest {
                resolve_one(
                    &mut chunker,
                    Chunk {
                        offset: chunk.offset,
                        bytes: chunk.bytes.clone(),
                    },
                )
                .expect("out of order chunk accepted");
            }

            let (pending, _) = drain(&receiver);
            assert_eq!(pending.last(), Some(&1), "one run is mid-message");

            resolve_one(
                &mut chunker,
                Chunk {
                    offset: first.offset,
                    bytes: first.bytes.clone(),
                },
            )
            .expect("gap-filling chunk accepted");

            let (pending, stalls) = drain(&receiver);
            assert_eq!(pending.last(), Some(&0), "the run drains once framed");
            assert_eq!(stalls, 1);
        }
    }

    /// Behaviour only the parallel assembler offers, so there is nothing to compare against.
    mod parallel_only {
        use super::*;

        /// The property that motivates the algorithm: a message whose bytes have all arrived
        /// is framed even while an earlier message is still short of its payload.
        #[test]
        fn a_message_is_framed_past_an_incomplete_earlier_one() {
            let first = WireMessage::success(1, 1, 0, false, vec![0xaa; 32]);
            let second = WireMessage::success(2, 2, 0, false, vec![0xbb; 4]);
            let stream = encode_all(&[first, second.clone()]);

            // The first message's header plus half its payload, then everything from the
            // second message onwards. The gap in the first payload is never filled.
            let tail_offset = COMMAND_HEADER_SIZE + 32;
            let chunks = vec![
                Chunk {
                    offset: 0,
                    bytes: Bytes::copy_from_slice(&stream[..COMMAND_HEADER_SIZE + 16]),
                },
                Chunk {
                    offset: tail_offset as u64,
                    bytes: Bytes::copy_from_slice(&stream[tail_offset..]),
                },
            ];

            assert_eq!(
                assemble::<ParallelChunking>(COMMAND_HEADER_SIZE, MAX_CHUNK, chunks),
                one_message(&second)
            );
        }
    }
}

mod resolve_arrived_into {
    use std::time::Duration;
    use std::time::Instant;

    use super::*;

    fn chunk(stream: &[u8], range: std::ops::Range<usize>) -> Chunk {
        Chunk {
            offset: range.start as u64,
            bytes: Bytes::copy_from_slice(&stream[range]),
        }
    }

    #[test]
    fn a_header_is_dated_from_the_chunk_completing_it() {
        let message = WireMessage::success(9, 1, 0, false, vec![0xab; 32]);
        let stream = encode_all(std::slice::from_ref(&message));
        let first = Instant::now();
        let second = first + Duration::from_millis(5);
        let last = second + Duration::from_millis(5);
        let mut chunker = ParallelChunking::new(COMMAND_HEADER_SIZE, MAX_CHUNK, None);
        let mut messages = Vec::new();

        for (range, arrived) in [
            (0..COMMAND_HEADER_SIZE / 2, first),
            (COMMAND_HEADER_SIZE / 2..COMMAND_HEADER_SIZE + 4, second),
            (COMMAND_HEADER_SIZE + 4..stream.len(), last),
        ] {
            chunker
                .resolve_arrived_into(chunk(&stream, range), Some(arrived), &mut messages)
                .expect("a valid stream");
        }

        let [message] = messages.as_slice() else {
            panic!("expected one message, got {messages:?}");
        };
        assert_eq!(message.header_at, Some(second));
    }

    #[test]
    fn a_header_arriving_ahead_of_the_one_before_it_is_dated_when_that_one_completes() {
        let earlier = WireMessage::success(9, 1, 0, false, vec![0xab; 32]);
        let later = WireMessage::success(9, 2, 0, false, vec![0xcd; 32]);
        let stream = encode_all(&[earlier.clone(), later]);
        let split = COMMAND_HEADER_SIZE + earlier.payload.len();
        let first = Instant::now();
        let second = first + Duration::from_millis(5);
        let mut chunker = ParallelChunking::new(COMMAND_HEADER_SIZE, MAX_CHUNK, None);
        let mut messages = Vec::new();

        chunker
            .resolve_arrived_into(
                chunk(&stream, split..stream.len()),
                Some(first),
                &mut messages,
            )
            .expect("a valid stream");
        chunker
            .resolve_arrived_into(chunk(&stream, 0..split), Some(second), &mut messages)
            .expect("a valid stream");

        let header_times: Vec<_> = messages
            .iter()
            .map(|message| (message.header.command_id, message.header_at))
            .collect();
        assert_eq!(header_times, vec![(1, Some(second)), (2, Some(second))]);
    }

    #[test]
    fn resolving_without_arrival_times_dates_nothing() {
        let message = WireMessage::success(9, 1, 0, false, vec![0xab; 32]);
        let stream = encode_all(std::slice::from_ref(&message));
        let mut chunker = ParallelChunking::new(COMMAND_HEADER_SIZE, MAX_CHUNK, None);
        let mut messages = Vec::new();

        chunker
            .resolve_into(chunk(&stream, 0..stream.len()), &mut messages)
            .expect("a valid stream");

        let [message] = messages.as_slice() else {
            panic!("expected one message, got {messages:?}");
        };
        assert_eq!(message.header_at, None);
    }
}
