use std::collections::HashMap;
use std::mem::size_of;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use aws_sdk_dynamodb::types::AttributeValue;
use bytes::Bytes;
use lore_base::error::SlowDown;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::Hash;
use lore_storage::StoreError;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::dynamodb::DynamoDb;
use crate::dynamodb::ScanConfig;
use crate::dynamodb::ScanPage;
use crate::store::immutable_store::AwsImmutableStore;
use crate::store::immutable_store::FragmentMetadataEntry;

const REWRITE_RETRY_DELAY_CAP: Duration = Duration::from_secs(5);

/// Running totals for a rewrite run.
#[derive(Debug, Default)]
pub struct RewriteStats {
    // num Fragments in this scan run
    pub scanned: AtomicU64,
    pub valid_metadata_entries: AtomicU64,

    // ========================
    // Outcomes

    // num fragments converted from Compressed -> Zstd and into the State table
    pub converted_zstd: AtomicU64,
    // num of Zstd fragments migrated to the state table without recompression
    pub maintained_zstd: AtomicU64,
    // num uncompressed fragments migrated to State table
    pub converted_uncompressed: AtomicU64,
    // num compressed metadata fragments that ended up in the new system as uncompressed fragments
    pub converted_compressed_to_uncompressed: AtomicU64,
    // the num fragments that we could not read and should be abandoned
    pub could_not_deduce_payload: AtomicU64,
    // num fragments already migrated to the State table
    pub skipped_migrated: AtomicU64,
    // num fragments that weren't migrated as they are obliterated
    pub skipped_obliterated: AtomicU64,
    // num fragments skipped because load returned Oversized
    pub skipped_malicious: AtomicU64,
    // num fragments that weren't migrated due to an unforeseen error
    pub errored: AtomicU64,

    // ========================
    // General stats

    // num payloads whose compression codec was not accurate and needed to be deduced
    pub payloads_deduced: AtomicU64,
}

/// Outcome of attempting to decompress and identify a fragment's codec.
#[derive(Debug)]
enum DecompressOutcome {
    /// Declared codec was correct and hash matched.
    PayloadAccurate(Fragment, Bytes),
    /// Declared codec was wrong; correct codec was deduced via brute-force probing.
    PayloadDeduced(Fragment, Bytes),
    /// All codec probes failed; payload is irrecoverable.
    CouldNotDeduce,
}

/// Result of attempting to convert a single fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConvertOutcome {
    SkippedObliterated,
    SkippedMigrated,
    // a server or client can't read this fragment.
    // It may as well not exist and simply be removed from consideration.
    // Do not count as a failure as we can't do anything with this fragment.
    CouldNotDeducePayload,
    // fragment claims a decompressed size exceeding FRAGMENT_SIZE_THRESHOLD;
    // treat as malicious and do not attempt to read.
    SkippedMaliciousFragment,
    ConvertedToZstd,
    MaintainedZstd,
    ConvertedUncompressed,
    ConvertedCompressedToUncompressed,
}

pub struct MetadataMigrator {
    dynamodb: DynamoDb,
    store: Arc<AwsImmutableStore>,
    metadata_table_name: Arc<str>,

    api_call_max_retries: usize,
    api_retry_base_delay: Duration,

    scan_config: ScanConfig,
}

impl MetadataMigrator {
    /// Scan the metadata table page by page, enqueueing the hash of every
    /// fragment that does not have a state entry
    pub async fn discover_legacy_fragments(
        &self,
        tx: &mpsc::Sender<Hash>,
        stats: &RewriteStats,
        aborted: Arc<AtomicBool>,
    ) -> Result<(), StoreError> {
        let mut start_key: Option<HashMap<String, AttributeValue>> = None;

        loop {
            if aborted.load(Ordering::Relaxed) {
                info!("discovery stopping early: run aborted");
                return Ok(());
            }

            // Scan one page, retrying transient failures within the configured budget.
            let mut attempt = 0;
            let scan_page: ScanPage = loop {
                match self
                    .dynamodb
                    .scan_page(
                        &self.metadata_table_name,
                        start_key.clone(),
                        &self.scan_config,
                    )
                    .await
                {
                    Ok(page) => break page,
                    Err(e) => {
                        if attempt < self.api_call_max_retries {
                            attempt += 1;
                            warn!(error = ?e, attempt, "scan page failed; retrying");
                            rewrite_backoff(self.api_retry_base_delay, attempt).await;
                            continue;
                        }
                        error!(error = ?e, "scan failed after retries exhausted; aborting discovery");
                        return Err(StoreError::from(SlowDown));
                    }
                }
            };

            for item in &scan_page.items {
                stats.scanned.fetch_add(1, Ordering::Relaxed);
                if let Some(hash) = parse_metadata_entry(item) {
                    stats.valid_metadata_entries.fetch_add(1, Ordering::Relaxed);
                    if tx.send(hash).await.is_err() {
                        return Err(StoreError::internal(
                            "Incomplete scan because of no consumers",
                        ));
                    }
                }
            }

            let Some(key) = scan_page.last_evaluated_key else {
                info!("rewrite discovery complete: reached end of metadata table");
                return Ok(());
            };
            start_key = Some(key);
        }
    }

