// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Offline checks of a store directory: its files as a killed process left them, then the store
//! reopened, its index against its packfiles, every key against the journal, and the mutable
//! store's keys and counters against it, see [`crate::mutable::verify`].

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic;
use std::time::Instant;

use lore_storage::FlushOptions;
use lore_storage::FragmentFlags;
use lore_storage::Hash;
use lore_storage::LocalImmutableStore;
use lore_storage::StoreError;
use lore_storage::StoreMatch;
use lore_storage::StoreMatchResult;
use lore_storage::immutable_store::ImmutableStore;
use lore_storage::local::immutable_store::ImmutableStoreEntry;

use crate::args::Args;
use crate::journal::Expect;
use crate::journal::History;
use crate::model::Config;
use crate::model::Served;

const HEADER_SIZE: usize = 16;
const SIZES_RECORDED: u32 = 1 << 31;
const SIZES_COUNT_SHIFT: u32 = 24;

/// Findings by class: violations fail the check, notes describe what was found.
#[derive(Default)]
pub struct Report {
    violations: BTreeMap<String, Vec<String>>,
    notes: BTreeMap<String, Vec<String>>,
}

impl Report {
    pub fn violation(&mut self, class: &str, detail: String) {
        self.violations
            .entry(class.to_string())
            .or_default()
            .push(detail);
    }

    pub fn note(&mut self, class: &str, detail: String) {
        self.notes
            .entry(class.to_string())
            .or_default()
            .push(detail);
    }

    /// The violations of class `class`.
    pub fn count(&self, class: &str) -> usize {
        self.violations.get(class).map_or(0, Vec::len)
    }

    pub fn violation_count(&self) -> usize {
        self.violations.values().map(Vec::len).sum()
    }

    pub fn print(&self) {
        for (title, findings) in [("NOTE", &self.notes), ("VIOLATION", &self.violations)] {
            for (class, details) in findings {
                println!("{title} {class}: {} found", details.len());
                for detail in details.iter().take(12) {
                    println!("    {detail}");
                }
                if details.len() > 12 {
                    println!("    ... {} more", details.len() - 12);
                }
            }
        }
    }
}

pub async fn main(args: &Args) -> u8 {
    let Some(dir) = args.dir() else {
        eprintln!("verify needs a store directory");
        return 2;
    };
    let config = match Config::load(&dir) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("no stress configuration in {}: {err}", dir.display());
            return 2;
        }
    };
    let history = match History::load(&dir) {
        Ok(history) => history,
        Err(err) => {
            eprintln!("failed reading the journal: {err}");
            return 2;
        }
    };
    let mut report = Report::default();
    for violation in &history.violations {
        // `<gen> v <class> <key> <variant> <detail>`
        let class = violation.split(' ').nth(2).unwrap_or("unknown");
        report.violation(&format!("online-{class}"), violation.clone());
    }
    for overlap in &history.overlaps {
        report.violation("claims-overlapped", overlap.clone());
    }
    audit_files(&dir, &config, &mut report);

    crate::coverage::capture_logs();
    let mut code = check_store(&dir, &config, &history, &mut report, "first open", false).await;
    crate::mutable::verify(&dir, &config, &history, &mut report).await;
    if code == 0 && args.flag("deep") {
        code = deep(&dir, &config, &history, &mut report).await;
    }
    repairs(&mut report);
    println!(
        "journal: {} lines, {} generations, {} clean ends",
        history.lines, history.generations, history.clean_ends
    );
    report.print();
    if code != 0 {
        return code;
    }
    if report.violation_count() > 0 {
        println!("RESULT: {} violations", report.violation_count());
        1
    } else {
        println!("RESULT: clean");
        0
    }
}

/// Each repair the stores logged while verify opened and checked them, as a violation: dropping a
/// bucket file they could not parse, or the locations of payloads past the end of their packfile.
/// Every file the stores write lands whole or not at all, and every payload before the entry that
/// points at it, so neither a process stopped at any point nor a failed file operation leaves what
/// these repair; only a machine losing writes, which the suite does not simulate, or a defect does.
fn repairs(report: &mut Report) {
    for (milestone, class) in crate::coverage::REPAIRS {
        let reached = crate::coverage::reached(milestone);
        if reached > 0 {
            report.violation(
                class,
                format!("the stores logged {milestone} {reached} times"),
            );
        }
    }
}

/// Compact every group, flush with a sync, reopen, and check again, so that whatever the first
/// open left latent, such as an entry compaction would rewrite wrongly, shows. Compaction evicts
/// by size first, which may take every payload eviction may take.
async fn deep(dir: &Path, config: &Config, history: &History, report: &mut Report) -> u8 {
    {
        let Ok(store) = open(dir, config).await else {
            report.violation("open-failed", "deep reopen".to_string());
            return 0;
        };
        let store: Arc<dyn ImmutableStore> = store;
        let _span = match store.clone().hold_for_command().await {
            Ok(span) => span,
            Err(err) => {
                report.violation("claim-failed", format!("deep reopen: {err}"));
                return 0;
            }
        };
        let mut at = None;
        loop {
            match store.clone().compact(1, at, true, None).await {
                Ok(Some(next)) => at = Some(next),
                Ok(None) => break,
                Err(err) => {
                    report.violation("deep-compaction-error", err.to_string());
                    break;
                }
            }
        }
        if let Err(err) = store.clone().flush(FlushOptions::SYNC_DATA).await {
            report.violation("deep-flush-failed", err.to_string());
        }
    }
    let code = check_store(dir, config, history, report, "after compaction", true).await;
    obliterated_bytes(dir, config, history, report);
    code
}

