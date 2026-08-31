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
use tokio::task::JoinSet;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::dynamodb::DynamoDb;
use crate::dynamodb::ScanConfig;
use crate::dynamodb::ScanPage;
use crate::store::immutable_store::AwsImmutableStore;
use crate::store::immutable_store::FragmentMetadataEntry;
use crate::store::object_metadata::from_object_metadata;

const REWRITE_RETRY_DELAY_CAP: Duration = Duration::from_secs(5);

/// Running totals for a rewrite run.
#[derive(Debug, Default)]
pub struct RewriteStats {
    // num Fragments in this scan run
    pub scanned: AtomicU64,
    pub valid_metadata_entries: AtomicU64,

    // ========================
    // Outcomes

    // num fragments whose codec was accurate and not Oodle; payload re-uploaded unchanged to set S3 metadata headers
    pub maintained: AtomicU64,
    // num Oodle fragments recompressed to Zstd
    pub recompressed_oodle: AtomicU64,
    // num fragments whose declared codec mismatched the stored bytes; recompressed to Zstd
    pub recompressed_mismatch: AtomicU64,
    // num compressed fragments that ended up uncompressed because compression was inefficient
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
    // num fragments that have a State item but no S3 head - implying a race in writing the same
    // fragment between an old legacy deployment and a new deployment writing to S3 at the same time
    pub state_with_no_head: AtomicU64,
}

/// Logs every total the migrator keeps, labelled with the point in the run it was read at.
///
/// The counters are read one at a time, so a snapshot taken while consumers are running is
/// approximate: it is a progress report, not a reconciliation.
pub fn log_stats(phase: &str, stats: &RewriteStats) {
    info!(
        phase,
        scanned = stats.scanned.load(Ordering::Relaxed),
        metadata_rows = stats.valid_metadata_entries.load(Ordering::Relaxed),
        maintained = stats.maintained.load(Ordering::Relaxed),
        recompressed_oodle = stats.recompressed_oodle.load(Ordering::Relaxed),
        recompressed_mismatch = stats.recompressed_mismatch.load(Ordering::Relaxed),
        stored_uncompressed = stats
            .converted_compressed_to_uncompressed
            .load(Ordering::Relaxed),
        payloads_deduced = stats.payloads_deduced.load(Ordering::Relaxed),
        state_with_no_head = stats.state_with_no_head.load(Ordering::Relaxed),
        already_migrated = stats.skipped_migrated.load(Ordering::Relaxed),
        obliterated = stats.skipped_obliterated.load(Ordering::Relaxed),
        oversized = stats.skipped_malicious.load(Ordering::Relaxed),
        unreadable = stats.could_not_deduce_payload.load(Ordering::Relaxed),
        errored = stats.errored.load(Ordering::Relaxed),
        "Migration totals"
    );
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
    // Accurate codec and not Oodle: payload re-uploaded unchanged to set S3 metadata headers.
    Maintained,
    // Oodle payload: recompressed to Zstd.
    RecompressedOodle,
    // Codec mismatch: recompressed to Zstd.
    RecompressedMismatch,
    // Recompression was inefficient; stored uncompressed instead.
    ConvertedCompressedToUncompressed,
}

pub struct OrchestrationConfig {
    pub num_consumers: i32,
}

pub struct MetadataMigratorConfig {
    pub dynamodb: DynamoDb,
    pub store: Arc<AwsImmutableStore>,
    pub metadata_table_name: Arc<str>,

    pub api_call_max_retries: usize,
    pub api_retry_base_delay: Duration,

    pub scan_config: ScanConfig,

    /// When `true`, fragments are analysed but no writes occur:
    /// `write_payload_and_state` is skipped and a `process_fragment` error
    /// does not abort the consumer loop.
    pub is_dry_run: bool,
}

pub struct MetadataMigrator {
    dynamodb: DynamoDb,
    store: Arc<AwsImmutableStore>,
    metadata_table_name: Arc<str>,

    api_call_max_retries: usize,
    api_retry_base_delay: Duration,

    scan_config: ScanConfig,
    is_dry_run: bool,
}

impl MetadataMigrator {
    pub fn new(config: MetadataMigratorConfig) -> Self {
        Self {
            dynamodb: config.dynamodb,
            store: config.store,
            metadata_table_name: config.metadata_table_name,
            api_call_max_retries: config.api_call_max_retries,
            api_retry_base_delay: config.api_retry_base_delay,
            scan_config: config.scan_config,
            is_dry_run: config.is_dry_run,
        }
    }

