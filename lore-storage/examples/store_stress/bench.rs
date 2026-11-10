// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Throughput and latency of the store's hot paths, so that a change to the store can be held to
//! no regression: puts of new payloads, puts of payloads already stored, gets, reads into a
//! caller's buffer, reads into buffers every task lends under one loan, and queries, each measured
//! on its own over a populated store.
//!
//! Every fragment and payload is built before anything is timed, so that what is measured is the
//! store. A round of new puts stores `--new-per-round` addresses no round stored before, reusing
//! the payloads built under new hashes, and is timed until all are stored.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use bytes::Bytes;
use lore_storage::CallerBuffer;
use lore_storage::FlushOptions;
use lore_storage::Fragment;
use lore_storage::Lender;
use lore_storage::LocalImmutableStore;
use lore_storage::StoreMatchResult;
use lore_storage::immutable_store::ImmutableStore;
use parking_lot::Mutex;

use crate::args::Args;
use crate::model::Config;
use crate::model::Rng;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    PutNew,
    PutStored,
    Get,
    GetInto,
    GetIntoShared,
    Query,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::PutNew => "put-new",
            Mode::PutStored => "put-stored",
            Mode::Get => "get",
            Mode::GetInto => "get-into",
            Mode::GetIntoShared => "get-into-shared",
            Mode::Query => "query",
        }
    }
}

pub async fn main(args: &Args) -> u8 {
    let Some(dir) = args.dir() else {
        eprintln!("bench needs a scratch directory");
        return 2;
    };
    let _ = std::fs::remove_dir_all(&dir);
    let mut config = Config::from_args(args);
    config.keys = args.number("keys", 20_000);
    let populated = config.keys / 2;
    let seconds = args.float("seconds", 3.0);
    let rounds = args.number("rounds", 5) as usize;
    let tasks = args.number("tasks", 8) as usize;
    let new_per_round = args.number("new-per-round", 20_000).max(1);
    let modes: Vec<Mode> = args
        .string("modes", "put-new,put-stored,get,get-into,query")
        .split(',')
        .filter_map(|name| {
            [
                Mode::PutNew,
                Mode::PutStored,
                Mode::Get,
                Mode::GetInto,
                Mode::GetIntoShared,
                Mode::Query,
            ]
            .into_iter()
            .find(|mode| mode.name() == name)
        })
        .collect();
    if std::fs::create_dir_all(&dir).is_err() {
        return 2;
    }
    let _ = std::fs::write(dir.join(crate::model::CONFIG_FILE), config.serialize());
    let store =
        match LocalImmutableStore::new(Some(dir.clone()), config.settings(false, 3600)).await {
            Ok(store) => store,
            Err(err) => {
                eprintln!("open failed: {err}");
                return 3;
            }
        };
    let store: Arc<dyn ImmutableStore> = store;
    let payloads: Arc<Vec<(Fragment, Bytes)>> = Arc::new(
        (0..config.keys)
            .map(|key| (config.fragment(key, 0), config.payload(key)))
            .collect(),
    );
    // Populate half the keyspace; puts of new payloads take addresses past the keyspace.
    let populate = Instant::now();
    let mut next_new = config.keys;
    run_tasks(
        &store,
        &config,
        &payloads,
        tasks,
        Duration::MAX,
        Mode::PutStored,
        0,
        populated,
        true,
    )
    .await;
    let _ = store.clone().flush(FlushOptions::default()).await;
    println!(
        "populated {populated} keys in {:.2}s",
        populate.elapsed().as_secs_f64()
    );
    for mode in modes {
        let mut rates = vec![];
        let mut p50s = vec![];
        let mut p99s = vec![];
        for _ in 0..rounds {
            let (ops, latencies, elapsed) = if mode == Mode::PutNew {
                let first = next_new;
                next_new += new_per_round;
                let started = Instant::now();
                let (ops, latencies) = run_tasks(
                    &store,
                    &config,
                    &payloads,
                    tasks,
                    Duration::MAX,
                    mode,
                    first,
                    next_new,
                    true,
                )
                .await;
                (ops, latencies, started.elapsed().as_secs_f64())
            } else {
                let (ops, latencies) = run_tasks(
                    &store,
                    &config,
                    &payloads,
                    tasks,
                    Duration::from_secs_f64(seconds),
                    mode,
                    0,
                    populated,
                    false,
                )
                .await;
                (ops, latencies, seconds)
            };
            rates.push(ops as f64 / elapsed);
            p50s.push(percentile(&latencies, 0.50));
            p99s.push(percentile(&latencies, 0.99));
            if mode == Mode::PutNew {
                let _ = store.clone().flush(FlushOptions::default()).await;
            }
        }
        println!(
            "BENCH {} ops/s median {:.0} (rounds {}) p50 {:.1}us p99 {:.1}us",
            mode.name(),
            median(&mut rates),
            rates
                .iter()
                .map(|rate| format!("{rate:.0}"))
                .collect::<Vec<_>>()
                .join(","),
            median(&mut p50s),
            median(&mut p99s),
        );
    }
    0
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values.get(values.len() / 2).copied().unwrap_or_default()
}