/// After compaction, payloads of keys obliterated under every variant they were stored under
/// should be gone from the packfiles: zeroed, or left behind by compaction. Bytes of one found
/// whole are noted with where they are.
fn obliterated_bytes(dir: &Path, config: &Config, history: &History, report: &mut Report) {
    let gone: std::collections::HashSet<u64> = history
        .obliterated_keys
        .iter()
        .copied()
        .filter(|key| {
            !config.is_list(*key)
                && (0..config.variants()).all(|variant| {
                    !history.events.contains_key(&(*key, variant))
                        || history.expect(*key, variant, config.evictable(variant))
                            == Expect::Obliterated
                })
        })
        .collect();
    if gone.is_empty() {
        return;
    }
    let Ok(groups) = std::fs::read_dir(dir.join("immutable").join("index")) else {
        return;
    };
    for group in groups.flatten() {
        let Ok(packs) = std::fs::read_dir(group.path().join("pack")) else {
            continue;
        };
        for pack in packs.flatten() {
            let Ok(file) = std::fs::File::open(pack.path()) else {
                continue;
            };
            for (offset, key) in payload_headers(&file, config) {
                if gone.contains(&key) {
                    report.note(
                        "obliterated-bytes-survive",
                        format!("key {key} at {} offset {offset}", pack.path().display()),
                    );
                }
            }
        }
    }
}

/// The offsets of whole payloads in `file`, each with its key, found by their headers, reading
/// only the parts of a sparse file that hold data.
fn payload_headers(file: &std::fs::File, config: &Config) -> Vec<(u64, u64)> {
    use std::os::fd::AsRawFd;
    let mut found = vec![];
    let Ok(length) = file.metadata().map(|metadata| metadata.len()) else {
        return found;
    };
    let mut offset = 0u64;
    let mut buffer = vec![0u8; 1 << 20];
    while offset < length {
        // SAFETY: lseek on a descriptor `file` owns.
        let data = unsafe { libc::lseek(file.as_raw_fd(), offset as libc::off_t, libc::SEEK_DATA) };
        if data < 0 {
            break;
        }
        // SAFETY: as above.
        let hole = unsafe { libc::lseek(file.as_raw_fd(), data, libc::SEEK_HOLE) };
        let end = if hole < 0 { length } else { hole as u64 };
        let mut at = data as u64;
        while at < end {
            let take = ((end - at) as usize).min(buffer.len());
            if file.read_exact_at(&mut buffer[..take], at).is_err() {
                return found;
            }
            for position in 0..take.saturating_sub(HEADER_SIZE) {
                if &buffer[position..position + 4] != crate::model::MAGIC {
                    continue;
                }
                let key = u64::from_le_bytes(
                    buffer[position + 4..position + 12]
                        .try_into()
                        .unwrap_or_default(),
                );
                if key >= config.keys {
                    continue;
                }
                let start = at + position as u64;
                let mut whole = vec![0u8; config.payload_len(key)];
                if file.read_exact_at(&mut whole, start).is_ok()
                    && config.classify(key, &whole) == Served::Correct
                {
                    found.push((start, key));
                }
            }
            // Overlap the reads by a header, so that one across the boundary is found.
            at += (take.saturating_sub(HEADER_SIZE)).max(1) as u64;
        }
        offset = end;
    }
    found
}

pub async fn open(dir: &Path, config: &Config) -> Result<Arc<LocalImmutableStore>, String> {
    LocalImmutableStore::new(Some(dir.to_path_buf()), config.settings(false, 3600))
        .await
        .map_err(|err| err.to_string())
}

async fn check_store(
    dir: &Path,
    config: &Config,
    history: &History,
    report: &mut Report,
    phase: &str,
    compacted: bool,
) -> u8 {
    let opened = Instant::now();
    let store = match open(dir, config).await {
        Ok(store) => store,
        Err(message) => {
            // A full disk can refuse an open its recovery has to write for, as rolling a fan-out
            // commit forward does: the store failed safe, and is checked where there is space.
            if ["(os error 28)", "(os error 122)"]
                .iter()
                .any(|code| message.contains(code))
            {
                report.note(
                    "open-refused-for-want-of-space",
                    format!("{phase}: {message}"),
                );
            } else {
                report.violation("open-failed", format!("{phase}: {message}"));
            }
            return 0;
        }
    };
    println!("{phase}: opened in {:.3}s", opened.elapsed().as_secs_f64());
    let dyn_store: Arc<dyn ImmutableStore> = store.clone();
    // Claimed as a command claims a store, so that the checks take no claim each.
    let _span = match dyn_store.clone().hold_for_command().await {
        Ok(span) => span,
        Err(err) => {
            report.violation("claim-failed", format!("{phase}: {err}"));
            return 0;
        }
    };
    let loaded = Instant::now();
    if let Err(err) = dyn_store.clone().load_index().await {
        report.violation("load-index-failed", format!("{phase}: {err}"));
    }
    println!(
        "{phase}: loaded index in {:.3}s",
        loaded.elapsed().as_secs_f64()
    );

    let keys_by_hash = keys_by_hash(config);
    structure(
        &store,
        dir,
        config,
        &keys_by_hash,
        &history.obliterated_keys,
        report,
        phase,
        true,
    )
    .await;
    keys(&store, config, history, report, phase, compacted).await;
    if config.real_hash
        && let Err(err) = dyn_store.clone().verify(false).await
    {
        report.violation("store-verify", format!("{phase}: {err}"));
    }
    0
}

