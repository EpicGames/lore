// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Bulk-fetch throughput against a live remote, run by hand.
//!
//! Fetches a fixed list of fragment addresses over a configurable number of QUIC storage
//! connections and requests in flight, and reports MiB/s, per-request latency, bytes per request,
//! per-stream in-flight depth, QUIC path statistics, client CPU and, given an interface, its total
//! receive rate over the same interval. Receive rates are read from `/proc`, so only Linux reports
//! them, and client CPU is `NaN` where `getrusage` is missing.
//!
//! | Variable | Meaning |
//! |---|---|
//! | `LORE_TP_REMOTE` | Remote URL, for example `lores://lore.example.com` |
//! | `LORE_TP_REPOSITORY` | Repository id, hex |
//! | `LORE_TP_ADDRESSES` | File with one `<hash>-<context>` address per line, optional size after |
//! | `LORE_TP_RUNS` | Comma-separated `<connections>x<requests in flight>`, default `1x64` |
//! | `LORE_TP_MIB` | MiB each run fetches before it stops, default 128 |
//! | `LORE_TP_SAME` | `1` gives every run the same addresses instead of disjoint ones |
//! | `LORE_TP_IFACE` | Interface whose receive counter is sampled, none when unset |
//! | `LORE_TP_SHOW` | `1` prints each fetched fragment |
//! | `LORE_TP_METADATA` | `1` asks for fragment metadata only, a 16-byte response per request; a run then stops after one request per 16 KiB of `LORE_TP_MIB` and reports requests and latency only |
//! | `LORE_TP_CONNECT_COST` | Comma-separated connection counts; times connect plus session start for each instead of measuring |
//! | `LORE_TP_EXPAND_TO` | Instead of measuring, write the address list with every uncompressed fragment list replaced by its leaves, down to fragments that are not lists, to this file |
//!
//! ```sh
//! LORE_TP_REMOTE=lores://lore.example.com LORE_TP_REPOSITORY=<id> \
//!   LORE_TP_ADDRESSES=/tmp/addresses.txt LORE_TP_RUNS=1x16,1x256,4x256 \
//!   cargo run --release -p lore-transport --example remote_throughput
//! ```

mod common;

use std::str::FromStr;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use bytes::Bytes;
use common::cpu_time;
use lore_base::lore_spawn;
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_base::types::FragmentReference;
use lore_base::types::Partition;
use lore_base::types::TypedBytes;
use lore_base::types::fragment_flags::FragmentFlags;
use lore_transport::ProtocolError;
use lore_transport::Storage;
use lore_transport::quic::client::STREAM_COUNT;
use lore_transport::quic::client::ServiceClient;
use lore_transport::quic::storage_service::client::StorageClient;

/// Times a request is retried on `SlowDown` before it counts as failed.
const SLOW_DOWN_RETRIES: u32 = 100;

/// What a metadata request counts against the run's budget: one fragment's worth.
const METADATA_BUDGET_BYTES: u64 = 16 * 1024;

struct Run {
    connections: usize,
    in_flight: usize,
}

#[derive(Default)]
struct WorkerRecord {
    /// Latency of each request that was never throttled.
    latency_us: Vec<u32>,
    bytes: Vec<u32>,
    bytes_total: u64,
    slow_down: u64,
    throttled: u64,
    failed: u64,
    /// Fragment lists that arrived compressed, which the write path does not produce; counted as
    /// a check on what the remote holds.
    compressed_lists: u64,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// The remote and repository `LORE_TP_REMOTE` and `LORE_TP_REPOSITORY` name.
fn target() -> (String, Partition) {
    let remote = env("LORE_TP_REMOTE").expect("LORE_TP_REMOTE");
    let repository =
        Partition::from_str(&env("LORE_TP_REPOSITORY").expect("LORE_TP_REPOSITORY")).expect("hex");
    (remote, repository)
}

fn parse_runs(spec: &str) -> Vec<Run> {
    spec.split(',')
        .map(|run| {
            let (connections, in_flight) = run.split_once('x').expect("run is <connections>x<n>");
            Run {
                connections: connections.trim().parse().expect("connection count"),
                in_flight: in_flight.trim().parse().expect("requests in flight"),
            }
        })
        .collect()
}

fn load_addresses(path: &str) -> Vec<Address> {
    let text = std::fs::read_to_string(path).expect("address file");
    let mut addresses: Vec<Address> = text
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter_map(|word| Address::from_str(word).ok())
        .filter(|address| !address.hash.is_zero())
        .collect();
    addresses.sort_unstable();
    addresses.dedup();
    shuffle(&mut addresses);
    addresses
}

/// Shuffles `addresses` in a fixed order, so disjoint runs draw from the same size distribution.
fn shuffle(addresses: &mut [Address]) {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    for index in (1..addresses.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        addresses.swap(index, (state % (index as u64 + 1)) as usize);
    }
}

fn interface_rx_bytes(interface: &str) -> u64 {
    let text = std::fs::read_to_string("/proc/net/dev").unwrap_or_default();
    text.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim() == interface)
        .and_then(|(_, counters)| counters.split_whitespace().next()?.parse().ok())
        .unwrap_or(0)
}

