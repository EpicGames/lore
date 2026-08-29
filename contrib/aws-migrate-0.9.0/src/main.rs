// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Migrates every fragment in an AWS immutable store onto the fragment state table.
//!
//! A fragment used to be described by its own `DynamoDB` row. It now travels on the S3 object that
//! carries the payload, and `DynamoDB` keeps only a state row recording that the hash exists.
//! `lore-aws` supplies the migrator that reconciles one to the other, one scan segment at a time.
//! This binary is the operator front end for it: it builds the store from the command line, covers
//! every segment of the legacy metadata table, reports progress while it runs, and exits non-zero
//! unless every segment completed.
//!
//! Running it again is safe and is how an interrupted run is finished: a fragment that already has
//! a state row and an object describing itself is skipped.
//!
//! The store must be offline or in maintenance mode for the whole run, so that nothing else
//! writing to it interferes with the migration. `--dry-run` writes nothing and is safe against a
//! live store, which is how a run is sized before the window is booked.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use clap::Parser;
use lore_aws::clients::AwsClientBuilder;
use lore_aws::clients::HttpClientSettings;
use lore_aws::clients::TimeoutConfig;
use lore_aws::dynamodb::DynamoDb;
use lore_aws::dynamodb::ScanConfig;
use lore_aws::s3::S3;
use lore_aws::store::immutable_store::AwsImmutableStore;
use lore_aws::store::immutable_store::AwsImmutableStoreSettings;
use lore_aws::store::immutable_store::DynamoDbImmutableStoreSettings;
use lore_aws::store::immutable_store::S3StoreSettings;
use lore_aws::store::immutable_store::metadata_migrator::MetadataMigratorConfig;
use lore_aws::store::immutable_store::metadata_migrator::OrchestrationConfig;
use lore_aws::store::immutable_store::metadata_migrator::RewriteStats;
use lore_aws::store::immutable_store::metadata_migrator::log_stats;
use lore_aws::store::immutable_store::metadata_migrator::run_migrator;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use tracing::error;
use tracing::info;
use tracing::warn;
use tracing_subscriber::EnvFilter;

/// What the migration needs to know to reach a store and how hard to drive it.
// Doc comments here are the --help text, so they carry no backticks.
#[allow(clippy::doc_markdown)]
#[derive(Debug, Parser)]
#[command(
    version,
    about = "Migrate every fragment in an AWS immutable store onto the fragment state table",
    long_about = "Migrate every fragment in an AWS immutable store onto the fragment state table.

The store must not be serving traffic: put every node that reaches it into maintenance mode, or \
otherwise out of service, for the whole run. A write that arrives while a fragment is being \
converted can revive an obliterated hash, undoing the obliteration. --dry-run writes nothing and \
is safe to run against a live store.

A zero exit status means every segment reached the end of its scan, not that every fragment was \
migrated: read the unreadable and oversized totals before reopening traffic. An obliterated \
fragment is passed over on purpose and needs no state row, so that total may be anything."
)]
struct Args {
    /// S3 bucket holding fragment payloads.
    #[arg(long, env = "LORE_MIGRATE_S3_BUCKET")]
    s3_bucket: String,

    /// DynamoDB table holding fragment associations.
    #[arg(long, env = "LORE_MIGRATE_FRAGMENTS_TABLE")]
    fragments_table: String,

    /// DynamoDB table holding fragment state, where a row is written for each fragment migrated.
    #[arg(long, env = "LORE_MIGRATE_FRAGMENT_STATE_TABLE")]
    fragment_state_table: String,

    /// DynamoDB table holding legacy fragment metadata. This is the table that is scanned, and the
    /// one a fragment is read from while its S3 object still carries no metadata of its own. On a
    /// deployment that never had a separate table, pass the same name as --fragment-state-table.
    #[arg(long, env = "LORE_MIGRATE_FRAGMENT_METADATA_TABLE")]
    fragment_metadata_table: String,

