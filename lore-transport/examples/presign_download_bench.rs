// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::error::Error;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
use lore_base::types::Hash;
use lore_proto::lore::model::v1 as model_v1;
use lore_proto::lore::storage::v1 as storage_v1;
use lore_proto::lore::storage::v1::storage_service_client::StorageServiceClient;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use tokio::sync::Semaphore;
use tonic::Request;
use tonic::metadata::MetadataValue;

const ENDPOINT: &str = "http://127.0.0.1:41337";
const DIRECT_DOWNLOAD_CONCURRENCY: usize = 64;
const ROUNDS: usize = 5;
const PRESIGN_EXPIRES_SECONDS: u64 = 300;

type BenchError = Box<dyn Error + Send + Sync>;

#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    count: usize,
    payload_size: usize,
}

#[derive(Clone)]
struct BenchFragment {
    address: Address,
    payload: Bytes,
}

struct RoundStats {
    bytes: usize,
    server_get_ms: f64,
    presign_ms: f64,
    direct_http_ms: f64,
    direct_total_ms: f64,
}

#[tokio::main]
async fn main() -> Result<(), BenchError> {
    let endpoint = std::env::args()
        .nth(1)
        .unwrap_or_else(|| ENDPOINT.to_string());
    let repository = Context::from([
        0x70, 0x72, 0x65, 0x73, 0x69, 0x67, 0x6e, 0x2d, 0x62, 0x65, 0x6e, 0x63, 0x68, 0x2d, 0x30,
        0x31,
    ]);

    let scenarios = [
        Scenario {
            name: "many-small",
            count: 256,
            payload_size: 4 * 1024,
        },
        Scenario {
            name: "medium",
            count: 64,
            payload_size: 64 * 1024,
        },
        Scenario {
            name: "max-fragment",
            count: 32,
            payload_size: FRAGMENT_SIZE_THRESHOLD,
        },
    ];

    println!(
        "endpoint={endpoint} repository={} rounds={ROUNDS} direct_http_concurrency={DIRECT_DOWNLOAD_CONCURRENCY}",
        repository
    );
    println!(
        "| scenario | fragments | bytes | server_get_avg_ms | presign_avg_ms | direct_http_avg_ms | direct_total_avg_ms | direct_vs_server |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|");

    for scenario in scenarios {
        let fragments = make_fragments(repository, scenario);
        seed_fragments(&endpoint, repository, &fragments).await?;

        let mut round_stats = Vec::new();
        let addresses = fragments
            .iter()
            .map(|fragment| fragment.address)
            .collect::<Vec<_>>();

        // Warm both paths before measuring.
        let _ = server_get(&endpoint, repository, &addresses).await?;
        let _ = direct_presign_download(&endpoint, repository, &addresses).await?;

        for _ in 0..ROUNDS {
            let server = server_get(&endpoint, repository, &addresses).await?;
            let direct = direct_presign_download(&endpoint, repository, &addresses).await?;
            round_stats.push(RoundStats {
                bytes: server.0,
                server_get_ms: server.1,
                presign_ms: direct.1,
                direct_http_ms: direct.2,
                direct_total_ms: direct.3,
            });
        }

        let avg = average(&round_stats);
        let ratio = avg.direct_total_ms / avg.server_get_ms;
        println!(
            "| {} | {} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2}x |",
            scenario.name,
            scenario.count,
            avg.bytes,
            avg.server_get_ms,
            avg.presign_ms,
            avg.direct_http_ms,
            avg.direct_total_ms,
            ratio
        );
    }

    Ok(())
}

fn average(stats: &[RoundStats]) -> RoundStats {
    let count = stats.len() as f64;
    RoundStats {
        bytes: stats.first().map(|stats| stats.bytes).unwrap_or_default(),
        server_get_ms: stats.iter().map(|stats| stats.server_get_ms).sum::<f64>() / count,
        presign_ms: stats.iter().map(|stats| stats.presign_ms).sum::<f64>() / count,
        direct_http_ms: stats.iter().map(|stats| stats.direct_http_ms).sum::<f64>() / count,
        direct_total_ms: stats.iter().map(|stats| stats.direct_total_ms).sum::<f64>() / count,
    }
}

fn make_fragments(repository: Context, scenario: Scenario) -> Vec<BenchFragment> {
    (0..scenario.count)
        .map(|index| {
            let payload = deterministic_payload(scenario.name, index, scenario.payload_size);
            let hash = Hash::hash_buffer(&payload);
            let mut context = repository;
            context.data_mut()[0] ^= scenario.name.len() as u8;
            context.data_mut()[1] ^= (index & 0xff) as u8;
            context.data_mut()[2] ^= ((index >> 8) & 0xff) as u8;
            BenchFragment {
                address: Address { hash, context },
                payload: Bytes::from(payload),
            }
        })
        .collect()
}