pub fn keys_by_hash(config: &Config) -> HashMap<Hash, u64> {
    if !config.real_hash {
        return HashMap::new();
    }
    (0..config.keys)
        .map(|key| (config.hash(key), key))
        .collect()
}

fn key_of(config: &Config, keys_by_hash: &HashMap<Hash, u64>, hash: &Hash) -> Option<u64> {
    if config.real_hash {
        keys_by_hash.get(hash).copied()
    } else {
        config.key_of_fake(hash)
    }
}

fn group_path(dir: &Path, group: usize) -> PathBuf {
    dir.join("immutable")
        .join("index")
        .join(format!("{group:02x}"))
}

/// The length of each packfile of a group, the packfile with id `n` at index `n - 1`, `None` for
/// a missing one.
fn pack_lengths(dir: &Path, group: usize) -> Vec<Option<u64>> {
    let pack = group_path(dir, group).join("pack");
    let mut lengths: Vec<Option<u64>> = vec![];
    let Ok(files) = std::fs::read_dir(&pack) else {
        return lengths;
    };
    for file in files.flatten() {
        let Some(id) = file
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<usize>().ok())
        else {
            continue;
        };
        if id == 0 {
            continue;
        }
        if lengths.len() < id {
            lengths.resize(id, None);
        }
        lengths[id - 1] = file.metadata().ok().map(|metadata| metadata.len());
    }
    lengths
}

/// Check the index the store loaded against its packfiles, read straight from disk. `strict`
/// holds every entry's flags to what the workload stores under its variant.
#[allow(clippy::too_many_arguments)]
pub async fn structure(
    store: &Arc<LocalImmutableStore>,
    dir: &Path,
    config: &Config,
    keys_by_hash: &HashMap<Hash, u64>,
    obliterated: &std::collections::HashSet<u64>,
    report: &mut Report,
    phase: &str,
    strict: bool,
) {
    let mut entries_seen = 0usize;
    let mut payloads_read = 0usize;
    for (group_index, group) in store.group.iter().enumerate() {
        let bucket_count = group.bucket_count.load(atomic::Ordering::Relaxed);
        let lengths = pack_lengths(dir, group_index);
        let mut files: HashMap<u32, std::fs::File> = HashMap::new();
        // Each payload's location and size, with the hash of an entry holding it.
        let mut ranges: Vec<(u32, u32, u32, Hash)> = vec![];
        for bucket_index in 0..bucket_count {
            let Some(bucket) = group.try_bucket(bucket_index) else {
                continue;
            };
            let bucket = bucket.read().await;
            let count = bucket.entry.len();
            entries_seen += count;
            if bucket.sorted_index.len() != count {
                report.violation(
                    "index-count",
                    format!("{phase} group {group_index} bucket {bucket_index}"),
                );
                continue;
            }
            let mut seen = vec![false; count];
            let mut previous: Option<&ImmutableStoreEntry> = None;
            for slot in 0..count {
                let index = bucket.sorted_index[slot] as usize;
                if index >= count || seen[index] {
                    report.violation(
                        "sorted-index",
                        format!("{phase} group {group_index} bucket {bucket_index} slot {slot}"),
                    );
                    break;
                }
                seen[index] = true;
                let entry = &bucket.entry[index];
                if let Some(previous) = previous {
                    let order = (
                        previous.address.hash,
                        previous.partition,
                        previous.address.context,
                    )
                        .cmp(&(
                            entry.address.hash,
                            entry.partition,
                            entry.address.context,
                        ));
                    match order {
                        std::cmp::Ordering::Greater => report.violation(
                            "unsorted",
                            format!(
                                "{phase} group {group_index} bucket {bucket_index} slot {slot}"
                            ),
                        ),
                        std::cmp::Ordering::Equal => report.violation(
                            "duplicate-entry",
                            format!(
                                "{phase} group {group_index} bucket {bucket_index} {}",
                                entry.address.hash
                            ),
                        ),
                        std::cmp::Ordering::Less => {}
                    }
                    if previous.address.hash == entry.address.hash
                        && previous.data.flags & FragmentFlags::PayloadObliterated.bits() == 0
                        && entry.data.flags & FragmentFlags::PayloadObliterated.bits() == 0
                        && previous.data.size_content != entry.data.size_content
                    {
                        report.violation(
                            "size-content-differs",
                            format!(
                                "{phase} group {group_index} hash {} sizes {} and {}",
                                entry.address.hash,
                                previous.data.size_content,
                                entry.data.size_content
                            ),
                        );
                    }
                }
                previous = Some(entry);
                check_entry(
                    entry,
                    (group_index, bucket_index, bucket_count),
                    &lengths,
                    config,
                    keys_by_hash,
                    report,
                    phase,
                    strict,
                );
                let data = &entry.data;
                if data.pack_file != 0 {
                    ranges.push((
                        data.pack_file,
                        data.pack_offset,
                        data.size_payload,
                        entry.address.hash,
                    ));
                }
            }
        }

        ranges.sort_unstable_by_key(|(file, offset, size, hash)| (*file, *offset, *size, *hash));
        ranges.dedup();
        for pair in ranges.windows(2) {
            let (file, offset, size, hash) = pair[0];
            let (next_file, next_offset, next_size, next_hash) = pair[1];
            if file != next_file {
                continue;
            }
            if offset == next_offset && size == next_size && hash != next_hash {
                report.violation(
                    "shared-location",
                    format!(
                        "{phase} group {group_index} packfile {file} offset {offset}: {hash} and {next_hash}"
                    ),
                );
            } else if u64::from(offset) + u64::from(size) > u64::from(next_offset)
                && hash != next_hash
            {
                report.violation(
                    "overlapping-payloads",
                    format!(
                        "{phase} group {group_index} packfile {file}: {offset}+{size} ({hash}) overlaps {next_offset}+{next_size} ({next_hash})"
                    ),
                );
            }
        }
        for (file, offset, size, hash) in ranges {
            let Some(key) = key_of(config, keys_by_hash, &hash) else {
                continue;
            };
            let handle = match files.entry(file) {
                std::collections::hash_map::Entry::Occupied(handle) => handle.into_mut(),
                std::collections::hash_map::Entry::Vacant(slot) => {
                    let path = group_path(dir, group_index)
                        .join("pack")
                        .join(file.to_string());
                    let Ok(handle) = std::fs::File::open(&path) else {
                        continue;
                    };
                    slot.insert(handle)
                }
            };
            let mut bytes = vec![0u8; size as usize];
            if handle.read_exact_at(&mut bytes, u64::from(offset)).is_err() {
                continue;
            }
            payloads_read += 1;
            let served = config.classify(key, &bytes);
            if matches!(served, Served::Zeros | Served::PartlyZeroed) && obliterated.contains(&key)
            {
                report.violation(
                    "obliterated-payload-zeroed-under-live-entry",
                    format!(
                        "{phase} group {group_index} packfile {file} offset {offset} key {key}"
                    ),
                );
            } else if served != Served::Correct {
                report.violation(
                    "packfile-content",
                    format!(
                        "{phase} group {group_index} packfile {file} offset {offset} size {size} key {key}: holds {served}"
                    ),
                );
            }
        }
        audit_packstore(store, group_index, &lengths, report, phase).await;
        orphan_bucket_files(dir, group_index, bucket_count, report, phase);
    }
    println!("{phase}: {entries_seen} entries, {payloads_read} payload locations read from disk");
}