    /// AWS region. Left unset, the environment's own region resolution applies.
    #[arg(long, env = "AWS_REGION")]
    region: Option<String>,

    /// S3 endpoint URL, for an S3-compatible store.
    #[arg(long)]
    s3_endpoint_url: Option<String>,

    /// DynamoDB endpoint URL, for a DynamoDB-compatible store.
    #[arg(long)]
    dynamodb_endpoint_url: Option<String>,

    /// Address buckets by path rather than by host name, as an S3-compatible store may require.
    #[arg(long)]
    s3_force_path_style: bool,

    /// How many parallel scan segments to divide the metadata table into.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(i32).range(1..))]
    total_segments: i32,

    /// A single segment to run, repeatable. Left unset every segment runs, which is what makes the
    /// migration a full one; naming segments splits one migration across several machines.
    #[arg(long)]
    segment: Vec<i32>,

    /// How many fragments each segment converts at once.
    #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(i32).range(1..))]
    consumers: i32,

    /// Items per scan page. Left unset, DynamoDB decides.
    #[arg(long)]
    scan_limit: Option<i32>,

    /// How many times a failed scan page or fragment conversion is retried before it is given up on.
    #[arg(long, default_value_t = 5)]
    max_retries: usize,

    /// Base retry delay, multiplied by the attempt number and capped at five seconds.
    #[arg(long, default_value_t = 200)]
    retry_base_delay_ms: u64,

    /// Timeout for one S3 or DynamoDB operation. A payload rewrite is one operation, so this
    /// bounds the largest fragment the migration can move.
    #[arg(long, default_value_t = 30_000)]
    timeout_millis: u64,

    /// Log any operation slower than this. Left unset, none is logged for its duration alone.
    #[arg(long)]
    slow_operation_threshold_millis: Option<u64>,

    /// Seconds between progress reports, or zero for none.
    #[arg(long, default_value_t = 30)]
    progress_interval_secs: u64,

    /// Analyse every fragment and report what would change, writing nothing.
    #[arg(long)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    init_tracing();

