// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Deterministic crash windows. Each scenario drives a store to an exact point, often a pause
//! point the store's `test-util` build offers, takes a snapshot of its directory there, which is
//! what a process killed at that point leaves, and opens the snapshot as the next process would.
//!
//! A scenario passes when the store keeps its guarantees: every payload served is the one stored,
//! what a flush acknowledged survives, and an obliteration stays obliterated. One that fails names
//! the guarantee broken and how.

use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic;
use std::time::Duration;

use lore_storage::FlushOptions;
use lore_storage::LocalImmutableStore;
use lore_storage::StoreError;
use lore_storage::StoreMatchResult;
use lore_storage::StoreObliterateStats;
use lore_storage::immutable_store::ImmutableStore;
use lore_storage::local::immutable_store::CompactionInstruments;
use lore_storage::local::immutable_store::ImmutableStoreGroup;
use lore_storage::local::immutable_store::Lookup;
use lore_storage::test_util::Pause;
use lore_storage::test_util::PausePoint;
use lore_storage::test_util::pause_at;

use crate::args::Args;
use crate::client::Stores;
use crate::model::Config;
use crate::model::Served;
use crate::snapshot::copy_tree;
use crate::verify::Report;

/// The group every key of the scenarios' keyspace lands in.
pub const GROUP: u8 = 0x10;
const LIMIT: u64 = 3 * 1024 * 1024 * 1024;

enum Outcome {
    Pass(String),
    Fail(String),
    /// A loss or corruption only a machine failure, outside the suite's scope, would expose:
    /// reported, not failed.
    Report(String),
}

type Scenario = fn(PathBuf) -> std::pin::Pin<Box<dyn Future<Output = Outcome>>>;

fn boxed(
    future: impl Future<Output = Outcome> + 'static,
) -> std::pin::Pin<Box<dyn Future<Output = Outcome>>> {
    Box::pin(future)
}

macro_rules! scenario {
    ($function:ident) => {
        (
            stringify!($function),
            (|dir| boxed($function(dir))) as Scenario,
        )
    };
}

fn scenarios() -> Vec<(&'static str, Scenario)> {
    vec![
        scenario!(flush_waits_for_a_claimed_bucket_write),
        scenario!(obliterate_then_crash_before_index_flush),
        scenario!(obliteration_survives_a_store_under_another_partition),
        scenario!(obliteration_survives_a_new_payload_under_another_partition),
        scenario!(stale_new_file_from_an_interrupted_fan_out),
        scenario!(crash_at_every_compaction_point),
        scenario!(crash_at_every_fan_out_write),
        scenario!(crash_with_a_packfile_write_in_flight),
        scenario!(crash_with_a_reserved_packfile_range),
        scenario!(crash_around_a_packfile_filling),
        scenario!(cancelled_fan_out_commit_keeps_flushed_entries),
        scenario!(cancelled_compaction_leaves_the_store_writable),
        scenario!(stat_failure_while_loading_packfiles),
        scenario!(failed_packfile_open_leaves_the_store_writable),
        scenario!(sync_failure_blocks_durable_flushes),
        scenario!(durable_flush_syncs_what_a_delayed_flush_wrote),
        scenario!(read_racing_compaction_truncate),
        scenario!(store_racing_compaction_stop),
        scenario!(copy_and_force_put_racing_compaction),
        scenario!(evicted_payload_restored_after_crash),
        scenario!(put_refuses_invalid_fragments),
        scenario!(flags_agree_with_payload_after_deduplication),
        scenario!(metadata_put_on_an_obliterated_address),
        scenario!(obliterate_a_fragment_list_not_held_locally),
        scenario!(obliterate_a_fragment_list_racing_compaction),
        scenario!(descriptor_exhaustion_at_open),
        scenario!(compaction_interrupted_by_a_fan_out),
        scenario!(size_eviction_interrupted_by_a_fan_out),
        scenario!(a_dropped_compaction_s_bucket_write_lands_before_the_next_flush),
        scenario!(a_successor_store_waits_for_a_dropped_store_s_writes),
        scenario!(a_store_opened_where_a_kept_store_s_directory_was_replaced_starts_anew),
    ]
}

pub async fn main(args: &Args) -> u8 {
    let name = args
        .positional
        .get(1)
        .cloned()
        .unwrap_or_else(|| "list".to_string());
    let scratch = args.get("scratch").map_or_else(
        || std::env::temp_dir().join(format!("store-stress-scenarios-{}", std::process::id())),
        PathBuf::from,
    );
    let selected = match select(&name, scenarios()) {
        Ok(selected) => selected,
        Err(code) => return code,
    };
    if selected.len() > 1 {
        // Each in a process of its own: the release profile aborts on a panic, as from a debug
        // assertion, which would end the others with it.
        return run_each_in_a_process(&selected, &scratch, args.flag("keep"));
    }
    let mut failed = 0;
    for (name, scenario) in selected {
        let dir = scratch.join(name);
        let _ = std::fs::remove_dir_all(&dir);
        if let Err(err) = std::fs::create_dir_all(&dir) {
            eprintln!("{name}: cannot create {}: {err}", dir.display());
            return 2;
        }
        // A panic, as from a debug assertion, fails the scenario rather than the run.
        let outcome =
            futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(scenario(dir))).await;
        match outcome {
            Ok(Outcome::Pass(detail)) => println!("PASS {name}: {detail}"),
            Ok(Outcome::Report(detail)) => println!("REPORT {name}: {detail}"),
            Ok(Outcome::Fail(detail)) => {
                failed += 1;
                println!("FAIL {name}: {detail}");
            }
            Err(panic) => {
                failed += 1;
                let message = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_string()))
                    .unwrap_or_default();
                println!("FAIL {name}: panicked: {message}");
            }
        }
    }
    if !args.flag("keep") {
        let _ = std::fs::remove_dir_all(&scratch);
    }
    u8::from(failed > 0)
}

