// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The mutable keyspace: keys holding values that name them, and counters that processes sharing
//! a store increment by compare-and-swap. A key's index picks its group, one of two, and its
//! bucket byte, so that a low fan-out threshold fans the groups out. A value carries its key and
//! a check derived from both, so that a value read back under the wrong key, or damaged, shows.
//!
//! The operations `calls` runs on them hold what they read to [`CallView`], what the journal
//! settled when the call began: no other process uses the store during a call, so a key no
//! operation of the call has written holds that value for the whole call.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use lore_storage::FlushOptions;
use lore_storage::Hash;
use lore_storage::KeyType;
use lore_storage::LocalImmutableStore;
use lore_storage::LocalMutableStore;
use lore_storage::MutableStore;
use lore_storage::Partition;
use lore_storage::StoreError;
use parking_lot::Mutex;

use crate::journal::History;
use crate::journal::Journal;
use crate::journal::Store;
use crate::journal::Value;
use crate::model::Config;
use crate::model::Rng;
use crate::model::splitmix;
use crate::verify::Report;

/// Keys that hold stored values, indexes `0..KEYS`.
pub const KEYS: u64 = 48;

/// Counters, indexes `KEYS..KEYS + COUNTERS`.
pub const COUNTERS: u64 = 2;

/// The type every key is stored under, one that `list` lists.
pub const KEY_TYPE: KeyType = KeyType::BranchMetadata;

const GROUPS: [u8; 2] = [0x5A, 0xA5];

/// The partition every key is stored in.
pub fn partition() -> Partition {
    Partition::from([0x6D; 16])
}

/// Key `index`: in group `GROUPS[index % 2]` with bucket byte `index / 2`, typed as the store
/// types it.
pub fn key(index: u64) -> Hash {
    let mut bytes = [0u8; 32];
    bytes[0] = GROUPS[(index % 2) as usize];
    bytes[1] = (index / 2) as u8;
    bytes[2] = KEY_TYPE as u8;
    bytes[3..11].copy_from_slice(&index.to_le_bytes());
    bytes[11..19].copy_from_slice(&splitmix(index ^ 0x4D55).to_le_bytes());
    Hash::from(bytes)
}

/// The index of `hash`, when it is a key of this keyspace.
pub fn index_of(hash: &Hash) -> Option<u64> {
    let index = u64::from_le_bytes(hash.data()[3..11].try_into().ok()?);
    (key(index) == *hash).then_some(index)
}

/// The hash that stores `value` for key `index`. A counter at 0 is a key with no value.
pub fn value(index: u64, value: u64) -> Hash {
    if value == 0 {
        return Hash::default();
    }
    let mut bytes = [0xA5u8; 32];
    bytes[..8].copy_from_slice(&value.to_le_bytes());
    bytes[8..16].copy_from_slice(&index.to_le_bytes());
    bytes[16..24].copy_from_slice(&splitmix(index ^ value.rotate_left(17)).to_le_bytes());
    Hash::from(bytes)
}

/// The value `hash` stores for key `index`, when it is one [`value`] makes for it.
pub fn value_of(index: u64, hash: &Hash) -> Option<u64> {
    let stored = u64::from_le_bytes(hash.data()[..8].try_into().ok()?);
    (value(index, stored) == *hash).then_some(stored)
}

fn is_counter(index: u64) -> bool {
    (KEYS..KEYS + COUNTERS).contains(&index)
}

/// What a read of a key found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    Absent,
    Is(u64),
    /// A hash that is no value of the key.
    Foreign,
}

impl Read {
    /// What `loaded` found for key `index`.
    pub fn of(index: u64, loaded: &Result<Hash, StoreError>) -> Result<Read, String> {
        match loaded {
            Ok(hash) => Ok(value_of(index, hash).map_or(Read::Foreign, Read::Is)),
            Err(StoreError::AddressNotFound(_)) => Ok(Read::Absent),
            Err(err) => Err(err.to_string()),
        }
    }

    /// The counter value it is, absent being 0.
    fn count(self) -> Option<u64> {
        match self {
            Read::Absent => Some(0),
            Read::Is(count) => Some(count),
            Read::Foreign => None,
        }
    }
}

/// Every key and value `list` gives for the keyspace's partition and type.
pub async fn list(store: &Arc<dyn MutableStore>) -> Result<Vec<(Hash, Hash)>, String> {
    let mut listed = store
        .clone()
        .list(partition(), KEY_TYPE)
        .await
        .map_err(|err| err.to_string())?
        .channel();
    let mut pairs = vec![];
    while let Some(pair) = listed.recv().await {
        pairs.push(pair);
    }
    Ok(pairs)
}