    match run(args).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            error!("Migration could not start: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Installs the log subscriber, at info level unless `RUST_LOG` asks for something else.
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

/// Migrates the segments this invocation covers, reporting whether all of them completed.
///
/// Completing is per segment, not per fragment: a segment that reached the end of its scan reports
/// success however many fragments it left without a state row. An unreadable payload and one past
/// the size threshold are both skipped and counted, so the totals rather than this answer are what
/// establish that a store is fully migrated. An obliterated fragment is skipped too, but that one
/// is intended — nothing can retrieve it, so it needs no state row.
///
/// Every segment shares one abort flag, so a store that cannot be written stops the whole run
/// rather than being discovered once per segment.
async fn run(args: Args) -> Result<bool> {
    let segments = segments(&args)?;

    let s3 = build_s3(&args).await?;
    let dynamodb = build_dynamodb(&args).await?;
    let store = Arc::new(build_store(s3, dynamodb.clone(), &args));

    let stats = Arc::new(RewriteStats::default());
    let aborted = Arc::new(AtomicBool::new(false));

    info!(
        bucket = %args.s3_bucket,
        metadata_table = %args.fragment_metadata_table,
        state_table = %args.fragment_state_table,
        ?segments,
        total_segments = args.total_segments,
        consumers_per_segment = args.consumers,
        dry_run = args.dry_run,
        "Migrating fragments onto the fragment state table"
    );

    stop_on_interrupt(aborted.clone());
    let progress = report_progress(stats.clone(), args.progress_interval_secs);

    let mut runs = JoinSet::new();
    for segment in segments {
        let config = MetadataMigratorConfig {
            dynamodb: dynamodb.clone(),
            store: store.clone(),
            metadata_table_name: Arc::from(args.fragment_metadata_table.as_str()),
            api_call_max_retries: args.max_retries,
            api_retry_base_delay: Duration::from_millis(args.retry_base_delay_ms),
            scan_config: ScanConfig {
                segment: Some(segment),
                total_segments: Some(args.total_segments),
                limit: args.scan_limit,
            },
            is_dry_run: args.dry_run,
        };
        let orchestration = OrchestrationConfig {
            num_consumers: args.consumers,
        };
        let stats = stats.clone();
        let aborted = aborted.clone();

        // This tool carries no execution context to propagate, as the migrator itself does not.
        #[allow(clippy::disallowed_methods)]
        runs.spawn(async move {
            (
                segment,
                run_migrator(config, orchestration, stats, aborted).await,
            )
        });
    }

    let mut incomplete = Vec::new();
    let mut unfinished = 0usize;
    while let Some(joined) = runs.join_next().await {
        match joined {
            Ok((segment, true)) => info!(segment, "Segment migrated"),
            Ok((segment, false)) => {
                error!(segment, "Segment did not complete");
                incomplete.push(segment);
            }
            Err(error) => {
                error!("A segment stopped without reporting: {error}");
                aborted.store(true, Ordering::Relaxed);
                unfinished += 1;
            }
        }
    }

    if let Some(progress) = progress {
        progress.abort();
    }
    log_stats("final", &stats);

    let interrupted = aborted.load(Ordering::Relaxed);
    if incomplete.is_empty() && unfinished == 0 && !interrupted {
        info!("Migration complete");
        return Ok(true);
    }

    warn!(
        ?incomplete,
        unfinished, interrupted, "Migration incomplete: run the same command again to resume"
    );
    Ok(false)
}

/// The segments to run: those named, or all of them when none is.
fn segments(args: &Args) -> Result<Vec<i32>> {
    if args.segment.is_empty() {
        return Ok((0..args.total_segments).collect());
    }

    for segment in &args.segment {
        if !(0..args.total_segments).contains(segment) {
            bail!(
                "segment {segment} lies outside the 0..{} this run is divided into",
                args.total_segments
            );
        }
    }

    let mut segments = args.segment.clone();
    segments.sort_unstable();
    segments.dedup();
    Ok(segments)
}

/// Opens the payload bucket, failing if it is not there to be read.
async fn build_s3(args: &Args) -> Result<S3> {
    AwsClientBuilder::builder()
        .with_http_settings(&HttpClientSettings::default())
        .maybe_endpoint(args.s3_endpoint_url.clone())
        .maybe_region(args.region.clone())
        .with_timeout_config(operation_timeout(args))
        .build_config()
        .await
        .with_slow_operation_threshold(slow_operation_threshold(args))
        .s3_with_path_style(args.s3_force_path_style)
        .ensure_bucket(&args.s3_bucket)
        .build()
        .await
        .map_err(|error| anyhow!("S3 bucket {} unusable: {error}", args.s3_bucket))
}

/// Opens the three tables the migration reads or writes, failing if any is missing.
async fn build_dynamodb(args: &Args) -> Result<DynamoDb> {
    AwsClientBuilder::builder()
        .with_http_settings(&HttpClientSettings::default())
        .maybe_endpoint(args.dynamodb_endpoint_url.clone())
        .maybe_region(args.region.clone())
        .with_timeout_config(operation_timeout(args))
        .build_config()
        .await
        .with_slow_operation_threshold(slow_operation_threshold(args))
        .dynamodb()
        .ensure_table(&args.fragments_table)
        .ensure_table(&args.fragment_state_table)
        .ensure_table(&args.fragment_metadata_table)
        .build()
        .await
        .map_err(|error| anyhow!("DynamoDB tables unusable: {error}"))
}

/// Builds the store the migration reads through, with the legacy metadata table as its fallback.
///
/// That fallback is the whole point: a fragment that has not been migrated has no metadata on its
/// S3 object, and the row in the metadata table is the only thing that still describes it.
fn build_store(s3: S3, dynamodb: DynamoDb, args: &Args) -> AwsImmutableStore {
    let s3_settings = S3StoreSettings {
        bucket: args.s3_bucket.clone(),
        endpoint_url: args.s3_endpoint_url.clone(),
        region: args.region.clone(),
        slow_operation_threshold_millis: slow_operation_threshold(args),
        timeout_millis: args.timeout_millis,
    };

    let dynamodb_settings = DynamoDbImmutableStoreSettings {
        fragments_table_name: args.fragments_table.clone(),
        fragment_state_table_name: args.fragment_state_table.clone(),
        fragment_metadata_table_name: Some(args.fragment_metadata_table.clone()),
        endpoint_url: args.dynamodb_endpoint_url.clone(),
        region: args.region.clone(),
        slow_operation_threshold_millis: slow_operation_threshold(args),
        timeout_millis: args.timeout_millis,
    };

    let settings = AwsImmutableStoreSettings::new(s3_settings, dynamodb_settings, false);
    AwsImmutableStore::new(s3, dynamodb, &settings)
}

/// The per-operation timeout, which a payload rewrite has to fit inside.
fn operation_timeout(args: &Args) -> TimeoutConfig {
    TimeoutConfig::builder()
        .operation_timeout(Duration::from_millis(args.timeout_millis))
        .build()
}

/// The duration past which an operation is logged as slow, never when unset.
fn slow_operation_threshold(args: &Args) -> u64 {
    args.slow_operation_threshold_millis.unwrap_or(u64::MAX)
}

/// Turns the first interrupt into an abort, so the run stops between fragments rather than during
/// one and the operator can resume from where it stopped.
fn stop_on_interrupt(aborted: Arc<AtomicBool>) {
    #[allow(clippy::disallowed_methods)]
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            warn!("Interrupted: finishing the fragments in flight, then stopping");
            aborted.store(true, Ordering::Relaxed);
        }
    });
}