    /// Scan the metadata table page by page, enqueueing the hash of every
    /// fragment that does not have a state entry
    pub async fn discover_legacy_fragments(
        self: Arc<Self>,
        tx: mpsc::Sender<Hash>,
        stats: Arc<RewriteStats>,
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
            if let Ok(s3_head) = self.store.s3_head_object(hash).await
                && from_object_metadata(s3_head.metadata()).is_ok()
            {
                return Ok(ConvertOutcome::SkippedMigrated);
            }
            stats.state_with_no_head.fetch_add(1, Ordering::Relaxed);
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

        let is_oodle = original_fragment.flags & FragmentFlags::PayloadCompressedOodle2 != 0;

        let (new_fragment, new_payload, outcome) =
            match decompress_hash(original_fragment, &original_payload, hash) {
                DecompressOutcome::CouldNotDeduce => {
                    return Ok(ConvertOutcome::CouldNotDeducePayload);
                }

                DecompressOutcome::PayloadAccurate(_, _) if !is_oodle => {
                    // Codec is declared correctly and is not Oodle: re-upload the same payload to
                    // set the S3 object metadata headers, then write state.
                    (
                        original_fragment,
                        original_payload,
                        ConvertOutcome::Maintained,
                    )
                }

                DecompressOutcome::PayloadAccurate(decompressed_fragment, decompressed) => {
                    // Oodle payload with correct codec: recompress to Zstd.
                    recompress_to_zstd(
                        decompressed_fragment,
                        decompressed,
                        ConvertOutcome::RecompressedOodle,
                    )?
                }

                DecompressOutcome::PayloadDeduced(decompressed_fragment, decompressed) => {
                    stats.payloads_deduced.fetch_add(1, Ordering::Relaxed);
                    // Codec mismatch: recompress to Zstd.
                    recompress_to_zstd(
                        decompressed_fragment,
                        decompressed,
                        ConvertOutcome::RecompressedMismatch,
                    )?
                }
            };

        if !self.is_dry_run {
            let mut attempt = 0;
            loop {
                match self
                    .store
                    .write_payload_and_state(hash, new_fragment, new_payload.clone())
                    .await
                {
                    Ok(()) => break,
                    Err(e) => {
                        if attempt < self.api_call_max_retries {
                            attempt += 1;
                            warn!(hash = %hash, error = ?e, attempt, "write_payload_and_state failed; retrying");
                            rewrite_backoff(self.api_retry_base_delay, attempt).await;
                            continue;
                        }
                        return Err(e);
                    }
                }
            }
        }

        Ok(outcome)
    }