    /// Converts a single legacy fragment to the new `State` table.
    async fn process_fragment(
        &self,
        hash: Hash,
        stats: &RewriteStats,
    ) -> Result<ConvertOutcome, StoreError> {
        if self.store.load_state(hash).await?.is_some() {
            return Ok(ConvertOutcome::SkippedMigrated);
        }

        // since the state retrieval failed above, this load will be reading from the metadata table
        let (original_fragment, original_payload) = match self.store.load(hash).await {
            Ok(result) => result,
            Err(StoreError::Oversized(_)) => {
                warn!(hash = %hash, "fragment exceeds size threshold; skipping as malicious");
                return Ok(ConvertOutcome::SkippedMaliciousFragment);
            }
            Err(e) => return Err(e),
        };
        if (original_fragment.flags & FragmentFlags::PayloadObliteration) != 0 {
            return Ok(ConvertOutcome::SkippedObliterated);
        }

        let (decompressed_fragment, decompressed) =
            match decompress_hash(original_fragment, &original_payload, hash) {
                DecompressOutcome::PayloadAccurate(f, b) => (f, b),
                DecompressOutcome::PayloadDeduced(f, b) => {
                    stats.payloads_deduced.fetch_add(1, Ordering::Relaxed);
                    (f, b)
                }
                DecompressOutcome::CouldNotDeduce => {
                    return Ok(ConvertOutcome::CouldNotDeducePayload);
                }
            };

        let (new_fragment, new_payload, outcome) = {
            if original_fragment.flags & FragmentFlags::PayloadCompressedZstd != 0 {
                (
                    original_fragment,
                    original_payload,
                    ConvertOutcome::MaintainedZstd,
                )
            } else {
                recompress_to_zstd(original_fragment.flags, decompressed_fragment, decompressed)?
            }
        };

        self.store
            .write_payload_and_state(hash, new_fragment, new_payload)
            .await?;

        Ok(outcome)
    }

