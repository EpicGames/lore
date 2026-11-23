// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Peer read benchmark over QUIC.
//!
//! ```text
//! quic-peer-bench local <hit|miss> <requests> <concurrency> [payload bytes]
//! quic-peer-bench serve <address> [payload bytes]
//! quic-peer-bench drive <hit|miss> <server address> <requests> <concurrency> [payload bytes]
//! ```
//!
//! The server admits requests as the internal endpoint does, and the client is a peer client
//! built as the replica factory builds one, so both ends run the production read path. Workers
//! issue `local_get` for addresses the server holds (`hit`) or does not (`miss`).
//!
//! `local` runs both ends in one process. `serve` and `drive` run them in two, so each can be
//! pinned to its own CPUs and measured on its own; `serve` prints `ready` once it is listening.
//! Both derive the held addresses from the same seed, so `drive` needs `serve`'s payload size.
//!
//! Metrics are exported every 30 s to an in-memory exporter, so every instrument aggregates as it
//! would in a server. Each run reports the client-observed latency and the CPU time per request of
//! its process: both ends for `local`, the client alone for `drive`.
use std::net::SocketAddr;
use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_server::protocol::attribute_map::AttributeMap;
use lore_server::protocol::replication_store::get::Get;
use lore_server::protocol::replication_store::header::ReplicationHeader;
use lore_server::quic::StreamHandlerFactory;
use lore_server::quic::quinn::QuinnConfigBuilder;
use lore_server::quic::quinn::QuinnServer;
use lore_server::quic::quinn::service_store::ServiceStore;
use lore_server::quic::quinn::service_store::StreamDataHandlerBuilder;
use lore_server::quic::replication_store_service::client::ReplicationStoreClient;
use lore_server::quic::replication_store_service::client::StoreClient;
use lore_server::quic::replication_store_service::client_container::ClientFactory;
use lore_server::quic::replication_store_service::client_container::QuicClientFactory;
use lore_server::quic::replication_store_service::peer_responses::PeerResponseObserver;
use lore_server::quic::replication_store_service::server::ReplicationStoreService;
use lore_server::quic::stream_handler::AdmissionLimits;
use lore_server::quic::stream_handler::StreamHandler;
use lore_storage::ImmutableStore;
use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
use lore_transport::quic::client::CertificateSettings;
use lore_transport::quic::client::ServiceClient as _;
use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::PeriodicReader;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use rand::Rng as _;
use rand::RngCore as _;
use rand::SeedableRng as _;
use rand::rngs::StdRng;

/// Distinct addresses the server holds, read in rotation.
const HELD_ADDRESSES: usize = 2_048;
const HELD_SEED: u64 = 0x5eed_0001;
const MISSING_SEED: u64 = 0x5eed_0002;
const DEFAULT_PAYLOAD_BYTES: usize = 56 * 1024;
/// Gets made before timing starts, to open the connection's streams and warm both runtimes.
const WARMUP_GETS: usize = 2_000;
/// Slow-response threshold for both ends, as the deployments configure it.
const SLOW_RESPONSE_THRESHOLD: Duration = Duration::from_millis(50);

/// The internal endpoint's admission limits.
fn internal_endpoint_limits() -> AdmissionLimits {
    AdmissionLimits {
        process_limit: 50_000,
        inflight_limit: 400_000,
        handler_timeout: Some(Duration::from_secs(50)),
        permit_timeout: Some(Duration::from_millis(100)),
        slow_send_threshold: Some(SLOW_RESPONSE_THRESHOLD),
    }
}

struct ReplicationOnly(ServiceStore);

impl StreamHandlerFactory for ReplicationOnly {
    fn supported_protocols(&self) -> Vec<String> {
        self.0.get_supported_services()
    }

    fn get_stream_handler_builder(
        &self,
        protocol: &str,
    ) -> Option<(&&'static str, &StreamDataHandlerBuilder)> {
        self.0.get_stream_builder(protocol)
    }
}