fn percentile(sorted: &[u32], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * fraction).round() as usize;
    f64::from(sorted[index])
}

/// The addresses of the fragments an uncompressed fragment list references, in `context`; `None`
/// for a fragment that is not one.
fn list_children(
    fragment: &Fragment,
    payload: Bytes,
    context: lore_base::types::Context,
) -> Option<Vec<Address>> {
    let fragmented = fragment.flags & FragmentFlags::PayloadFragmented.bits() != 0;
    let compressed = fragment.flags & FragmentFlags::PayloadCompressed.bits() != 0;
    if !fragmented || compressed {
        return None;
    }
    let list = payload.to_aligned::<FragmentReference>();
    Some(
        list.as_type_slice::<FragmentReference>()
            .iter()
            .map(|reference| Address {
                hash: reference.hash,
                context,
            })
            .collect(),
    )
}

/// Runs `request` until it answers with anything but `SlowDown`, at most
/// [`SLOW_DOWN_RETRIES`] times more, counting the throttling in `record`. Answers the result and
/// whether the request was throttled.
async fn unthrottled<T, F, Fut>(
    record: &mut WorkerRecord,
    request: F,
) -> (Result<T, ProtocolError>, bool)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, ProtocolError>>,
{
    let mut throttled = false;
    for _ in 0..SLOW_DOWN_RETRIES {
        match request().await {
            Err(err) if err.is_slow_down() => {
                record.slow_down += 1;
                throttled = true;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            other => return (other, throttled),
        }
    }
    (request().await, true)
}

/// Work shared by every worker: queued leaves first, newest first, then file addresses in list
/// order, so a large file spreads over every worker instead of one.
///
/// A worker finding neither waits while any fetch is in flight, since that fetch may queue the
/// leaves of a list.
struct Work {
    addresses: Arc<Vec<Address>>,
    cursor: Arc<AtomicUsize>,
    leaves: parking_lot::Mutex<Vec<Address>>,
    /// Leaves in `leaves`, so a worker finding none takes no lock.
    queued: AtomicUsize,
    /// Fetches handed out and not yet finished.
    in_flight: AtomicUsize,
    /// Notified when leaves are queued or a fetch finishes.
    changed: tokio::sync::Notify,
    budget: u64,
    /// What the run has counted against `budget`: payload bytes, or [`METADATA_BUDGET_BYTES`] per
    /// metadata request.
    spent: AtomicU64,
    /// Payload bytes fetched.
    bytes: AtomicU64,
    /// Microseconds from the start until `spent` reached `budget`, 0 until then.
    budget_reached_us: AtomicU64,
    started: Instant,
    metadata: bool,
    show: bool,
}

impl Work {
    /// The next address to fetch, counted in flight until [`finished`](Self::finished); `None`
    /// once the budget is spent, or nothing is queued, left or in flight.
    async fn next(&self) -> Option<Address> {
        loop {
            if self.spent.load(Ordering::Relaxed) >= self.budget {
                return None;
            }
            self.in_flight.fetch_add(1, Ordering::SeqCst);
            if self.queued.load(Ordering::SeqCst) > 0
                && let Some(leaf) = self.leaves.lock().pop()
            {
                self.queued.fetch_sub(1, Ordering::SeqCst);
                return Some(leaf);
            }
            let index = self.cursor.fetch_add(1, Ordering::Relaxed);
            if let Some(address) = self.addresses.get(index) {
                return Some(*address);
            }
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.in_flight.fetch_sub(1, Ordering::SeqCst) == 1
                && self.queued.load(Ordering::SeqCst) == 0
            {
                self.changed.notify_waiters();
                return None;
            }
            if self.queued.load(Ordering::SeqCst) == 0 {
                changed.await;
            }
        }
    }

    /// Queues `leaves` for any worker to fetch.
    fn queue(&self, leaves: Vec<Address>) {
        let count = leaves.len();
        self.leaves.lock().extend(leaves);
        self.queued.fetch_add(count, Ordering::SeqCst);
        self.changed.notify_waiters();
    }

    /// Ends a fetch [`next`](Self::next) handed out, after it queued what it found.
    fn finished(&self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.changed.notify_waiters();
    }

    fn account(&self, spent: u64, bytes: u64) {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        let before = self.spent.fetch_add(spent, Ordering::Relaxed);
        if before < self.budget && before + spent >= self.budget {
            self.budget_reached_us.store(
                u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
    }
}

/// Fetches `address`, queueing the leaves of an uncompressed fragment list it names.
async fn fetch_one(
    client: &StorageClient,
    session_id: u32,
    address: Address,
    record: &mut WorkerRecord,
    work: &Work,
) {
    let started = Instant::now();
    if work.metadata {
        let (result, throttled) =
            unthrottled(record, || client.get_metadata(session_id, &address)).await;
        let elapsed = started.elapsed();
        if result.is_err() {
            record.failed += 1;
            return;
        }
        if throttled {
            record.throttled += 1;
        } else {
            record
                .latency_us
                .push(u32::try_from(elapsed.as_micros()).unwrap_or(u32::MAX));
        }
        work.account(METADATA_BUDGET_BYTES, 0);
        return;
    }
    let (result, throttled) = unthrottled(record, || client.get(session_id, &address)).await;
    let elapsed = started.elapsed();
    let Ok((fragment, payload)) = result else {
        record.failed += 1;
        return;
    };
    if throttled {
        record.throttled += 1;
    } else {
        record
            .latency_us
            .push(u32::try_from(elapsed.as_micros()).unwrap_or(u32::MAX));
    }
    record.bytes.push(payload.len() as u32);
    record.bytes_total += payload.len() as u64;
    work.account(payload.len() as u64, payload.len() as u64);
    if work.show {
        println!("{address}: {fragment:?}, {} payload bytes", payload.len());
    }

    if let Some(leaves) = list_children(&fragment, payload, address.context) {
        work.queue(leaves);
    } else if fragment.flags & FragmentFlags::PayloadFragmented.bits() != 0 {
        record.compressed_lists += 1;
    }
}

#[derive(Default)]
struct StreamDepth {
    sum: [u64; STREAM_COUNT as usize],
    max: [u64; STREAM_COUNT as usize],
    samples: u64,
}

/// Storage clients for a run, each with a session started, over `connections` connections of
/// their own. The connection that resolves their storage URL and credentials closes its storage
/// connection first, so the remote holds only the run's.
async fn connect_clients(
    remote: &str,
    repository: Partition,
    connections: usize,
) -> Arc<Vec<(Arc<StorageClient>, u32)>> {
    let connection = lore_transport::connect(remote, "", repository, 1, "", "")
        .await
        .expect("connect");
    connection
        .ensure_storage_connected()
        .await
        .expect("storage connect");
    let storage_url = connection
        .environment
        .storage_url(connection.remote_url())
        .to_string();
    let domain = lore_credential::domain_from_url_str_or_url(&storage_url).expect("domain");
    lore_transport::remove_connection(connection.clone());
    connection.close_transport().await;

    let mut clients = Vec::with_capacity(connections);
    for _ in 0..connections {
        let client = StorageClient::connect(
            Weak::new(),
            &storage_url,
            domain.clone(),
            connection.auth_url(),
            connection.identity(),
            repository,
            connection.credentials(),
            None,
        )
        .await
        .expect("storage client");
        let session_id = client
            .session_start(repository, "remote-throughput")
            .await
            .expect("session start");
        clients.push((Arc::new(client), session_id));
    }
    Arc::new(clients)
}

/// Runs `in_flight` workers over `clients` until `work` is done, answering every worker's record
/// merged and the payload bytes each connection carried.
async fn run_workers(
    clients: &Arc<Vec<(Arc<StorageClient>, u32)>>,
    in_flight: usize,
    work: &Arc<Work>,
) -> (WorkerRecord, Vec<u64>) {
    let mut workers = tokio::task::JoinSet::new();
    for worker in 0..in_flight {
        let clients = clients.clone();
        let work = work.clone();
        lore_spawn!(workers, async move {
            let mut record = WorkerRecord::default();
            let connection = worker % clients.len();
            let (client, session_id) = &clients[connection];
            while let Some(address) = work.next().await {
                fetch_one(client, *session_id, address, &mut record, &work).await;
                work.finished();
            }
            (connection, record)
        });
    }

    let mut merged = WorkerRecord::default();
    let mut per_connection = vec![0u64; clients.len()];
    while let Some(result) = workers.join_next().await {
        let (connection, record) = result.expect("worker");
        per_connection[connection] += record.bytes_total;
        merged.latency_us.extend(record.latency_us);
        merged.bytes.extend(record.bytes);
        merged.slow_down += record.slow_down;
        merged.throttled += record.throttled;
        merged.failed += record.failed;
        merged.compressed_lists += record.compressed_lists;
    }
    (merged, per_connection)
}

/// What a run measured besides the workers' records.
struct Measured {
    elapsed: f64,
    cpu: Option<f64>,
    rx: Option<u64>,
    depth: StreamDepth,
    per_connection: Vec<u64>,
}

async fn measure(run: &Run, addresses: Arc<Vec<Address>>, cursor: Arc<AtomicUsize>) {
    let (remote, repository) = target();
    let budget = env("LORE_TP_MIB")
        .and_then(|mib| mib.parse::<u64>().ok())
        .unwrap_or(128)
        * 1024
        * 1024;
    let interface = env("LORE_TP_IFACE");
    let clients = connect_clients(&remote, repository, run.connections).await;

    let done = Arc::new(AtomicBool::new(false));
    let sampler = {
        let clients = clients.clone();
        let done = done.clone();
        lore_spawn!(async move {
            let mut depth = StreamDepth::default();
            while !done.load(Ordering::Relaxed) {
                for (client, _) in clients.iter() {
                    let inflight = client.quic().stream_inflight();
                    for (stream, count) in inflight.iter().enumerate() {
                        depth.sum[stream] += count;
                        depth.max[stream] = depth.max[stream].max(*count);
                    }
                }
                depth.samples += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            depth
        })
    };

    let rx_start = interface.as_deref().map_or(0, interface_rx_bytes);
    let cpu_start = cpu_time();
    let work = Arc::new(Work {
        addresses,
        cursor,
        leaves: parking_lot::Mutex::new(Vec::new()),
        queued: AtomicUsize::new(0),
        in_flight: AtomicUsize::new(0),
        changed: tokio::sync::Notify::new(),
        budget,
        spent: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        budget_reached_us: AtomicU64::new(0),
        started: Instant::now(),
        metadata: env("LORE_TP_METADATA").is_some_and(|value| value == "1"),
        show: env("LORE_TP_SHOW").is_some_and(|value| value == "1"),
    });

    let (merged, per_connection) = run_workers(&clients, run.in_flight, &work).await;
    let elapsed = work.started.elapsed().as_secs_f64();
    let cpu = cpu_start
        .zip(cpu_time())
        .map(|(start, end)| end.saturating_sub(start).as_secs_f64());
    let rx = interface
        .as_deref()
        .map(|interface| interface_rx_bytes(interface).saturating_sub(rx_start));
    done.store(true, Ordering::Relaxed);
    let depth = sampler.await.expect("sampler");

    report(
        run,
        &work,
        merged,
        Measured {
            elapsed,
            cpu,
            rx,
            depth,
            per_connection,
        },
        &clients,
        interface.as_deref(),
    )
    .await;

    for (client, session_id) in clients.iter() {
        let _ = client.session_stop(*session_id).await;
    }
}

/// Prints what a run measured.
async fn report(
    run: &Run,
    work: &Work,
    mut merged: WorkerRecord,
    measured: Measured,
    clients: &[(Arc<StorageClient>, u32)],
    interface: Option<&str>,
) {
    let Measured {
        elapsed,
        cpu,
        rx,
        depth,
        per_connection,
    } = measured;
    let bytes = work.bytes.load(Ordering::Relaxed) as f64;
    let mib = 1024.0 * 1024.0;
    let reached = work.budget_reached_us.load(Ordering::Relaxed) as f64 / 1e6;
    merged.latency_us.sort_unstable();
    merged.bytes.sort_unstable();
    let requests = merged.latency_us.len() as u64 + merged.throttled;

    if work.metadata {
        println!(
            "run {}x{} (metadata): {requests} requests in {elapsed:.1} s = {:.0}/s, slow_down {}, throttled {}, failed {}",
            run.connections,
            run.in_flight,
            requests as f64 / elapsed,
            merged.slow_down,
            merged.throttled,
            merged.failed,
        );
    } else {
        println!(
            "run {}x{}: {:.2} MiB in {:.1} s = {:.2} MiB/s, {requests} requests ({:.0}/s), budget reached at {:.1} s = {:.2} MiB/s, slow_down {}, throttled {}, failed {}, compressed lists {}",
            run.connections,
            run.in_flight,
            bytes / mib,
            elapsed,
            bytes / mib / elapsed,
            requests as f64 / elapsed,
            reached,
            if reached > 0.0 {
                work.budget as f64 / mib / reached
            } else {
                0.0
            },
            merged.slow_down,
            merged.throttled,
            merged.failed,
            merged.compressed_lists,
        );
    }
    println!(
        "  latency ms (unthrottled): p10 {:.0} p50 {:.0} p90 {:.0} p99 {:.0} max {:.0}",
        percentile(&merged.latency_us, 0.10) / 1000.0,
        percentile(&merged.latency_us, 0.50) / 1000.0,
        percentile(&merged.latency_us, 0.90) / 1000.0,
        percentile(&merged.latency_us, 0.99) / 1000.0,
        percentile(&merged.latency_us, 1.0) / 1000.0,
    );
    if !work.metadata {
        let fetched = merged.bytes.len();
        println!(
            "  bytes/request: mean {:.0} p50 {:.0} p90 {:.0} max {:.0}",
            if fetched > 0 {
                bytes / fetched as f64
            } else {
                0.0
            },
            percentile(&merged.bytes, 0.50),
            percentile(&merged.bytes, 0.90),
            percentile(&merged.bytes, 1.0),
        );
    }
    let samples = depth.samples.max(1) as f64 * clients.len() as f64;
    let mean_depth: Vec<String> = depth
        .sum
        .iter()
        .map(|sum| format!("{:.1}", *sum as f64 / samples))
        .collect();
    println!(
        "  stream depth mean [{}] max {:?}",
        mean_depth.join(" "),
        depth.max
    );
    let cpu = cpu.unwrap_or(f64::NAN);
    println!("  client cpu {cpu:.2} s ({:.2} cores)", cpu / elapsed);
    if let (Some(interface), Some(rx)) = (interface, rx) {
        println!(
            "  {interface} rx {:.2} MiB/s, other traffic {:.2} MiB/s",
            rx as f64 / mib / elapsed,
            (rx as f64 - bytes).max(0.0) / mib / elapsed,
        );
    }
    for (index, (client, _)) in clients.iter().enumerate() {
        let stats = client.quic().connection_stats().await;
        println!(
            "  conn {index}: payload {:.1} MiB, rtt {} ms, rx {:.1} MiB in {} datagrams, lost {} of {} sent, congestion events {}, mtu {}",
            per_connection[index] as f64 / mib,
            stats.path.rtt.as_millis(),
            stats.udp_rx.bytes as f64 / mib,
            stats.udp_rx.datagrams,
            stats.path.lost_packets,
            stats.path.sent_packets,
            stats.path.congestion_events,
            stats.path.current_mtu,
        );
    }
}

/// Replaces each fragment-list address with its leaves, down to fragments that are not lists, so
/// a run's requests are uniform and no worker waits on a list fetched by another. An address
/// whose fetch fails is kept as it is. Answers the expanded list and how many fetches failed.
async fn expand(addresses: &[Address]) -> (Vec<Address>, usize) {
    let (remote, repository) = target();
    let connection = lore_transport::connect(&remote, "", repository, 4, "", "")
        .await
        .expect("connect");
    let pool = connection
        .session_pool(repository, "remote-throughput-expand")
        .await
        .expect("session pool");
    let mut pending = addresses.to_vec();
    let mut expanded = Vec::with_capacity(addresses.len());
    let mut failed = 0;
    while !pending.is_empty() {
        let cursor = Arc::new(AtomicUsize::new(0));
        let round = Arc::new(std::mem::take(&mut pending));
        let mut workers = tokio::task::JoinSet::new();
        for _ in 0..256 {
            let pool = pool.clone();
            let cursor = cursor.clone();
            let round = round.clone();
            lore_spawn!(workers, async move {
                let (mut leaves, mut lists, mut failed) = (Vec::new(), Vec::new(), 0);
                while let Some(address) = round.get(cursor.fetch_add(1, Ordering::Relaxed)) {
                    if let Ok((fragment, payload)) = pool.pick().get(address).await {
                        match list_children(&fragment, payload, address.context) {
                            Some(children) => lists.extend(children),
                            None => leaves.push(*address),
                        }
                    } else {
                        failed += 1;
                        leaves.push(*address);
                    }
                }
                (leaves, lists, failed)
            });
        }
        while let Some(result) = workers.join_next().await {
            let (leaves, lists, round_failed) = result.expect("worker");
            expanded.extend(leaves);
            pending.extend(lists);
            failed += round_failed;
        }
    }
    (expanded, failed)
}

/// Times a fresh connection with `count` storage connections up to a usable session pool, the
/// cost every command that reaches the remote pays before its first request.
async fn connect_cost(counts: &str) {
    let (remote, repository) = target();
    for round in 0..3 {
        for count in counts
            .split(',')
            .map(|count| count.trim().parse::<usize>().expect("count"))
        {
            let started = Instant::now();
            let connection = lore_transport::connect(&remote, "", repository, count, "", "")
                .await
                .expect("connect");
            let pool = connection
                .session_pool(repository, &format!("connect-cost-{round}-{count}"))
                .await
                .expect("session pool");
            let elapsed = started.elapsed();
            println!(
                "round {round}: {} connections usable in {} ms",
                count.clamp(1, lore_transport::MAX_STORAGE_CONNECTIONS),
                elapsed.as_millis()
            );
            drop(pool);
            lore_transport::remove_connection(connection.clone());
            connection.close_transport().await;
        }
    }
}

fn main() {
    if let Some(counts) = env("LORE_TP_CONNECT_COST") {
        lore_base::runtime::runtime().block_on(connect_cost(&counts));
        return;
    }
    let addresses = Arc::new(load_addresses(
        &env("LORE_TP_ADDRESSES").expect("LORE_TP_ADDRESSES"),
    ));
    let runs = parse_runs(&env("LORE_TP_RUNS").unwrap_or_else(|| "1x64".to_string()));
    let same = env("LORE_TP_SAME").is_some_and(|value| value == "1");
    println!("{} addresses, {} runs", addresses.len(), runs.len());

    if let Some(output) = env("LORE_TP_EXPAND_TO") {
        let (expanded, failed) = lore_base::runtime::runtime().block_on(expand(&addresses));
        let mut lines = String::new();
        for address in &expanded {
            lines.push_str(&address.to_string());
            lines.push('\n');
        }
        std::fs::write(&output, lines).expect("write expanded list");
        println!(
            "{} addresses expanded to {} in {output}, {failed} fetches failed and kept as they were",
            addresses.len(),
            expanded.len()
        );
        return;
    }

    lore_base::runtime::runtime().block_on(async move {
        let cursor = Arc::new(AtomicUsize::new(0));
        for run in &runs {
            if same {
                cursor.store(0, Ordering::Relaxed);
            }
            measure(run, addresses.clone(), cursor.clone()).await;
        }
    });
}