    pub async fn fragment_stream_consumer(
        &self,
        rx: Arc<Mutex<mpsc::Receiver<Hash>>>,
        stats: &RewriteStats,
        aborted: Arc<AtomicBool>,
    ) -> Result<(), StoreError> {
        loop {
            if aborted.load(Ordering::Relaxed) {
                break Ok(());
            }

            let hash = {
                let mut receiver = rx.lock().await;
                receiver.recv().await
            };
            let Some(hash) = hash else { break Ok(()) };

            let process_outcome = {
                let mut attempt = 0;
                loop {
                    match self.process_fragment(hash, stats).await {
                        Ok(outcome) => break outcome,
                        Err(e) => {
                            if attempt < self.api_call_max_retries {
                                attempt += 1;
                                warn!(hash = %hash, error = ?e, attempt, "fragment conversion failed; retrying");
                                rewrite_backoff(self.api_retry_base_delay, attempt).await;
                                continue;
                            }
                            error!(hash = %hash, error = ?e, "fragment conversion failed after retries; giving up on fragment");
                            stats.errored.fetch_add(1, Ordering::Relaxed);
                            return Err(e);
                        }
                    }
                }
            };

            match process_outcome {
                ConvertOutcome::MaintainedZstd => {
                    stats.maintained_zstd.fetch_add(1, Ordering::Relaxed);
                }
                ConvertOutcome::ConvertedToZstd => {
                    stats.converted_zstd.fetch_add(1, Ordering::Relaxed);
                }
                ConvertOutcome::ConvertedUncompressed => {
                    stats.converted_uncompressed.fetch_add(1, Ordering::Relaxed);
                }
                ConvertOutcome::ConvertedCompressedToUncompressed => {
                    stats
                        .converted_compressed_to_uncompressed
                        .fetch_add(1, Ordering::Relaxed);
                }
                ConvertOutcome::CouldNotDeducePayload => {
                    warn!(hash = %hash, "rewrite skipped fragment: could not deduce payload codec");
                    stats
                        .could_not_deduce_payload
                        .fetch_add(1, Ordering::Relaxed);
                }
                ConvertOutcome::SkippedMigrated => {
                    debug!(hash = %hash, "rewrite skipped fragment: already migrated");
                    stats.skipped_migrated.fetch_add(1, Ordering::Relaxed);
                }
                ConvertOutcome::SkippedObliterated => {
                    debug!(hash = %hash, "rewrite skipped fragment: obliterated");
                    stats.skipped_obliterated.fetch_add(1, Ordering::Relaxed);
                }
                ConvertOutcome::SkippedMaliciousFragment => {
                    stats.skipped_malicious.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

pub async fn rewrite_backoff(base_delay: Duration, attempt: usize) {
    if base_delay.is_zero() {
        return;
    }
    let delay = base_delay
        .saturating_mul(attempt as u32)
        .min(REWRITE_RETRY_DELAY_CAP);
    tokio::time::sleep(delay).await;
}

fn parse_metadata_entry(item: &HashMap<String, AttributeValue>) -> Option<Hash> {
    let entry: FragmentMetadataEntry = serde_dynamo::from_item(item.clone())
        .inspect_err(|e| {
            warn!(?e, ?item, "Failed to parse fragment from item");
        })
        .ok()?;
    Some(entry.hash)
}

/// Decide how to store a decompressed fragment, re-compressing with Zstd if the
/// original was compressed. Returns `(fragment, payload, outcome)` ready for writing.
fn recompress_to_zstd(
    original_flags: u32,
    decompressed_fragment: Fragment,
    decompressed: Bytes,
) -> Result<(Fragment, Bytes, ConvertOutcome), StoreError> {
    if original_flags & FragmentFlags::PayloadCompressed == 0 {
        return Ok((
            decompressed_fragment,
            decompressed,
            ConvertOutcome::ConvertedUncompressed,
        ));
    }

    match lore_storage::compress(
        decompressed_fragment,
        &decompressed,
        lore_storage::CompressionMode::Zstd,
    ) {
        Ok((fragment, payload)) => Ok((fragment, payload, ConvertOutcome::ConvertedToZstd)),
        // Zstd could not beat the size threshold; store the content uncompressed
        Err(err) if err.is_inefficient_compression() => Ok((
            decompressed_fragment,
            decompressed,
            ConvertOutcome::ConvertedCompressedToUncompressed,
        )),
        Err(err) => Err(StoreError::internal_with_context(err, "failed to compress")),
    }
}

fn decompress_hash(
    original_fragment: Fragment,
    original_payload: &Bytes,
    expected_hash: Hash,
) -> DecompressOutcome {
    // Fast path: try decompressing with the declared codec and verify hash.
    if (original_fragment.flags & FragmentFlags::PayloadCompressed) != 0 {
        match lore_storage::decompress(original_fragment, original_payload) {
            Ok((decompressed_fragment, decompressed)) => {
                if lore_storage::hash_slice(decompressed.as_ref()) == expected_hash {
                    return DecompressOutcome::PayloadAccurate(
                        decompressed_fragment,
                        decompressed.freeze(),
                    );
                }
                warn!(
                    hash = %expected_hash,
                    codec = FragmentFlags::compression_label(original_fragment.flags),
                    "decompress succeeded but hash mismatch; probing all codecs",
                );
            }
            Err(_) => {
                warn!(
                    hash = %expected_hash,
                    codec = FragmentFlags::compression_label(original_fragment.flags),
                    "decompression failed; probing all codecs",
                );
            }
        }
    } else {
        if lore_storage::hash_slice(original_payload.as_ref()) == expected_hash {
            return DecompressOutcome::PayloadAccurate(original_fragment, original_payload.clone());
        }
        warn!(
            hash = %expected_hash,
            "uncompressed payload hash mismatch; probing all codecs",
        );
    }

    // Fallback: the declared codec (or lack thereof) is wrong. Strip all compression
    // flags so we can try each codec in isolation.
    let base_flags = original_fragment.flags & !FragmentFlags::PayloadCompressed;

    if let Some((fragment, bytes)) = try_codec_probes(
        original_payload,
        base_flags,
        original_fragment.size_content,
        expected_hash,
    ) {
        warn!(
            hash = %expected_hash,
            codec = FragmentFlags::compression_label(fragment.flags),
            "recovered: deduced codec via brute-force probe",
        );
        return DecompressOutcome::PayloadDeduced(fragment, bytes);
    }

    // Legacy fallback: old S3 blobs had the Fragment metadata struct prepended to
    // the payload before it was stored. Strip the prefix and probe again.
    let prefix_len = size_of::<Fragment>();
    if original_payload.len() > prefix_len {
        let stripped = original_payload.slice(prefix_len..);
        if let Some((fragment, bytes)) = try_codec_probes(
            &stripped,
            base_flags,
            original_fragment.size_content,
            expected_hash,
        ) {
            warn!(
                hash = %expected_hash,
                codec = FragmentFlags::compression_label(fragment.flags),
                "recovered: deduced codec after stripping legacy metadata prefix",
            );
            return DecompressOutcome::PayloadDeduced(fragment, bytes);
        }
    }

    warn!(hash = %expected_hash, "payload irrecoverable: all codec probes failed");
    DecompressOutcome::CouldNotDeduce
}

/// Try every known codec (plus uncompressed) against `payload`, returning the
/// decompressed `(Fragment, Bytes)` on the first match with `expected_hash`,
/// or `None` if no codec succeeds.
fn try_codec_probes(
    payload: &Bytes,
    base_flags: u32,
    size_content: u64,
    expected_hash: Hash,
) -> Option<(Fragment, Bytes)> {
    // Cheapest probe: raw bytes are already the content.
    if lore_storage::hash_slice(payload.as_ref()) == expected_hash {
        return Some((
            Fragment {
                flags: base_flags,
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            },
            payload.clone(),
        ));
    }

    for codec_flag in [
        FragmentFlags::PayloadCompressedZstd,
        FragmentFlags::PayloadCompressedOodle2,
        FragmentFlags::PayloadCompressedLZ4,
    ] {
        let probe_fragment = Fragment {
            flags: base_flags | codec_flag.bits(),
            size_payload: payload.len() as u32,
            size_content,
        };

        if let Ok((decompressed_fragment, decompressed)) =
            lore_storage::decompress(probe_fragment, payload.as_ref())
            && lore_storage::hash_slice(decompressed.as_ref()) == expected_hash
        {
            return Some((decompressed_fragment, decompressed.freeze()));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::mem::size_of;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use aws_sdk_dynamodb::types::AttributeValue;
    use aws_smithy_types::Blob;
    use bytes::Bytes;
    use lore_base::types::Fragment;
    use lore_base::types::FragmentFlags;
    use lore_base::types::Hash;
    use tokio::sync::Mutex;
    use tokio::sync::mpsc;

    use super::*;
    use crate::store::immutable_store::FragmentState;
    use crate::store::test_util::FRAGMENT_METADATA_TABLE_NAME;
    use crate::store::test_util::Fake;
    use crate::store::test_util::store;
    use crate::store::test_util::store_with_separate_metadata_table;

    async fn make_migrator(fake: &Fake) -> MetadataMigrator {
        MetadataMigrator {
            dynamodb: crate::dynamodb::MockDynamoDb::default(),
            store: store_with_separate_metadata_table(fake).await,
            metadata_table_name: FRAGMENT_METADATA_TABLE_NAME.into(),
            api_call_max_retries: 0,
            api_retry_base_delay: Duration::ZERO,
            scan_config: ScanConfig::default(),
        }
    }

    fn make_zstd_payload(content: &[u8]) -> (Fragment, Bytes, Hash) {
        let hash = lore_storage::hash_slice(content);
        let raw = Fragment {
            flags: 0,
            size_payload: content.len() as u32,
            size_content: content.len() as u64,
        };
        let (frag, payload) =
            lore_storage::compress(raw, content, lore_storage::CompressionMode::Zstd)
                .expect("zstd compression should succeed on compressible data");
        (frag, payload, hash)
    }

    fn make_lz4_payload(content: &[u8]) -> (Fragment, Bytes, Hash) {
        let hash = lore_storage::hash_slice(content);
        let raw = Fragment {
            flags: 0,
            size_payload: content.len() as u32,
            size_content: content.len() as u64,
        };
        let (frag, payload) =
            lore_storage::compress(raw, content, lore_storage::CompressionMode::Lz4)
                .expect("lz4 compression should succeed");
        (frag, payload, hash)
    }

    mod try_codec_probes_tests {
        use super::*;

        #[test]
        fn uncompressed_payload_matches_hash() {
            let content = b"hello from lore migration tests";
            let payload = Bytes::copy_from_slice(content);
            let hash = lore_storage::hash_slice(content);
            let (frag, bytes) = try_codec_probes(&payload, 0, content.len() as u64, hash)
                .expect("should match own hash");
            assert_eq!(frag.flags & FragmentFlags::PayloadCompressed, 0);
            assert_eq!(bytes.as_ref(), content.as_slice());
            assert_eq!(frag.size_payload as usize, bytes.len());
            assert_eq!(frag.size_content, content.len() as u64);
        }

        #[test]
        fn zstd_compressed_payload_matches_hash() {
            let content = vec![0xABu8; 300];
            let (in_frag, compressed, hash) = make_zstd_payload(&content);
            let (out_frag, decompressed) =
                try_codec_probes(&compressed, 0, in_frag.size_content, hash)
                    .expect("zstd payload should be identified");
            assert_eq!(lore_storage::hash_slice(&decompressed), hash);
            assert_eq!(out_frag.size_payload as usize, decompressed.len());
            assert_eq!(out_frag.size_content, content.len() as u64);
        }

        #[test]
        fn lz4_compressed_payload_matches_hash() {
            let content = vec![0x77u8; 300];
            let (in_frag, compressed, hash) = make_lz4_payload(&content);
            let (out_frag, decompressed) =
                try_codec_probes(&compressed, 0, in_frag.size_content, hash)
                    .expect("lz4 payload should be identified");
            assert_eq!(lore_storage::hash_slice(&decompressed), hash);
            assert_eq!(out_frag.size_payload as usize, decompressed.len());
            assert_eq!(out_frag.size_content, content.len() as u64);
        }

        #[test]
        fn garbage_bytes_return_none() {
            let garbage = Bytes::from(vec![0xFFu8; 64]);
            let hash = lore_storage::hash_slice(b"something else entirely");
            assert!(try_codec_probes(&garbage, 0, 64, hash).is_none());
        }
    }

    mod decompress_hash_tests {
        use super::*;

        #[test]
        fn accurate_uncompressed() {
            let content = vec![0x11u8; 50];
            let hash = lore_storage::hash_slice(&content);
            let frag = Fragment {
                flags: 0,
                size_payload: content.len() as u32,
                size_content: content.len() as u64,
            };
            match decompress_hash(frag, &Bytes::from(content.clone()), hash) {
                DecompressOutcome::PayloadAccurate(_, b) => {
                    assert_eq!(b.as_ref(), content.as_slice());
                }
                other => panic!("expected PayloadAccurate, got {other:?}"),
            }
        }

        #[test]
        fn accurate_zstd() {
            let content = vec![0xAAu8; 400];
            let (frag, compressed, hash) = make_zstd_payload(&content);
            match decompress_hash(frag, &compressed, hash) {
                DecompressOutcome::PayloadAccurate(_, b) => {
                    assert_eq!(lore_storage::hash_slice(&b), hash);
                }
                other => panic!("expected PayloadAccurate, got {other:?}"),
            }
        }

        #[test]
        fn deduced_wrong_codec_flag() {
            let content = vec![0xBBu8; 400];
            let (mut frag, compressed, hash) = make_zstd_payload(&content);
            // Declare LZ4 but data is actually Zstd
            frag.flags = (frag.flags & !FragmentFlags::PayloadCompressed)
                | FragmentFlags::PayloadCompressedLZ4.bits();
            match decompress_hash(frag, &compressed, hash) {
                DecompressOutcome::PayloadDeduced(_, b) => {
                    assert_eq!(lore_storage::hash_slice(&b), hash);
                }
                other => panic!("expected PayloadDeduced, got {other:?}"),
            }
        }

        #[test]
        fn deduced_declared_uncompressed_actually_zstd() {
            let content = vec![0xCCu8; 400];
            let hash = lore_storage::hash_slice(&content);
            let raw = Fragment {
                flags: 0,
                size_payload: content.len() as u32,
                size_content: content.len() as u64,
            };
            let (comp_frag, compressed) =
                lore_storage::compress(raw, &content, lore_storage::CompressionMode::Zstd).unwrap();
            // Claim uncompressed
            let lying = Fragment {
                flags: 0,
                size_payload: comp_frag.size_payload,
                size_content: content.len() as u64,
            };
            match decompress_hash(lying, &compressed, hash) {
                DecompressOutcome::PayloadDeduced(_, b) => {
                    assert_eq!(lore_storage::hash_slice(&b), hash);
                }
                other => panic!("expected PayloadDeduced, got {other:?}"),
            }
        }

        #[test]
        fn deduced_legacy_metadata_prefix_stripped() {
            let content = vec![0xDDu8; 400];
            let hash = lore_storage::hash_slice(&content);
            let raw = Fragment {
                flags: 0,
                size_payload: content.len() as u32,
                size_content: content.len() as u64,
            };
            let (comp_frag, compressed) =
                lore_storage::compress(raw, &content, lore_storage::CompressionMode::Zstd).unwrap();
            let prefix = vec![0u8; size_of::<Fragment>()];
            let prefixed: Bytes = [prefix.as_slice(), compressed.as_ref()].concat().into();
            let legacy = Fragment {
                flags: comp_frag.flags,
                size_payload: prefixed.len() as u32,
                size_content: content.len() as u64,
            };
            match decompress_hash(legacy, &prefixed, hash) {
                DecompressOutcome::PayloadDeduced(out_frag, b) => {
                    assert_eq!(lore_storage::hash_slice(&b), hash);
                    // The returned fragment must describe the decompressed content,
                    // not the prefixed blob — both sizes reflect the stripped payload.
                    assert_eq!(out_frag.size_payload as usize, b.len());
                    assert_eq!(out_frag.size_content, content.len() as u64);
                }
                other => panic!("expected PayloadDeduced for legacy prefix, got {other:?}"),
            }
        }

        #[test]
        fn could_not_deduce_garbage() {
            let garbage = vec![0xFFu8; 64];
            let hash = lore_storage::hash_slice(b"definitely not this");
            let frag = Fragment {
                flags: FragmentFlags::PayloadCompressedZstd.bits(),
                size_payload: 64,
                size_content: 200,
            };
            let garbage = Bytes::from(garbage);
            match decompress_hash(frag, &garbage, hash) {
                DecompressOutcome::CouldNotDeduce => {}
                other => panic!("expected CouldNotDeduce, got {other:?}"),
            }
        }
    }

    mod recompress_tests {
        use super::*;

        fn uncompressed_fragment(len: usize) -> (Fragment, Bytes) {
            let content = vec![0x42u8; len];
            let frag = Fragment {
                flags: 0,
                size_payload: len as u32,
                size_content: len as u64,
            };
            (frag, Bytes::from(content))
        }

        fn decompressed_of_zstd(content: &[u8]) -> (Fragment, Bytes) {
            // Simulate what decompress_hash returns: a fragment with no compression flags,
            // size_payload == size_content == content length.
            let frag = Fragment {
                flags: 0,
                size_payload: content.len() as u32,
                size_content: content.len() as u64,
            };
            (frag, Bytes::copy_from_slice(content))
        }

        #[test]
        fn uncompressed_original_returns_converted_uncompressed() {
            let (frag, payload) = uncompressed_fragment(100);
            let original_flags = 0u32; // no PayloadCompressed
            let (out_frag, out_payload, outcome) =
                recompress_to_zstd(original_flags, frag, payload.clone()).unwrap();
            assert_eq!(outcome, ConvertOutcome::ConvertedUncompressed);
            assert_eq!(
                out_payload, payload,
                "uncompressed payload must pass through unchanged"
            );
            assert_eq!(out_frag.flags & FragmentFlags::PayloadCompressed, 0);
            assert_eq!(out_frag.size_payload as usize, out_payload.len());
            assert_eq!(out_frag.size_content, frag.size_content);
        }

        #[test]
        fn compressed_original_recompresses_to_zstd_and_round_trips() {
            let content = vec![0xAAu8; 500]; // highly compressible
            let (frag, decompressed) = decompressed_of_zstd(&content);
            let original_flags = FragmentFlags::PayloadCompressedLZ4.bits(); // was compressed

            let (out_frag, out_payload, outcome) =
                recompress_to_zstd(original_flags, frag, decompressed).unwrap();
            assert_eq!(outcome, ConvertOutcome::ConvertedToZstd);
            assert_ne!(
                out_frag.flags & FragmentFlags::PayloadCompressedZstd,
                0,
                "output should be zstd"
            );
            assert_eq!(
                out_frag.size_content,
                content.len() as u64,
                "size_content preserved"
            );
            assert_eq!(
                out_frag.size_payload as usize,
                out_payload.len(),
                "size_payload matches actual bytes"
            );

            // Round-trip: decompress the output and verify it matches the original content.
            let (_, roundtripped) = lore_storage::decompress(out_frag, &out_payload).unwrap();
            assert_eq!(
                roundtripped.as_ref(),
                content.as_slice(),
                "decompressed output must equal original content"
            );
        }

        #[test]
        fn incompressible_data_falls_back_to_uncompressed() {
            // Random-looking bytes that Zstd cannot compress by 5%+.
            // Use a small buffer: compress refuses payloads below FRAGMENT_COMPRESS_SIZE_LIMIT,
            // but we need the *content* to be incompressible. Use 33 unique bytes (just above the
            // 32-byte limit) so zstd can't beat the threshold.
            let content: Vec<u8> = (0u8..=32).collect(); // 33 bytes, no repetition
            let (frag, decompressed) = decompressed_of_zstd(&content);
            let original_flags = FragmentFlags::PayloadCompressedZstd.bits();

            let (out_frag, out_payload, outcome) =
                recompress_to_zstd(original_flags, frag, decompressed.clone()).unwrap();
            assert_eq!(outcome, ConvertOutcome::ConvertedCompressedToUncompressed);
            assert_eq!(
                out_frag.flags & FragmentFlags::PayloadCompressed,
                0,
                "output should be uncompressed"
            );
            assert_eq!(
                out_payload, decompressed,
                "payload passed through unchanged"
            );
            assert_eq!(out_frag.size_content, content.len() as u64);
            assert_eq!(out_frag.size_payload as usize, out_payload.len());
        }
    }

    mod parse_metadata_entry_tests {
        use super::*;

        #[test]
        fn valid_hash_returns_some() {
            let hash: Hash = rand::random();
            let item = HashMap::from([(
                "hash".to_owned(),
                AttributeValue::B(Blob::new(hash.data().to_vec())),
            )]);
            assert_eq!(parse_metadata_entry(&item), Some(hash));
        }

        #[test]
        fn empty_item_returns_none() {
            assert!(parse_metadata_entry(&HashMap::new()).is_none());
        }

        #[test]
        fn wrong_type_for_hash_returns_none() {
            let item = HashMap::from([(
                "hash".to_owned(),
                AttributeValue::S("not-a-blob".to_owned()),
            )]);
            assert!(parse_metadata_entry(&item).is_none());
        }
    }

    mod discover_legacy_fragments_tests {
        use super::*;
        use crate::dynamodb::MockDynamoDb;
        use crate::dynamodb::ScanPage;

        fn item_for_hash(hash: Hash) -> HashMap<String, AttributeValue> {
            HashMap::from([(
                "hash".to_owned(),
                AttributeValue::B(Blob::new(hash.data().to_vec())),
            )])
        }

        async fn make_discover_migrator(dynamodb: MockDynamoDb) -> MetadataMigrator {
            let fake = Fake::default();
            MetadataMigrator {
                dynamodb,
                store: store(&fake).await,
                metadata_table_name: "test-metadata".into(),
                api_call_max_retries: 0,
                api_retry_base_delay: Duration::ZERO,
                scan_config: ScanConfig::default(),
            }
        }

        #[tokio::test]
        async fn single_page_enqueues_all_hashes() {
            let hash1: Hash = rand::random();
            let hash2: Hash = rand::random();
            let h1 = hash1;
            let h2 = hash2;
            let mut dynamodb = MockDynamoDb::default();
            dynamodb
                .expect_scan_page()
                .returning(move |_, start_key, _| {
                    if start_key.is_none() {
                        Ok(ScanPage {
                            items: vec![item_for_hash(h1), item_for_hash(h2)],
                            last_evaluated_key: None,
                        })
                    } else {
                        panic!("unexpected second page call")
                    }
                });
            let migrator = make_discover_migrator(dynamodb).await;
            let (tx, mut rx) = mpsc::channel(10);
            let stats = RewriteStats::default();
            let aborted = Arc::new(AtomicBool::new(false));
            migrator
                .discover_legacy_fragments(&tx, &stats, aborted)
                .await
                .unwrap();
            drop(tx);
            let mut received = vec![];
            while let Some(h) = rx.recv().await {
                received.push(h);
            }
            assert_eq!(received.len(), 2);
            assert!(received.contains(&hash1));
            assert!(received.contains(&hash2));
            assert_eq!(stats.scanned.load(Ordering::Relaxed), 2);
            assert_eq!(stats.valid_metadata_entries.load(Ordering::Relaxed), 2);
        }

        #[tokio::test]
        async fn paginated_scan_follows_last_evaluated_key() {
            let hash1: Hash = rand::random();
            let hash2: Hash = rand::random();
            let (h1, h2) = (hash1, hash2);
            let call_count = Arc::new(std::sync::atomic::AtomicU8::new(0));
            let cc = call_count.clone();
            let mut dynamodb = MockDynamoDb::default();
            dynamodb
                .expect_scan_page()
                .returning(move |_, start_key, _| {
                    let n = cc.fetch_add(1, Ordering::SeqCst);
                    match n {
                        0 => {
                            assert!(start_key.is_none());
                            Ok(ScanPage {
                                items: vec![item_for_hash(h1)],
                                last_evaluated_key: Some(HashMap::from([(
                                    "pk".to_owned(),
                                    AttributeValue::S("page1".to_owned()),
                                )])),
                            })
                        }
                        1 => {
                            assert!(start_key.is_some());
                            Ok(ScanPage {
                                items: vec![item_for_hash(h2)],
                                last_evaluated_key: None,
                            })
                        }
                        _ => panic!("unexpected extra scan call"),
                    }
                });
            let migrator = make_discover_migrator(dynamodb).await;
            let (tx, mut rx) = mpsc::channel(10);
            let stats = RewriteStats::default();
            let aborted = Arc::new(AtomicBool::new(false));
            migrator
                .discover_legacy_fragments(&tx, &stats, aborted)
                .await
                .unwrap();
            drop(tx);
            let mut received = vec![];
            while let Some(h) = rx.recv().await {
                received.push(h);
            }
            assert_eq!(received.len(), 2);
            assert!(received.contains(&hash1));
            assert!(received.contains(&hash2));
        }

        #[tokio::test]
        async fn aborted_flag_stops_before_scanning() {
            let mut dynamodb = MockDynamoDb::default();
            dynamodb.expect_scan_page().never();
            let migrator = make_discover_migrator(dynamodb).await;
            let (tx, _rx) = mpsc::channel(10);
            let stats = RewriteStats::default();
            let aborted = Arc::new(AtomicBool::new(true));
            migrator
                .discover_legacy_fragments(&tx, &stats, aborted)
                .await
                .unwrap();
            assert_eq!(stats.scanned.load(Ordering::Relaxed), 0);
        }
    }

    mod process_fragment_tests {
        use super::*;

        #[tokio::test]
        async fn skips_already_migrated() {
            let fake = Fake::default();
            let migrator = make_migrator(&fake).await;
            let hash: Hash = rand::random();
            fake.set_state(hash, FragmentState::Stored);
            let stats = RewriteStats::default();
            assert_eq!(
                migrator.process_fragment(hash, &stats).await.unwrap(),
                ConvertOutcome::SkippedMigrated
            );
        }

        #[tokio::test]
        async fn skips_obliterated() {
            let fake = Fake::default();
            let migrator = make_migrator(&fake).await;
            let content = vec![0x42u8; 100];
            let hash = lore_storage::hash_slice(&content);
            fake.put_object_without_metadata(hash, &content);
            fake.set_legacy_metadata_row(
                hash,
                Fragment {
                    flags: FragmentFlags::PayloadObliterated.bits(),
                    size_payload: content.len() as u32,
                    size_content: content.len() as u64,
                },
            );
            let stats = RewriteStats::default();
            assert_eq!(
                migrator.process_fragment(hash, &stats).await.unwrap(),
                ConvertOutcome::SkippedObliterated
            );
        }

        #[tokio::test]
        async fn converts_uncompressed_writes_state() {
            let fake = Fake::default();
            let migrator = make_migrator(&fake).await;
            let content = vec![0x33u8; 150];
            let hash = lore_storage::hash_slice(&content);
            fake.put_object_without_metadata(hash, &content);
            fake.set_legacy_metadata_row(
                hash,
                Fragment {
                    flags: 0,
                    size_payload: content.len() as u32,
                    size_content: content.len() as u64,
                },
            );
            let stats = RewriteStats::default();
            assert_eq!(
                migrator.process_fragment(hash, &stats).await.unwrap(),
                ConvertOutcome::ConvertedUncompressed
            );
            assert_eq!(fake.state_of(hash), Some(FragmentState::Stored));
        }

        #[tokio::test]
        async fn maintains_zstd_writes_state() {
            let fake = Fake::default();
            let migrator = make_migrator(&fake).await;
            let content = vec![0x44u8; 500];
            let (frag, compressed, hash) = make_zstd_payload(&content);
            fake.put_object_without_metadata(hash, &compressed);
            fake.set_legacy_metadata_row(hash, frag);
            let stats = RewriteStats::default();
            assert_eq!(
                migrator.process_fragment(hash, &stats).await.unwrap(),
                ConvertOutcome::MaintainedZstd
            );
            assert_eq!(fake.state_of(hash), Some(FragmentState::Stored));
        }

        #[tokio::test]
        async fn converts_lz4_to_zstd_writes_state() {
            let fake = Fake::default();
            let migrator = make_migrator(&fake).await;
            let content = vec![0x55u8; 500];
            let (frag, compressed, hash) = make_lz4_payload(&content);
            fake.put_object_without_metadata(hash, &compressed);
            fake.set_legacy_metadata_row(hash, frag);
            let stats = RewriteStats::default();
            assert_eq!(
                migrator.process_fragment(hash, &stats).await.unwrap(),
                ConvertOutcome::ConvertedToZstd
            );
            assert_eq!(fake.state_of(hash), Some(FragmentState::Stored));
        }

        #[tokio::test]
        async fn could_not_deduce_irrecoverable_payload() {
            let fake = Fake::default();
            let migrator = make_migrator(&fake).await;
            let garbage = vec![0xFFu8; 64];
            let hash: Hash = rand::random(); // hash doesn't match garbage
            fake.put_object_without_metadata(hash, &garbage);
            fake.set_legacy_metadata_row(
                hash,
                Fragment {
                    flags: FragmentFlags::PayloadCompressedZstd.bits(),
                    size_payload: garbage.len() as u32,
                    size_content: 200,
                },
            );
            let stats = RewriteStats::default();
            assert_eq!(
                migrator.process_fragment(hash, &stats).await.unwrap(),
                ConvertOutcome::CouldNotDeducePayload
            );
        }

        #[tokio::test]
        async fn deduced_codec_increments_payloads_deduced_stat() {
            let fake = Fake::default();
            let migrator = make_migrator(&fake).await;
            let content = vec![0x66u8; 500];
            let (mut frag, compressed, hash) = make_zstd_payload(&content);
            // Lie: claim LZ4 when actually Zstd
            frag.flags = (frag.flags & !FragmentFlags::PayloadCompressed)
                | FragmentFlags::PayloadCompressedLZ4.bits();
            fake.put_object_without_metadata(hash, &compressed);
            fake.set_legacy_metadata_row(hash, frag);
            let stats = RewriteStats::default();
            migrator.process_fragment(hash, &stats).await.unwrap();
            assert_eq!(stats.payloads_deduced.load(Ordering::Relaxed), 1);
        }
    }

    mod fragment_stream_consumer_tests {
        use super::*;

        #[tokio::test]
        async fn stops_cleanly_when_channel_closed() {
            let fake = Fake::default();
            let migrator = make_migrator(&fake).await;
            let (_, rx) = mpsc::channel::<Hash>(10);
            let stats = RewriteStats::default();
            let aborted = Arc::new(AtomicBool::new(false));
            migrator
                .fragment_stream_consumer(Arc::new(Mutex::new(rx)), &stats, aborted)
                .await
                .unwrap();
        }

        #[tokio::test]
        async fn converts_fragment_and_writes_state() {
            let fake = Fake::default();
            let migrator = make_migrator(&fake).await;
            let content = vec![0x77u8; 600];
            let (frag, compressed, hash) = make_zstd_payload(&content);
            fake.put_object_without_metadata(hash, &compressed);
            fake.set_legacy_metadata_row(hash, frag);
            let (tx, rx) = mpsc::channel(10);
            tx.send(hash).await.unwrap();
            drop(tx);
            let stats = RewriteStats::default();
            let aborted = Arc::new(AtomicBool::new(false));
            migrator
                .fragment_stream_consumer(Arc::new(Mutex::new(rx)), &stats, aborted)
                .await
                .unwrap();
            assert_eq!(stats.maintained_zstd.load(Ordering::Relaxed), 1);
            assert_eq!(fake.state_of(hash), Some(FragmentState::Stored));
        }

        #[tokio::test]
        async fn stats_accumulate_across_multiple_outcomes() {
            let fake = Fake::default();
            let migrator = make_migrator(&fake).await;

            // already migrated
            let migrated: Hash = rand::random();
            fake.set_state(migrated, FragmentState::Stored);

            // obliterated
            let obl_content = vec![0x20u8; 100];
            let obl_hash = lore_storage::hash_slice(&obl_content);
            fake.put_object_without_metadata(obl_hash, &obl_content);
            fake.set_legacy_metadata_row(
                obl_hash,
                Fragment {
                    flags: FragmentFlags::PayloadObliterated.bits(),
                    size_payload: obl_content.len() as u32,
                    size_content: obl_content.len() as u64,
                },
            );

            // uncompressed conversion
            let unc = vec![0x30u8; 150];
            let unc_hash = lore_storage::hash_slice(&unc);
            fake.put_object_without_metadata(unc_hash, &unc);
            fake.set_legacy_metadata_row(
                unc_hash,
                Fragment {
                    flags: 0,
                    size_payload: unc.len() as u32,
                    size_content: unc.len() as u64,
                },
            );

            // zstd — already correct codec, maintained in place
            let zstd_content = vec![0x40u8; 500];
            let (zstd_frag, zstd_compressed, zstd_hash) = make_zstd_payload(&zstd_content);
            fake.put_object_without_metadata(zstd_hash, &zstd_compressed);
            fake.set_legacy_metadata_row(zstd_hash, zstd_frag);

            // lz4 — recompressed to zstd
            let lz4_content = vec![0x50u8; 500];
            let (lz4_frag, lz4_compressed, lz4_hash) = make_lz4_payload(&lz4_content);
            fake.put_object_without_metadata(lz4_hash, &lz4_compressed);
            fake.set_legacy_metadata_row(lz4_hash, lz4_frag);

            let (tx, rx) = mpsc::channel(10);
            for h in [migrated, obl_hash, unc_hash, zstd_hash, lz4_hash] {
                tx.send(h).await.unwrap();
            }
            drop(tx);

            let stats = RewriteStats::default();
            let aborted = Arc::new(AtomicBool::new(false));
            migrator
                .fragment_stream_consumer(Arc::new(Mutex::new(rx)), &stats, aborted)
                .await
                .unwrap();

            assert_eq!(stats.skipped_migrated.load(Ordering::Relaxed), 1);
            assert_eq!(stats.skipped_obliterated.load(Ordering::Relaxed), 1);
            assert_eq!(stats.converted_uncompressed.load(Ordering::Relaxed), 1);
            assert_eq!(stats.maintained_zstd.load(Ordering::Relaxed), 1);
            assert_eq!(stats.converted_zstd.load(Ordering::Relaxed), 1);
        }

        #[tokio::test]
        async fn aborted_flag_stops_consumer_without_processing() {
            let fake = Fake::default();
            let migrator = make_migrator(&fake).await;
            let (tx, rx) = mpsc::channel::<Hash>(10);
            for _ in 0..5 {
                tx.send(rand::random()).await.unwrap();
            }
            let stats = RewriteStats::default();
            let aborted = Arc::new(AtomicBool::new(true));
            migrator
                .fragment_stream_consumer(Arc::new(Mutex::new(rx)), &stats, aborted)
                .await
                .unwrap();
            assert_eq!(stats.converted_zstd.load(Ordering::Relaxed), 0);
            assert_eq!(stats.converted_uncompressed.load(Ordering::Relaxed), 0);
        }
    }
}