/// The entries of `all` that `name` names: every one for `all`, or those a comma-separated list
/// names. `list` prints their names instead, and a name that matches none is an error; both end the
/// run with the code returned.
pub fn select<T>(name: &str, all: Vec<(&'static str, T)>) -> Result<Vec<(&'static str, T)>, u8> {
    if name == "list" {
        for (name, _) in &all {
            println!("{name}");
        }
        return Err(0);
    }
    let selected: Vec<_> = all
        .into_iter()
        .filter(|(entry, _)| name == "all" || name.split(',').any(|wanted| wanted == *entry))
        .collect();
    if selected.is_empty() {
        eprintln!("no entry named {name}");
        return Err(2);
    }
    Ok(selected)
}

fn run_each_in_a_process(selected: &[(&'static str, Scenario)], scratch: &Path, keep: bool) -> u8 {
    let Ok(exe) = std::env::current_exe() else {
        eprintln!("cannot find this executable");
        return 2;
    };
    let mut failed = 0;
    for (name, _) in selected {
        let mut command = std::process::Command::new(&exe);
        command.args(["scenario", name, "--scratch"]).arg(scratch);
        if keep {
            command.arg("--keep");
        }
        let output = command.output();
        match output {
            Ok(output) => {
                print!("{}", String::from_utf8_lossy(&output.stdout));
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !output.status.success() {
                    failed += 1;
                    if output.status.code().is_none() || output.status.code() == Some(134) {
                        let reason: Vec<&str> = stderr
                            .lines()
                            .filter(|line| line.contains("panicked") || line.starts_with("  "))
                            .take(6)
                            .collect();
                        println!(
                            "FAIL {name}: the process died ({}): {}",
                            output.status,
                            reason.join(" | ")
                        );
                    }
                }
            }
            Err(err) => {
                failed += 1;
                println!("FAIL {name}: could not run: {err}");
            }
        }
    }
    u8::from(failed > 0)
}

/// The keyspace scenarios use: every key in group [`GROUP`], placement and size picked by search.
pub fn config(fan_out_threshold: usize) -> Config {
    let mut config = Config::from_args(&Args::parse(std::iter::empty()));
    config.seed = 7;
    config.keys = 1 << 22;
    config.groups = vec![GROUP];
    config.fan_out_threshold = fan_out_threshold;
    config.initial_level = 1;
    config
}

/// Keys of `config` whose hash's bucket byte lies in `buckets` and whose payload length lies in
/// `lengths`, `count` of them, none a fragment list or in `taken`, which they are added to.
pub fn keys(
    config: &Config,
    buckets: std::ops::Range<u8>,
    lengths: std::ops::Range<usize>,
    count: usize,
    taken: &mut HashSet<u64>,
) -> Vec<u64> {
    let mut found = vec![];
    for key in 0..config.keys {
        if found.len() == count {
            break;
        }
        if taken.contains(&key) || config.is_list(key) {
            continue;
        }
        let bucket = config.hash(key).data()[1];
        if buckets.contains(&bucket) && lengths.contains(&config.payload_len(key)) {
            found.push(key);
            taken.insert(key);
        }
    }
    assert_eq!(
        found.len(),
        count,
        "the keyspace holds the keys a scenario needs"
    );
    found
}

/// A client store over `dir` whose delayed flush never fires on its own, so that a scenario
/// decides every write of the index.
async fn open(dir: &Path, config: &Config) -> Arc<LocalImmutableStore> {
    std::fs::create_dir_all(dir).expect("store directory");
    let _ = std::fs::write(dir.join(crate::model::CONFIG_FILE), config.serialize());
    LocalImmutableStore::new(Some(dir.to_path_buf()), config.settings(false, 3600))
        .await
        .expect("store opens")
}

/// [`open`], with a span of the store held, as a command holds one, so that the lock is not
/// released between operations: a release closes the packfiles, and the next operation's claim
/// reloads the store under whatever the scenario holds part way, as a compaction.
async fn open_in_span(
    dir: &Path,
    config: &Config,
) -> (
    Arc<LocalImmutableStore>,
    Option<lore_storage::local::store_lock::StoreGuard>,
) {
    let store = open(dir, config).await;
    let span = dyn_store(&store)
        .hold_for_command()
        .await
        .expect("the store is claimed");
    (store, span)
}

fn dyn_store(store: &Arc<LocalImmutableStore>) -> Arc<dyn ImmutableStore> {
    store.clone()
}

async fn put(store: &Arc<LocalImmutableStore>, config: &Config, key: u64, variant: u32) {
    dyn_store(store)
        .put(
            config.partition(variant),
            config.address(key, variant),
            config.fragment(key, variant),
            Some(config.payload(key)),
            false,
        )
        .await
        .expect("put succeeds");
}

async fn try_put(
    store: &Arc<LocalImmutableStore>,
    config: &Config,
    key: u64,
    variant: u32,
) -> Result<(), StoreError> {
    dyn_store(store)
        .put(
            config.partition(variant),
            config.address(key, variant),
            config.fragment(key, variant),
            Some(config.payload(key)),
            false,
        )
        .await
}

async fn obliterate(store: &Arc<LocalImmutableStore>, config: &Config, key: u64, variant: u32) {
    dyn_store(store)
        .obliterate(
            config.partition(variant),
            config.address(key, variant),
            Arc::new(StoreObliterateStats::default()),
        )
        .await
        .expect("obliterate succeeds");
}

async fn flush(store: &Arc<LocalImmutableStore>, sync: bool) {
    dyn_store(store)
        .flush(FlushOptions {
            sync_data: sync,
            ..FlushOptions::default()
        })
        .await
        .expect("flush succeeds");
}

/// What a read of `key` under `variant` serves.
#[derive(Debug, PartialEq, Eq)]
enum Read {
    Served(Served),
    NotFound,
    PayloadNotFound,
    Failed(String),
}

async fn read(store: &Arc<LocalImmutableStore>, config: &Config, key: u64, variant: u32) -> Read {
    match dyn_store(store)
        .get(config.partition(variant), config.address(key, variant))
        .await
    {
        Ok(data) => match data.payload {
            Some(payload) => Read::Served(config.classify(key, &payload)),
            None => Read::Failed("no payload".to_string()),
        },
        Err(StoreError::AddressNotFound(_)) => Read::NotFound,
        Err(StoreError::PayloadNotFound(_)) => Read::PayloadNotFound,
        Err(err) => Read::Failed(err.to_string()),
    }
}

/// Wait for the `n`th task to reach `point` after `task` starts, holding it there, or the number
/// that reached it when none more does within `wait`. A pause left armed then is released ahead,
/// so that a later task reaching the point passes it.
async fn pause_nth_within(
    point: PausePoint,
    n: usize,
    wait: Duration,
    task: impl FnOnce(),
) -> Result<Arc<Pause>, usize> {
    let mut pause = pause_at(point);
    task();
    for reached in 1..=n {
        if tokio::time::timeout(wait, pause.reached.notified())
            .await
            .is_err()
        {
            pause.resume.notify_one();
            return Err(reached - 1);
        }
        if reached == n {
            break;
        }
        let next = pause_at(point);
        pause.resume.notify_one();
        pause = next;
    }
    Ok(pause)
}

/// As [`pause_nth_within`], for a point the scenario knows is reached.
async fn pause_nth(point: PausePoint, n: usize, task: impl FnOnce()) -> Arc<Pause> {
    match pause_nth_within(point, n, Duration::from_secs(20), task).await {
        Ok(pause) => pause,
        Err(reached) => panic!("{point:?} reached {reached} times, not {n}"),
    }
}

/// Snapshot `dir` as a process killed now leaves it, into `name` beside it.
fn crash(dir: &Path, name: &str) -> PathBuf {
    let snapshot = dir.with_file_name(format!(
        "{}-{name}",
        dir.file_name().unwrap_or_default().to_string_lossy()
    ));
    let _ = std::fs::remove_dir_all(&snapshot);
    copy_tree(dir, &snapshot).expect("snapshot");
    // A snapshot of a store that is still open copies its lock file, which is no lock.
    snapshot
}

/// Open the store in `dir` and check its index against its packfiles and every key in `reads`,
/// each of which must read back correct when listed in `present`.
pub async fn check(
    dir: &Path,
    config: &Config,
    reads: &[(u64, u32)],
    present: &[(u64, u32)],
) -> Report {
    let mut report = Report::default();
    let store = open(dir, config).await;
    if let Err(err) = dyn_store(&store).load_index().await {
        report.violation("load-index-failed", err.to_string());
    }
    let keys_by_hash = crate::verify::keys_by_hash(config);
    crate::verify::structure(
        &store,
        dir,
        config,
        &keys_by_hash,
        &HashSet::new(),
        &mut report,
        "snapshot",
        false,
    )
    .await;
    for &(key, variant) in reads {
        match read(&store, config, key, variant).await {
            Read::Served(Served::Correct) => {}
            Read::Served(served) => {
                report.violation(
                    "corrupt-read",
                    format!("key {key} variant {variant}: {served}"),
                );
            }
            Read::NotFound | Read::PayloadNotFound if present.contains(&(key, variant)) => {
                report.violation("lost", format!("key {key} variant {variant}"));
            }
            Read::NotFound | Read::PayloadNotFound => {}
            Read::Failed(err) => {
                report.violation("unreadable", format!("key {key} variant {variant}: {err}"));
            }
        }
    }
    report
}

fn outcome(report: &Report, pass: &str) -> Outcome {
    if report.violation_count() == 0 {
        Outcome::Pass(pass.to_string())
    } else {
        report.print();
        Outcome::Fail(format!(
            "{} violations, printed above",
            report.violation_count()
        ))
    }
}

/// A flush must not return while an entry acknowledged before it is only in a bucket write that
/// another flush claimed and has not finished: a process killed once it returns loses the entry.
async fn flush_waits_for_a_claimed_bucket_write(dir: PathBuf) -> Outcome {
    let config = config(1000);
    let mut taken = HashSet::new();
    let [first, second] = keys(&config, 0..255, 32..4096, 2, &mut taken)[..] else {
        unreachable!()
    };
    let store = open(&dir, &config).await;
    put(&store, &config, first, 0).await;
    flush(&store, false).await;
    put(&store, &config, second, 0).await;

    let weak = Arc::downgrade(&store);
    let mut delayed = None;
    let pause = pause_nth(PausePoint::Serializing, 1, || {
        delayed = Some(lore_base::lore_spawn!(ImmutableStoreGroup::flush_delayed(
            weak,
            GROUP as usize,
            Duration::ZERO
        )));
    })
    .await;
    let flushing = dyn_store(&store);
    let mut flush =
        lore_base::lore_spawn!(async move { flushing.flush(FlushOptions::default()).await });
    let returned = tokio::time::timeout(Duration::from_secs(2), &mut flush).await;
    let outcome = match &returned {
        Ok(Ok(Ok(()))) => {
            let snapshot = crash(&dir, "killed-after-flush");
            let reopened = open(&snapshot, &config).await;
            match read(&reopened, &config, second, 0).await {
                Read::Served(Served::Correct) => Outcome::Pass("the entry was on disk".to_string()),
                other => Outcome::Fail(format!(
                    "flush() returned Ok while the delayed flush that had claimed the bucket was \
                     still writing it; a process killed then reads the acknowledged entry as {other:?}"
                )),
            }
        }
        Ok(other) => Outcome::Fail(format!("flush failed: {other:?}")),
        Err(_) => Outcome::Pass("flush waited for the claimed write".to_string()),
    };
    pause.resume.notify_one();
    if let Some(delayed) = delayed {
        let _ = delayed.await;
    }
    if returned.is_err() {
        let _ = flush.await;
    }
    outcome
}

/// An obliterate zeroes the payload before its tombstone reaches the bucket file. A process
/// killed in between must not leave a live entry serving the zeros.
async fn obliterate_then_crash_before_index_flush(dir: PathBuf) -> Outcome {
    let config = config(1000);
    let key = keys(&config, 0..255, 64..4096, 1, &mut HashSet::new())[0];
    let store = open(&dir, &config).await;
    put(&store, &config, key, 0).await;
    flush(&store, false).await;
    obliterate(&store, &config, key, 0).await;
    let snapshot = crash(&dir, "killed-after-obliterate");
    let reopened = open(&snapshot, &config).await;
    match read(&reopened, &config, key, 0).await {
        Read::NotFound => Outcome::Pass("the tombstone was on disk".to_string()),
        Read::Served(Served::Zeros) => Outcome::Fail(
            "obliterate zeroed and synced the payload, but the tombstone waits for the delayed \
             index flush: a process killed in between reopens with the live entry serving zeros \
             as the content"
                .to_string(),
        ),
        other => Outcome::Fail(format!("the obliterated entry reads as {other:?}")),
    }
}

/// An obliterated association must stay obliterated when the same content is stored under
/// another partition.
async fn obliteration_survives_a_store_under_another_partition(dir: PathBuf) -> Outcome {
    let config = config(1000);
    let key = keys(&config, 0..255, 64..4096, 1, &mut HashSet::new())[0];
    let (kept, obliterated, stored) = (2, 0, 4);
    let store = open(&dir, &config).await;
    put(&store, &config, key, obliterated).await;
    put(&store, &config, key, kept).await;
    obliterate(&store, &config, key, obliterated).await;
    if read(&store, &config, key, obliterated).await != Read::NotFound {
        return Outcome::Fail(
            "the obliterated association reads before any other store".to_string(),
        );
    }
    put(&store, &config, key, stored).await;
    obliteration_outcome(&store, &config, key, obliterated).await
}

/// As [`obliteration_survives_a_store_under_another_partition`], with every association
/// obliterated, so that the payload is zeroed, and a new payload stored under another partition.
async fn obliteration_survives_a_new_payload_under_another_partition(dir: PathBuf) -> Outcome {
    let config = config(1000);
    let key = keys(&config, 0..255, 64..4096, 1, &mut HashSet::new())[0];
    let store = open(&dir, &config).await;
    put(&store, &config, key, 0).await;
    put(&store, &config, key, 2).await;
    obliterate(&store, &config, key, 0).await;
    obliterate(&store, &config, key, 2).await;
    put(&store, &config, key, 4).await;
    obliteration_outcome(&store, &config, key, 0).await
}

async fn obliteration_outcome(
    store: &Arc<LocalImmutableStore>,
    config: &Config,
    key: u64,
    obliterated: u32,
) -> Outcome {
    let read_back = read(store, config, key, obliterated).await;
    let mut results = [StoreMatchResult::default()];
    let _ = dyn_store(store)
        .query(
            config.partition(obliterated),
            &[config.address(key, obliterated)],
            &mut results,
        )
        .await;
    flush(store, false).await;
    match read_back {
        Read::NotFound => Outcome::Pass("the association stays obliterated".to_string()),
        other => Outcome::Fail(format!(
            "storing the same content under another partition rewrote the tombstone through the \
             deduplication pass: the obliterated association reads as {other:?} and query reports \
             {:?}",
            results[0].match_made
        )),
    }
}

fn write_level_pending(group_dir: &Path, level: u32) {
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(b"LVNO");
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&level.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    std::fs::write(group_dir.join("level.pending"), bytes).expect("level.pending");
}

/// The directory of group [`GROUP`] of the immutable store in `dir`.
pub fn group_dir(dir: &Path) -> PathBuf {
    dir.join("immutable")
        .join("index")
        .join(format!("{GROUP:02x}"))
}

/// A fan-out commit killed before `level.pending` leaves `.new` files no open removes. The next
/// commit to the same level writes `.new` files only for buckets holding entries, and recovery
/// from a kill once its `level.pending` is written renames every `.new` file there is, so a
/// bucket empty at the second commit takes the first commit's file. Its entries point where
/// compaction has since truncated and rewritten the packfile.
async fn stale_new_file_from_an_interrupted_fan_out(dir: PathBuf) -> Outcome {
    let config = config(4);
    let mut taken = HashSet::new();
    let old = keys(&config, 0x00..0x08, 512..4096, 4, &mut taken);
    let new = keys(&config, 0x28..0x30, 60_000..65_536, 2, &mut taken);
    let ninth = keys(&config, 0x48..0x50, 512..4096, 1, &mut taken);
    let later = keys(&config, 0x00..0x08, 60_000..65_536, 3, &mut taken);

    // The first process: a flushed level-1 group, then a fan-out to level 32 killed after it
    // wrote the `.new` files of buckets 0 and 5 and before bucket 9's and `level.pending`.
    let store = open(&dir, &config).await;
    for &key in &old {
        put(&store, &config, key, 0).await;
    }
    flush(&store, false).await;
    for &key in new.iter().chain(&ninth) {
        put(&store, &config, key, 0).await;
    }
    let flushing = dyn_store(&store);
    let mut fan_out = None;
    let pause = pause_nth(PausePoint::Serializing, 3, || {
        fan_out = Some(lore_base::lore_spawn!(async move {
            flushing.flush(FlushOptions::default()).await
        }));
    })
    .await;
    let first_crash = crash(&dir, "killed-during-fan-out");
    pause.resume.notify_one();
    if let Some(fan_out) = fan_out {
        let _ = fan_out.await;
    }
    drop(store);
    if !group_dir(&first_crash).join("index_05.new").exists() {
        return Outcome::Fail("the first fan-out left no index_05.new to test with".to_string());
    }

    // The second process: compaction truncates packfile 1, whose new entries the kill lost, new
    // payloads fill it again, and a fan-out to level 32 commits with bucket 5 empty.
    let store = open(&first_crash, &config).await;
    let _ = dyn_store(&store).load_index().await;
    let compacted = store
        .clone()
        .compact_group_packfiles(
            GROUP as usize,
            store.path.clone(),
            0,
            true,
            false,
            CompactionInstruments::default(),
            None,
        )
        .await;
    let sizes = store
        .packstore(GROUP as usize)
        .sizes()
        .await
        .unwrap_or_default();
    if compacted.is_err() || sizes.first().copied().unwrap_or(1) != 0 {
        return Outcome::Fail(format!(
            "compaction did not truncate packfile 1: {compacted:?}, sizes {sizes:?}"
        ));
    }
    for &key in &later {
        put(&store, &config, key, 0).await;
    }
    flush(&store, false).await;
    let level = store.group[GROUP as usize]
        .bucket_count
        .load(atomic::Ordering::Relaxed);
    drop(store);
    if level != 32 {
        return Outcome::Fail(format!("the second fan-out reached level {level}, not 32"));
    }

    // Killed between the second commit's marker write and its removal of `level.pending`.
    let second_crash = crash(&first_crash, "killed-before-pending-removed");
    write_level_pending(&group_dir(&second_crash), 32);
    let reads: Vec<(u64, u32)> = old
        .iter()
        .chain(&new)
        .chain(&ninth)
        .chain(&later)
        .map(|key| (*key, 0))
        .collect();
    let present: Vec<(u64, u32)> = old.iter().chain(&later).map(|key| (*key, 0)).collect();
    let report = check(&second_crash, &config, &reads, &present).await;
    outcome(&report, "no stale .new file came back")
}

/// Kill the process at each point a compaction pauses at, and at each bucket write it makes, and
/// check what the next open finds.
async fn crash_at_every_compaction_point(dir: PathBuf) -> Outcome {
    let config = config(8);
    let mut taken = HashSet::new();
    let kept = keys(&config, 0..255, 512..20_000, 40, &mut taken);
    let dropped = keys(&config, 0..255, 512..20_000, 40, &mut taken);
    let reads: Vec<(u64, u32)> = kept.iter().chain(&dropped).map(|key| (*key, 0)).collect();
    let present: Vec<(u64, u32)> = kept.iter().map(|key| (*key, 0)).collect();
    let mut report = Report::default();
    let points = [
        (PausePoint::Selected, 1),
        (PausePoint::Stopped, 1),
        (PausePoint::Serializing, 1),
        (PausePoint::Serializing, 2),
        (PausePoint::Serializing, 5),
        (PausePoint::Serializing, 9),
        (PausePoint::Syncing, 1),
        (PausePoint::Syncing, 3),
        (PausePoint::Syncing, 6),
    ];
    for (index, (point, nth)) in points.into_iter().enumerate() {
        let run = dir.join(format!("run{index}"));
        let store = open(&run, &config).await;
        for &key in kept.iter().chain(&dropped) {
            put(&store, &config, key, 0).await;
        }
        flush(&store, false).await;
        for &key in &dropped {
            obliterate(&store, &config, key, 0).await;
        }
        flush(&store, false).await;
        let _ = dyn_store(&store).load_index().await;
        let compacting = store.clone();
        let mut task = None;
        let pause = pause_nth_within(point, nth, Duration::from_secs(5), || {
            task = Some(lore_base::lore_spawn!(async move {
                compacting
                    .clone()
                    .compact_group_packfiles(
                        GROUP as usize,
                        compacting.path.clone(),
                        0,
                        true,
                        false,
                        CompactionInstruments::default(),
                        None,
                    )
                    .await
            }));
        })
        .await;
        let Ok(pause) = pause else {
            report.note("point-not-reached", format!("{point:?} #{nth}"));
            if let Some(task) = task {
                task.abort();
            }
            continue;
        };
        let snapshot = crash(&run, "killed");
        pause.resume.notify_one();
        if let Some(task) = task {
            let _ = task.await;
        }
        drop(store);
        let found = check(&snapshot, &config, &reads, &present).await;
        for _ in 0..found.violation_count() {
            report.violation("compaction-crash", format!("killed at {point:?} #{nth}"));
        }
        if found.violation_count() > 0 {
            found.print();
        }
    }
    outcome(&report, "every compaction crash point reopens clean")
}

/// Kill the process at each `.new` write of a fan-out commit, and with its `level.pending` left
/// behind after it committed, and check what the next open finds.
async fn crash_at_every_fan_out_write(dir: PathBuf) -> Outcome {
    let config = config(6);
    let mut taken = HashSet::new();
    let first = keys(&config, 0..255, 64..8_000, 6, &mut taken);
    let second = keys(&config, 0..255, 64..8_000, 60, &mut taken);
    let reads: Vec<(u64, u32)> = first.iter().chain(&second).map(|key| (*key, 0)).collect();
    let present: Vec<(u64, u32)> = first.iter().map(|key| (*key, 0)).collect();
    let mut report = Report::default();
    for nth in 1..=24 {
        let run = dir.join(format!("run{nth}"));
        let store = open(&run, &config).await;
        for &key in &first {
            put(&store, &config, key, 0).await;
        }
        flush(&store, false).await;
        for &key in &second {
            put(&store, &config, key, 0).await;
        }
        let flushing = dyn_store(&store);
        let mut task = None;
        let pause = pause_nth_within(PausePoint::Serializing, nth, Duration::from_secs(3), || {
            task = Some(lore_base::lore_spawn!(async move {
                flushing.flush(FlushOptions::default()).await
            }));
        })
        .await;
        let Ok(pause) = pause else {
            if let Some(task) = task {
                let _ = task.await;
            }
            // Past the last write: the commit finished, so leave its pending marker behind.
            let snapshot = crash(&run, "killed-before-pending-removed");
            let level = store.group[GROUP as usize]
                .bucket_count
                .load(atomic::Ordering::Relaxed) as u32;
            write_level_pending(&group_dir(&snapshot), level);
            drop(store);
            let all: Vec<(u64, u32)> = reads.clone();
            let found = check(&snapshot, &config, &reads, &all).await;
            if found.violation_count() > 0 {
                found.print();
                report.violation("fan-out-crash", "killed before pending removed".to_string());
            }
            break;
        };
        let snapshot = crash(&run, "killed");
        pause.resume.notify_one();
        if let Some(task) = task {
            let _ = task.await;
        }
        drop(store);
        let found = check(&snapshot, &config, &reads, &present).await;
        if found.violation_count() > 0 {
            found.print();
            report.violation("fan-out-crash", format!("killed at write {nth}"));
        }
    }
    outcome(&report, "every fan-out crash point reopens clean")
}

/// A process killed with a packfile write reserved and not landed, then with a bucket file
/// recording that reservation, must lose nothing that landed.
async fn crash_with_a_packfile_write_in_flight(dir: PathBuf) -> Outcome {
    let config = config(1000);
    let mut taken = HashSet::new();
    let landed = keys(&config, 0..255, 64..4096, 6, &mut taken);
    let held = keys(&config, 0..255, 64..4096, 1, &mut taken)[0];
    let store = open(&dir, &config).await;
    for &key in &landed[..3] {
        put(&store, &config, key, 0).await;
    }
    let in_flight = lore_storage::test_util::hold_packfile_write();
    let holding = store.clone();
    let held_config = config.clone();
    let held_put =
        lore_base::lore_spawn!(async move { put(&holding, &held_config, held, 0).await });
    in_flight.issued.notified().await;
    for &key in &landed[3..] {
        put(&store, &config, key, 0).await;
    }
    flush(&store, false).await;
    let snapshot = crash(&dir, "killed-with-write-in-flight");
    held_put.abort();
    in_flight.complete();
    let reads: Vec<(u64, u32)> = landed.iter().chain([&held]).map(|key| (*key, 0)).collect();
    let present: Vec<(u64, u32)> = landed.iter().map(|key| (*key, 0)).collect();
    let report = check(&snapshot, &config, &reads, &present).await;
    outcome(&report, "nothing that landed was lost")
}

/// A process killed with a packfile range reserved and not yet written, after later stores landed
/// past it, must lose nothing that landed, before a flush records them and after.
async fn crash_with_a_reserved_packfile_range(dir: PathBuf) -> Outcome {
    let config = config(1000);
    let mut taken = HashSet::new();
    let landed = keys(&config, 0..255, 64..4096, 6, &mut taken);
    let held = keys(&config, 0..255, 64..4096, 1, &mut taken)[0];
    let store = open(&dir, &config).await;
    for &key in &landed[..3] {
        put(&store, &config, key, 0).await;
    }
    flush(&store, false).await;
    let holding = store.clone();
    let held_config = config.clone();
    let mut held_put = None;
    let pause = pause_nth(PausePoint::Reserved, 1, || {
        held_put = Some(lore_base::lore_spawn!(async move {
            put(&holding, &held_config, held, 0).await;
        }));
    })
    .await;
    let later = tokio::time::timeout(Duration::from_secs(10), async {
        for &key in &landed[3..] {
            try_put(&store, &config, key, 0).await?;
        }
        Ok::<(), StoreError>(())
    })
    .await;
    let unflushed = crash(&dir, "killed-before-a-flush");
    // A flush may wait for the reserved write, which stays held.
    let flushed = tokio::time::timeout(
        Duration::from_secs(3),
        dyn_store(&store).flush(FlushOptions::default()),
    )
    .await;
    let after_flush = matches!(flushed, Ok(Ok(()))).then(|| crash(&dir, "killed-after-a-flush"));
    pause.resume.notify_one();
    if let Some(task) = held_put {
        let _ = task.await;
    }
    drop(store);
    match later {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            return Outcome::Fail(format!("a store past the reserved range failed: {err}"));
        }
        Err(_) => {
            return Outcome::Fail("stores past a reserved range waited for its write".to_string());
        }
    }
    let reads: Vec<(u64, u32)> = landed.iter().chain([&held]).map(|key| (*key, 0)).collect();
    let flushed_before: Vec<(u64, u32)> = landed[..3].iter().map(|key| (*key, 0)).collect();
    let mut report = check(&unflushed, &config, &reads, &flushed_before).await;
    if let Some(after_flush) = after_flush {
        let all: Vec<(u64, u32)> = landed.iter().map(|key| (*key, 0)).collect();
        let found = check(&after_flush, &config, &reads, &all).await;
        for _ in 0..found.violation_count() {
            report.violation("after-flush", "killed after the flush".to_string());
        }
        if found.violation_count() > 0 {
            found.print();
        }
    }
    outcome(&report, "nothing that landed was lost")
}

/// A flush dropped part way through a fan-out commit, as a caller giving up on it drops it, must
/// leave the store able to commit the fan-out later, losing nothing an earlier flush made durable
/// or the later flush covered.
async fn cancelled_fan_out_commit_keeps_flushed_entries(dir: PathBuf) -> Outcome {
    let config = config(6);
    let mut taken = HashSet::new();
    let first = keys(&config, 0..255, 64..8_000, 6, &mut taken);
    let second = keys(&config, 0..255, 64..8_000, 60, &mut taken);
    let reads: Vec<(u64, u32)> = first.iter().chain(&second).map(|key| (*key, 0)).collect();
    let mut report = Report::default();
    let mut reached = 0;
    for nth in [1, 2, 3, 5, 8, 13, 21] {
        let run = dir.join(format!("run{nth}"));
        let store = open(&run, &config).await;
        for &key in &first {
            put(&store, &config, key, 0).await;
        }
        flush(&store, false).await;
        for &key in &second {
            put(&store, &config, key, 0).await;
        }
        let flushing = dyn_store(&store);
        let mut task = None;
        let pause = pause_nth_within(PausePoint::Serializing, nth, Duration::from_secs(3), || {
            task = Some(lore_base::lore_spawn!(async move {
                flushing.flush(FlushOptions::default()).await
            }));
        })
        .await;
        let Ok(pause) = pause else {
            if let Some(task) = task {
                let _ = task.await;
            }
            break;
        };
        reached += 1;
        // The caller gives up: the flush is dropped where it is held.
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        pause.resume.notify_one();
        let flushed = dyn_store(&store).flush(FlushOptions::default()).await;
        let snapshot = crash(&run, "after-a-later-flush");
        drop(store);
        if let Err(err) = flushed {
            report.violation(
                "flush-after-cancel",
                format!("dropped at write {nth}, the next flush failed: {err}"),
            );
            continue;
        }
        let found = check(&snapshot, &config, &reads, &reads).await;
        if found.violation_count() > 0 {
            found.print();
            report.violation("cancelled-commit", format!("dropped at write {nth}"));
        }
    }
    if reached == 0 {
        return Outcome::Fail("no fan-out write was reached".to_string());
    }
    outcome(
        &report,
        &format!("{reached} fan-out commits dropped part way, each committed by the next flush"),
    )
}

/// A compaction dropped at each point it pauses at, writes to the packfiles it rewrites stopped at
/// some, must leave the store taking writes, and lose nothing.
async fn cancelled_compaction_leaves_the_store_writable(dir: PathBuf) -> Outcome {
    let config = config(8);
    let mut taken = HashSet::new();
    let kept = keys(&config, 0..255, 512..20_000, 30, &mut taken);
    let dropped = keys(&config, 0..255, 512..20_000, 30, &mut taken);
    let mut report = Report::default();
    let points = [
        (PausePoint::Selected, 1),
        (PausePoint::Stopped, 1),
        (PausePoint::Serializing, 1),
        (PausePoint::Serializing, 4),
        (PausePoint::Syncing, 1),
        (PausePoint::Syncing, 3),
    ];
    for (index, (point, nth)) in points.into_iter().enumerate() {
        let run = dir.join(format!("run{index}"));
        let store = open(&run, &config).await;
        for &key in kept.iter().chain(&dropped) {
            put(&store, &config, key, 0).await;
        }
        flush(&store, false).await;
        for &key in &dropped {
            obliterate(&store, &config, key, 0).await;
        }
        flush(&store, false).await;
        let _ = dyn_store(&store).load_index().await;
        let compacting = store.clone();
        let mut task = None;
        let pause = pause_nth_within(point, nth, Duration::from_secs(5), || {
            task = Some(lore_base::lore_spawn!(async move {
                compacting
                    .clone()
                    .compact_group_packfiles(
                        GROUP as usize,
                        compacting.path.clone(),
                        0,
                        true,
                        false,
                        CompactionInstruments::default(),
                        None,
                    )
                    .await
            }));
        })
        .await;
        let Ok(pause) = pause else {
            report.note("point-not-reached", format!("{point:?} #{nth}"));
            if let Some(task) = task {
                task.abort();
            }
            continue;
        };
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        pause.resume.notify_one();
        let fresh = keys(&config, 0..255, 512..20_000, 4, &mut taken);
        let stored = tokio::time::timeout(Duration::from_secs(10), async {
            for &key in &fresh {
                try_put(&store, &config, key, 0).await?;
            }
            dyn_store(&store).flush(FlushOptions::default()).await
        })
        .await;
        let snapshot = crash(&run, "after-writes");
        drop(store);
        match stored {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                report.violation(
                    "writes-after-cancelled-compaction",
                    format!("dropped at {point:?} #{nth}: {err}"),
                );
                continue;
            }
            Err(_) => {
                report.violation(
                    "writes-after-cancelled-compaction",
                    format!("dropped at {point:?} #{nth}: stores and the flush hung"),
                );
                continue;
            }
        }
        let reads: Vec<(u64, u32)> = kept
            .iter()
            .chain(&dropped)
            .chain(&fresh)
            .map(|key| (*key, 0))
            .collect();
        let present: Vec<(u64, u32)> = kept.iter().chain(&fresh).map(|key| (*key, 0)).collect();
        let found = check(&snapshot, &config, &reads, &present).await;
        if found.violation_count() > 0 {
            found.print();
            report.violation(
                "cancelled-compaction",
                format!("dropped at {point:?} #{nth}"),
            );
        }
    }
    outcome(
        &report,
        "the store took writes after every dropped compaction, and lost nothing",
    )
}

/// Run `work` on the store in `dir` under the fault shim `$STRESS_SHIM`, with `env` telling the
/// shim what to fail, and which files under `dir` to count when it names `STRESS_PATH`, returning
/// its output, or `None` without a shim.
fn work_under_shim(dir: &Path, args: &[&str], env: &[(&str, String)]) -> Option<String> {
    let shim = std::env::var_os("STRESS_SHIM")?;
    let exe = std::env::current_exe().ok()?;
    let output = std::process::Command::new(exe)
        .arg("work")
        .arg(dir)
        .args(args)
        .env("LD_PRELOAD", shim)
        .env("STRESS_PATH", dir.join("immutable"))
        .envs(env.iter().map(|(name, value)| (*name, value.as_str())))
        .output()
        .ok()?;
    Some(format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

/// A packfile whose size could not be read once while the store loaded, as a transient I/O error
/// leaves it, still holds its payloads: they must not be taken for lost, and must be served once
/// the store is opened again. The store holds two packfiles that both take writes, as after a
/// failed sync took the first off writes until a restart, so that a load missing the second has
/// one to write to and opens no other. Needs the shim, named by `$STRESS_SHIM`.
async fn stat_failure_while_loading_packfiles(dir: PathBuf) -> Outcome {
    if std::env::var_os("STRESS_SHIM").is_none() {
        return Outcome::Pass("skipped: $STRESS_SHIM names no fault shim".to_string());
    }
    let config = config(1000);
    let stored = keys(&config, 0..255, 4096..60_000, 24, &mut HashSet::new());
    let base = dir.join("base");
    {
        let store = open(&base, &config).await;
        for &key in &stored[..12] {
            put(&store, &config, key, 0).await;
        }
        lore_storage::test_util::fail_packfile_syncs(1);
        if dyn_store(&store)
            .flush(FlushOptions::SYNC_DATA)
            .await
            .is_ok()
        {
            return Outcome::Fail("the injected sync failure did not fail the flush".to_string());
        }
        for &key in &stored[12..] {
            put(&store, &config, key, 0).await;
        }
        flush(&store, false).await;
    }
    if !group_dir(&base).join("pack").join("2").exists() {
        return Outcome::Fail("the failed sync left no second packfile to test with".to_string());
    }
    let reads: Vec<(u64, u32)> = stored.iter().map(|key| (*key, 0)).collect();
    let mut report = Report::default();
    for at in 1..=4 {
        let run = dir.join(format!("stat{at}"));
        let _ = std::fs::remove_dir_all(&run);
        copy_tree(&base, &run).expect("copy");
        // The store loads every group's packfiles: only the scenario's group is counted.
        let env = [
            ("STRESS_PATH", format!("{}/", group_dir(&run).display())),
            ("STRESS_OPS", "stat".to_string()),
            ("STRESS_CLASS", "pack".to_string()),
            ("STRESS_FAIL_AT", at.to_string()),
            ("STRESS_FAIL_ERRNO", "EIO".to_string()),
        ];
        let Some(output) = work_under_shim(
            &run,
            &["--ops", "0", "--load-index", "--end", "flush"],
            &env,
        ) else {
            return Outcome::Fail("could not run work under the shim".to_string());
        };
        let found = check(&run, &config, &reads, &reads).await;
        if found.violation_count() > 0 {
            found.print();
            let cleared = output.lines().any(|line| line.contains("Cleared"));
            report.violation(
                "lost-after-a-failed-stat",
                format!("stat {at} of a packfile failed; payload locations cleared: {cleared}"),
            );
        }
    }
    outcome(
        &report,
        "every payload was served after a packfile stat failed at load",
    )
}

/// A store that fails to open the packfile it moves on to when one fills, as with no descriptor
/// left, must take writes again once descriptors are free, in the same process. Each run fails one
/// of the first packfile opens a run of puts makes: the load of the packfile created near the size
/// limit, the next packfile once it fills, and one after. Needs the shim, named by `$STRESS_SHIM`.
async fn failed_packfile_open_leaves_the_store_writable(dir: PathBuf) -> Outcome {
    if std::env::var_os("STRESS_SHIM").is_none() {
        return Outcome::Pass("skipped: $STRESS_SHIM names no fault shim".to_string());
    }
    let config = config(1000);
    let mut probes = vec![];
    for at in 1..=3 {
        let run = dir.join(format!("open{at}"));
        std::fs::create_dir_all(&run).expect("store directory");
        std::fs::write(run.join(crate::model::CONFIG_FILE), config.serialize()).expect("config");
        let env = [
            ("STRESS_OPS", "open".to_string()),
            ("STRESS_CLASS", "pack".to_string()),
            ("STRESS_DURING", "put".to_string()),
            ("STRESS_FAIL_AT", at.to_string()),
            ("STRESS_FAIL_ERRNO", "EMFILE".to_string()),
        ];
        let args = [
            "--prefill-near-limit",
            "64k",
            "--ops",
            "400",
            "--tasks",
            "1",
            "--gc-tasks",
            "0",
            "--mix",
            "*=0,put=1",
            "--liveness",
            "--end",
            "flush",
        ];
        let Some(output) = work_under_shim(&run, &args, &env) else {
            return Outcome::Fail("could not run work under the shim".to_string());
        };
        if let Some(line) = output
            .lines()
            .find(|line| line.contains("stuck-after-faults"))
        {
            return Outcome::Fail(format!(
                "open {at} failed: {}",
                line.chars().take(400).collect::<String>()
            ));
        }
        probes.push(format!(
            "open {at}: {}",
            output
                .lines()
                .find(|line| line.starts_with("probe after"))
                .unwrap_or("no probe ran")
        ));
    }
    if !probes.iter().any(|probe| probe.contains("after 1 faults")) {
        return Outcome::Fail(format!("no packfile open failed: {probes:?}"));
    }
    Outcome::Pass(format!("the store took writes again: {probes:?}"))
}

/// Fill a packfile created near the size limit, killing the process at each step of taking it
/// off writes and settling it.
async fn crash_around_a_packfile_filling(dir: PathBuf) -> Outcome {
    let config = config(1000);
    let pack = group_dir(&dir).join("pack");
    std::fs::create_dir_all(&pack).expect("pack directory");
    std::fs::File::create(pack.join("1"))
        .and_then(|file| file.set_len(LIMIT - 300 * 1024))
        .expect("sparse packfile");
    let keys = keys(&config, 0..255, 60_000..250_000, 12, &mut HashSet::new());
    let (store, _span) = open_in_span(&dir, &config).await;
    let mut report = Report::default();
    let mut stored = vec![];
    for (index, &key) in keys.iter().enumerate() {
        put(&store, &config, key, 0).await;
        stored.push((key, 0));
        flush(&store, false).await;
        let snapshot = crash(&dir, &format!("killed-after-{index}"));
        let found = check(&snapshot, &config, &stored, &stored).await;
        if found.violation_count() > 0 {
            found.print();
            report.violation("fill-crash", format!("killed after store {index}"));
        }
        let _ = std::fs::remove_dir_all(&snapshot);
    }
    let packstore = store.packstore(GROUP as usize);
    let settled = packstore.is_settled(1).await;
    let writeable = packstore.writeable_ids().await;
    if !settled || writeable.contains(&1) {
        report.violation(
            "not-settled",
            format!("packfile 1 settled {settled}, writeable {writeable:?}"),
        );
    }
    outcome(&report, "every kill around the fill reopens clean")
}

/// One failed packfile sync fails every later durable flush of its group until the process
/// restarts, as the design chooses, while plain flushes and reads go on, and a restarted process
/// flushes durably again.
async fn sync_failure_blocks_durable_flushes(dir: PathBuf) -> Outcome {
    let config = config(1000);
    let keys = keys(&config, 0..255, 64..4096, 3, &mut HashSet::new());
    let store = open(&dir, &config).await;
    put(&store, &config, keys[0], 0).await;
    lore_storage::test_util::fail_packfile_syncs(1);
    let first = dyn_store(&store).flush(FlushOptions::SYNC_DATA).await;
    put(&store, &config, keys[1], 0).await;
    let second = dyn_store(&store).flush(FlushOptions::SYNC_DATA).await;
    let plain = dyn_store(&store).flush(FlushOptions::default()).await;
    put(&store, &config, keys[2], 0).await;
    let after = dyn_store(&store).flush(FlushOptions::SYNC_DATA).await;
    let reads = [(keys[0], 0), (keys[1], 0), (keys[2], 0)];
    let mut served = vec![];
    for (key, variant) in reads {
        served.push(read(&store, &config, key, variant).await);
    }
    drop(store);
    let reopened = open(&dir, &config).await;
    let restarted = dyn_store(&reopened).flush(FlushOptions::SYNC_DATA).await;
    if served
        .iter()
        .any(|read| *read != Read::Served(Served::Correct))
    {
        return Outcome::Fail(format!("reads after the failed sync: {served:?}"));
    }
    if first.is_ok() {
        return Outcome::Fail("the injected sync failure did not fail the flush".to_string());
    }
    if let Err(err) = &plain {
        return Outcome::Fail(format!("a plain flush after the failed sync failed: {err}"));
    }
    if let Err(err) = &restarted {
        return Outcome::Fail(format!("a durable flush after a restart failed: {err}"));
    }
    Outcome::Pass(format!(
        "first durable flush {:?}, later ones {:?} and {:?}, a plain flush {:?}, after a restart \
         {:?}: every durable flush of the group fails until the process restarts",
        first.is_ok(),
        second.is_ok(),
        after.is_ok(),
        plain.is_ok(),
        restarted.is_ok()
    ))
}

/// A durable flush promises every write so far is on the storage media. A group whose buckets a
/// delayed flush already wrote has no dirty bucket, and must still have its packfiles synced. Only
/// a machine failure would lose what it leaves unsynced, so a gap is reported rather than failed.
async fn durable_flush_syncs_what_a_delayed_flush_wrote(dir: PathBuf) -> Outcome {
    let config = config(1000);
    let key = keys(&config, 0..255, 64..4096, 1, &mut HashSet::new())[0];
    let store = open(&dir, &config).await;
    put(&store, &config, key, 0).await;
    ImmutableStoreGroup::flush_delayed(Arc::downgrade(&store), GROUP as usize, Duration::ZERO)
        .await;
    flush(&store, true).await;
    let packstore = store.packstore(GROUP as usize);
    let mut unsynced = vec![];
    for id in 1..=packstore.sizes().await.map_or(0, |sizes| sizes.len()) as u32 {
        if packstore.is_dirty(id).await {
            unsynced.push(id);
        }
    }
    if unsynced.is_empty() {
        Outcome::Pass("the packfiles were synced".to_string())
    } else {
        Outcome::Report(format!(
            "flush(sync_data: true) returned with packfiles {unsynced:?} holding writes no sync \
             covered, and the bucket file the delayed flush wrote unsynced: flush_all skips a group \
             with no dirty bucket before reaching the packstore sync. Only a machine failure loses \
             them"
        ))
    }
}

/// A read that located a payload in a packfile compaction then truncates loads it before the
/// truncate, as the payload gate orders.
async fn read_racing_compaction_truncate(dir: PathBuf) -> Outcome {
    let config = config(1000);
    let mut taken = HashSet::new();
    let kept = keys(&config, 0..255, 512..4096, 8, &mut taken);
    let dropped = keys(&config, 0..255, 20_000..60_000, 8, &mut taken);
    let store = open(&dir, &config).await;
    for &key in kept.iter().chain(&dropped) {
        put(&store, &config, key, 0).await;
    }
    for &key in &dropped {
        obliterate(&store, &config, key, 0).await;
    }
    flush(&store, false).await;
    let _ = dyn_store(&store).load_index().await;
    let reading = store.clone();
    let read_config = config.clone();
    let target = kept[3];
    let mut reader = None;
    let pause = pause_nth(PausePoint::Located, 1, || {
        reader = Some(lore_base::lore_spawn!(async move {
            read(&reading, &read_config, target, 0).await
        }));
    })
    .await;
    let compacting = store.clone();
    let mut compaction = lore_base::lore_spawn!(async move {
        compacting
            .clone()
            .compact_group_packfiles(
                GROUP as usize,
                compacting.path.clone(),
                0,
                true,
                false,
                CompactionInstruments::default(),
                None,
            )
            .await
    });
    let blocked = tokio::time::timeout(Duration::from_secs(2), &mut compaction)
        .await
        .is_err();
    pause.resume.notify_one();
    let served = match reader {
        Some(reader) => reader.await.ok(),
        None => None,
    };
    let _ = compaction.await;
    let after = read(&store, &config, target, 0).await;
    match (blocked, served, after) {
        (true, Some(Read::Served(Served::Correct)), Read::Served(Served::Correct)) => {
            Outcome::Pass("the truncate waited for the read".to_string())
        }
        other => Outcome::Fail(format!("blocked, served, after: {other:?}")),
    }
}

/// A store that wrote its payload before compaction stopped writes to the packfile has its entry
/// moved by the rewrite.
async fn store_racing_compaction_stop(dir: PathBuf) -> Outcome {
    let config = config(1000);
    let mut taken = HashSet::new();
    let kept = keys(&config, 0..255, 512..4096, 8, &mut taken);
    let dropped = keys(&config, 0..255, 20_000..60_000, 8, &mut taken);
    let late = keys(&config, 0..255, 512..4096, 1, &mut taken)[0];
    let store = open(&dir, &config).await;
    for &key in kept.iter().chain(&dropped) {
        put(&store, &config, key, 0).await;
    }
    for &key in &dropped {
        obliterate(&store, &config, key, 0).await;
    }
    flush(&store, false).await;
    let _ = dyn_store(&store).load_index().await;
    let storing = store.clone();
    let store_config = config.clone();
    let mut writer = None;
    let pause = pause_nth(PausePoint::Written, 1, || {
        writer = Some(lore_base::lore_spawn!(async move {
            put(&storing, &store_config, late, 0).await;
        }));
    })
    .await;
    let compacting = store.clone();
    let mut compaction = lore_base::lore_spawn!(async move {
        compacting
            .clone()
            .compact_group_packfiles(
                GROUP as usize,
                compacting.path.clone(),
                0,
                true,
                false,
                CompactionInstruments::default(),
                None,
            )
            .await
    });
    let _ = tokio::time::timeout(Duration::from_millis(500), &mut compaction).await;
    pause.resume.notify_one();
    if let Some(writer) = writer {
        let _ = writer.await;
    }
    let _ = compaction.await;
    flush(&store, false).await;
    let reads: Vec<(u64, u32)> = kept.iter().chain([&late]).map(|key| (*key, 0)).collect();
    drop(store);
    let report = check(&dir, &config, &reads, &reads).await;
    outcome(&report, "the late store survived the rewrite")
}

/// Copies and forced stores landing while compaction rewrites the packfile they point into.
async fn copy_and_force_put_racing_compaction(dir: PathBuf) -> Outcome {
    let config = config(1000);
    let mut taken = HashSet::new();
    let kept = keys(&config, 0..255, 512..4096, 16, &mut taken);
    let dropped = keys(&config, 0..255, 20_000..60_000, 8, &mut taken);
    let mut report = Report::default();
    for point in [
        PausePoint::Selected,
        PausePoint::Stopped,
        PausePoint::Serializing,
    ] {
        let run = dir.join(format!("{point:?}"));
        let (store, span) = open_in_span(&run, &config).await;
        for &key in kept.iter().chain(&dropped) {
            put(&store, &config, key, 0).await;
        }
        for &key in &dropped {
            obliterate(&store, &config, key, 0).await;
        }
        flush(&store, false).await;
        let _ = dyn_store(&store).load_index().await;
        let compacting = store.clone();
        let mut compaction = None;
        let pause = pause_nth(point, 1, || {
            compaction = Some(lore_base::lore_spawn!(async move {
                compacting
                    .clone()
                    .compact_group_packfiles(
                        GROUP as usize,
                        compacting.path.clone(),
                        0,
                        true,
                        false,
                        CompactionInstruments::default(),
                        None,
                    )
                    .await
            }));
        })
        .await;
        for &key in &kept[..8] {
            let _ = dyn_store(&store)
                .copy(
                    config.partition(0),
                    config.address(key, 0),
                    config.partition(1),
                    config.context(1),
                    lore_storage::immutable_store::CopyBehavior {
                        durable: false,
                        do_not_replicate: false,
                    },
                )
                .await;
        }
        let forcing = store.clone();
        let force_config = config.clone();
        let forced: Vec<u64> = kept[8..].to_vec();
        let force = lore_base::lore_spawn!(async move {
            for key in forced {
                let _ = dyn_store(&forcing)
                    .put(
                        force_config.partition(0),
                        force_config.address(key, 0),
                        force_config.fragment(key, 0),
                        Some(force_config.payload(key)),
                        true,
                    )
                    .await;
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        pause.resume.notify_one();
        let _ = force.await;
        if let Some(compaction) = compaction {
            let _ = compaction.await;
        }
        flush(&store, false).await;
        let snapshot = crash(&run, "after");
        drop(span);
        drop(store);
        let mut reads: Vec<(u64, u32)> = kept.iter().map(|key| (*key, 0)).collect();
        reads.extend(kept[..8].iter().map(|key| (*key, 1)));
        let found = check(&snapshot, &config, &reads, &reads).await;
        if found.violation_count() > 0 {
            found.print();
            report.violation("racing-compaction", format!("paused at {point:?}"));
        }
    }
    outcome(
        &report,
        "copies and forced stores survived every rewrite point",
    )
}

/// Payloads compaction evicts by size, killed at each bucket write of the eviction and the rewrite,
/// and after new payloads refill the reclaimed range, read back as the payload or as missing,
/// never as other bytes.
async fn evicted_payload_restored_after_crash(dir: PathBuf) -> Outcome {
    let mut config = config(1000);
    config.evictable_partitions = 3;
    let mut report = Report::default();
    let mut details = vec![];
    for nth in [0, 1, 2, 4, 8] {
        let run = dir.join(format!("run{nth}"));
        let mut taken = HashSet::new();
        let evictable = keys(&config, 0..255, 4096..30_000, 24, &mut taken);
        let store = open(&run, &config).await;
        for &key in &evictable {
            put(&store, &config, key, 0).await;
        }
        flush(&store, false).await;
        let _ = dyn_store(&store).load_index().await;
        let compacting = store.clone();
        let mut task = None;
        let compact = || {
            task = Some(lore_base::lore_spawn!(async move {
                compacting
                    .clone()
                    .compact_group_packfiles(
                        GROUP as usize,
                        compacting.path.clone(),
                        0,
                        true,
                        false,
                        CompactionInstruments::default(),
                        None,
                    )
                    .await
            }));
        };
        // At each bucket write of the pass, or, for 0, once it is done.
        let mut inside = None;
        if nth == 0 {
            compact();
        } else if let Ok(pause) = pause_nth_within(
            PausePoint::Serializing,
            nth,
            Duration::from_secs(5),
            compact,
        )
        .await
        {
            inside = Some(crash(&run, "killed-inside-compaction"));
            pause.resume.notify_one();
        }
        let compacted = match task {
            Some(task) => task.await.map_err(|err| err.to_string()),
            None => Err("not run".to_string()),
        };
        details.push(format!("{nth}: {}", compacted.is_ok()));
        let fresh = keys(&config, 0..255, 4096..30_000, 24, &mut taken);
        for &key in &fresh {
            put(&store, &config, key, 0).await;
        }
        let refilled = crash(&run, "killed-after-refill");
        drop(store);
        let reads: Vec<(u64, u32)> = evictable
            .iter()
            .chain(&fresh)
            .map(|key| (*key, 0))
            .collect();
        for snapshot in inside.iter().chain([&refilled]) {
            let found = check(snapshot, &config, &reads, &[]).await;
            if found.violation_count() > 0 {
                found.print();
                report.violation("evicted-served", format!("bucket write {nth}"));
            }
        }
    }
    outcome(
        &report,
        &format!("no evicted entry served other bytes; compactions {details:?}"),
    )
}

/// A put must refuse a fragment whose flags or sizes no store should keep, or keep it sanitized:
/// flags the store manages, two compressors, a payload claimed local without one, sizes that
/// contradict each other.
async fn put_refuses_invalid_fragments(dir: PathBuf) -> Outcome {
    let config = config(1000);
    let store = open(&dir, &config).await;
    let mut taken = HashSet::new();
    let mut accepted = vec![];
    let mut sanitized = vec![];
    let mut tried = std::collections::BTreeSet::new();
    let mut rng = crate::model::Rng::new(5);
    for _ in 0..200 {
        let key = keys(&config, 0..255, 64..4096, 1, &mut taken)[0];
        let (name, fragment, payload) = crate::work::invalid_fragment(&config, key, 0, &mut rng);
        if !tried.insert(name) {
            continue;
        }
        let result = dyn_store(&store)
            .put(
                config.partition(0),
                config.address(key, 0),
                fragment,
                payload,
                false,
            )
            .await;
        if result.is_ok() {
            let found = store
                .find(config.partition(0), config.address(key, 0), Lookup::Read)
                .await;
            if let Ok(found) = &found
                && crate::work::kept_sanitized(found)
            {
                sanitized.push(name);
                continue;
            }
            let stored = found
                .map(|found| {
                    format!(
                        "flags {:#x} sizes {}/{}",
                        found.data.flags, found.data.size_payload, found.data.size_content
                    )
                })
                .unwrap_or_default();
            accepted.push(format!("{name} (stored {stored})"));
        }
    }
    if accepted.is_empty() {
        Outcome::Pass(format!(
            "refused {} of {}, and kept {sanitized:?} sanitized",
            tried.len() - sanitized.len(),
            tried.len()
        ))
    } else {
        Outcome::Fail(format!(
            "put accepted {} of {} invalid fragments as given: {}",
            accepted.len(),
            tried.len(),
            accepted.join("; ")
        ))
    }
}

/// An entry's local flag says whether it holds a payload: a store that deduplicates onto a stored
/// payload, or eviction or compaction that moves or drops one, keeps the two agreeing.
async fn flags_agree_with_payload_after_deduplication(dir: PathBuf) -> Outcome {
    let config = config(1000);
    let key = keys(&config, 0..255, 64..4096, 1, &mut HashSet::new())[0];
    let store = open(&dir, &config).await;
    put(&store, &config, key, 0).await;
    let mut fragment = config.fragment(key, 2);
    fragment.flags = 0;
    let stored = dyn_store(&store)
        .put(
            config.partition(2),
            config.address(key, 2),
            fragment,
            None,
            false,
        )
        .await;
    let found = store
        .find(config.partition(2), config.address(key, 2), Lookup::Read)
        .await;
    match (stored, found) {
        (Ok(()), Ok(found)) => {
            let local =
                found.data.flags & lore_storage::FragmentFlags::PayloadStoredLocal.bits() != 0;
            if local == (found.data.pack_file != 0) {
                Outcome::Pass("the flag agrees with the payload".to_string())
            } else {
                Outcome::Fail(format!(
                    "a store without a payload deduplicated onto the stored payload and recorded \
                     packfile {} with flags {:#x}, without PayloadStoredLocal",
                    found.data.pack_file, found.data.flags
                ))
            }
        }
        other => Outcome::Fail(format!("{other:?}")),
    }
}

/// A store without a payload of an obliterated address is refused with `PayloadNotFound` and
/// records nothing: the address stays obliterated, to reads and to metadata alike.
async fn metadata_put_on_an_obliterated_address(dir: PathBuf) -> Outcome {
    let config = config(1000);
    let key = keys(&config, 0..255, 64..4096, 1, &mut HashSet::new())[0];
    let store = open(&dir, &config).await;
    put(&store, &config, key, 0).await;
    obliterate(&store, &config, key, 0).await;
    let mut fragment = config.fragment(key, 0);
    fragment.flags = 0;
    let stored = dyn_store(&store)
        .put(
            config.partition(0),
            config.address(key, 0),
            fragment,
            None,
            false,
        )
        .await;
    let read_back = read(&store, &config, key, 0).await;
    let described = dyn_store(&store)
        .get_metadata(config.partition(0), config.address(key, 0))
        .await
        .map(|metadata| metadata.match_made);
    let detail = format!(
        "the put returned {:?}, the address reads as {read_back:?} and is described at {:?}",
        stored.as_ref().map_err(ToString::to_string),
        described.as_ref().map_err(ToString::to_string)
    );
    if !matches!(stored, Err(StoreError::PayloadNotFound(_)))
        || matches!(read_back, Read::Served(_))
        || matches!(described, Ok(lore_storage::StoreMatch::MatchFull))
    {
        return Outcome::Fail(detail);
    }
    Outcome::Pass(detail)
}

/// The payload of a fragment list naming `children`, and the content size it describes.
fn fragment_list(config: &Config, children: &[u64]) -> (bytes::Bytes, u64) {
    let mut payload = Vec::new();
    let mut offset = 0u64;
    for &child in children {
        let reference = lore_storage::FragmentReference {
            hash: config.hash(child),
            offset_content: offset,
        };
        payload.extend_from_slice(zerocopy::IntoBytes::as_bytes(&reference));
        offset += config.payload_len(child) as u64;
    }
    (bytes::Bytes::from(payload), offset)
}

async fn put_fragment_list(
    store: &Arc<LocalImmutableStore>,
    config: &Config,
    parent: u64,
    children: &[u64],
    with_payload: bool,
    durable: bool,
) {
    let (payload, content) = fragment_list(config, children);
    let mut flags = lore_storage::FragmentFlags::PayloadFragmented.bits();
    if with_payload {
        flags |= lore_storage::FragmentFlags::PayloadStoredLocal.bits();
    }
    if durable {
        flags |= lore_storage::FragmentFlags::PayloadStoredDurable.bits();
    }
    let fragment = lore_storage::Fragment {
        flags,
        size_payload: payload.len() as u32,
        size_content: content,
    };
    dyn_store(store)
        .put(
            config.partition(0),
            config.address(parent, 0),
            fragment,
            with_payload.then_some(payload),
            false,
        )
        .await
        .expect("fragment list stored");
}

/// Obliterating a fragmented payload obliterates the fragments its list names. A list the store
/// holds only the representation of, or one eviction took, cannot be walked: the obliterate must
/// fail rather than report success with the fragments left readable.
async fn obliterate_a_fragment_list_not_held_locally(dir: PathBuf) -> Outcome {
    let config = config(1000);
    let mut taken = HashSet::new();
    let mut findings = vec![];
    for (case, with_payload) in [("representation only", false), ("evicted list", true)] {
        let run = dir.join(case.replace(' ', "-"));
        let children = keys(&config, 0..255, 512..4096, 3, &mut taken);
        let parent = keys(&config, 0..255, 64..4096, 1, &mut taken)[0];
        let store = open(&run, &config).await;
        for &child in &children {
            put(&store, &config, child, 0).await;
        }
        put_fragment_list(&store, &config, parent, &children, with_payload, true).await;
        if with_payload {
            let _ = dyn_store(&store).load_index().await;
            let _ = store
                .clone()
                .compact_group_packfiles(
                    GROUP as usize,
                    store.path.clone(),
                    0,
                    true,
                    false,
                    CompactionInstruments::default(),
                    None,
                )
                .await;
            if read(&store, &config, parent, 0).await != Read::PayloadNotFound {
                findings.push(format!("{case}: the list was not evicted"));
                continue;
            }
        }
        let obliterated = dyn_store(&store)
            .obliterate(
                config.partition(0),
                config.address(parent, 0),
                Arc::new(StoreObliterateStats::default()),
            )
            .await;
        let mut readable = vec![];
        for &child in &children {
            if read(&store, &config, child, 0).await == Read::Served(Served::Correct) {
                readable.push(child);
            }
        }
        if obliterated.is_ok() {
            if !readable.is_empty() {
                findings.push(format!(
                    "{case}: obliterate returned Ok and {} of {} fragments it names still read back",
                    readable.len(),
                    children.len()
                ));
            }
            continue;
        }
        if readable.len() != children.len()
            || read(&store, &config, parent, 0).await == Read::NotFound
        {
            findings.push(format!(
                "{case}: a refused obliterate changed the store: {} of {} fragments read back, \
                 the list {:?}",
                readable.len(),
                children.len(),
                read(&store, &config, parent, 0).await
            ));
            continue;
        }
        // With the list stored, a retry reaches every fragment.
        put_fragment_list(&store, &config, parent, &children, true, true).await;
        let retried = dyn_store(&store)
            .obliterate(
                config.partition(0),
                config.address(parent, 0),
                Arc::new(StoreObliterateStats::default()),
            )
            .await;
        let mut left = 0;
        for &key in children.iter().chain([&parent]) {
            if read(&store, &config, key, 0).await != Read::NotFound {
                left += 1;
            }
        }
        if retried.is_err() || left > 0 {
            findings.push(format!(
                "{case}: the retry with the list stored returned {retried:?}, {left} addresses left"
            ));
        }
    }
    if findings.is_empty() {
        Outcome::Pass(
            "an obliterate that could not reach a list's fragments failed and changed nothing, and \
             a retry with the list stored obliterated them all"
                .to_string(),
        )
    } else {
        Outcome::Fail(findings.join("; "))
    }
}

/// Obliterating fragment lists while compaction rewrites the packfile holding them and their
/// fragments: every fragment named must end obliterated, and nothing else lost.
async fn obliterate_a_fragment_list_racing_compaction(dir: PathBuf) -> Outcome {
    let config = config(1000);
    let mut taken = HashSet::new();
    let mut report = Report::default();
    for point in [
        PausePoint::Selected,
        PausePoint::Stopped,
        PausePoint::Serializing,
    ] {
        let run = dir.join(format!("{point:?}"));
        let (store, span) = open_in_span(&run, &config).await;
        let mut parents = vec![];
        for _ in 0..4 {
            let children = keys(&config, 0..255, 512..8000, 4, &mut taken);
            let parent = keys(&config, 0..255, 64..4096, 1, &mut taken)[0];
            for &child in &children {
                put(&store, &config, child, 0).await;
            }
            put_fragment_list(&store, &config, parent, &children, true, false).await;
            parents.push((parent, children));
        }
        let dropped = keys(&config, 0..255, 20_000..60_000, 6, &mut taken);
        for &key in &dropped {
            put(&store, &config, key, 0).await;
            obliterate(&store, &config, key, 0).await;
        }
        flush(&store, false).await;
        let _ = dyn_store(&store).load_index().await;
        let compacting = store.clone();
        let mut compaction = None;
        let pause = pause_nth(point, 1, || {
            compaction = Some(lore_base::lore_spawn!(async move {
                compacting
                    .clone()
                    .compact_group_packfiles(
                        GROUP as usize,
                        compacting.path.clone(),
                        0,
                        true,
                        false,
                        CompactionInstruments::default(),
                        None,
                    )
                    .await
            }));
        })
        .await;
        let obliterating = store.clone();
        let parent_keys: Vec<u64> = parents.iter().map(|(parent, _)| *parent).collect();
        let obliterate_config = config.clone();
        let obliterator = lore_base::lore_spawn!(async move {
            for parent in parent_keys {
                obliterate(&obliterating, &obliterate_config, parent, 0).await;
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        pause.resume.notify_one();
        let _ = obliterator.await;
        if let Some(compaction) = compaction {
            let _ = compaction.await;
        }
        flush(&store, false).await;
        drop(span);
        drop(store);
        let reopened = open(&run, &config).await;
        for (parent, children) in &parents {
            for &key in [parent].into_iter().chain(children) {
                let read_back = read(&reopened, &config, key, 0).await;
                if read_back != Read::NotFound {
                    report.violation(
                        "survived-obliterate",
                        format!("{point:?}: key {key} reads as {read_back:?}"),
                    );
                }
            }
        }
    }
    outcome(
        &report,
        "every fragment list and fragment ended obliterated",
    )
}

/// Takes every descriptor the process has free and gives it back, in bursts of up to `hold`
/// microseconds `gap` microseconds apart on average, on a thread of its own until stopped: to anything
/// else in the process, a busy process at its limit.
struct DescriptorPressure {
    stop: Arc<atomic::AtomicBool>,
    thread: std::thread::JoinHandle<usize>,
}

impl DescriptorPressure {
    fn start(seed: u64, hold: u64, gap: u64) -> Self {
        // A sleep that short oversleeps by tens of microseconds; spinning keeps the bursts short.
        fn spin(micros: u64) {
            let until = std::time::Instant::now() + Duration::from_micros(micros);
            while std::time::Instant::now() < until {
                std::hint::spin_loop();
            }
        }
        let stop = Arc::new(atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut rng = crate::model::Rng::new(seed);
            let mut held = vec![];
            let mut exhausted = 0;
            while !stopping.load(atomic::Ordering::Relaxed) {
                spin(gap / 2 + rng.below(gap));
                while let Ok(file) = std::fs::File::open("/dev/null") {
                    held.push(file);
                }
                exhausted += 1;
                spin(rng.below(hold + 1));
                held.clear();
            }
            exhausted
        });
        Self { stop, thread }
    }

    /// Stop, and give back how many times every descriptor was taken.
    fn stop(self) -> usize {
        self.stop.store(true, atomic::Ordering::Relaxed);
        self.thread.join().expect("descriptor pressure thread")
    }
}

/// The process's descriptor limit, soft and hard.
fn descriptor_limit() -> libc::rlimit {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes the struct it is given.
    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
    limit
}

fn set_descriptor_limit(limit: libc::rlimit) {
    // SAFETY: setrlimit reads the struct it is given.
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) };
}

/// A store opened while the process runs out of descriptors, holding a group laid out before
/// fan-out, without a level marker, beside a group with one. Another thread takes and gives back
/// the last free descriptors throughout, as the rest of a busy process near its limit does. Every
/// open either fails or reads the group at the level it is laid out at: a group whose directory
/// listing found no descriptor, where its marker read had found one, taken for an unwritten group
/// would be laid out at the initial level, where lookups miss its bucket files, and a write would
/// then commit that level over them.
async fn descriptor_exhaustion_at_open(dir: PathBuf) -> Outcome {
    let config = config(1000);
    let mut taken = HashSet::new();
    let stored = keys(&config, 0..255, 64..4096, 30, &mut taken);
    let base = dir.join("store");
    {
        // A group laid out before fan-out existed: 256 buckets, no level marker, bucket files at
        // the version that came before, as an older client wrote it.
        let mut server = config.clone();
        server.initial_level = 256;
        let store = open(&base, &server).await;
        for &key in &stored {
            put(&store, &server, key, 0).await;
        }
        flush(&store, false).await;
    }
    let group = group_dir(&base);
    let _ = std::fs::remove_file(group.join("level"));
    for file in std::fs::read_dir(&group).expect("group").flatten() {
        let name = file.file_name().to_string_lossy().to_string();
        if name.starts_with("index_") && name.len() == 8 {
            let mut bytes = std::fs::read(file.path()).expect("bucket");
            bytes[..4].copy_from_slice(&4u32.to_le_bytes());
            bytes[4..8].copy_from_slice(&0u32.to_le_bytes());
            std::fs::write(file.path(), bytes).expect("bucket");
        }
    }
    // Another group written since, with a marker, so that the store is fan-out aware.
    let other = dir.join("store").join("immutable").join("index").join("11");
    std::fs::create_dir_all(&other).expect("other group");
    write_level_pending(&other, 1);
    std::fs::rename(other.join("level.pending"), other.join("level")).expect("marker");

    let reads: Vec<(u64, u32)> = stored.iter().map(|key| (*key, 0)).collect();
    let before = check(&base, &config, &reads, &reads).await;
    if before.violation_count() > 0 {
        before.print();
        return Outcome::Fail(
            "the pre-fan-out store does not read back before the test".to_string(),
        );
    }

    let normal = descriptor_limit();
    let (mut opened, mut refused, mut exhausted) = (0, 0, 0);
    let mut took = vec![];
    let mut refusals: std::collections::BTreeMap<String, usize> = Default::default();
    for attempt in 0..2000u64 {
        let run = dir.join(format!("attempt{attempt}"));
        let _ = std::fs::remove_dir_all(&run);
        copy_tree(&base, &run).expect("copy");
        let in_use = std::fs::read_dir("/proc/self/fd").map_or(64, |fds| fds.count()) as u64;
        set_descriptor_limit(libc::rlimit {
            rlim_cur: (in_use + 24).min(normal.rlim_max),
            rlim_max: normal.rlim_max,
        });
        let pressure = DescriptorPressure::start(attempt, 40, 500 << (attempt % 4));
        let started = std::time::Instant::now();
        let result =
            LocalImmutableStore::new(Some(run.clone()), config.settings(false, 3600)).await;
        took.push(started.elapsed().as_micros() as u64);
        exhausted += pressure.stop();
        set_descriptor_limit(normal);
        let store = match result {
            Ok(store) => store,
            Err(err) => {
                refused += 1;
                let reason: String = err.to_string().chars().take(160).collect();
                *refusals.entry(reason).or_default() += 1;
                let _ = std::fs::remove_dir_all(&run);
                continue;
            }
        };
        opened += 1;
        let level = store.group[GROUP as usize]
            .bucket_count
            .load(atomic::Ordering::Relaxed);
        if level == 256 {
            drop(store);
            let _ = std::fs::remove_dir_all(&run);
            continue;
        }
        let mut missing = 0;
        for &key in &stored {
            if read(&store, &config, key, 0).await != Read::Served(Served::Correct) {
                missing += 1;
            }
        }
        let extra = keys(&config, 0..255, 64..4096, 1, &mut taken)[0];
        put(&store, &config, extra, 0).await;
        flush(&store, false).await;
        drop(store);
        let after = check(&run, &config, &reads, &reads).await;
        return Outcome::Fail(format!(
            "attempt {attempt}: with the process out of descriptors, the pre-fan-out group opened \
             at level {level}: {missing} of {} entries missed while it was open, and after a write \
             and a reopen with descriptors to spare {} violations: {:?}",
            stored.len(),
            after.violation_count(),
            after.count("lost")
        ));
    }
    if refused == 0 || opened == 0 || exhausted == 0 {
        return Outcome::Fail(format!(
            "the opens did not cover both sides of the limit: {opened} opened, {refused} failed for \
             want of a descriptor, every descriptor taken {exhausted} times"
        ));
    }
    took.sort_unstable();
    Outcome::Pass(format!(
        "{opened} opens read the group at its level and {refused} failed for want of a \
         descriptor, while every descriptor was taken {exhausted} times; an open took {}us at the \
         median; failures: {refusals:?}",
        took[took.len() / 2]
    ))
}

/// Keys enough to fan group [`GROUP`] of `store` out from the level it is at, all in its first
/// bucket, none in `taken`, or `None` for a group at the last level.
fn fan_out_keys(
    store: &LocalImmutableStore,
    config: &Config,
    taken: &mut HashSet<u64>,
) -> Option<Vec<u64>> {
    let level = store.group[GROUP as usize]
        .bucket_count
        .load(atomic::Ordering::Relaxed);
    let width = (256 / level.max(1)).min(255) as u8;
    (level < 256).then(|| {
        keys(
            config,
            0..width,
            512..4096,
            config.fan_out_threshold + 1,
            taken,
        )
    })
}

/// A flush that fans the group out while a compaction rewrites one of its packfiles stops the
/// compaction at that packfile, whose sweep followed the layout before the fan-out, and loses
/// nothing. The compaction is held in its first bucket write, which holds the group's flush lock;
/// the flush waits for that lock, and takes it before the compaction's next bucket.
async fn compaction_interrupted_by_a_fan_out(dir: PathBuf) -> Outcome {
    crate::coverage::capture_logs();
    let config = config(8);
    let mut taken = HashSet::new();
    let kept = keys(&config, 0..255, 512..20_000, 40, &mut taken);
    let dropped = keys(&config, 0..255, 512..20_000, 40, &mut taken);
    let (store, span) = open_in_span(&dir, &config).await;
    for &key in kept.iter().chain(&dropped) {
        put(&store, &config, key, 0).await;
    }
    flush(&store, false).await;
    for &key in &dropped {
        obliterate(&store, &config, key, 0).await;
    }
    flush(&store, false).await;
    let _ = dyn_store(&store).load_index().await;
    let Some(fresh) = fan_out_keys(&store, &config, &mut taken) else {
        return Outcome::Fail("the group reached the last level before the test".to_string());
    };
    let compacting = store.clone();
    let mut compaction = None;
    let pause = pause_nth(PausePoint::Serializing, 1, || {
        compaction = Some(lore_base::lore_spawn!(async move {
            compacting
                .clone()
                .compact_group_packfiles(
                    GROUP as usize,
                    compacting.path.clone(),
                    0,
                    true,
                    false,
                    CompactionInstruments::default(),
                    None,
                )
                .await
        }));
    })
    .await;
    for &key in &fresh {
        put(&store, &config, key, 0).await;
    }
    let flushing = dyn_store(&store);
    let fan_out =
        lore_base::lore_spawn!(async move { flushing.flush(FlushOptions::default()).await });
    // The flush queues on the flush lock the held bucket write holds.
    tokio::time::sleep(Duration::from_millis(200)).await;
    pause.resume.notify_one();
    let compacted = match compaction {
        Some(compaction) => compaction.await,
        None => return Outcome::Fail("no compaction ran".to_string()),
    };
    let flushed = fan_out.await;
    drop(span);
    drop(store);
    if !matches!(flushed, Ok(Ok(()))) {
        return Outcome::Fail(format!("the fan-out's flush failed: {flushed:?}"));
    }
    let interrupted = crate::coverage::reached("compaction interrupted by a fan-out");
    if interrupted == 0 || !matches!(compacted, Ok(Ok(false))) {
        return Outcome::Fail(format!(
            "the fan-out did not interrupt the compaction, which returned {compacted:?}"
        ));
    }
    let reads: Vec<(u64, u32)> = kept
        .iter()
        .chain(&dropped)
        .chain(&fresh)
        .map(|key| (*key, 0))
        .collect();
    let present: Vec<(u64, u32)> = kept.iter().chain(&fresh).map(|key| (*key, 0)).collect();
    let snapshot = crash(&dir, "after");
    let report = check(&snapshot, &config, &reads, &present).await;
    outcome(
        &report,
        "the fan-out stopped the compaction at its packfile, and nothing was lost",
    )
}

/// A flush that fans the group out while a size eviction walks its buckets stops the eviction
/// there, as the sizes it goes by follow the layout before the fan-out, and loses nothing. The
/// eviction is held at its last bucket, so that the buckets before it are evicted and their
/// writes under way when the fan-out lands.
async fn size_eviction_interrupted_by_a_fan_out(dir: PathBuf) -> Outcome {
    crate::coverage::capture_logs();
    let mut config = config(8);
    config.partitions = 2;
    config.contexts = 1;
    config.evictable_partitions = 1;
    let durable = 1;
    let mut taken = HashSet::new();
    let evictable = keys(&config, 0..255, 512..20_000, 40, &mut taken);
    let (store, span) = open_in_span(&dir, &config).await;
    for &key in &evictable {
        put(&store, &config, key, durable).await;
    }
    flush(&store, false).await;
    let _ = dyn_store(&store).load_index().await;
    let Some(fresh) = fan_out_keys(&store, &config, &mut taken) else {
        return Outcome::Fail("the group reached the last level before the test".to_string());
    };
    let buckets = store.group[GROUP as usize]
        .bucket_count
        .load(atomic::Ordering::Relaxed);
    let compacting = store.clone();
    let mut compaction = None;
    let pause = pause_nth_within(
        PausePoint::Evicting,
        buckets,
        Duration::from_secs(10),
        || {
            compaction = Some(lore_base::lore_spawn!(async move {
                compacting
                    .clone()
                    .compact_group_packfiles(
                        GROUP as usize,
                        compacting.path.clone(),
                        0,
                        true,
                        false,
                        CompactionInstruments::default(),
                        None,
                    )
                    .await
            }));
        },
    )
    .await;
    let Ok(pause) = pause else {
        crate::coverage::report_logs();
        return Outcome::Fail("the size eviction did not reach its last bucket".to_string());
    };
    for &key in &fresh {
        put(&store, &config, key, 0).await;
    }
    let flushed = dyn_store(&store).flush(FlushOptions::default()).await;
    pause.resume.notify_one();
    let compacted = match compaction {
        Some(compaction) => compaction.await,
        None => return Outcome::Fail("no compaction ran".to_string()),
    };
    drop(span);
    drop(store);
    if let Err(err) = flushed {
        return Outcome::Fail(format!("the fan-out's flush failed: {err}"));
    }
    if crate::coverage::reached("size eviction interrupted by a fan-out") == 0 {
        return Outcome::Fail(format!(
            "the fan-out did not interrupt the size eviction; the compaction returned {compacted:?}"
        ));
    }
    if crate::coverage::reached("size eviction evicted") == 0 {
        return Outcome::Fail("the size eviction evicted nothing before the fan-out".to_string());
    }
    let reads: Vec<(u64, u32)> = evictable
        .iter()
        .map(|key| (*key, durable))
        .chain(fresh.iter().map(|key| (*key, 0)))
        .collect();
    let present: Vec<(u64, u32)> = fresh.iter().map(|key| (*key, 0)).collect();
    let snapshot = crash(&dir, "after");
    let report = check(&snapshot, &config, &reads, &present).await;
    outcome(
        &report,
        "the fan-out stopped the size eviction at its bucket, and nothing was lost",
    )
}

/// A compaction dropped once one of its bucket writes is in flight, as a caller giving up on it
/// drops it, leaves the write running, and the group's next flush waits for it to land before it
/// writes the bucket again, so that the older write cannot land over the newer one: the files then
/// hold what the store holds. For a bucket write of a packfile's rewrite and one of a size
/// eviction. A flush dropped by its caller runs its group flushes on, detached, so only these and
/// the store's own tasks leave a bucket write behind.
async fn a_dropped_compaction_s_bucket_write_lands_before_the_next_flush(dir: PathBuf) -> Outcome {
    let mut config = config(1000);
    config.partitions = 2;
    config.contexts = 1;
    config.evictable_partitions = 1;
    let mut report = Report::default();
    for (case, variant) in [("rewrite", 0), ("size-eviction", 1)] {
        let run = dir.join(case);
        let mut taken = HashSet::new();
        let kept = keys(&config, 0..255, 512..20_000, 20, &mut taken);
        let dropped = keys(&config, 0..255, 512..20_000, 20, &mut taken);
        let fresh = keys(&config, 0..255, 512..20_000, 1, &mut taken)[0];
        let (store, span) = open_in_span(&run, &config).await;
        for &key in kept.iter().chain(&dropped) {
            put(&store, &config, key, variant).await;
        }
        flush(&store, false).await;
        for &key in &dropped {
            obliterate(&store, &config, key, variant).await;
        }
        flush(&store, false).await;
        let _ = dyn_store(&store).load_index().await;
        let held = lore_storage::test_util::hold_store_write();
        let compacting = store.clone();
        let compaction = lore_base::lore_spawn!(async move {
            compacting
                .clone()
                .compact_group_packfiles(
                    GROUP as usize,
                    compacting.path.clone(),
                    0,
                    true,
                    false,
                    CompactionInstruments::default(),
                    None,
                )
                .await
        });
        if tokio::time::timeout(Duration::from_secs(10), held.issued.notified())
            .await
            .is_err()
        {
            return Outcome::Fail(format!("{case}: the compaction issued no bucket write"));
        }
        compaction.abort();
        let _ = compaction.await;
        put(&store, &config, fresh, 0).await;
        let (waited, flushed) = {
            let mut next = std::pin::pin!(dyn_store(&store).flush(FlushOptions::default()));
            let waited = tokio::time::timeout(Duration::from_secs(1), &mut next)
                .await
                .is_err();
            held.complete();
            (
                waited,
                tokio::time::timeout(Duration::from_secs(10), next).await,
            )
        };
        if !waited || !matches!(flushed, Ok(Ok(()))) {
            report.violation(
                "dropped-bucket-write",
                format!(
                    "{case}: the next flush waited for the dropped write {waited}, then returned \
                     {flushed:?}"
                ),
            );
            continue;
        }
        for finding in crate::verify::files_behind_memory(&store).await {
            report.violation("bucket-file-behind-memory", format!("{case}: {finding}"));
        }
        drop(span);
        drop(store);
        let reads: Vec<(u64, u32)> = kept
            .iter()
            .chain(&dropped)
            .map(|key| (*key, variant))
            .chain([(fresh, 0)])
            .collect();
        let mut present = vec![(fresh, 0)];
        if !config.evictable(variant) {
            present.extend(kept.iter().map(|key| (*key, variant)));
        }
        let snapshot = crash(&run, "after");
        let found = check(&snapshot, &config, &reads, &present).await;
        if found.violation_count() > 0 {
            found.print();
            report.violation("dropped-bucket-write", format!("{case}: entries lost"));
        }
    }
    outcome(
        &report,
        "each next flush waited for the dropped compaction's bucket write, and the files held \
         every entry",
    )
}

/// A store object dropped with a packfile or bucket write in flight leaves the write running, and
/// a store object opened after it on the same directory, in the same process, waits for the write
/// to land before it loads the store and sizes its packfiles, so that the write lands neither
/// over a payload the successor stores nor over a bucket file it writes. With and without reusing
/// state across holds, as a client's store cache drops a store whose keep-alive window ended.
async fn a_successor_store_waits_for_a_dropped_store_s_writes(dir: PathBuf) -> Outcome {
    let config = config(1000);
    let mut report = Report::default();
    for keeps in [false, true] {
        for packfile in [true, false] {
            let case = format!(
                "{} write, keeps {keeps}",
                if packfile { "packfile" } else { "bucket" }
            );
            let run = dir.join(case.replace([' ', ','], "-"));
            std::fs::create_dir_all(&run).expect("store directory");
            let mut taken = HashSet::new();
            let first = keys(&config, 0..255, 64..4096, 2, &mut taken);
            let held_key = keys(&config, 0..255, 64..4096, 1, &mut taken)[0];
            let later = keys(&config, 0..255, 64..4096, 4, &mut taken);
            let Ok((stores, claims)) = Stores::open(&run, &config, keeps, 3600, false).await else {
                return Outcome::Fail(format!("{case}: the stores did not open"));
            };
            for &key in &first {
                put(&stores.immutable, &config, key, 0).await;
            }
            flush(&stores.immutable, false).await;
            let (held, abandoned) = if packfile {
                let held = lore_storage::test_util::hold_packfile_write();
                let putting = stores.immutable.clone();
                let held_config = config.clone();
                let task = lore_base::lore_spawn!(async move {
                    try_put(&putting, &held_config, held_key, 0).await
                });
                (held, task)
            } else {
                put(&stores.immutable, &config, held_key, 0).await;
                let held = lore_storage::test_util::hold_store_write();
                let flushing = dyn_store(&stores.immutable);
                (
                    held,
                    lore_base::lore_spawn!(
                        async move { flushing.flush(FlushOptions::default()).await }
                    ),
                )
            };
            if tokio::time::timeout(Duration::from_secs(5), held.issued.notified())
                .await
                .is_err()
            {
                return Outcome::Fail(format!("{case}: no write was issued to hold"));
            }
            abandoned.abort();
            let _ = abandoned.await;
            drop(claims);
            drop(stores);
            let successor_dir = run.clone();
            let successor_config = config.clone();
            let mut opening = lore_base::lore_spawn!(async move {
                Stores::open(&successor_dir, &successor_config, keeps, 3600, false).await
            });
            let waited = tokio::time::timeout(Duration::from_secs(1), &mut opening)
                .await
                .is_err();
            held.complete();
            let Ok(Ok(Ok((stores, claims)))) =
                tokio::time::timeout(Duration::from_secs(10), opening).await
            else {
                return Outcome::Fail(format!("{case}: the successor did not open"));
            };
            for &key in &later {
                put(&stores.immutable, &config, key, 0).await;
            }
            flush(&stores.immutable, false).await;
            for finding in crate::verify::files_behind_memory(&stores.immutable).await {
                report.violation("bucket-file-behind-memory", format!("{case}: {finding}"));
            }
            drop(claims);
            drop(stores);
            if !waited {
                report.violation(
                    "successor-did-not-wait",
                    format!(
                        "{case}: the successor opened with the dropped store's write in flight"
                    ),
                );
            }
            let reads: Vec<(u64, u32)> = first
                .iter()
                .chain([&held_key])
                .chain(&later)
                .map(|key| (*key, 0))
                .collect();
            let present: Vec<(u64, u32)> =
                first.iter().chain(&later).map(|key| (*key, 0)).collect();
            let snapshot = crash(&run, "after");
            let found = check(&snapshot, &config, &reads, &present).await;
            if found.violation_count() > 0 {
                found.print();
                report.violation(
                    "successor",
                    format!("{case}: the payloads did not read back"),
                );
            }
        }
    }
    outcome(
        &report,
        "every successor waited for the dropped store's write, and every payload read back",
    )
}

/// A store opened on a path whose directory was deleted, or deleted and created again, while a
/// store object of the same process kept the old one, retaining its state across holds, starts
/// from what the path holds now, and so does the kept store object at its next claim: neither
/// serves what the old directory held.
async fn a_store_opened_where_a_kept_store_s_directory_was_replaced_starts_anew(
    dir: PathBuf,
) -> Outcome {
    let config = config(1000);
    let mut report = Report::default();
    for recreate in [false, true] {
        let case = if recreate { "recreated" } else { "deleted" };
        let run = dir.join(case);
        std::fs::create_dir_all(&run).expect("store directory");
        let [old, new] = keys(&config, 0..255, 64..4096, 2, &mut HashSet::new())[..] else {
            unreachable!()
        };
        let Ok((kept, claims)) = Stores::open(&run, &config, true, 3600, false).await else {
            return Outcome::Fail(format!("{case}: the stores did not open"));
        };
        put(&kept.immutable, &config, old, 0).await;
        flush(&kept.immutable, false).await;
        drop(claims);
        let _ = std::fs::remove_dir_all(&run);
        if recreate {
            std::fs::create_dir_all(&run).expect("store directory");
        }
        let opened = tokio::time::timeout(
            Duration::from_secs(10),
            Stores::open(&run, &config, true, 3600, false),
        )
        .await;
        let (stores, claims) = match opened {
            Ok(Ok(opened)) => opened,
            Ok(Err(err)) => return Outcome::Fail(format!("{case}: the path did not open: {err}")),
            Err(_) => return Outcome::Fail(format!("{case}: opening the path again hung")),
        };
        let fresh = read(&stores.immutable, &config, old, 0).await;
        put(&stores.immutable, &config, new, 0).await;
        flush(&stores.immutable, false).await;
        drop(claims);
        drop(stores);
        let claimed = dyn_store(&kept.immutable).hold_for_command().await;
        let kept_reads = [
            read(&kept.immutable, &config, old, 0).await,
            read(&kept.immutable, &config, new, 0).await,
        ];
        drop(claimed);
        drop(kept);
        if fresh != Read::NotFound || kept_reads != [Read::NotFound, Read::Served(Served::Correct)]
        {
            report.violation(
                "replaced-directory",
                format!(
                    "{case}: the store opened again read the old entry as {fresh:?}, and the kept \
                     store read the old and the new as {kept_reads:?}"
                ),
            );
        }
        let snapshot = crash(&run, "after");
        let found = check(&snapshot, &config, &[(old, 0), (new, 0)], &[(new, 0)]).await;
        let reopened = open(&snapshot, &config).await;
        let old_read = read(&reopened, &config, old, 0).await;
        if found.violation_count() > 0 || old_read != Read::NotFound {
            found.print();
            report.violation(
                "replaced-directory",
                format!("{case}: the files hold the old entry as {old_read:?}"),
            );
        }
    }
    outcome(
        &report,
        "the path opened again and the kept store both read the directory there now",
    )
}