/// What a call holds its mutable reads to, see the module documentation, and what this process
/// has seen of the counters.
#[derive(Default)]
pub struct CallView {
    /// What each key held when the call began, from the journal.
    values: HashMap<u64, Value>,
    /// Keys a store or swap of this call started on.
    touched: HashSet<u64>,
    /// Per counter, the least it may hold: the most the journal settled, or a swap of this call
    /// left.
    floors: HashMap<u64, u64>,
    /// Per counter, the last value this process found, which a swap without a load starts from.
    seen: HashMap<u64, u64>,
    /// Per counter, the swaps of this process that succeeded.
    pub increments: BTreeMap<u64, u64>,
}

impl CallView {
    /// Start a call against what `history` settled.
    pub fn begin(&mut self, history: &History) {
        self.touched.clear();
        for index in 0..KEYS + COUNTERS {
            self.values.insert(index, history.expect_value(index));
        }
        for counter in KEYS..KEYS + COUNTERS {
            let floor = history
                .swaps(counter)
                .filter(|(_, settled)| *settled)
                .map(|(from, _)| from + 1)
                .max()
                .unwrap_or(0);
            self.floors.insert(counter, floor);
        }
    }

    /// What key `index` must hold now, unless this call wrote it.
    fn expected(&self, index: u64) -> Value {
        match self.touched.contains(&index) {
            true => Value::Unknown,
            false => self.values.get(&index).copied().unwrap_or(Value::Unknown),
        }
    }

    fn floor(&self, counter: u64) -> u64 {
        self.floors.get(&counter).copied().unwrap_or(0)
    }
}

/// What a mutable operation needs of the run: the store, the journal, the call's view, where a
/// violation goes, by class, key and detail, and whether faults reach the store's files.
pub struct Context<'a> {
    pub store: Arc<dyn MutableStore>,
    pub journal: &'a Journal,
    pub view: &'a Mutex<CallView>,
    pub violation: &'a (dyn Fn(&str, u64, &str) + Sync),
    pub faults: bool,
}

/// Store a new value for a random key.
pub async fn store(context: &Context<'_>, rng: &mut Rng) -> Result<(), String> {
    store_key(context, rng.below(KEYS)).await.map(drop)
}

/// Store a new value for key `index`, journaled, and return the value.
async fn store_key(context: &Context<'_>, index: u64) -> Result<u64, String> {
    let stored = (context.journal.generation() << 32) | context.journal.unique();
    context.view.lock().touched.insert(index);
    let id = context.journal.start_with("ms", index, stored);
    let result = context
        .store
        .clone()
        .store(partition(), key(index), value(index, stored), KEY_TYPE)
        .await;
    match result {
        Ok(()) => {
            context.journal.ack(id);
            Ok(stored)
        }
        Err(err) => {
            context.journal.fail(id, false);
            Err(failed_write(context, index, &err))
        }
    }
}

/// Store a new value for one key, flush the store, `strong` when no other write of it can be
/// running, and load the value back, adding what failed to `failures`: the check that the store
/// takes writes again once faults end, see [`crate::work::Work::probe`].
pub async fn probe(context: &Context<'_>, strong: bool, failures: &mut Vec<String>) {
    let index = context.journal.generation() % KEYS;
    let stored = match store_key(context, index).await {
        Ok(stored) => Some(stored),
        Err(message) => {
            failures.push(format!("store of mutable key {index}: {message}"));
            None
        }
    };
    let id = context.journal.flush_start(Store::Mutable, false, strong);
    let flushed = context.store.clone().flush(FlushOptions::default()).await;
    context
        .journal
        .flush_end(Store::Mutable, id, flushed.is_ok());
    if let Err(err) = flushed {
        failures.push(format!("flush of the mutable store: {err}"));
    }
    let loaded = context
        .store
        .clone()
        .load(partition(), key(index), KEY_TYPE)
        .await;
    match (Read::of(index, &loaded), stored) {
        (Ok(Read::Is(found)), Some(stored)) if found == stored => {}
        (Ok(found), Some(stored)) => failures.push(format!(
            "load of mutable key {index} found {found:?} where {stored} was stored"
        )),
        (Err(message), _) => failures.push(format!("load of mutable key {index}: {message}")),
        (Ok(_), None) => {}
    }
}

/// Load a random key or counter, and hold what it found to the call's view.
pub async fn load(context: &Context<'_>, rng: &mut Rng) -> Result<(), String> {
    let index = rng.below(KEYS + COUNTERS);
    let (expected, floor) = {
        let view = context.view.lock();
        (view.expected(index), view.floor(index))
    };
    let loaded = context
        .store
        .clone()
        .load(partition(), key(index), KEY_TYPE)
        .await;
    let found = read(context, index, &loaded)?;
    // A write of this call that ran alongside the load decides what it found.
    let expected = match context.view.lock().touched.contains(&index) {
        true => Value::Unknown,
        false => expected,
    };
    check(context, index, expected, found, "load");
    if is_counter(index) {
        check_floor(context, index, floor, found, "load");
    }
    Ok(())
}

