// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Storage round-trip benchmark for the machine server prototype.
//!
//! ```text
//! uds-bench ping <socket> <count> <concurrency>
//! uds-bench get <remote url> <partition> <address file> <concurrency> [passes]
//! ```
//!
//! `ping` measures the socket and the server's dispatch alone: the floor under every command,
//! not a stand-in for one. `get` reads every address in the file, 48 bytes each, the hash and then
//! the context, through any remote: `lore-unix://` for the machine server, `lores://` for the
//! upstream directly. Workers run on the core runtime, where real callers run.
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Partition;
use lore_transport::auth::AuthMode;
use zerocopy::IntoBytes;

fn report(label: &str, mut latencies: Vec<u64>, elapsed: Duration, bytes: u64) {
    latencies.sort_unstable();
    let count = latencies.len().max(1);
    let at = |fraction: f64| {
        latencies
            .get(((count as f64 * fraction) as usize).min(count - 1))
            .copied()
            .unwrap_or_default() as f64
            / 1000.0
    };
    let mean = latencies.iter().sum::<u64>() as f64 / count as f64 / 1000.0;
    println!(
        "{label}: n={} {:.0} ops/s {:.1} MiB/s mean={mean:.1}us p50={:.1}us p90={:.1}us p99={:.1}us max={:.1}us",
        latencies.len(),
        latencies.len() as f64 / elapsed.as_secs_f64(),
        bytes as f64 / elapsed.as_secs_f64() / (1024.0 * 1024.0),
        at(0.50),
        at(0.90),
        at(0.99),
        at(1.0),
    );
}

async fn ping(socket: &str, count: usize, concurrency: usize) -> Result<()> {
    let connection =
        lore_transport::uds::client::shared_connection(std::path::Path::new(socket)).await?;
    // Warm the connection and both runtimes.
    for _ in 0..100 {
        connection.ping().await?;
    }
    let next = Arc::new(AtomicUsize::new(0));
    let start = Instant::now();
    let mut workers = Vec::new();
    for _ in 0..concurrency {
        let connection = connection.clone();
        let next = next.clone();
        workers.push(lore_base::lore_spawn_core!(async move {
            let mut latencies = Vec::new();
            while next.fetch_add(1, Ordering::Relaxed) < count {
                let elapsed = connection.ping().await?;
                latencies.push(elapsed.as_nanos() as u64);
            }
            Ok::<_, anyhow::Error>(latencies)
        }));
    }
    let mut latencies = Vec::with_capacity(count);
    for worker in workers {
        latencies.extend(worker.await??);
    }
    report(
        &format!("ping (dispatch floor) c={concurrency}"),
        latencies,
        start.elapsed(),
        0,
    );
    Ok(())
}

async fn get(
    remote_url: &str,
    partition: Partition,
    addresses: Vec<Address>,
    concurrency: usize,
    passes: usize,
) -> Result<()> {
    let connection = lore_transport::connect(remote_url, "", partition, 1, "", "", AuthMode::Auto)
        .await
        .with_context(|| format!("connecting to {remote_url}"))?;
    let storage = connection.storage().await?;
    let session = storage.session_start(partition, "uds-bench").await?;
    let addresses = Arc::new(addresses);
    for pass in 0..passes {
        let next = Arc::new(AtomicUsize::new(0));
        let bytes = Arc::new(AtomicU64::new(0));
        let start = Instant::now();
        let mut workers = Vec::new();
        for _ in 0..concurrency {
            let storage = storage.clone();
            let addresses = addresses.clone();
            let next = next.clone();
            let bytes = bytes.clone();
            workers.push(lore_base::lore_spawn_core!(async move {
                let mut latencies = Vec::new();
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(address) = addresses.get(index) else {
                        break;
                    };
                    let issued = Instant::now();
                    let (_fragment, payload) = storage.get(session, address).await?;
                    latencies.push(issued.elapsed().as_nanos() as u64);
                    bytes.fetch_add(payload.len() as u64, Ordering::Relaxed);
                }
                Ok::<_, anyhow::Error>(latencies)
            }));
        }
        let mut latencies = Vec::with_capacity(addresses.len());
        for worker in workers {
            latencies.extend(worker.await??);
        }
        report(
            &format!("get pass={pass} c={concurrency}"),
            latencies,
            start.elapsed(),
            bytes.load(Ordering::Relaxed),
        );
    }
    let _ = storage.session_stop(session).await;
    Ok(())
}

fn read_addresses(path: &str) -> Result<Vec<Address>> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {path}"))?;
    let mut seen = std::collections::HashSet::new();
    let mut addresses = Vec::new();
    for chunk in bytes.as_chunks::<{ size_of::<Address>() }>().0 {
        if seen.insert(*chunk) {
            let mut address = Address::default();
            address.as_mut_bytes().copy_from_slice(chunk);
            addresses.push(address);
        }
    }
    Ok(addresses)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let arg = |index: usize| -> Result<&str> {
        args.get(index)
            .map(String::as_str)
            .with_context(|| format!("missing argument {index}"))
    };
    let execution = lore_server::util::setup_execution("uds-bench", String::new(), String::new());
    let runtime = lore_base::runtime::runtime();
    match arg(1)? {
        "ping" => {
            let socket = arg(2)?.to_string();
            let count = arg(3)?.parse()?;
            let concurrency = arg(4)?.parse()?;
            runtime.block_on(LORE_CONTEXT.scope(execution, async move {
                ping(&socket, count, concurrency).await
            }))
        }
        "get" => {
            let remote_url = arg(2)?.to_string();
            let partition: Partition = arg(3)?.parse()?;
            let addresses = read_addresses(arg(4)?)?;
            let concurrency = arg(5)?.parse()?;
            let passes = args.get(6).map_or(Ok(1), |value| value.parse())?;
            println!("{} distinct addresses", addresses.len());
            runtime.block_on(LORE_CONTEXT.scope(execution, async move {
                get(&remote_url, partition, addresses, concurrency, passes).await
            }))
        }
        other => bail!("unknown mode {other}"),
    }
}