fn percentile(latencies: &[u64], at: f64) -> f64 {
    if latencies.is_empty() {
        return 0.0;
    }
    let mut sorted = latencies.to_vec();
    sorted.sort_unstable();
    sorted[((sorted.len() - 1) as f64 * at) as usize] as f64 / 1000.0
}

/// Run `tasks` tasks of `mode` for `duration`, or, when `exhaust`, until each key in `first..end`
/// is done once. A key past the keyspace stores the payload built for the key it wraps to, under
/// its own hash. Returns the operations done and a sample of their latencies in nanoseconds.
#[allow(clippy::too_many_arguments)]
async fn run_tasks(
    store: &Arc<dyn ImmutableStore>,
    config: &Config,
    payloads: &Arc<Vec<(Fragment, Bytes)>>,
    tasks: usize,
    duration: Duration,
    mode: Mode,
    first: u64,
    end: u64,
    exhaust: bool,
) -> (u64, Vec<u64>) {
    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicU64::new(0));
    let cursor = Arc::new(AtomicU64::new(first));
    let latencies = Arc::new(Mutex::new(Vec::<u64>::new()));
    let lender = Lender::new();
    let mut set = tokio::task::JoinSet::new();
    for task in 0..tasks {
        let (store, config, payloads, stop, done, cursor, latencies, lender) = (
            store.clone(),
            config.clone(),
            payloads.clone(),
            stop.clone(),
            done.clone(),
            cursor.clone(),
            latencies.clone(),
            lender.clone(),
        );
        lore_base::lore_spawn!(set, async move {
            let mut rng = Rng::new(task as u64 * 7919 + 1);
            let mut sample = Vec::with_capacity(4096);
            let mut buffer = vec![0u8; config.max_payload];
            let mut count = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let key = if exhaust {
                    let key = cursor.fetch_add(1, Ordering::Relaxed);
                    if key >= end {
                        break;
                    }
                    key
                } else {
                    first + rng.below(end - first)
                };
                let partition = config.partition(0);
                let address = config.address(key, 0);
                let (fragment, payload) = &payloads[(key % payloads.len() as u64) as usize];
                let (fragment, payload) = (*fragment, payload.clone());
                let started = Instant::now();
                match mode {
                    Mode::PutNew | Mode::PutStored => {
                        let _ = store
                            .clone()
                            .put(partition, address, fragment, Some(payload), false)
                            .await;
                    }
                    Mode::Get => {
                        let _ = store.clone().get(partition, address).await;
                    }
                    Mode::GetInto => {
                        // SAFETY: `buffer` outlives the read and nothing else touches it meanwhile.
                        let mut destination =
                            unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
                        let _ = store
                            .clone()
                            .get_into(partition, address, &mut destination)
                            .await;
                    }
                    Mode::GetIntoShared => {
                        // SAFETY: `buffer` outlives the read and nothing else touches it meanwhile.
                        let mut destination =
                            unsafe { lender.lend(buffer.as_mut_ptr(), buffer.len()) };
                        let _ = store
                            .clone()
                            .get_into(partition, address, &mut destination)
                            .await;
                    }
                    Mode::Query => {
                        let addresses: Vec<_> = (0..16)
                            .map(|_| config.address(first + rng.below(end - first), 0))
                            .collect();
                        let mut results = [StoreMatchResult::default(); 16];
                        let _ = store
                            .clone()
                            .query(partition, &addresses, &mut results)
                            .await;
                    }
                }
                let elapsed = started.elapsed().as_nanos() as u64;
                count += 1;
                if sample.len() < sample.capacity() && count.is_multiple_of(7) {
                    sample.push(elapsed);
                }
            }
            done.fetch_add(count, Ordering::Relaxed);
            latencies.lock().extend(sample);
        });
    }
    if !exhaust {
        tokio::time::sleep(duration).await;
        stop.store(true, Ordering::Relaxed);
    }
    while set.join_next().await.is_some() {}
    let latencies = std::mem::take(&mut *latencies.lock());
    (done.load(Ordering::Relaxed), latencies)
}