/// Increment a random counter by compare-and-swap: from the value a load finds, or now and then
/// from the last value this process found, as a caller holding a value from an earlier call does.
pub async fn swap(context: &Context<'_>, rng: &mut Rng) -> Result<(), String> {
    let counter = KEYS + rng.below(COUNTERS);
    let floor = context.view.lock().floor(counter);
    let from = if rng.chance(0.3) {
        context.view.lock().seen.get(&counter).copied().unwrap_or(0)
    } else {
        let loaded = context
            .store
            .clone()
            .load(partition(), key(counter), KEY_TYPE)
            .await;
        let found = read(context, counter, &loaded)?;
        check_floor(context, counter, floor, found, "load");
        found.count().unwrap_or(0)
    };
    context.view.lock().touched.insert(counter);
    let id = context.journal.start_with("cas", counter, from);
    let expected = value(counter, from);
    let swapped = context
        .store
        .clone()
        .compare_and_swap(
            partition(),
            key(counter),
            expected,
            value(counter, from + 1),
            KEY_TYPE,
        )
        .await;
    let previous = match swapped {
        Ok(previous) => previous,
        Err(err) => {
            context.journal.fail(id, false);
            return Err(failed_write(context, counter, &err));
        }
    };
    if previous != expected {
        context.journal.fail(id, true);
        let found = value_of(counter, &previous).map_or(Read::Foreign, Read::Is);
        if let Some(current) = found.count() {
            context.view.lock().seen.insert(counter, current);
        }
        check_floor(context, counter, floor, found, "swap");
        return Ok(());
    }
    context.journal.ack(id);
    if from < floor {
        (context.violation)(
            "lost-update",
            counter,
            &format!("a swap from {from} succeeded where the counter had reached {floor}"),
        );
    }
    let mut view = context.view.lock();
    *view.increments.entry(counter).or_default() += 1;
    view.seen.insert(counter, from + 1);
    let raised = view.floor(counter).max(from + 1);
    view.floors.insert(counter, raised);
    Ok(())
}

/// List every key, and hold each pair to the call's view. Where faults reach the store's files, a
/// key the list leaves out is classed apart: a list that cannot read a bucket must fail, not end
/// with that bucket's keys left out.
pub async fn list_all(context: &Context<'_>) -> Result<(), String> {
    let expected: Vec<Value> = {
        let view = context.view.lock();
        (0..KEYS + COUNTERS)
            .map(|index| view.expected(index))
            .collect()
    };
    let pairs = list(&context.store).await.inspect_err(|err| {
        if !context.faults {
            (context.violation)("list-failed", 0, err);
        }
    })?;
    let mut listed: HashMap<u64, Read> = HashMap::new();
    for (hash, stored) in pairs {
        let Some(index) = index_of(&hash) else {
            (context.violation)("list-unknown-key", 0, &format!("listed {hash}"));
            continue;
        };
        let found = value_of(index, &stored).map_or(Read::Foreign, Read::Is);
        if listed.insert(index, found).is_some() {
            (context.violation)("list-duplicate", index, "listed twice");
        }
    }
    let touched = context.view.lock().touched.clone();
    for (index, expected) in (0..KEYS + COUNTERS).zip(expected) {
        if touched.contains(&index) {
            continue;
        }
        let found = listed.get(&index).copied().unwrap_or(Read::Absent);
        if context.faults && found == Read::Absent && matches!(expected, Value::Is(_)) {
            (context.violation)(
                "list-left-out-a-key-under-faults",
                index,
                &format!("list found nothing where the journal settled {expected:?}"),
            );
            continue;
        }
        check(context, index, expected, found, "list");
    }
    Ok(())
}

/// What `loaded` found for key `index`. A failure is a violation unless faults reach the store's
/// files.
fn read(
    context: &Context<'_>,
    index: u64,
    loaded: &Result<Hash, StoreError>,
) -> Result<Read, String> {
    Read::of(index, loaded).inspect_err(|err| {
        if !context.faults {
            (context.violation)("unreadable-value", index, err);
        }
    })
}

/// Report a write of key `index` that failed, a violation for the reason [`read`] gives, and
/// return its message.
fn failed_write(context: &Context<'_>, index: u64, err: &StoreError) -> String {
    let message = err.to_string();
    if !context.faults {
        (context.violation)("mutable-write-failed", index, &message);
    }
    message
}

