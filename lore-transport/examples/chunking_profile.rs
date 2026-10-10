// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Profiles the serial and parallel chunk assemblers against each other.
//!
//! Each case builds a stream of messages, cuts it into randomly sized chunks, reorders them to a
//! chosen degree, and replays that chunk list through one assembler for a fixed wall-clock budget.
//! The two traffic shapes are representative workloads - one with reads and writes balanced, one
//! read-heavy with larger writes - rather than a capture of any particular deployment.
//! One run measures a single case and mode, so the process's peak resident size is attributable
//! to it. `corpus` mode builds the stream and exits, giving the baseline to subtract; `floor` mode
//! replays the chunk list with no assembler at all, the cost both assemblers sit on top of.
//! Memory is left to the caller to read off the process, since the allocator this binary links is
//! fixed before `main` and reports no statistics.
//!
//! Run as `chunking_profile <seconds> <case-index> <corpus|floor|serial|parallel|threshold…>`,
//! or with no
//! arguments to list the cases.

use std::time::Duration;
use std::time::Instant;

use bytes::Bytes;
use lore_transport::quic::chunking::ParallelChunking;
use lore_transport::quic::chunking::QuicMessage;
use lore_transport::quic::chunking::SerialChunking;
use lore_transport::quic::chunking::ThresholdSerialChunking;
use lore_transport::quic::command_header::COMMAND_HEADER_SIZE;
use lore_transport::quic::command_header::CommandHeader;
use quinn::Chunk;

mod common;

use common::cpu_time;

const MAX_CHUNK_SIZE: usize = 1024 * 1024;

struct Random(u64);

impl Random {
    fn new(seed: u64) -> Self {
        Random(if seed == 0 { 1 } else { seed })
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
        (self.next() >> 33) as usize % bound.max(1)
    }