/// Logs the running totals every `interval_secs`, or not at all when that is zero.
fn report_progress(stats: Arc<RewriteStats>, interval_secs: u64) -> Option<JoinHandle<()>> {
    if interval_secs == 0 {
        return None;
    }

    #[allow(clippy::disallowed_methods)]
    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        ticker.tick().await;
        loop {
            ticker.tick().await;
            log_stats("progress", &stats);
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_with(total_segments: i32, segment: Vec<i32>) -> Args {
        Args {
            s3_bucket: "bucket".to_owned(),
            fragments_table: "fragments".to_owned(),
            fragment_state_table: "fragment-state".to_owned(),
            fragment_metadata_table: "metadata".to_owned(),
            region: None,
            s3_endpoint_url: None,
            dynamodb_endpoint_url: None,
            s3_force_path_style: false,
            total_segments,
            segment,
            consumers: 16,
            scan_limit: None,
            max_retries: 5,
            retry_base_delay_ms: 200,
            timeout_millis: 30_000,
            slow_operation_threshold_millis: None,
            progress_interval_secs: 30,
            dry_run: false,
        }
    }

    #[test]
    fn no_segment_named_covers_every_segment() {
        assert_eq!(segments(&args_with(4, vec![])).unwrap(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn a_single_undivided_run_is_one_segment() {
        assert_eq!(segments(&args_with(1, vec![])).unwrap(), vec![0]);
    }

    #[test]
    fn named_segments_are_ordered_and_deduplicated() {
        assert_eq!(segments(&args_with(4, vec![3, 1, 3])).unwrap(), vec![1, 3]);
    }

    #[test]
    fn a_segment_outside_the_division_is_rejected() {
        assert!(segments(&args_with(4, vec![4])).is_err());
        assert!(segments(&args_with(4, vec![-1])).is_err());
    }

    #[test]
    fn slow_operation_threshold_defaults_to_never() {
        assert_eq!(slow_operation_threshold(&args_with(1, vec![])), u64::MAX);
    }

    #[test]
    fn command_line_is_valid() {
        use clap::CommandFactory;
        Args::command().debug_assert();
    }
}