fn deterministic_payload(name: &str, index: usize, size: usize) -> Vec<u8> {
    let mut state = 0xcbf29ce484222325u64 ^ index as u64;
    for byte in name.as_bytes() {
        state = state.wrapping_mul(0x100000001b3) ^ (*byte as u64);
    }

    let mut payload = vec![0u8; size];
    for (offset, byte) in payload.iter_mut().enumerate() {
        state ^= offset as u64;
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *byte = (state >> 32) as u8;
    }
    payload
}

async fn seed_fragments(
    endpoint: &str,
    repository: Context,
    fragments: &[BenchFragment],
) -> Result<(), BenchError> {
    let mut client = StorageServiceClient::connect(endpoint.to_string()).await?;
    let requests = fragments
        .iter()
        .map(|fragment| storage_v1::PutRequest {
            address: Some(to_proto_address(fragment.address)),
            fragment: Some(model_v1::Fragment {
                flags: 0,
                size_payload: fragment.payload.len() as u32,
                size_content: fragment.payload.len() as u64,
            }),
            payload: Some(fragment.payload.clone()),
        })
        .collect::<Vec<_>>();

    let mut request = Request::new(stream::iter(requests));
    inject_repository(&mut request, repository);
    let mut responses = client.put(request).await?.into_inner();
    let mut count = 0usize;
    while let Some(response) = responses.next().await {
        response?;
        count += 1;
    }
    if count != fragments.len() {
        return Err(format!(
            "put response count mismatch: got {count}, expected {}",
            fragments.len()
        )
        .into());
    }
    Ok(())
}

async fn server_get(
    endpoint: &str,
    repository: Context,
    addresses: &[Address],
) -> Result<(usize, f64), BenchError> {
    let mut client = StorageServiceClient::connect(endpoint.to_string()).await?;
    let requests = addresses
        .iter()
        .copied()
        .map(to_proto_address)
        .collect::<Vec<_>>();

    let mut request = Request::new(stream::iter(requests));
    inject_repository(&mut request, repository);

    let started = Instant::now();
    let mut responses = client.get(request).await?.into_inner();
    let mut bytes = 0usize;
    let mut count = 0usize;
    while let Some(response) = responses.next().await {
        let response = response?;
        bytes += response.payload.len();
        count += 1;
    }
    if count != addresses.len() {
        return Err(format!(
            "get response count mismatch: got {count}, expected {}",
            addresses.len()
        )
        .into());
    }

    Ok((bytes, elapsed_ms(started)))
}

async fn direct_presign_download(
    endpoint: &str,
    repository: Context,
    addresses: &[Address],
) -> Result<(usize, f64, f64, f64), BenchError> {
    let mut client = StorageServiceClient::connect(endpoint.to_string()).await?;
    let mut request = Request::new(storage_v1::PresignDownloadRequest {
        addresses: addresses.iter().copied().map(to_proto_address).collect(),
        expires_in_seconds: PRESIGN_EXPIRES_SECONDS,
    });
    inject_repository(&mut request, repository);

    let total_started = Instant::now();
    let presign_started = Instant::now();
    let response = client.presign_download(request).await?.into_inner();
    let presign_ms = elapsed_ms(presign_started);
    if response.downloads.len() != addresses.len() {
        return Err(format!(
            "presign response count mismatch: got {}, expected {}",
            response.downloads.len(),
            addresses.len()
        )
        .into());
    }

    let http = reqwest::Client::new();
    let limiter = Arc::new(Semaphore::new(DIRECT_DOWNLOAD_CONCURRENCY));
    let http_started = Instant::now();
    let mut tasks = Vec::with_capacity(response.downloads.len());
    for download in response.downloads {
        let http = http.clone();
        let limiter = limiter.clone();
        tasks.push(tokio::spawn(async move {
            let _permit = limiter.acquire_owned().await?;
            let expected = download
                .fragment
                .ok_or("presign response missing fragment metadata")?
                .size_payload as usize;
            let bytes = http
                .get(download.url)
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await?;
            if bytes.len() != expected {
                return Err(format!(
                    "direct response size mismatch: got {}, expected {expected}",
                    bytes.len()
                )
                .into());
            }
            Ok::<usize, BenchError>(bytes.len())
        }));
    }

    let mut bytes = 0usize;
    for task in tasks {
        bytes += task.await??;
    }
    let http_ms = elapsed_ms(http_started);
    let total_ms = elapsed_ms(total_started);

    Ok((bytes, presign_ms, http_ms, total_ms))
}

fn inject_repository<T>(request: &mut Request<T>, repository: Context) {
    let value = MetadataValue::from_bytes(repository.data());
    request.metadata_mut().append_bin(REPOSITORY_ID_KEY, value);
}

fn to_proto_address(address: Address) -> model_v1::Address {
    model_v1::Address {
        hash: Bytes::copy_from_slice(address.hash.data()),
        context: Bytes::copy_from_slice(address.context.data()),
    }
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}