/// Hold what `via` found for key `index` to `expected`.
fn check(context: &Context<'_>, index: u64, expected: Value, found: Read, via: &str) {
    let class = match (expected, found) {
        (_, Read::Foreign) => "foreign-value",
        (Value::Is(value), Read::Is(found)) if found == value => return,
        (Value::Absent, Read::Absent) | (Value::Unknown, _) => return,
        (Value::Is(_), _) => "stale-value",
        (Value::Absent, Read::Is(_)) => "phantom-value",
    };
    (context.violation)(
        class,
        index,
        &format!("{via} found {found:?} where the journal settled {expected:?}"),
    );
}

/// Hold what `via` found for `counter` to `floor`, the least it may hold.
fn check_floor(context: &Context<'_>, counter: u64, floor: u64, found: Read, via: &str) {
    if found.count().is_some_and(|count| count < floor) {
        (context.violation)(
            "stale-counter",
            counter,
            &format!("{via} found {found:?} where the counter had reached {floor}"),
        );
    }
}

/// Check the mutable store of `dir` against `history`: each key holds what the journal settled,
/// or a value some process stored; each counter holds every increment a flush settled, once, and
/// none no process made; and `list` agrees with the loads.
pub async fn verify(dir: &Path, config: &Config, history: &History, report: &mut Report) {
    if history.mutable.is_empty() {
        return;
    }
    let immutable = match LocalImmutableStore::new(None, config.settings(false, 3600)).await {
        Ok(store) => store,
        Err(err) => {
            report.violation("mutable-open-failed", err.to_string());
            return;
        }
    };
    let store: Arc<dyn MutableStore> =
        match LocalMutableStore::new(Some(dir), config.mutable_settings(3600, false), immutable)
            .await
        {
            Ok(store) => Arc::new(store),
            Err(err) => {
                report.violation("mutable-open-failed", err.to_string());
                return;
            }
        };
    let _span = match store.clone().hold_for_command().await {
        Ok(span) => span,
        Err(err) => {
            report.violation("claim-failed", format!("mutable store: {err}"));
            return;
        }
    };
    let mut loaded = HashMap::new();
    for index in 0..KEYS + COUNTERS {
        let place = format!("mutable key {index}");
        let result = store.clone().load(partition(), key(index), KEY_TYPE).await;
        let found = match Read::of(index, &result) {
            Ok(found) => found,
            Err(err) => {
                report.violation("unreadable-value", format!("{place}: {err}"));
                continue;
            }
        };
        loaded.insert(index, found);
        let expected = history.expect_value(index);
        let class = match (expected, found) {
            (_, Read::Foreign) => Some("foreign-value"),
            (Value::Is(value), Read::Is(found)) if found == value => None,
            (Value::Is(_), _) => Some("stale-value"),
            (Value::Absent, Read::Is(_)) => Some("phantom-value"),
            _ => None,
        };
        if let Some(class) = class {
            report.violation(class, format!("{place}: {found:?}, settled {expected:?}"));
        }
        match found {
            Read::Is(value) if !is_counter(index) && !history.store_started(index, value) => {
                report.violation(
                    "phantom-value",
                    format!("{place}: {value} was never stored"),
                );
            }
            _ if is_counter(index) => {
                if let Some(count) = found.count() {
                    counter(history, index, count, report);
                }
            }
            _ => {}
        }
    }
    match list(&store).await {
        Ok(pairs) => {
            let listed: HashMap<u64, Read> = pairs
                .iter()
                .filter_map(|(hash, stored)| {
                    let index = index_of(hash)?;
                    Some((
                        index,
                        value_of(index, stored).map_or(Read::Foreign, Read::Is),
                    ))
                })
                .collect();
            for (index, found) in &loaded {
                let listed = listed.get(index).copied().unwrap_or(Read::Absent);
                if listed != *found {
                    report.violation(
                        "list-disagrees-with-load",
                        format!("mutable key {index}: listed {listed:?}, loaded {found:?}"),
                    );
                }
            }
        }
        Err(err) => report.violation("list-failed", err),
    }
}

/// Account for counter `index`, found at `count`: every increment a flush settled counted once and
/// kept, and every increment it holds made by a swap some process started.
fn counter(history: &History, index: u64, count: u64, report: &mut Report) {
    let mut settled = HashSet::new();
    let mut acknowledged = 0;
    for (from, is_settled) in history.swaps(index) {
        acknowledged += 1;
        if !is_settled {
            continue;
        }
        if !settled.insert(from) {
            report.violation(
                "lost-update",
                format!("counter {index}: two settled swaps from {from}"),
            );
        }
        if from >= count {
            report.violation(
                "lost-increment",
                format!("counter {index}: at {count}, a settled swap from {from} is gone"),
            );
        }
    }
    if let Some(missing) = (0..count).find(|from| !history.swap_started(index, *from)) {
        report.violation(
            "phantom-increment",
            format!("counter {index}: at {count}, no swap from {missing} started"),
        );
    }
    println!(
        "counter {index}: {count}, {} settled and {acknowledged} acknowledged increments",
        settled.len()
    );
}
