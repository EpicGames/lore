// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use lore_base::error::NotFound;
use lore_base::error::NotSupported;
use lore_base::error::SlowDown;
use lore_base::lore_debug;
use lore_base::lore_warn;
use lore_base::types::Address;
use lore_base::types::DirectDownload;
use lore_base::types::Fragment;
use reqwest::StatusCode;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::oneshot;

use crate::error::ProtocolError;

const DEFAULT_MAX_BATCH_SIZE: usize = 512;
const DEFAULT_COALESCE_DELAY: Duration = Duration::from_millis(2);
const DEFAULT_EXPIRES_IN: Duration = Duration::from_secs(300);
const DEFAULT_MAX_HTTP_DOWNLOADS: usize = 128;

type PendingSender = oneshot::Sender<Result<DirectDownload, ProtocolError>>;

struct PendingRequest {
    address: Address,
    sender: PendingSender,
}

#[derive(Default)]
struct BatchState {
    pending: Vec<PendingRequest>,
    flushing: bool,
}

pub(crate) struct DirectDownloadBatcher {
    state: Mutex<BatchState>,
    http: reqwest::Client,
    max_batch_size: usize,
    coalesce_delay: Duration,
    expires_in: Duration,
    http_downloads: Semaphore,
    disabled: AtomicBool,
}