/// The repository, and the fragments with their addresses, that `seed` yields. Both processes
/// derive them, so the client knows which addresses the server holds.
fn seeded_fragments(
    seed: u64,
    count: usize,
    payload_bytes: usize,
) -> (Context, Vec<(Fragment, Address, Bytes)>) {
    let mut rng = StdRng::seed_from_u64(seed);
    let repository: Context = rng.random();
    let fragments = (0..count)
        .map(|_| {
            let mut payload = vec![0; payload_bytes];
            rng.fill_bytes(&mut payload);
            let fragment = Fragment {
                flags: 0,
                size_payload: payload_bytes as u32,
                size_content: payload_bytes as u64,
            };
            let address = Address {
                hash: Hash::hash_buffer(&payload),
                context: rng.random(),
            };
            (fragment, address, Bytes::from(payload))
        })
        .collect();
    (repository, fragments)
}

async fn start_server(
    address: SocketAddr,
    payload_bytes: usize,
) -> Result<(SocketAddr, QuinnServer)> {
    let store: Arc<dyn ImmutableStore> = lore_storage::local::immutable_store::create(
        None::<&str>,
        ImmutableStoreCreateOptions::none(),
        false,
        lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
    )
    .await?;
    let (repository, fragments) = seeded_fragments(HELD_SEED, HELD_ADDRESSES, payload_bytes);
    for (fragment, address, payload) in fragments {
        store
            .clone()
            .put(repository.into(), address, fragment, Some(payload), false)
            .await?;
    }

    // Port 0 asks the OS for a free port, for the server to bind.
    let address = if address.port() == 0 {
        UdpSocket::bind(address)?.local_addr()?
    } else {
        address
    };

    let mut services = ServiceStore::default();
    services.add_service(
        ReplicationStoreClient::ALPN,
        Box::new(move |context: Arc<AttributeMap>| {
            let service = ReplicationStoreService::new(
                store.clone(),
                store.clone(),
                Arc::new(lore_telemetry::user_agent_filter::UserAgentFilter::default()),
            );
            Box::new(StreamHandler::new(
                Arc::new(service),
                context,
                internal_endpoint_limits(),
            ))
        }),
    );

    let (cert, key, _ca) = lore_server::quic::tests::server_certs()?;
    let server = QuinnServer::start(
        QuinnConfigBuilder::new()
            .address(address)
            .cert_file(cert)
            .pkey_file(key)
            .stream_handler_factory(Box::new(ReplicationOnly(services)))
            .build()?,
    )?;
    Ok((address, server))
}

async fn connect(server: SocketAddr) -> Result<ReplicationStoreClient> {
    let mut factory = QuicClientFactory::new(
        format!("quic://{server}"),
        CertificateSettings {
            custom_ca: None,
            client: None,
        },
    );
    factory.response_observer = Some(Arc::new(PeerResponseObserver::new(
        [
            KeyValue::new("peer_id", "bench-peer"),
            KeyValue::new("peer_locality", "SameRegion"),
            KeyValue::new("replica_type", "read"),
        ],
        Some(SLOW_RESPONSE_THRESHOLD),
    )));
    Ok(factory.make_client(Some(8 * 1024 * 1024)).await?)
}

/// CPU time this process has used, user and system together.
fn cpu_time() -> Duration {
    let pid = sysinfo::get_current_pid().expect("current pid");
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    let process = system.process(pid).expect("this process");
    Duration::from_millis(process.accumulated_cpu_time())
}

fn report(mode: &str, mut latencies: Vec<u64>, elapsed: Duration, cpu: Duration) {
    latencies.sort_unstable();
    let count = latencies.len().max(1);
    let at = |fraction: f64| {
        latencies[((count as f64 * fraction) as usize).min(count - 1)] as f64 / 1000.0
    };
    let mean = latencies.iter().sum::<u64>() as f64 / count as f64 / 1000.0;
    println!(
        "{mode}: n={} ops/s={:.0} cpu_us_per_op={:.2} mean_us={mean:.1} p50_us={:.1} p90_us={:.1} p99_us={:.1} p999_us={:.1}",
        latencies.len(),
        latencies.len() as f64 / elapsed.as_secs_f64(),
        cpu.as_secs_f64() * 1e6 / count as f64,
        at(0.50),
        at(0.90),
        at(0.99),
        at(0.999),
    );
}