    /// A draw in `0.0..=1.0`, for indexing a cumulative distribution.
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// How far a chunk can travel from its place in the stream.
#[derive(Clone, Copy)]
enum Reorder {
    InOrder,
    Window(usize),
}

impl Reorder {
    fn label(self) -> String {
        match self {
            Reorder::InOrder => "in order".to_owned(),
            Reorder::Window(window) => format!("window {window}"),
        }
    }
}

/// A payload size distribution: the cumulative share of messages at or below each bucket's upper
/// bound. The bounds are the fragment size ladder, topping out at the fragment maximum.
struct SizeDistribution(&'static [(usize, f64)]);

impl SizeDistribution {
    /// Picks a bucket by its share, then a size uniformly inside it.
    fn sample(&self, random: &mut Random) -> usize {
        let draw = random.unit();

        let mut lower = 0;
        for &(upper, cumulative) in self.0 {
            if draw <= cumulative {
                let span = upper - lower;
                return lower + if span > 0 { random.below(span) } else { 0 };
            }
            lower = upper;
        }
        lower
    }
}

#[derive(Clone, Copy)]
enum Command {
    Put,
    Get,
    GetMetadata,
    Query,
}

/// A traffic shape: how large the payloads are, and how the commands are mixed.
struct Profile {
    name: &'static str,
    put_payloads: SizeDistribution,
    get_payloads: SizeDistribution,
    /// Cumulative shares of each command, in the order the variants are listed.
    command_mix: [(f64, Command); 4],
}

/// Reads and writes roughly balanced, with a fifth of writes under 256 bytes.
const BALANCED: Profile = Profile {
    name: "balanced",
    put_payloads: SizeDistribution(&[
        (64, 0.010),
        (256, 0.209),
        (512, 0.290),
        (1_024, 0.313),
        (2_048, 0.353),
        (4_096, 0.533),
        (8_192, 0.588),
        (16_384, 0.653),
        (32_768, 0.717),
        (65_536, 0.795),
        (131_072, 0.971),
        (262_144, 1.000),
    ]),
    get_payloads: SizeDistribution(&[
        (64, 0.002),
        (256, 0.042),
        (512, 0.052),
        (1_024, 0.069),
        (2_048, 0.082),
        (4_096, 0.122),
        (8_192, 0.176),
        (16_384, 0.272),
        (32_768, 0.344),
        (65_536, 0.522),
        (131_072, 0.931),
        (262_144, 1.000),
    ]),
    command_mix: [
        (0.499, Command::Put),
        (0.960, Command::Get),
        (0.993, Command::GetMetadata),
        (1.000, Command::Query),
    ],
};

/// Read-heavy, with writes an order of magnitude larger than the balanced shape.
const READ_HEAVY: Profile = Profile {
    name: "read-heavy",
    put_payloads: SizeDistribution(&[
        (64, 0.016),
        (256, 0.112),
        (512, 0.173),
        (1_024, 0.193),
        (2_048, 0.212),
        (4_096, 0.311),
        (8_192, 0.339),
        (16_384, 0.375),
        (32_768, 0.400),
        (65_536, 0.543),
        (131_072, 0.938),
        (262_144, 1.000),
    ]),
    get_payloads: SizeDistribution(&[
        (64, 0.002),
        (256, 0.018),
        (512, 0.028),
        (1_024, 0.047),
        (2_048, 0.062),
        (4_096, 0.085),
        (8_192, 0.121),
        (16_384, 0.183),
        (32_768, 0.221),
        (65_536, 0.414),
        (131_072, 0.918),
        (262_144, 1.000),
    ]),
    command_mix: [
        (0.781, Command::Get),
        (0.984, Command::Put),
        (0.999, Command::GetMetadata),
        (1.000, Command::Query),
    ],
};

/// Read-dominated with mid-sized reads, and a notably higher share of multi-address queries.
const MEDIUM_READS: Profile = Profile {
    name: "medium-reads",
    put_payloads: SizeDistribution(&[
        (64, 0.009),
        (256, 0.054),
        (512, 0.064),
        (1_024, 0.076),
        (2_048, 0.123),
        (4_096, 0.271),
        (8_192, 0.366),
        (16_384, 0.480),
        (32_768, 0.698),
        (65_536, 0.818),
        (131_072, 0.953),
        (262_144, 1.000),
    ]),
    get_payloads: SizeDistribution(&[
        (64, 0.025),
        (256, 0.165),
        (512, 0.182),
        (1_024, 0.206),
        (2_048, 0.244),
        (4_096, 0.327),
        (8_192, 0.402),
        (16_384, 0.513),
        (32_768, 0.630),
        (65_536, 0.751),
        (131_072, 0.967),
        (262_144, 1.000),
    ]),
    command_mix: [
        (0.862, Command::Get),
        (0.947, Command::Query),
        (0.990, Command::Put),
        (1.000, Command::GetMetadata),
    ],
};

/// Write-heavy, and the only shape where reads are dominated by very small payloads - four in five
/// under 256 bytes.
const SMALL_READS: Profile = Profile {
    name: "small-reads",
    put_payloads: SizeDistribution(&[
        (64, 0.003),
        (256, 0.038),
        (512, 0.048),
        (1_024, 0.061),
        (2_048, 0.108),
        (4_096, 0.280),
        (8_192, 0.384),
        (16_384, 0.502),
        (32_768, 0.691),
        (65_536, 0.800),
        (131_072, 0.946),
        (262_144, 1.000),
    ]),
    get_payloads: SizeDistribution(&[
        (64, 0.124),
        (256, 0.813),
        (512, 0.833),
        (1_024, 0.859),
        (2_048, 0.884),
        (4_096, 0.911),
        (8_192, 0.944),
        (16_384, 0.975),
        (32_768, 0.997),
        (65_536, 0.998),
        (131_072, 1.000),
        (262_144, 1.000),
    ]),
    command_mix: [
        (0.581, Command::Put),
        (0.991, Command::Get),
        (0.997, Command::Query),
        (1.000, Command::GetMetadata),
    ],
};

impl Profile {
    fn pick_command(&self, random: &mut Random) -> Command {
        let draw = random.unit();
        for &(cumulative, command) in &self.command_mix {
            if draw <= cumulative {
                return command;
            }
        }
        Command::Query
    }

    /// The bytes a message occupies after its command header.
    fn body_size(&self, command: Command, direction: Direction, random: &mut Random) -> usize {
        match (command, direction) {
            // Replication header, address, fragment and flags, then the payload being written.
            (Command::Put, Direction::Requests) => {
                REPLICATION_HEADER_SIZE
                    + ADDRESS_SIZE
                    + FRAGMENT_SIZE
                    + 1
                    + self.put_payloads.sample(random)
            }
            // A put is acknowledged with nothing but its command header.
            (Command::Put, Direction::Responses) => 0,
            (Command::Get | Command::GetMetadata, Direction::Requests) => {
                REPLICATION_HEADER_SIZE + ADDRESS_SIZE
            }
            (Command::Get, Direction::Responses) => {
                FRAGMENT_SIZE + self.get_payloads.sample(random)
            }
            (Command::GetMetadata, Direction::Responses) => FRAGMENT_SIZE + 1 + random.below(256),
            (Command::Query, Direction::Requests) => {
                REPLICATION_HEADER_SIZE + (1 + random.below(32)) * ADDRESS_SIZE
            }
            (Command::Query, Direction::Responses) => (1 + random.below(32)) * ADDRESS_SIZE,
        }
    }
}

// Fixed wire sizes, from `lore_base::types` and the replication protocol messages.
const ADDRESS_SIZE: usize = 48;
const FRAGMENT_SIZE: usize = 16;
const REPLICATION_HEADER_SIZE: usize = 32;

/// Which side of the connection the assembler is reading.
#[derive(Clone, Copy)]
enum Direction {
    /// What a server reads: mostly small fixed-size commands, with put carrying a payload.
    Requests,
    /// What a client reads: header-only put acknowledgements and large get payloads.
    Responses,
}

impl Direction {
    fn label(self) -> &'static str {
        match self {
            Direction::Requests => "server reads requests",
            Direction::Responses => "client reads responses",
        }
    }
}

struct Case {
    profile: &'static Profile,
    direction: Direction,
    message_count: usize,
    /// One message in this many is an error response, carrying a status and no payload.
    error_in: usize,
    /// Smallest full-frame chunk, i.e. the payload that fits under the lowest path MTU in play.
    min_chunk: usize,
    read_cap: usize,
    reorder: Reorder,
}

impl Case {
    fn name(&self) -> String {
        format!("{} {}", self.profile.name, self.direction.label())
    }
}

fn cases() -> Vec<Case> {
    let mut cases = vec![];
    for profile in [&BALANCED, &READ_HEAVY, &MEDIUM_READS, &SMALL_READS] {
        for direction in [Direction::Requests, Direction::Responses] {
            // In-order and a short window stand for intra-region delivery; the long window
            // stands for a cross-region peer, where more bytes are in flight and reordering is
            // correspondingly worse.
            for reorder in [Reorder::InOrder, Reorder::Window(32), Reorder::Window(256)] {
                cases.push(Case {
                    profile,
                    direction,
                    // Sized so every case builds a stream of roughly ten megabytes.
                    message_count: match (profile.name, direction) {
                        ("balanced", Direction::Requests) => 680,
                        ("balanced", Direction::Responses) => 360,
                        ("read-heavy", Direction::Requests) => 1_000,
                        ("read-heavy", Direction::Responses) => 165,
                        ("medium-reads", Direction::Requests) => 5_600,
                        ("medium-reads", Direction::Responses) => 310,
                        (_, Direction::Requests) => 450,
                        (_, Direction::Responses) => 15_500,
                    },
                    error_in: 64,
                    min_chunk: 1150,
                    read_cap: 1400,
                    reorder,
                });
            }
        }
    }
    cases
}

fn build_stream(case: &Case, random: &mut Random) -> Vec<u8> {
    let mut stream = vec![];

    for index in 0..case.message_count {
        let command_id = index as u32 + 1;
        let command = case.profile.pick_command(random);
        let opcode = random.below(256) as u8;

        if random.below(case.error_in) == 0 {
            let status = random.below(300) as u32;
            let header = CommandHeader::new(opcode, command_id, 0).response_error(status);
            stream.extend_from_slice(&header.to_bytes());
            continue;
        }

        let size = case.profile.body_size(command, case.direction, random);
        let header = CommandHeader::new(opcode, command_id, size);
        stream.extend_from_slice(&header.to_bytes());
        stream.extend((0..size).map(|_| random.next() as u8));
    }

    stream
}

/// One chunk's length.
///
/// Quinn hands back one received STREAM frame per read and never coalesces, and the `max_length`
/// the callers pass is the 256 KiB oversize guard rather than anything that bites. So a chunk is
/// as large as fits in a datagram: clustered just under the path MTU, with a thin tail of short
/// chunks where a writer flushed less than a full frame.
fn draw_chunk_size(case: &Case, random: &mut Random) -> usize {
    const SHORT_CHUNK_IN: usize = 12;

    if random.below(SHORT_CHUNK_IN) == 0 {
        return 1 + random.below(case.read_cap);
    }
    case.min_chunk + random.below(case.read_cap - case.min_chunk + 1)
}

/// The chunk offsets and payload slices a replay hands to an assembler, already reordered.
fn build_chunk_template(case: &Case, stream: &[u8], random: &mut Random) -> Vec<(u64, Bytes)> {
    let mut chunks = vec![];
    let mut offset = 0;
    while offset < stream.len() {
        let remaining = stream.len() - offset;
        let size = draw_chunk_size(case, random).min(remaining);
        let end = offset + size;
        chunks.push((offset as u64, Bytes::copy_from_slice(&stream[offset..end])));
        offset = end;
    }

    match case.reorder {
        Reorder::InOrder => {}
        Reorder::Window(window) => {
            for index in 0..chunks.len() {
                let reach = (chunks.len() - index).min(window);
                chunks.swap(index, index + random.below(reach));
            }
        }
    }

    chunks
}

fn replay_chunks(template: &[(u64, Bytes)]) -> Vec<Chunk> {
    template
        .iter()
        .map(|(offset, bytes)| Chunk {
            offset: *offset,
            bytes: bytes.clone(),
        })
        .collect()
}

#[derive(Default)]
struct Measurement {
    replays: usize,
    messages: usize,
    wall: Duration,
    cpu: Option<Duration>,
}

/// Replays the chunk list until the budget is spent, building a fresh assembler per replay.
fn measure<A>(
    template: &[(u64, Bytes)],
    budget: Duration,
    build: impl Fn() -> A,
    resolve: impl Fn(&mut A, Chunk, &mut Vec<QuicMessage>),
) -> Measurement {
    let mut measurement = Measurement::default();
    let mut messages = Vec::new();

    let cpu_before = cpu_time();
    let start = Instant::now();

    while start.elapsed() < budget {
        let mut assembler = build();
        for chunk in replay_chunks(template) {
            resolve(&mut assembler, chunk, &mut messages);
            measurement.messages += messages.len();
            messages.clear();
        }
        measurement.replays += 1;
    }

    measurement.wall = start.elapsed();
    measurement.cpu = cpu_time()
        .zip(cpu_before)
        .map(|(after, before)| after - before);

    measurement
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let cases = cases();

    if arguments.len() < 3 {
        println!(
            "usage: chunking_profile <seconds> <case-index> \
<corpus|floor|serial|parallel|threshold<n>>"
        );
        for (index, case) in cases.iter().enumerate() {
            println!("  {index}  {} ({})", case.name(), case.reorder.label());
        }
        return;
    }

    let budget = Duration::from_secs(arguments[0].parse().expect("seconds"));
    let case_index: usize = arguments[1].parse().expect("case index");
    let mode = arguments[2].as_str();
    let case = &cases[case_index];

    let mut random = Random::new(0xdead_beef_cafe_f00d ^ case_index as u64);
    let stream = build_stream(case, &mut random);
    let template = build_chunk_template(case, &stream, &mut random);

    let measurement = match mode {
        "corpus" => Measurement::default(),
        "floor" => measure(
            &template,
            budget,
            || (),
            |_, chunk, _| {
                std::hint::black_box(&chunk);
            },
        ),
        "serial" => measure(
            &template,
            budget,
            || SerialChunking::new(COMMAND_HEADER_SIZE, MAX_CHUNK_SIZE, None),
            |assembler, chunk, messages| {
                assembler
                    .resolve_into(chunk, messages)
                    .expect("chunk accepted");
            },
        ),
        "parallel" => measure(
            &template,
            budget,
            || ParallelChunking::new(COMMAND_HEADER_SIZE, MAX_CHUNK_SIZE, None),
            |assembler, chunk, messages| {
                assembler
                    .resolve_into(chunk, messages)
                    .expect("chunk accepted");
            },
        ),
        // `threshold<n>` splits a boundary once it has queued n chunks.
        _ if mode.starts_with("threshold") => {
            let threshold = mode
                .trim_start_matches("threshold")
                .parse()
                .expect("threshold<n>");
            measure(
                &template,
                budget,
                || {
                    ThresholdSerialChunking::new(
                        COMMAND_HEADER_SIZE,
                        MAX_CHUNK_SIZE,
                        threshold,
                        None,
                    )
                },
                |assembler, chunk, messages| {
                    assembler
                        .resolve_into(chunk, messages)
                        .expect("chunk accepted");
                },
            )
        }
        other => panic!("unknown mode {other}"),
    };

    // One tab-separated record, for a caller assembling a table across runs.
    println!(
        "RESULT\t{case_index}\t{}\t{}\t{}\t{}\t{}\t{:.3}\t{:.3}\t{}\t{}\t{}",
        case.name(),
        case.reorder.label(),
        mode,
        stream.len(),
        template.len(),
        measurement.wall.as_secs_f64(),
        measurement.cpu.map_or(f64::NAN, |cpu| cpu.as_secs_f64()),
        measurement.replays,
        measurement.messages,
        case.message_count,
    );
}