    pub async fn fragment_stream_consumer(
        self: Arc<Self>,
        rx: Arc<Mutex<mpsc::Receiver<Hash>>>,
        stats: Arc<RewriteStats>,
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

            // `None` means a dry-run error: the fragment failed but we continue the loop.
            let process_outcome: Option<ConvertOutcome> = {
                let mut attempt = 0;
                loop {
                    match self.process_fragment(hash, &stats).await {
                        Ok(outcome) => break Some(outcome),
                        Err(e) => {
                            if attempt < self.api_call_max_retries {
                                attempt += 1;
                                warn!(hash = %hash, error = ?e, attempt, "fragment conversion failed; retrying");
                                rewrite_backoff(self.api_retry_base_delay, attempt).await;
                                continue;
                            }
                            error!(hash = %hash, error = ?e, "fragment conversion failed after retries; giving up on fragment");
                            stats.errored.fetch_add(1, Ordering::Relaxed);
                            if self.is_dry_run {
                                break None;
                            }
                            return Err(e);
                        }
                    }
                }
            };
            let Some(process_outcome) = process_outcome else {
                continue;
            };

            match process_outcome {
                ConvertOutcome::Maintained => {
                    stats.maintained.fetch_add(1, Ordering::Relaxed);
                }
                ConvertOutcome::RecompressedOodle => {
                    stats.recompressed_oodle.fetch_add(1, Ordering::Relaxed);
                }
                ConvertOutcome::RecompressedMismatch => {
                    stats.recompressed_mismatch.fetch_add(1, Ordering::Relaxed);
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

/// Compress a decompressed fragment to Zstd, tagging the success case with `on_success`.
/// Falls back to uncompressed with `ConvertedCompressedToUncompressed` if Zstd cannot beat
/// the size threshold.
fn recompress_to_zstd(
    decompressed_fragment: Fragment,
    decompressed: Bytes,
    on_success: ConvertOutcome,
) -> Result<(Fragment, Bytes, ConvertOutcome), StoreError> {
    match lore_storage::compress(
        decompressed_fragment,
        &decompressed,
        lore_storage::CompressionMode::Zstd,
    ) {
        Ok((fragment, payload)) => Ok((fragment, payload, on_success)),
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

/// Starts the consumers, each drawing from the one receiver.
///
/// The receiver is left held by the consumers alone, so the channel closes as the last of them
/// stops. Discovery waiting to hand over a hash then fails its send instead of waiting on consumers
/// that have already gone — which is what an aborted run, and one whose consumers all failed,
/// leaves behind.
fn spawn_consumers(
    migrator: &Arc<MetadataMigrator>,
    rx: mpsc::Receiver<Hash>,
    stats: &Arc<RewriteStats>,
    aborted: &Arc<AtomicBool>,
    orchestration_config: &OrchestrationConfig,
) -> JoinSet<Result<(), StoreError>> {
    let rx = Arc::new(Mutex::new(rx));
    let mut consumers = JoinSet::new();

    for _ in 0..orchestration_config.num_consumers {
        // no execution context in migrator runtime
        #[allow(clippy::disallowed_methods)]
        consumers.spawn(migrator.clone().fragment_stream_consumer(
            rx.clone(),
            stats.clone(),
            aborted.clone(),
        ));
    }

    consumers
}

pub async fn run_migrator(
    migrator_config: MetadataMigratorConfig,
    orchestration_config: OrchestrationConfig,
    stats: Arc<RewriteStats>,
    aborted: Arc<AtomicBool>,
) -> bool {
    let is_dry_run = migrator_config.is_dry_run;
    info!(
        is_dry_run,
        num_consumers = orchestration_config.num_consumers,
        segment = migrator_config.scan_config.segment,
        total_segments = migrator_config.scan_config.total_segments,
        "Starting migrator"
    );

    let migrator = Arc::new(MetadataMigrator::new(migrator_config));

    let (tx, rx) = mpsc::channel((orchestration_config.num_consumers * 2) as usize);

    // no execution context in migrator runtime
    #[allow(clippy::disallowed_methods)]
    let discover_task = tokio::spawn(migrator.clone().discover_legacy_fragments(
        tx,
        stats.clone(),
        aborted.clone(),
    ));

    let mut consumers = spawn_consumers(&migrator, rx, &stats, &aborted, &orchestration_config);

    let mut num_consumer_errors = 0;
    while let Some(handle) = consumers.join_next().await {
        match handle {
            Ok(consumer_result) => match consumer_result {
                Ok(_) => {
                    info!("Consumer completed");
                }
                Err(error) => {
                    warn!(%error, "Consumer failed");
                    num_consumer_errors += 1;
                    aborted.store(true, Ordering::Relaxed);
                }
            },
            Err(error) => {
                warn!(%error, "Consumer orchestration error");
                num_consumer_errors += 1;
                aborted.store(true, Ordering::Relaxed);
            }
        }
    }

    // A scan that gave up short of the end leaves rows nobody looked at, so the segment did not
    // complete however cleanly its consumers finished.
    let discovery_task_ok = match discover_task.await {
        Ok(Ok(())) => {
            info!("Discovery task completed");
            true
        }
        Ok(Err(error)) => {
            warn!(%error, "Discovery stopped short of the end of the metadata table");
            false
        }
        Err(error) => {
            warn!(%error, "Discovery task failed");
            false
        }
    };

    let num_error_stats = stats.errored.load(Ordering::Relaxed);
    let is_aborted = aborted.load(Ordering::Relaxed);
    info!(
        is_dry_run,
        discovery_task_ok,
        num_consumer_errors,
        num_error_stats,
        is_aborted,
        ?stats,
        "Migrator tasks complete"
    );

    if !discovery_task_ok || num_consumer_errors > 0 || num_error_stats > 0 || is_aborted {
        warn!(is_dry_run, "Migration segment incomplete");
        false
    } else {
        info!(is_dry_run, "Migration segment completed");
        true
    }
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

    async fn make_migrator(fake: &Fake) -> Arc<MetadataMigrator> {
        let migrator = MetadataMigrator {
            dynamodb: crate::dynamodb::MockDynamoDb::default(),
            store: store_with_separate_metadata_table(fake).await,
            metadata_table_name: FRAGMENT_METADATA_TABLE_NAME.into(),
            api_call_max_retries: 0,
            api_retry_base_delay: Duration::ZERO,
            scan_config: ScanConfig::default(),
            is_dry_run: false,
        };
        Arc::new(migrator)
    }

    async fn make_dry_run_migrator(fake: &Fake) -> Arc<MetadataMigrator> {
        let migrator = MetadataMigrator {
            dynamodb: crate::dynamodb::MockDynamoDb::default(),
            store: store_with_separate_metadata_table(fake).await,
            metadata_table_name: FRAGMENT_METADATA_TABLE_NAME.into(),
            api_call_max_retries: 0,
            api_retry_base_delay: Duration::ZERO,
            scan_config: ScanConfig::default(),
            is_dry_run: true,
        };
        Arc::new(migrator)
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

        fn decompressed_fragment(content: &[u8]) -> (Fragment, Bytes) {
            // Simulate what decompress_hash returns: a fragment with no compression flags,
            // size_payload == size_content == content length.
            let frag = Fragment {
                flags: 0,
                size_payload: content.len() as u32,
                size_content: content.len() as u64,
            };
            (frag, Bytes::copy_from_slice(content))
        }

        mod recompress_to_zstd_tests {
            use super::*;

            #[test]
            fn compressible_data_recompresses_to_zstd_and_round_trips() {
                let content = vec![0xAAu8; 500];
                let (frag, decompressed) = decompressed_fragment(&content);

                let (out_frag, out_payload, outcome) =
                    recompress_to_zstd(frag, decompressed, ConvertOutcome::RecompressedOodle)
                        .unwrap();
                assert_eq!(outcome, ConvertOutcome::RecompressedOodle);
                assert_ne!(out_frag.flags & FragmentFlags::PayloadCompressedZstd, 0);
                assert_eq!(out_frag.size_content, content.len() as u64);
                assert_eq!(out_frag.size_payload as usize, out_payload.len());

                let (_, roundtripped) = lore_storage::decompress(out_frag, &out_payload).unwrap();
                assert_eq!(roundtripped.as_ref(), content.as_slice());
            }

            #[test]
            fn on_success_outcome_is_forwarded() {
                let content = vec![0xBBu8; 500];
                let (frag, decompressed) = decompressed_fragment(&content);

                let (_, _, outcome) =
                    recompress_to_zstd(frag, decompressed, ConvertOutcome::RecompressedMismatch)
                        .unwrap();
                assert_eq!(outcome, ConvertOutcome::RecompressedMismatch);
            }

            #[test]
            fn incompressible_data_falls_back_to_uncompressed() {
                let content: Vec<u8> = (0u8..=32).collect(); // 33 bytes, no repetition
                let (frag, decompressed) = decompressed_fragment(&content);

                let (out_frag, out_payload, outcome) = recompress_to_zstd(
                    frag,
                    decompressed.clone(),
                    ConvertOutcome::RecompressedOodle,
                )
                .unwrap();
                assert_eq!(outcome, ConvertOutcome::ConvertedCompressedToUncompressed);
                assert_eq!(out_frag.flags & FragmentFlags::PayloadCompressed, 0);
                assert_eq!(out_payload, decompressed);
                assert_eq!(out_frag.size_content, content.len() as u64);
                assert_eq!(out_frag.size_payload as usize, out_payload.len());
            }
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

        async fn make_discover_migrator(dynamodb: MockDynamoDb) -> Arc<MetadataMigrator> {
            let fake = Fake::default();
            let migrator = MetadataMigrator {
                dynamodb,
                store: store(&fake).await,
                metadata_table_name: "test-metadata".into(),
                api_call_max_retries: 0,
                api_retry_base_delay: Duration::ZERO,
                scan_config: ScanConfig::default(),
                is_dry_run: false,
            };
            Arc::new(migrator)
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
            let stats = Arc::new(RewriteStats::default());
            let aborted = Arc::new(AtomicBool::new(false));
            migrator
                .discover_legacy_fragments(tx, stats.clone(), aborted)
                .await
                .unwrap();
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
            let stats = Arc::new(RewriteStats::default());
            let aborted = Arc::new(AtomicBool::new(false));
            migrator
                .discover_legacy_fragments(tx, stats, aborted)
                .await
                .unwrap();
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
            let stats = Arc::new(RewriteStats::default());
            let aborted = Arc::new(AtomicBool::new(true));
            migrator
                .discover_legacy_fragments(tx, stats.clone(), aborted)
                .await
                .unwrap();
            assert_eq!(stats.scanned.load(Ordering::Relaxed), 0);
        }
    }

    mod process_fragment_tests {
        use super::*;

        #[tokio::test]
        async fn skips_already_migrated() {
            // A fully migrated fragment has both a state entry and a properly-headered S3 object.
            // Migrate a legacy fragment first so both are set up correctly, then verify the
            // second call skips cleanly without touching state_with_no_head.
            let fake = Fake::default();
            let migrator = make_migrator(&fake).await;
            let content = vec![0x11u8; 100];
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
            migrator
                .process_fragment(hash, &RewriteStats::default())
                .await
                .unwrap();

            let stats = RewriteStats::default();
            assert_eq!(
                migrator.process_fragment(hash, &stats).await.unwrap(),
                ConvertOutcome::SkippedMigrated
            );
            assert_eq!(stats.state_with_no_head.load(Ordering::Relaxed), 0);
        }

        #[tokio::test]
        async fn state_with_no_head_is_counted_and_fragment_is_reprocessed() {
            // State entry exists but head_fragment returns 404 — a race between deployments.
            // The stat is incremented and processing falls through to load the fragment from
            // the legacy path and re-migrate it. publish_state handles the pre-existing Stored
            // row via its RowAbsent conditional write, so the write succeeds without error.
            let fake = Fake::default();
            let migrator = make_migrator(&fake).await;
            let content = vec![0x42u8; 150];
            let hash = lore_storage::hash_slice(&content);
            fake.set_state(hash, FragmentState::Stored);
            // put_object_once: visible to get_object (load) but not head_object (head_fragment)
            fake.put_object_once(hash, &content);
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
                ConvertOutcome::Maintained
            );
            assert_eq!(stats.state_with_no_head.load(Ordering::Relaxed), 1);
            assert_eq!(fake.state_of(hash), Some(FragmentState::Stored));
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
        async fn maintains_uncompressed_writes_state() {
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
                ConvertOutcome::Maintained
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
                ConvertOutcome::Maintained
            );
            assert_eq!(fake.state_of(hash), Some(FragmentState::Stored));
        }

        #[tokio::test]
        async fn maintains_lz4_writes_state() {
            let fake = Fake::default();
            let migrator = make_migrator(&fake).await;
            let content = vec![0x55u8; 500];
            let (frag, compressed, hash) = make_lz4_payload(&content);
            fake.put_object_without_metadata(hash, &compressed);
            fake.set_legacy_metadata_row(hash, frag);
            let stats = RewriteStats::default();
            assert_eq!(
                migrator.process_fragment(hash, &stats).await.unwrap(),
                ConvertOutcome::Maintained
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
            let stats = Arc::new(RewriteStats::default());
            assert_eq!(
                migrator.process_fragment(hash, &stats).await.unwrap(),
                ConvertOutcome::CouldNotDeducePayload
            );
        }

        #[tokio::test]
        async fn skips_malicious_when_size_content_exceeds_threshold() {
            let fake = Fake::default();
            let migrator = make_migrator(&fake).await;
            let content = vec![0x42u8; 64];
            let hash: Hash = lore_storage::hash_slice(&content);
            fake.put_object_without_metadata(hash, &content);
            fake.set_legacy_metadata_row(
                hash,
                Fragment {
                    flags: FragmentFlags::PayloadCompressedZstd.bits(),
                    size_payload: content.len() as u32,
                    size_content: lore_storage::FRAGMENT_SIZE_THRESHOLD as u64 + 1,
                },
            );
            let stats = RewriteStats::default();
            assert_eq!(
                migrator.process_fragment(hash, &stats).await.unwrap(),
                ConvertOutcome::SkippedMaliciousFragment
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

        #[tokio::test]
        async fn dry_run_skips_write_payload_and_state() {
            let fake = Fake::default();
            let migrator = make_dry_run_migrator(&fake).await;
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
            // Outcome is still reported correctly — only the write is suppressed.
            assert_eq!(
                migrator.process_fragment(hash, &stats).await.unwrap(),
                ConvertOutcome::Maintained
            );
            assert_eq!(fake.state_of(hash), None, "dry_run must not write state");
        }

        #[tokio::test]
        async fn mismatch_recompresses_to_zstd_and_writes_state() {
            let fake = Fake::default();
            let migrator = make_migrator(&fake).await;
            let content = vec![0x77u8; 500];
            let (mut frag, compressed, hash) = make_zstd_payload(&content);
            // Declare LZ4 but store Zstd bytes — a codec mismatch.
            frag.flags = (frag.flags & !FragmentFlags::PayloadCompressed)
                | FragmentFlags::PayloadCompressedLZ4.bits();
            fake.put_object_without_metadata(hash, &compressed);
            fake.set_legacy_metadata_row(hash, frag);
            let stats = RewriteStats::default();
            assert_eq!(
                migrator.process_fragment(hash, &stats).await.unwrap(),
                ConvertOutcome::RecompressedMismatch
            );
            assert_eq!(stats.payloads_deduced.load(Ordering::Relaxed), 1);
            assert_eq!(fake.state_of(hash), Some(FragmentState::Stored));
        }
    }

    mod fragment_stream_consumer_tests {
        use super::*;

        #[tokio::test]
        async fn stops_cleanly_when_channel_closed() {
            let fake = Fake::default();
            let migrator = make_migrator(&fake).await;
            let (_, rx) = mpsc::channel::<Hash>(10);
            let stats = Arc::new(RewriteStats::default());
            let aborted = Arc::new(AtomicBool::new(false));
            migrator
                .fragment_stream_consumer(Arc::new(Mutex::new(rx)), stats.clone(), aborted)
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
            let stats = Arc::new(RewriteStats::default());
            let aborted = Arc::new(AtomicBool::new(false));
            migrator
                .fragment_stream_consumer(Arc::new(Mutex::new(rx)), stats.clone(), aborted)
                .await
                .unwrap();
            assert_eq!(stats.maintained.load(Ordering::Relaxed), 1);
            assert_eq!(fake.state_of(hash), Some(FragmentState::Stored));
        }

        #[tokio::test]
        async fn stats_accumulate_across_multiple_outcomes() {
            let fake = Fake::default();
            let migrator = make_migrator(&fake).await;

            // already migrated: run process_fragment once so both state and a
            // properly-headered S3 object exist, making head_fragment succeed on the second pass.
            let migrated_content = vec![0x10u8; 100];
            let migrated = lore_storage::hash_slice(&migrated_content);
            fake.put_object_without_metadata(migrated, &migrated_content);
            fake.set_legacy_metadata_row(
                migrated,
                Fragment {
                    flags: 0,
                    size_payload: migrated_content.len() as u32,
                    size_content: migrated_content.len() as u64,
                },
            );
            migrator
                .process_fragment(migrated, &RewriteStats::default())
                .await
                .unwrap();

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

            // lz4 — accurate codec, maintained in place
            let lz4_content = vec![0x50u8; 500];
            let (lz4_frag, lz4_compressed, lz4_hash) = make_lz4_payload(&lz4_content);
            fake.put_object_without_metadata(lz4_hash, &lz4_compressed);
            fake.set_legacy_metadata_row(lz4_hash, lz4_frag);

            let (tx, rx) = mpsc::channel(10);
            for h in [migrated, obl_hash, unc_hash, zstd_hash, lz4_hash] {
                tx.send(h).await.unwrap();
            }
            drop(tx);

            let stats = Arc::new(RewriteStats::default());
            let aborted = Arc::new(AtomicBool::new(false));
            migrator
                .fragment_stream_consumer(Arc::new(Mutex::new(rx)), stats.clone(), aborted)
                .await
                .unwrap();

            assert_eq!(stats.skipped_migrated.load(Ordering::Relaxed), 1);
            assert_eq!(stats.skipped_obliterated.load(Ordering::Relaxed), 1);
            // uncompressed, zstd, and lz4 fragments all have accurate codecs: maintained in place
            assert_eq!(stats.maintained.load(Ordering::Relaxed), 3);
        }

        #[tokio::test]
        async fn dry_run_error_continues_loop_without_stopping() {
            // One fragment has no S3 object → load() returns an error.
            // One fragment is valid and uncompressed.
            // In dry_run mode the consumer must process both rather than aborting on the error.
            let fake = Fake::default();
            let migrator = make_dry_run_migrator(&fake).await;

            // Fragment that will error: metadata row exists but no S3 object.
            let error_content = vec![0x01u8; 100];
            let (error_frag, _compressed, error_hash) = make_zstd_payload(&error_content);
            fake.set_legacy_metadata_row(error_hash, error_frag);

            // Fragment that succeeds: uncompressed, valid.
            let ok_content = vec![0x02u8; 100];
            let ok_hash = lore_storage::hash_slice(&ok_content);
            fake.put_object_without_metadata(ok_hash, &ok_content);
            fake.set_legacy_metadata_row(
                ok_hash,
                Fragment {
                    flags: 0,
                    size_payload: ok_content.len() as u32,
                    size_content: ok_content.len() as u64,
                },
            );

            let (tx, rx) = mpsc::channel(10);
            tx.send(error_hash).await.unwrap();
            tx.send(ok_hash).await.unwrap();
            drop(tx);

            let stats = Arc::new(RewriteStats::default());
            let aborted = Arc::new(AtomicBool::new(false));
            // Should complete without returning an error despite the first fragment failing.
            migrator
                .fragment_stream_consumer(Arc::new(Mutex::new(rx)), stats.clone(), aborted)
                .await
                .unwrap();

            assert_eq!(stats.errored.load(Ordering::Relaxed), 1);
            assert_eq!(stats.maintained.load(Ordering::Relaxed), 1);
            // dry_run: neither fragment should have been written to state.
            assert_eq!(fake.state_of(ok_hash), None);
        }

        #[tokio::test]
        async fn aborted_flag_stops_consumer_without_processing() {
            let fake = Fake::default();
            let migrator = make_migrator(&fake).await;
            let (tx, rx) = mpsc::channel::<Hash>(10);
            for _ in 0..5 {
                tx.send(rand::random()).await.unwrap();
            }
            let stats = Arc::new(RewriteStats::default());
            let aborted = Arc::new(AtomicBool::new(true));
            migrator
                .fragment_stream_consumer(Arc::new(Mutex::new(rx)), stats.clone(), aborted)
                .await
                .unwrap();
            assert_eq!(stats.maintained.load(Ordering::Relaxed), 0);
            assert_eq!(stats.recompressed_mismatch.load(Ordering::Relaxed), 0);
        }
    }

    mod run_migrator_tests {
        use super::*;
        use crate::dynamodb::MockDynamoDb;
        use crate::dynamodb::ScanPage;

        async fn make_config(fake: &Fake, dynamodb: MockDynamoDb) -> MetadataMigratorConfig {
            MetadataMigratorConfig {
                dynamodb,
                store: store_with_separate_metadata_table(fake).await,
                metadata_table_name: FRAGMENT_METADATA_TABLE_NAME.into(),
                api_call_max_retries: 0,
                api_retry_base_delay: Duration::ZERO,
                scan_config: ScanConfig::default(),
                is_dry_run: false,
            }
        }

        #[tokio::test]
        async fn empty_scan_returns_true() {
            let fake = Fake::default();
            let mut dynamodb = MockDynamoDb::default();
            dynamodb.expect_scan_page().returning(|_, _, _| {
                Ok(ScanPage {
                    items: vec![],
                    last_evaluated_key: None,
                })
            });
            let config = make_config(&fake, dynamodb).await;
            let stats = Arc::new(RewriteStats::default());
            let aborted = Arc::new(AtomicBool::new(false));
            assert!(
                run_migrator(
                    config,
                    OrchestrationConfig { num_consumers: 2 },
                    stats,
                    aborted
                )
                .await
            );
        }

        #[tokio::test]
        async fn pre_aborted_returns_false() {
            let fake = Fake::default();
            let config = make_config(&fake, MockDynamoDb::default()).await;
            let stats = Arc::new(RewriteStats::default());
            let aborted = Arc::new(AtomicBool::new(true));
            assert!(
                !run_migrator(
                    config,
                    OrchestrationConfig { num_consumers: 2 },
                    stats,
                    aborted
                )
                .await
            );
        }

        #[tokio::test]
        async fn fragments_are_migrated_and_stats_updated() {
            let fake = Fake::default();

            let content = vec![0x42u8; 500];
            let (frag, compressed, hash) = make_zstd_payload(&content);
            fake.put_object_without_metadata(hash, &compressed);
            fake.set_legacy_metadata_row(hash, frag);

            let item = HashMap::from([(
                "hash".to_owned(),
                AttributeValue::B(Blob::new(hash.data().to_vec())),
            )]);
            let mut dynamodb = MockDynamoDb::default();
            dynamodb
                .expect_scan_page()
                .returning(move |_, start_key, _| {
                    if start_key.is_none() {
                        Ok(ScanPage {
                            items: vec![item.clone()],
                            last_evaluated_key: None,
                        })
                    } else {
                        Ok(ScanPage {
                            items: vec![],
                            last_evaluated_key: None,
                        })
                    }
                });

            let config = make_config(&fake, dynamodb).await;
            let stats = Arc::new(RewriteStats::default());
            let aborted = Arc::new(AtomicBool::new(false));

            assert!(
                run_migrator(
                    config,
                    OrchestrationConfig { num_consumers: 2 },
                    stats.clone(),
                    aborted,
                )
                .await
            );

            assert_eq!(stats.maintained.load(Ordering::Relaxed), 1);
            assert_eq!(fake.state_of(hash), Some(FragmentState::Stored));
        }

        #[tokio::test]
        async fn consumer_error_returns_false() {
            let fake = Fake::default();

            let content = vec![0x42u8; 500];
            let (frag, _compressed, hash) = make_zstd_payload(&content);
            // Set up the metadata row so discovery returns the hash, but omit the S3 object.
            // load() will hit NoSuchKey → StoreError::AddressNotFound → consumer error.
            fake.set_legacy_metadata_row(hash, frag);

            let item = HashMap::from([(
                "hash".to_owned(),
                AttributeValue::B(Blob::new(hash.data().to_vec())),
            )]);
            let mut dynamodb = MockDynamoDb::default();
            dynamodb
                .expect_scan_page()
                .returning(move |_, start_key, _| {
                    if start_key.is_none() {
                        Ok(ScanPage {
                            items: vec![item.clone()],
                            last_evaluated_key: None,
                        })
                    } else {
                        Ok(ScanPage {
                            items: vec![],
                            last_evaluated_key: None,
                        })
                    }
                });

            let config = make_config(&fake, dynamodb).await;
            let stats = Arc::new(RewriteStats::default());
            let aborted = Arc::new(AtomicBool::new(false));

            assert!(
                !run_migrator(
                    config,
                    OrchestrationConfig { num_consumers: 2 },
                    stats.clone(),
                    aborted,
                )
                .await
            );

            assert_eq!(stats.errored.load(Ordering::Relaxed), 1);
        }

        /// A scan that keeps failing leaves most of the table unread, so the segment did not
        /// complete — however cleanly the consumers that had nothing to do finished.
        #[tokio::test]
        async fn a_run_whose_scan_gave_up_reports_the_segment_incomplete() {
            let fake = Fake::default();
            let mut dynamodb = MockDynamoDb::default();
            dynamodb.expect_scan_page().returning(|_, _, _| {
                Err(crate::store::test_util::throughput_exceeded(
                    aws_sdk_dynamodb::operation::scan::ScanError::ProvisionedThroughputExceededException(
                        crate::store::test_util::throttling_exception(),
                    ),
                ))
            });

            let config = make_config(&fake, dynamodb).await;

            assert!(
                !run_migrator(
                    config,
                    OrchestrationConfig { num_consumers: 2 },
                    Arc::new(RewriteStats::default()),
                    Arc::new(AtomicBool::new(false)),
                )
                .await,
                "a segment whose scan gave up did not complete"
            );
        }

        /// Discovery hands hashes over a bounded channel, so a page wider than that channel leaves
        /// it waiting to send. Every consumer here fails on its first fragment and stops, which is
        /// also the shape an interrupted run ends in: the run has to notice they have gone rather
        /// than wait on them.
        #[tokio::test]
        async fn a_run_whose_consumers_have_all_stopped_ends_rather_than_waiting_on_them() {
            let fake = Fake::default();

            // Metadata rows with no S3 object behind them, so every fragment fails to load.
            let items: Vec<_> = std::iter::repeat_with(|| {
                let hash: Hash = rand::random();
                HashMap::from([(
                    "hash".to_owned(),
                    AttributeValue::B(Blob::new(hash.data().to_vec())),
                )])
            })
            .take(64)
            .collect();

            let mut dynamodb = MockDynamoDb::default();
            dynamodb
                .expect_scan_page()
                .returning(move |_, start_key, _| {
                    if start_key.is_none() {
                        Ok(ScanPage {
                            items: items.clone(),
                            last_evaluated_key: None,
                        })
                    } else {
                        Ok(ScanPage {
                            items: vec![],
                            last_evaluated_key: None,
                        })
                    }
                });

            let config = make_config(&fake, dynamodb).await;
            let run = run_migrator(
                config,
                OrchestrationConfig { num_consumers: 2 },
                Arc::new(RewriteStats::default()),
                Arc::new(AtomicBool::new(false)),
            );

            assert!(
                !tokio::time::timeout(Duration::from_secs(30), run)
                    .await
                    .expect("the run should end rather than wait on a channel nothing drains"),
                "a run every consumer stopped in did not complete"
            );
        }

        #[tokio::test]
        async fn multiple_consumers_empty_scan_returns_true() {
            let fake = Fake::default();
            let mut dynamodb = MockDynamoDb::default();
            dynamodb.expect_scan_page().returning(|_, _, _| {
                Ok(ScanPage {
                    items: vec![],
                    last_evaluated_key: None,
                })
            });
            let config = make_config(&fake, dynamodb).await;
            let stats = Arc::new(RewriteStats::default());
            let aborted = Arc::new(AtomicBool::new(false));
            assert!(
                run_migrator(
                    config,
                    OrchestrationConfig { num_consumers: 4 },
                    stats,
                    aborted
                )
                .await
            );
        }
    }
}