async fn drive(
    mode: String,
    server: SocketAddr,
    requests: usize,
    concurrency: usize,
    payload_bytes: usize,
) -> Result<()> {
    let (repository, held) = seeded_fragments(HELD_SEED, HELD_ADDRESSES, payload_bytes);
    let addresses: Arc<Vec<Address>> = Arc::new(match mode.as_str() {
        "hit" => held.into_iter().map(|(_, address, _)| address).collect(),
        "miss" => seeded_fragments(MISSING_SEED, HELD_ADDRESSES, 1)
            .1
            .into_iter()
            .map(|(_, address, _)| address)
            .collect(),
        other => bail!("unknown mode {other}"),
    });

    let client = Arc::new(connect(server).await?);
    let get = move |client: Arc<ReplicationStoreClient>, address: Address| async move {
        client
            .local_get(Get {
                header: ReplicationHeader {
                    correlation_id: uuid::Uuid::nil(),
                    repository,
                },
                address,
            })
            .await
    };

    for address in addresses.iter().cycle().take(WARMUP_GETS) {
        let _ = get(client.clone(), *address).await;
    }

    let next = Arc::new(AtomicUsize::new(0));
    let cpu_before = cpu_time();
    let started = Instant::now();
    let mut workers = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let client = client.clone();
        let addresses = addresses.clone();
        let next = next.clone();
        let mode = mode.clone();
        workers.push(lore_base::lore_spawn!(async move {
            let mut latencies = Vec::new();
            loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= requests {
                    break;
                }
                let address = addresses[index % addresses.len()];
                let began = Instant::now();
                let result = get(client.clone(), address).await;
                latencies.push(began.elapsed().as_nanos() as u64);
                match (mode.as_str(), result) {
                    ("hit", Err(error)) => panic!("hit failed: {error}"),
                    ("miss", Ok(_)) => panic!("miss found data"),
                    _ => {}
                }
            }
            latencies
        }));
    }
    let mut latencies = Vec::with_capacity(requests);
    for worker in workers {
        latencies.extend(worker.await.context("worker panicked")?);
    }
    let elapsed = started.elapsed();
    let cpu = cpu_time() - cpu_before;
    report(&mode, latencies, elapsed, cpu);
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let arg = |index: usize| -> Result<&str> {
        args.get(index)
            .map(String::as_str)
            .with_context(|| format!("missing argument {index}"))
    };
    let payload_at = |index: usize| -> Result<usize> {
        Ok(args
            .get(index)
            .map_or(Ok(DEFAULT_PAYLOAD_BYTES), |value| value.parse())?)
    };

    lore_telemetry::set_meter_provider(
        SdkMeterProvider::builder()
            .with_reader(
                PeriodicReader::builder(InMemoryMetricExporter::default())
                    .with_interval(Duration::from_secs(30))
                    .build(),
            )
            .build(),
    );
    let execution =
        lore_server::util::setup_execution("quic-peer-bench", String::new(), String::new());
    let runtime = lore_base::runtime::runtime();
    match arg(1)? {
        "local" => {
            let mode = arg(2)?.to_string();
            let requests = arg(3)?.parse()?;
            let concurrency = arg(4)?.parse()?;
            let payload_bytes = payload_at(5)?;
            runtime.block_on(LORE_CONTEXT.scope(execution, async move {
                let (address, _server) =
                    start_server("127.0.0.1:0".parse()?, payload_bytes).await?;
                drive(mode, address, requests, concurrency, payload_bytes).await
            }))
        }
        "serve" => {
            let address = arg(2)?.parse()?;
            let payload_bytes = payload_at(3)?;
            runtime.block_on(LORE_CONTEXT.scope(execution, async move {
                let (address, _server) = start_server(address, payload_bytes).await?;
                println!("ready {address}");
                std::future::pending::<()>().await;
                Ok(())
            }))
        }
        "drive" => {
            let mode = arg(2)?.to_string();
            let server = arg(3)?.parse()?;
            let requests = arg(4)?.parse()?;
            let concurrency = arg(5)?.parse()?;
            let payload_bytes = payload_at(6)?;
            runtime.block_on(LORE_CONTEXT.scope(execution, async move {
                drive(mode, server, requests, concurrency, payload_bytes).await
            }))
        }
        other => bail!("unknown command {other}"),
    }
}
