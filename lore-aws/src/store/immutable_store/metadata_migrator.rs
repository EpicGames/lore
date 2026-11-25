use std::mem::size_of;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
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

use crate::aws::item::Item;
use crate::aws::item::from_item;
use crate::dynamodb::DynamoDb;
use crate::dynamodb::ScanConfig;
use crate::dynamodb::ScanPage;
use crate::store::immutable_store::AwsImmutableStore;
use crate::store::immutable_store::FragmentMetadataEntry;
use crate::store::object_metadata::from_object_metadata;
use crate::store::store_failure;

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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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
        let mut start_key: Option<Item> = None;

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
                        return Err(store_failure(
                            e,
                            "DynamoDB scan failed after retries; aborting discovery",
                        ));
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
    #[lore_macro::test_pub]
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
        if (original_fragment.flags & FragmentFlags::PayloadCompressed > 0)
            && (original_fragment.flags & FragmentFlags::PayloadFragmented > 0)
        {
            warn!(hash = %hash, "a valid fragment cannot be compressed and fragmented");
            return Ok(ConvertOutcome::SkippedMaliciousFragment);
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

#[lore_macro::test_pub]
fn parse_metadata_entry(item: &Item) -> Option<Hash> {
    let entry: FragmentMetadataEntry = from_item::<FragmentMetadataEntry>(item.clone())
        .inspect_err(|e| {
            warn!(?e, ?item, "Failed to parse fragment from item");
        })
        .ok()?;
    Some(entry.hash)
}

/// Compress a decompressed fragment to Zstd, tagging the success case with `on_success`.
/// Falls back to uncompressed with `ConvertedCompressedToUncompressed` if Zstd cannot beat
/// the size threshold.
#[lore_macro::test_pub]
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

#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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