impl DirectDownloadBatcher {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(BatchState::default()),
            http: reqwest::Client::new(),
            max_batch_size: DEFAULT_MAX_BATCH_SIZE,
            coalesce_delay: DEFAULT_COALESCE_DELAY,
            expires_in: DEFAULT_EXPIRES_IN,
            http_downloads: Semaphore::new(DEFAULT_MAX_HTTP_DOWNLOADS),
            disabled: AtomicBool::new(false),
        }
    }

    pub(crate) async fn presign<F, Fut>(
        &self,
        address: Address,
        fetch_batch: F,
    ) -> Result<DirectDownload, ProtocolError>
    where
        F: Fn(Vec<Address>, Duration) -> Fut,
        Fut: Future<Output = Result<Vec<DirectDownload>, ProtocolError>>,
    {
        if self.disabled.load(Ordering::Relaxed) {
            return Err(ProtocolError::from(NotSupported {
                operation: "direct download".to_string(),
            }));
        }

        let (sender, receiver) = oneshot::channel();
        let should_flush = {
            let mut state = self.state.lock().await;
            state.pending.push(PendingRequest { address, sender });
            if state.flushing {
                false
            } else {
                state.flushing = true;
                true
            }
        };

        if should_flush {
            self.flush(fetch_batch).await;
        }

        receiver
            .await
            .map_err(|err| ProtocolError::internal_with_context(err, "direct download batch"))?
    }

    async fn flush<F, Fut>(&self, fetch_batch: F)
    where
        F: Fn(Vec<Address>, Duration) -> Fut,
        Fut: Future<Output = Result<Vec<DirectDownload>, ProtocolError>>,
    {
        loop {
            tokio::time::sleep(self.coalesce_delay).await;

            let batch = {
                let mut state = self.state.lock().await;
                if state.pending.is_empty() {
                    state.flushing = false;
                    return;
                }
                let take = state.pending.len().min(self.max_batch_size);
                state.pending.drain(..take).collect::<Vec<_>>()
            };

            let addresses = batch.iter().map(|request| request.address).collect();
            let result = fetch_batch(addresses, self.expires_in).await;

            match result {
                Ok(downloads) => {
                    let mut by_address: HashMap<Address, DirectDownload> = downloads
                        .into_iter()
                        .map(|download| (download.address, download))
                        .collect();

                    for request in batch {
                        let result = by_address
                            .remove(&request.address)
                            .ok_or_else(|| ProtocolError::from(NotFound));
                        let _ = request.sender.send(result);
                    }
                }
                Err(err) => {
                    if matches!(err, ProtocolError::NotSupported(_)) {
                        self.disabled.store(true, Ordering::Relaxed);
                    }
                    for request in batch {
                        let _ = request.sender.send(Err(err.clone()));
                    }
                }
            }
        }
    }

    pub(crate) async fn download(
        &self,
        download: DirectDownload,
    ) -> Result<(Fragment, Bytes), ProtocolError> {
        let _permit =
            self.http_downloads.acquire().await.map_err(|err| {
                ProtocolError::internal_with_context(err, "direct download permit")
            })?;

        if let Err(reason) = lore_base::types::validate_fragment_response(&download.fragment) {
            return Err(ProtocolError::internal(format!(
                "direct download: invalid fragment {:?}: {reason}",
                download.fragment
            )));
        }

        let response =
            self.http.get(&download.url).send().await.map_err(|err| {
                ProtocolError::internal_with_context(err, "direct download http get")
            })?;

        let status = response.status();
        if !status.is_success() {
            lore_warn!(
                "Direct download failed for {} with HTTP status {}",
                download.address,
                status
            );
            return Err(match status {
                StatusCode::NOT_FOUND | StatusCode::FORBIDDEN => ProtocolError::from(NotFound),
                StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                    ProtocolError::from(SlowDown)
                }
                _ => ProtocolError::internal(format!(
                    "direct download HTTP status {status} for {}",
                    download.address
                )),
            });
        }

        let payload = response.bytes().await.map_err(|err| {
            ProtocolError::internal_with_context(err, "direct download response body")
        })?;

        if payload.len() != download.fragment.size_payload as usize {
            return Err(ProtocolError::internal(format!(
                "direct download payload length mismatch for {}: got {}, expected {}",
                download.address,
                payload.len(),
                download.fragment.size_payload
            )));
        }

        lore_debug!(
            "Direct downloaded {} bytes for {}",
            payload.len(),
            download.address
        );
        Ok((download.fragment, payload))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use lore_base::types::Context;
    use lore_base::types::FragmentFlags;
    use lore_base::types::Hash;

    use super::*;

    fn test_batcher(coalesce_delay: Duration) -> DirectDownloadBatcher {
        DirectDownloadBatcher {
            state: Mutex::new(BatchState::default()),
            http: reqwest::Client::new(),
            max_batch_size: DEFAULT_MAX_BATCH_SIZE,
            coalesce_delay,
            expires_in: DEFAULT_EXPIRES_IN,
            http_downloads: Semaphore::new(DEFAULT_MAX_HTTP_DOWNLOADS),
            disabled: AtomicBool::new(false),
        }
    }

    fn address(value: u64) -> Address {
        Address {
            hash: Hash::from_u64(value),
            context: Context::from([value as u8; 16]),
        }
    }

    fn download(address: Address) -> DirectDownload {
        DirectDownload {
            address,
            fragment: Fragment {
                flags: FragmentFlags::PayloadStoredDurable.bits(),
                size_payload: 1,
                size_content: 1,
            },
            url: format!("http://127.0.0.1/{address}"),
            expires_at_epoch_seconds: 1,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn presign_coalesces_concurrent_requests() {
        let batcher = Arc::new(test_batcher(Duration::from_millis(20)));
        let calls = Arc::new(Mutex::new(Vec::<Vec<Address>>::new()));

        let mut tasks = Vec::new();
        for address in (0..8).map(address) {
            let batcher = batcher.clone();
            let calls = calls.clone();
            tasks.push(tokio::spawn(async move {
                batcher
                    .presign(address, move |addresses, _expires_in| {
                        let calls = calls.clone();
                        async move {
                            calls.lock().await.push(addresses.clone());
                            Ok(addresses.into_iter().map(download).collect())
                        }
                    })
                    .await
            }));
        }

        let mut results = Vec::new();
        for task in tasks {
            results.push(task.await.expect("task failed").expect("presign failed"));
        }

        assert_eq!(results.len(), 8);
        let calls = calls.lock().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].len(), 8);
    }

    #[tokio::test]
    async fn not_supported_disables_future_presign_attempts() {
        let batcher = test_batcher(Duration::from_millis(1));
        let calls = Arc::new(AtomicUsize::new(0));

        let first = batcher
            .presign(address(1), {
                let calls = calls.clone();
                move |_addresses, _expires_in| {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::Relaxed);
                        Err(ProtocolError::from(NotSupported {
                            operation: "direct download".to_string(),
                        }))
                    }
                }
            })
            .await;
        assert!(first.expect_err("expected NotSupported").is_not_supported());

        let second = batcher
            .presign(address(2), |_addresses, _expires_in| async {
                panic!("presign should be disabled after NotSupported")
            })
            .await;
        assert!(
            second
                .expect_err("expected disabled NotSupported")
                .is_not_supported()
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}