/// Bucket files of a group at an index its level does not reach, which a later fan-out could
/// load as they are.
fn orphan_bucket_files(
    dir: &Path,
    group_index: usize,
    bucket_count: usize,
    report: &mut Report,
    phase: &str,
) {
    let Ok(files) = std::fs::read_dir(group_path(dir, group_index)) else {
        return;
    };
    for file in files.flatten() {
        let name = file.file_name().to_string_lossy().to_string();
        if let Some(index) = name
            .strip_prefix("index_")
            .filter(|index| index.len() == 2)
            .and_then(|index| usize::from_str_radix(index, 16).ok())
            && index >= bucket_count
        {
            report.note(
                "orphan-bucket-file",
                format!("{phase} group {group_index:02x} {name} at level {bucket_count}"),
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn check_entry(
    entry: &ImmutableStoreEntry,
    (group_index, bucket_index, bucket_count): (usize, usize, usize),
    lengths: &[Option<u64>],
    config: &Config,
    keys_by_hash: &HashMap<Hash, u64>,
    report: &mut Report,
    phase: &str,
    strict: bool,
) {
    let data = &entry.data;
    let hash = entry.address.hash;
    let place = format!("{phase} group {group_index} bucket {bucket_index} {hash}");
    if hash.data()[0] as usize != group_index {
        report.violation("wrong-group", place.clone());
    }
    if lore_storage::local::fan_out::bucket_index_for(&hash, bucket_count) != bucket_index {
        report.violation("wrong-bucket", place.clone());
    }
    let obliterated = data.flags & FragmentFlags::PayloadObliterated.bits() != 0;
    if obliterated {
        if data.pack_file != 0 || data.size_content != 0 {
            report.violation("tombstone-with-payload", place.clone());
        }
        // The store tells a tombstone from a live reference by these exact flags: one that gained
        // another counts as live, and keeps a payload it should zero.
        if data.flags != FragmentFlags::PayloadObliterated.bits() {
            report.violation(
                "tombstone-flags",
                format!("{place} flags {:#x}", data.flags),
            );
        }
        return;
    }
    // A client store evicts an entry by its durable flag, which every put and copy of the
    // workload sets just where the variant may be evicted.
    if strict
        && !config.server
        && let Some(variant) = config.variant_of(&entry.partition, &entry.address.context)
        && (data.flags & FragmentFlags::PayloadStoredDurable.bits() != 0)
            != config.evictable(variant)
    {
        report.violation(
            "durable-flag-drift",
            format!("{place} variant {variant} flags {:#x}", data.flags),
        );
    }
    let local = data.flags & FragmentFlags::PayloadStoredLocal.bits() != 0;
    if local != (data.pack_file != 0) {
        report.violation(
            "flags-contradict-data",
            format!(
                "{place} flags {:#x} packfile {}",
                data.flags, data.pack_file
            ),
        );
    }
    if let Some(key) = key_of(config, keys_by_hash, &hash) {
        let len = config.payload_len(key) as u64;
        let content = config.content_len(key);
        if data.size_content != content
            || (data.pack_file != 0 && u64::from(data.size_payload) != len)
        {
            report.violation(
                "entry-sizes",
                format!(
                    "{place} key {key} size_payload {} size_content {} expected {len} and {content}",
                    data.size_payload, data.size_content
                ),
            );
        }
    } else {
        report.violation("unknown-hash", place.clone());
    }
    if data.pack_file == 0 {
        return;
    }
    let end = u64::from(data.pack_offset) + u64::from(data.size_payload);
    match lengths.get(data.pack_file as usize - 1).copied().flatten() {
        None => report.violation(
            "entry-in-missing-packfile",
            format!("{place} packfile {}", data.pack_file),
        ),
        Some(length) if end > length => report.violation(
            "entry-past-packfile-end",
            format!(
                "{place} packfile {} ends at {end}, file is {length} bytes",
                data.pack_file
            ),
        ),
        Some(_) => {}
    }
}

/// What the packstore holds in memory against the files: the lengths it opened them at, and the
/// sizes it reserved, which a file must reach once every write has landed.
async fn audit_packstore(
    store: &Arc<LocalImmutableStore>,
    group_index: usize,
    lengths: &[Option<u64>],
    report: &mut Report,
    phase: &str,
) {
    let packstore = store.packstore(group_index);
    let Ok(sizes) = packstore.sizes().await else {
        return;
    };
    for (index, size) in sizes.iter().enumerate() {
        let length = lengths.get(index).copied().flatten().unwrap_or(0);
        if (*size as u64) < length {
            report.note(
                "reserved-below-file-length",
                format!(
                    "{phase} group {group_index} packfile {} reserved {size} file {length}",
                    index + 1
                ),
            );
        }
    }
}

/// Every key under every variant, read every way the store serves it, against the journal and
/// against each other. `compacted` after a compaction that may have evicted payloads.
async fn keys(
    store: &Arc<LocalImmutableStore>,
    config: &Config,
    history: &History,
    report: &mut Report,
    phase: &str,
    compacted: bool,
) {
    let dyn_store: Arc<dyn ImmutableStore> = store.clone();
    let mut expected = BTreeMap::<&str, usize>::new();
    for key in 0..config.keys {
        for variant in 0..config.variants() {
            let evictable = config.evictable(variant);
            let strong = match compacted {
                true => history.expect_after_compaction(key, variant, evictable),
                false => history.expect(key, variant, evictable),
            };
            let weak = match compacted {
                true => strong,
                false => history.expect_weakly(key, variant, evictable),
            };
            // Presence settled only by a flush another bucket write ran alongside is checked apart:
            // such a flush returned before that write landed when it skipped the group, and a loss
            // of this kind names that.
            let (expect, loss) = match (strong, weak) {
                (Expect::Unknown, Expect::Present { payload }) => {
                    (Expect::Present { payload }, "-after-overlapping-flush")
                }
                (strong, _) => (strong, ""),
            };
            *expected
                .entry(match expect {
                    Expect::Present { payload: true } => "present-with-payload",
                    Expect::Present { payload: false } => "present",
                    Expect::Obliterated => "obliterated",
                    Expect::Unknown => "unknown",
                })
                .or_default() += 1;
            let partition = config.partition(variant);
            let address = config.address(key, variant);
            let place = format!("{phase} key {key} variant {variant}");
            let got = dyn_store.clone().get(partition, address).await;
            match &got {
                Ok(data) => {
                    if let Some(payload) = &data.payload {
                        let served = config.classify(key, payload);
                        if served != Served::Correct {
                            let class = match served {
                                Served::Zeros | Served::PartlyZeroed
                                    if history.obliterated_keys.contains(&key) =>
                                {
                                    "obliterated-payload-served-as-zeros"
                                }
                                _ => "corrupt-read",
                            };
                            report.violation(class, format!("{place}: served {served}"));
                        }
                        get_into(&dyn_store, config, key, variant, payload, report, &place).await;
                    }
                    if expect == Expect::Obliterated && data.match_made == StoreMatch::MatchFull {
                        let class = if history.stored_elsewhere_after(key, variant) {
                            "obliteration-undone-after-store-under-other-variant"
                        } else {
                            "obliteration-undone"
                        };
                        report.violation(class, place.clone());
                    } else if expect == Expect::Obliterated {
                        report.violation(
                            "tombstone-lost",
                            format!("{place}: served at {:?}", data.match_made),
                        );
                    }
                    if let Expect::Present { .. } = expect
                        && data.match_made != StoreMatch::MatchFull
                    {
                        report.violation(
                            &format!("lost-entry{loss}"),
                            format!("{place}: only {:?}", data.match_made),
                        );
                    }
                    if data.fragment.flags & FragmentFlags::PayloadStoredLocal.bits() == 0 {
                        report.violation("get-local-flag-missing", place.clone());
                    }
                }
                Err(StoreError::AddressNotFound(_)) => {
                    if let Expect::Present { .. } = expect {
                        report.violation(&format!("lost-entry{loss}"), place.clone());
                    }
                }
                Err(StoreError::PayloadNotFound(_)) => {
                    if expect == (Expect::Present { payload: true }) {
                        report.violation(&format!("lost-payload{loss}"), place.clone());
                    }
                }
                Err(err) => report.violation("unreadable-entry", format!("{place}: {err}")),
            }
            // The level get served the payload at, which may be another context's entry.
            let served_at = match &got {
                Ok(data) if data.payload.is_some() => Some(data.match_made),
                _ => None,
            };
            let served = served_at.is_some();

            let metadata = dyn_store.clone().get_metadata(partition, address).await;
            let described = match &metadata {
                Ok(metadata) => metadata.match_made,
                Err(err) => {
                    report.violation("metadata-failed", format!("{place}: {err}"));
                    StoreMatch::MatchNone
                }
            };
            if let Ok(metadata) = &metadata
                && metadata.match_made != StoreMatch::MatchNone
            {
                if expect == Expect::Obliterated && metadata.match_made == StoreMatch::MatchFull {
                    report.violation("obliterated-described", place.clone());
                }
                let len = config.content_len(key);
                if metadata.fragment.size_content != len {
                    report.violation(
                        "wrong-fragment",
                        format!("{place}: size_content {}", metadata.fragment.size_content),
                    );
                }
            }
            // What get serves, get_metadata describes, as far, with the payload held locally.
            if let Some(served_at) = served_at {
                if (described as u32) < (served_at as u32) {
                    report.violation(
                        "metadata-disagrees-with-get",
                        format!("{place}: served at {served_at:?}, described at {described:?}"),
                    );
                } else if let Ok(metadata) = &metadata
                    && metadata.fragment.flags & FragmentFlags::PayloadStoredLocal.bits() == 0
                {
                    report.violation("metadata-local-flag-missing", place.clone());
                }
            }
            query(
                &dyn_store,
                config,
                (key, variant),
                expect,
                served,
                described,
                report,
                &place,
            )
            .await;
        }
    }
    println!("{phase}: expectations {expected:?}");
}

/// A read into a caller's buffer serves what `get` served.
async fn get_into(
    store: &Arc<dyn ImmutableStore>,
    config: &Config,
    key: u64,
    variant: u32,
    payload: &[u8],
    report: &mut Report,
    place: &str,
) {
    let mut buffer = vec![0u8; payload.len()];
    // SAFETY: `buffer` outlives the read and nothing else touches it meanwhile.
    let mut destination =
        unsafe { lore_storage::CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
    let read = store
        .clone()
        .get_into(
            config.partition(variant),
            config.address(key, variant),
            &mut destination,
        )
        .await;
    let into: Option<Vec<u8>> = match read {
        Ok((fragment, lore_storage::PayloadRead::IntoBuffer)) => {
            Some(buffer[..(fragment.size_payload as usize).min(buffer.len())].to_vec())
        }
        Ok((_, lore_storage::PayloadRead::Returned(bytes))) => Some(bytes.to_vec()),
        Err(err) => {
            report.violation(
                "get-into-disagrees",
                format!("{place}: get served the payload, get_into failed: {err}"),
            );
            None
        }
    };
    if let Some(into) = into
        && into != payload
    {
        report.violation(
            "get-into-disagrees",
            format!("{place}: get_into served {}", config.classify(key, &into)),
        );
    }
}

/// `query` against the contract: never a level the store does not hold, the partition asked
/// about when it holds the hash, whether the payload is held as a read finds it, and an
/// obliterated address never a full match.
#[allow(clippy::too_many_arguments)]
async fn query(
    store: &Arc<dyn ImmutableStore>,
    config: &Config,
    (key, variant): (u64, u32),
    expect: Expect,
    served: bool,
    described: StoreMatch,
    report: &mut Report,
    place: &str,
) {
    let partition = config.partition(variant);
    let mut results = [StoreMatchResult::default()];
    if let Err(err) = store
        .clone()
        .query(partition, &[config.address(key, variant)], &mut results)
        .await
    {
        report.violation("query-failed", format!("{place}: {err}"));
        return;
    }
    let reported = results[0];
    let full = reported.match_made == StoreMatch::MatchFull;
    if full && expect == Expect::Obliterated {
        report.violation("obliterated-reported", place.to_string());
    }
    if full && described == StoreMatch::MatchNone {
        report.violation(
            "query-over-reports",
            format!("{place}: get_metadata found nothing"),
        );
    }
    if full && reported.partition != partition {
        report.violation("query-wrong-partition", place.to_string());
    }
    if full && reported.stored_local != served {
        report.violation(
            "query-local-flag-wrong",
            format!(
                "{place}: stored_local {} where get served {served}",
                reported.stored_local
            ),
        );
    }
    // A store may report less than it holds, at a cost to its callers only.
    if !full && described == StoreMatch::MatchFull {
        report.note(
            "query-under-reports",
            format!("{place}: {:?}", reported.match_made),
        );
    }
}

/// The store's files as a killed process left them, before an open recovers anything.
fn audit_files(dir: &Path, config: &Config, report: &mut Report) {
    let index = dir.join("immutable").join("index");
    let Ok(groups) = std::fs::read_dir(&index) else {
        return;
    };
    for group in groups.flatten() {
        let Some(group_index) = group
            .file_name()
            .to_str()
            .and_then(|name| usize::from_str_radix(name, 16).ok())
        else {
            continue;
        };
        let lengths = pack_lengths(dir, group_index);
        let Ok(files) = std::fs::read_dir(group.path()) else {
            continue;
        };
        let mut pending = false;
        let mut new_files = vec![];
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().to_string();
            if name == "level.pending" {
                pending = true;
            } else if name.ends_with(".new") {
                new_files.push(name);
            } else if name.ends_with(".tmp") {
                report.note(
                    "leftover-temporary",
                    format!("group {group_index:02x} {name}"),
                );
            } else if name.starts_with("index_") && name.len() == 8 {
                audit_bucket_file(&file.path(), group_index, &lengths, report);
            }
        }
        if pending {
            report.note("pending-fan-out", format!("group {group_index:02x}"));
        } else {
            for name in new_files {
                report.note(
                    "stale-new-file",
                    format!("group {group_index:02x} {name}, no level.pending"),
                );
                stale_new_file(&group.path().join(&name), config, &lengths, report);
            }
        }
    }
}

/// The entries of a `.new` file no commit is pending for, against the packfiles. A commit removes
/// the `.new` files it did not write before its commit point, so such a file is only garbage, but
/// one holding entries that point where their payloads are not shows what a recovery renaming it
/// in would serve.
fn stale_new_file(path: &Path, config: &Config, lengths: &[Option<u64>], report: &mut Report) {
    use zerocopy::FromBytes;
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };
    if bytes.len() < HEADER_SIZE {
        return;
    }
    let count = u32::from_le_bytes(bytes[8..12].try_into().unwrap_or_default()) as usize;
    let entries_start = HEADER_SIZE + count * 4;
    let size = size_of::<ImmutableStoreEntry>();
    let keys_by_hash = keys_by_hash(config);
    let pack = path.with_file_name("pack");
    for index in 0..count {
        let start = entries_start + index * size;
        let Some(Ok(entry)) = bytes
            .get(start..start + size)
            .map(ImmutableStoreEntry::read_from_bytes)
        else {
            break;
        };
        let data = entry.data;
        if data.pack_file == 0 || data.flags & FragmentFlags::PayloadObliterated.bits() != 0 {
            continue;
        }
        let place = format!(
            "{} entry {index} packfile {}",
            path.display(),
            data.pack_file
        );
        let end = u64::from(data.pack_offset) + u64::from(data.size_payload);
        let length = (data.pack_file as usize)
            .checked_sub(1)
            .and_then(|index| lengths.get(index).copied().flatten());
        if length.is_none_or(|length| end > length) {
            report.note(
                "stale-new-file-holds-invalid-entry",
                format!("{place} ends at {end}, past its packfile"),
            );
            continue;
        }
        let Some(key) = key_of(config, &keys_by_hash, &entry.address.hash) else {
            continue;
        };
        let mut payload = vec![0u8; data.size_payload as usize];
        let read = std::fs::File::open(pack.join(data.pack_file.to_string()))
            .and_then(|file| file.read_exact_at(&mut payload, u64::from(data.pack_offset)));
        if read.is_ok() {
            let served = config.classify(key, &payload);
            if served != Served::Correct {
                report.note(
                    "stale-new-file-holds-invalid-entry",
                    format!("{place} key {key} holds {served}"),
                );
            }
        }
    }
}

/// What an entry records, its access stamp left out.
type EntryRecord = (
    Hash,
    lore_storage::Partition,
    lore_storage::Context,
    u32,
    u32,
    u64,
    u32,
    u32,
);

/// What `entry` records, its access stamp left out.
fn record_of(entry: &ImmutableStoreEntry) -> EntryRecord {
    let data = &entry.data;
    (
        entry.address.hash,
        entry.partition,
        entry.address.context,
        data.flags,
        data.size_payload,
        data.size_content,
        data.pack_offset,
        data.pack_file,
    )
}

/// The entries the bucket file at `path` records, sorted, none for a missing file, or why they
/// cannot be read: the file could not be read, or is not laid out as this store writes them.
fn bucket_file_records(path: &Path) -> Result<Vec<EntryRecord>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(err) => return Err(format!("the file could not be read: {err}")),
    };
    parse_bucket_file(&bytes).ok_or_else(|| "the file does not parse".to_string())
}

/// The entries `bytes`, a bucket file's contents, record, sorted, or `None` for a file not laid
/// out as this store writes them.
fn parse_bucket_file(bytes: &[u8]) -> Option<Vec<EntryRecord>> {
    use zerocopy::FromBytes;
    let count = u32::from_le_bytes(bytes.get(8..12)?.try_into().ok()?) as usize;
    let size = size_of::<ImmutableStoreEntry>();
    let entries = bytes.get(HEADER_SIZE + count * 4..HEADER_SIZE + count * (4 + size))?;
    let mut records: Vec<EntryRecord> = entries
        .chunks_exact(size)
        .map(|entry| {
            ImmutableStoreEntry::read_from_bytes(entry)
                .ok()
                .map(|entry| record_of(&entry))
        })
        .collect::<Option<_>>()?;
    records.sort_unstable();
    Some(records)
}

/// Each bucket of `store` whose file on disk records other entries than its memory holds, leaving
/// out buckets flagged for a flush: what an older write of a bucket file landing over a newer one
/// leaves, which reads from memory never see and the next flush of the bucket repairs. Each group
/// is held under its flush lock, so that no write of it is in flight.
pub async fn files_behind_memory(store: &Arc<LocalImmutableStore>) -> Vec<String> {
    let Some(path) = store.path.clone() else {
        return vec![];
    };
    let index = path.join("index");
    let mut findings = vec![];
    for (group_index, group) in store.group.iter().enumerate() {
        let level = group.bucket_count.load(atomic::Ordering::Relaxed);
        if !(0..level).any(|bucket| group.try_bucket(bucket).is_some()) {
            continue;
        }
        let _flushing =
            lore_storage::local::fan_out::lock_flush(&group.flush_lock, &group.writes).await;
        if group.pending_level.load(atomic::Ordering::Relaxed) != 0
            || group.committed_level.load(atomic::Ordering::Relaxed) != level
        {
            continue;
        }
        let directory = lore_storage::local::fan_out::group_dir_path(&index, group_index);
        for bucket_index in 0..level {
            let Some(bucket) = group.try_bucket(bucket_index) else {
                continue;
            };
            let bucket = bucket.read().await;
            if !bucket.deserialized || group.needs_flush(bucket_index) {
                continue;
            }
            let mut memory: Vec<EntryRecord> = bucket.entry.iter().map(record_of).collect();
            memory.sort_unstable();
            let file = lore_storage::local::fan_out::bucket_path(&directory, bucket_index);
            match bucket_file_records(&file) {
                Ok(records) if records == memory => {}
                Ok(records) => findings.push(format!(
                    "group {group_index:02x} bucket {bucket_index:02x}: the file records {} entries, \
                     {} of them not in memory, which holds {} entries",
                    records.len(),
                    records.iter().filter(|record| memory.binary_search(record).is_err()).count(),
                    memory.len()
                )),
                Err(reason) => findings.push(format!(
                    "group {group_index:02x} bucket {bucket_index:02x}: {reason}"
                )),
            }
        }
    }
    findings
}

/// The header and recorded packfile sizes of one bucket file, against the packfiles.
fn audit_bucket_file(
    path: &Path,
    group_index: usize,
    lengths: &[Option<u64>],
    report: &mut Report,
) {
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };
    let name = format!(
        "group {group_index:02x} {}",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    if bytes.len() < HEADER_SIZE {
        report.note("short-bucket-file", name);
        return;
    }
    let word = |index: usize| {
        u32::from_le_bytes(
            bytes[index * 4..index * 4 + 4]
                .try_into()
                .unwrap_or_default(),
        )
    };
    let (version, sizes, count, first_size) = (word(0), word(1), word(2), word(3));
    let per_entry = 4 + size_of::<ImmutableStoreEntry>();
    let entries_end = HEADER_SIZE + count as usize * per_entry;
    if version >= 4 && entries_end > bytes.len() {
        report.note(
            "bucket-file-shorter-than-count",
            format!("{name} count {count}"),
        );
        return;
    }
    if sizes & SIZES_RECORDED == 0 {
        return;
    }
    let recorded = ((sizes & !SIZES_RECORDED) >> SIZES_COUNT_SHIFT) as usize;
    let trailer = &bytes[entries_end.min(bytes.len())..];
    if recorded.saturating_sub(1) * 8 != trailer.len() {
        report.note(
            "torn-recorded-sizes",
            format!("{name} records {recorded}, {} trailer bytes", trailer.len()),
        );
        return;
    }
    let mut pairs = vec![];
    if recorded > 0 {
        pairs.push((sizes & ((1 << SIZES_COUNT_SHIFT) - 1), first_size));
    }
    for pair in trailer.as_chunks::<8>().0 {
        pairs.push((
            u32::from_le_bytes(pair[..4].try_into().unwrap_or_default()),
            u32::from_le_bytes(pair[4..].try_into().unwrap_or_default()),
        ));
    }
    for (id, size) in pairs {
        let length = (id as usize)
            .checked_sub(1)
            .and_then(|index| lengths.get(index).copied().flatten())
            .unwrap_or(0);
        if u64::from(size) > length {
            report.note(
                "recorded-size-beyond-packfile",
                format!("{name} packfile {id} recorded {size} file {length}"),
            );
        }
    }
}

/// Print a store directory's layout: per group, its level, packfiles and bucket files.
pub fn info(args: &Args) -> u8 {
    let Some(dir) = args.dir() else {
        return 2;
    };
    let index = dir.join("immutable").join("index");
    let Ok(groups) = std::fs::read_dir(&index) else {
        eprintln!("no index under {}", dir.display());
        return 2;
    };
    let mut groups: Vec<_> = groups.flatten().map(|group| group.path()).collect();
    groups.sort();
    for group in groups {
        let mut names: Vec<String> = std::fs::read_dir(&group)
            .map(|files| {
                files
                    .flatten()
                    .map(|file| {
                        let len = file.metadata().map_or(0, |metadata| metadata.len());
                        format!("{}({len})", file.file_name().to_string_lossy())
                    })
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        let mut packs: Vec<String> = std::fs::read_dir(group.join("pack"))
            .map(|files| {
                files
                    .flatten()
                    .map(|file| {
                        let len = file.metadata().map_or(0, |metadata| metadata.len());
                        format!("{}:{len}", file.file_name().to_string_lossy())
                    })
                    .collect()
            })
            .unwrap_or_default();
        packs.sort();
        println!(
            "{}: files [{}] packs [{}]",
            group.file_name().unwrap_or_default().to_string_lossy(),
            names.join(" "),
            packs.join(" ")
        );
    }
    let mut report = Report::default();
    if let Ok(config) = Config::load(&dir) {
        audit_files(&dir, &config, &mut report);
    }
    report.print();
    0
}
