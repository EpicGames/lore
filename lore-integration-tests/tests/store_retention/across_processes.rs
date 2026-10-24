// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Stores a process keeps between calls, changed by another process in between.
//!
//! Each test runs three store clients on one store directory. Each is a copy of this binary that
//! drives the local stores directly, reading one request per line on its stdin and writing one
//! reply per line on its stdout. A writes and keeps its stores between calls. B keeps its stores,
//! as a process that retains stores does, and C opens them afresh for every call, as one that
//! does not. A call is what a client call is: both stores claimed, the operations, a flush, and
//! the claims let go. The test reads each reply before it sends the next request, so the
//! interleaving is the script's and nothing waits on time. A reply carries how many times each
//! store loaded during the request, from its directory's load count, and what each operation
//! found.

use std::collections::HashMap;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStdin;
use std::process::ChildStdout;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use lore_base::test_util::TempDir;
use lore_storage::Address;
use lore_storage::Context;
use lore_storage::Fragment;
use lore_storage::FragmentFlags;
use lore_storage::Hash;
use lore_storage::Partition;
use lore_storage::hash::hash_slice;
use lore_storage::immutable_store::CopyBehavior;
use lore_storage::immutable_store::ImmutableStore;
use lore_storage::immutable_store::StoreError;
use lore_storage::immutable_store::query_one;
use lore_storage::local::immutable_store::ImmutableStoreGroup;
use lore_storage::local::immutable_store::ImmutableStoreSettings;
use lore_storage::local::immutable_store::LocalImmutableStore;
use lore_storage::local::mutable_store::LocalMutableStore;
use lore_storage::local::mutable_store::MutableStoreSettings;
use lore_storage::local::store_lock::Intent;
use lore_storage::local::store_lock::StoreGuard;
use lore_storage::local::store_lock::StoreLock;
use lore_storage::mutable_store::MutableStore;
use lore_storage::store_types::KeyType;
use lore_storage::store_types::StoreMatch;
use lore_storage::store_types::StoreObliterateStats;
use serde::Deserialize;
use serde::Serialize;

use self::Outcome::Absent;
use self::Outcome::Done;
use self::Outcome::Found;

/// Names, for a copy of this binary, the store directory it serves as a store client.
const CLIENT_STORES: &str = "LORE_STORE_RETENTION_CLIENT_STORES";

/// Tells a store client whether it keeps its stores between calls: `1` if it does.
const CLIENT_KEEPS: &str = "LORE_STORE_RETENTION_CLIENT_KEEPS";

/// What a store client writes once it serves requests. The test harness writes to stdout before
/// it runs a test, so the replies start after this line.
const READY: &str = "store client ready";

/// Seconds before a store's delayed flush runs: longer than any test, so what a call leaves
/// unflushed is written only by a flush or a sweep the script asks for.
const FLUSH_DELAY_SECONDS: u64 = 60 * 60;

/// Entries in a bucket over which a flush fans its group out: low, so four puts reach it.
const FAN_OUT_THRESHOLD: usize = 2;

/// The partition operations name unless they name another, see [`partition`].
const FIRST: u8 = 1;

/// The partition a payload is copied to.
const SECOND: u8 = 2;

/// One operation of a call. A key names a payload, see [`payload`], and a mutable key, see
/// [`mutable_key`]; a mutable key's values are numbers, see [`value_hash`].
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
enum Op {
    /// Puts the key's payload in `partition`, held locally, and durable if `durable`.
    Put {
        key: u32,
        partition: u8,
        durable: bool,
    },
    /// Reads the key's payload in `partition`.
    Get { key: u32, partition: u8 },
    /// Asks where the key's payload is, from `partition`.
    Query { key: u32, partition: u8 },
    /// Asks what the key's payload is, from `partition`.
    Metadata { key: u32, partition: u8 },
    /// Copies the key's payload from partition `from` to partition `to`.
    Copy { key: u32, from: u8, to: u8 },
    /// Obliterates the key's payload in `partition`.
    Obliterate { key: u32, partition: u8 },
    /// Evicts everything eviction may take.
    Evict,
    /// Compacts as far as compaction goes.
    Compact,
    /// Reports the level of the group the key's payload is in.
    Level { key: u32 },
    /// Stores `value` for the mutable key.
    Store { key: u32, value: u32 },
    /// Loads the mutable key's value.
    Load { key: u32 },
    /// Swaps the mutable key's value for `value` if it is `expected`.
    Swap { key: u32, expected: u32, value: u32 },
}

/// What an operation found.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum Outcome {
    /// The operation did what it was asked.
    Done,
    /// The payload was there, with the key's bytes.
    Found,
    /// The payload was there, with other bytes.
    Wrong,
    /// Nothing was there.
    Absent,
    /// The payload was there at `level`, in `partition`.
    Matched { level: Match, partition: u8 },
    /// Eviction took this many entries.
    Evicted(usize),
    /// The group is at this level: it has this many buckets.
    Level(usize),
    /// A store ran this many delayed flushes.
    Swept(usize),
    /// The mutable key holds this value.
    Value(u32),
    /// The swap found `current` rather than the value it expected, and changed nothing.
    Refused { current: u32 },
    /// The store failed.
    Failed(String),
}

/// How closely a payload was found, as [`StoreMatch`] says.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
enum Match {
    Hash,
    Partition,
    Full,
}

/// What the test asks a store client to do.
#[derive(Debug, Serialize, Deserialize)]
enum Request {
    /// A call running `ops`, flushing the stores before it lets its claims go if `flush`.
    Call { ops: Vec<Op>, flush: bool },
    /// A call running `ops` that answers [`Reply::Probed`] before it claims the stores.
    ProbedCall { ops: Vec<Op> },
    /// A flush of both stores, outside any call.
    Flush,
    /// The delayed flushes of both stores that are due, run now and outside any call.
    Sweep,
}

/// What a store client answers.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
enum Reply {
    /// Whether another process held the immutable and the mutable store's flock.
    Probed { busy: [bool; 2] },
    /// How many times the immutable and the mutable store loaded while the request ran, and what
    /// each operation found, or how each store's flush went.
    Ran {
        loads: [usize; 2],
        outcomes: Vec<Outcome>,
    },
}

/// Runs `script` against store clients A, B and C on one store directory, where this process is
/// the test named `test`. Where it is one of the clients, serves the requests the test sends it.
fn across_processes(test: &str, script: impl FnOnce(&mut Client, &mut Client, &mut Client)) {
    if let Some(stores) = std::env::var_os(CLIENT_STORES) {
        serve(
            PathBuf::from(stores),
            std::env::var(CLIENT_KEEPS).is_ok_and(|keeps| keeps == "1"),
        );
        return;
    }

    let sandbox = TempDir::new("lore-store-retention-processes-");
    let stores = sandbox.path().join("stores");
    let test = super::harness_name(module_path!(), test);
    let [mut a, mut b, mut c] = [("A", true), ("B", true), ("C", false)]
        .map(|(name, keeps)| Client::spawn(&test, name, keeps, &stores, sandbox.path()));
    for client in [&mut a, &mut b, &mut c] {
        client.ready();
    }
    script(&mut a, &mut b, &mut c);
    for client in [a, b, c] {
        client.finish();
    }
}

/// A store client, as the test drives it.
struct Client {
    name: &'static str,
    process: Child,
    requests: ChildStdin,
    replies: BufReader<ChildStdout>,
}

impl Client {
    /// Starts store client `name` on the store directory `stores`, keeping its stores between
    /// calls if `keeps`.
    fn spawn(test: &str, name: &'static str, keeps: bool, stores: &Path, sandbox: &Path) -> Self {
        let mut process = super::copy_of_this_binary(test, sandbox)
            .env(CLIENT_STORES, stores)
            .env(CLIENT_KEEPS, if keeps { "1" } else { "0" })
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn a store client");
        let requests = process.stdin.take().expect("the client's stdin is piped");
        let replies = BufReader::new(process.stdout.take().expect("the client's stdout is piped"));
        Self {
            name,
            process,
            requests,
            replies,
        }
    }

    /// Reads past what the test harness writes before the client serves requests.
    fn ready(&mut self) {
        while self.line() != READY {}
    }

    /// The next line the client writes. Fails the test if the client exited instead.
    fn line(&mut self) -> String {
        let mut line = String::new();
        let read = self
            .replies
            .read_line(&mut line)
            .expect("read from a store client");
        if read == 0 {
            let status = self.process.wait().expect("wait for a store client");
            panic!(
                "store client {} exited with {status} instead of answering",
                self.name
            );
        }
        line.truncate(line.trim_end().len());
        line
    }

    fn send(&mut self, request: &Request) {
        let mut line = serde_json::to_string(request).expect("a request encodes");
        line.push('\n');
        self.requests
            .write_all(line.as_bytes())
            .and_then(|()| self.requests.flush())
            .expect("send a store client a request");
    }

    /// The next reply. Fails the test with what else the client wrote if it is not one, as the
    /// harness reports a client that failed.
    fn reply(&mut self) -> Reply {
        let line = self.line();
        serde_json::from_str(&line).unwrap_or_else(|_| {
            let mut rest = String::new();
            let _ = self.replies.read_to_string(&mut rest);
            panic!("store client {} answered {line:?}\n{rest}", self.name)
        })
    }

    /// Checks that the reply to the last request names `loads` and `outcomes`.
    fn expect(&mut self, step: &str, loads: [usize; 2], outcomes: &[Outcome]) {
        let expected = Reply::Ran {
            loads,
            outcomes: outcomes.to_vec(),
        };
        assert_eq!(self.reply(), expected, "{} {step}", self.name);
    }

    /// Runs a call of `ops` and checks that it loaded the immutable and the mutable store `loads`
    /// times and had `outcomes`.
    fn call(&mut self, step: &str, ops: &[Op], loads: [usize; 2], outcomes: &[Outcome]) {
        self.send(&Request::Call {
            ops: ops.to_vec(),
            flush: true,
        });
        self.expect(step, loads, outcomes);
    }

    /// As [`Self::call`], for a call that does not flush the stores.
    fn call_unflushed(&mut self, step: &str, ops: &[Op], loads: [usize; 2], outcomes: &[Outcome]) {
        self.send(&Request::Call {
            ops: ops.to_vec(),
            flush: false,
        });
        self.expect(step, loads, outcomes);
    }

    /// Starts a call of `ops` and checks that another process held each store's flock as `busy`
    /// says when the call began. [`Self::expect`] reads its reply.
    fn start_probed_call(&mut self, step: &str, ops: &[Op], busy: [bool; 2]) {
        self.send(&Request::ProbedCall { ops: ops.to_vec() });
        assert_eq!(self.reply(), Reply::Probed { busy }, "{} {step}", self.name);
    }

    /// Flushes the stores outside any call, and checks that neither loaded.
    fn flush(&mut self, step: &str) {
        self.send(&Request::Flush);
        self.expect(step, [0, 0], &[Done, Done]);
    }

    /// Runs the delayed flushes that are due outside any call, and checks that each store had
    /// `due` of them and that neither loaded.
    fn sweep(&mut self, step: &str, due: [usize; 2]) {
        self.send(&Request::Sweep);
        self.expect(step, [0, 0], &due.map(Outcome::Swept));
    }

    /// Ends the client's requests, and checks that it exits as it should.
    fn finish(mut self) {
        drop(self.requests);
        let status = self.process.wait().expect("wait for a store client");
        assert!(
            status.success(),
            "store client {} exited with {status}",
            self.name
        );
    }
}

/// Serves the requests the test sends this process as a store client on the store directory
/// `root`, keeping its stores between calls if `keeps`, until the requests end.
#[expect(
    clippy::exit,
    reason = "a hung client leaves at once, so the test waiting on its reply reads its end"
)]
fn serve(root: PathBuf, keeps: bool) {
    std::thread::spawn(|| {
        std::thread::sleep(super::HANG_GUARD);
        let _ = writeln!(std::io::stderr(), "a store client counts itself hung");
        std::process::exit(2);
    });
    let runtime = lore_base::runtime::runtime();
    let mut client = StoreClient::new(root, keeps);
    // Written past the harness's output capture, which holds what `println!` writes until the
    // test ends.
    let answer = |line: &str| {
        let mut out = std::io::stdout().lock();
        writeln!(out, "{line}")
            .and_then(|()| out.flush())
            .expect("answer the test");
    };
    let reply = |reply: Reply| answer(&serde_json::to_string(&reply).expect("a reply encodes"));
    answer(READY);
    for line in std::io::stdin().lines() {
        let request = serde_json::from_str(&line.expect("read a request")).expect("a request");
        match request {
            Request::Call { ops, flush } => reply(runtime.block_on(client.call(&ops, flush))),
            Request::ProbedCall { ops } => {
                reply(Reply::Probed {
                    busy: runtime.block_on(client.busy()),
                });
                reply(runtime.block_on(client.call(&ops, true)));
            }
            Request::Flush => reply(runtime.block_on(client.flush())),
            Request::Sweep => reply(runtime.block_on(client.sweep())),
        }
    }
}

/// A store client's state.
struct StoreClient {
    root: PathBuf,
    keeps: bool,
    /// Locks on the immutable and the mutable store directory, held for the client's life so
    /// that each directory's load count lasts across calls. They take no flock of their own.
    counters: [Arc<StoreLock>; 2],
    /// The stores the last call kept, where the client keeps them.
    kept: Option<Stores>,
}

impl StoreClient {
    fn new(root: PathBuf, keeps: bool) -> Self {
        let counters = ["immutable", "mutable"].map(|store| {
            let directory = root.join(store);
            std::fs::create_dir_all(&directory).expect("create a store directory");
            StoreLock::new(directory, false).expect("a lock on a store directory")
        });
        Self {
            root,
            keeps,
            counters,
            kept: None,
        }
    }

    fn loads(&self) -> [usize; 2] {
        self.counters.each_ref().map(|lock| lock.loads())
    }

    /// The loads each store made since the count was `before`.
    fn loads_since(&self, before: [usize; 2]) -> [usize; 2] {
        let after = self.loads();
        [after[0] - before[0], after[1] - before[1]]
    }

    /// Opens the stores, with the claims they were opened under.
    async fn open(&self) -> (Stores, [Option<StoreGuard>; 2]) {
        let (immutable, immutable_opening) = LocalImmutableStore::new_claimed(
            Some(self.root.clone()),
            ImmutableStoreSettings {
                protect_local_fragment: true,
                flush_delay_seconds: FLUSH_DELAY_SECONDS,
                fan_out_threshold: FAN_OUT_THRESHOLD,
                reuse_state_across_holds: self.keeps,
                ..ImmutableStoreSettings::default()
            },
        )
        .await
        .expect("open the immutable store");
        let (mutable, mutable_opening) = LocalMutableStore::new_claimed(
            Some(&self.root),
            MutableStoreSettings {
                flush_delay_seconds: FLUSH_DELAY_SECONDS,
                fan_out_threshold: FAN_OUT_THRESHOLD,
                reuse_state_across_holds: self.keeps,
                ..MutableStoreSettings::default()
            },
            immutable.clone(),
        )
        .await
        .expect("open the mutable store");
        let stores = Stores {
            immutable,
            mutable: Arc::new(mutable),
        };
        (stores, [immutable_opening, mutable_opening])
    }

    /// Runs a call of `ops`, flushing the stores before it lets its claims go if `flush`. The
    /// first call of a client that keeps its stores, and every call of one that does not, opens
    /// them, and its claims join the ones they were opened under.
    async fn call(&mut self, ops: &[Op], flush: bool) -> Reply {
        let before = self.loads();
        let (stores, openings) = if let Some(stores) = self.kept.take() {
            (stores, None)
        } else {
            let (stores, openings) = self.open().await;
            (stores, Some(openings))
        };
        let claims = (
            ImmutableStore::hold_for_command(stores.immutable.clone())
                .await
                .expect("claim the immutable store"),
            MutableStore::hold_for_command(stores.mutable.clone())
                .await
                .expect("claim the mutable store"),
        );
        drop(openings);
        let mut outcomes = Vec::with_capacity(ops.len());
        for op in ops {
            outcomes.push(stores.run(*op).await);
        }
        if flush {
            assert_eq!(stores.flush().await, [Done, Done], "the call's flush");
        }
        drop(claims);
        if self.keeps {
            self.kept = Some(stores);
        }
        Reply::Ran {
            loads: self.loads_since(before),
            outcomes,
        }
    }

    /// Whether another process holds each store's flock.
    async fn busy(&self) -> [bool; 2] {
        let mut busy = [false; 2];
        for (lock, busy) in self.counters.iter().zip(&mut busy) {
            *busy = lock
                .try_acquire(Intent::Read)
                .await
                .expect("look at the store's flock")
                .is_none();
        }
        busy
    }

    /// Flushes the kept stores, outside any call.
    async fn flush(&self) -> Reply {
        let before = self.loads();
        let outcomes = self.kept().flush().await.to_vec();
        Reply::Ran {
            loads: self.loads_since(before),
            outcomes,
        }
    }

    /// Runs the delayed flush of each group of the kept stores that has one due now, outside any
    /// call, as it runs once its delay is up.
    async fn sweep(&self) -> Reply {
        let before = self.loads();
        let stores = self.kept();
        let mut swept = [0; 2];
        let immutable = Arc::downgrade(&stores.immutable);
        for (index, group) in stores.immutable.group.iter().enumerate() {
            if group.scheduled.load(Ordering::Relaxed) {
                ImmutableStoreGroup::flush_delayed(immutable.clone(), index, 0).await;
                swept[0] += 1;
            }
        }
        let mutable = Arc::downgrade(&stores.mutable);
        for (index, group) in stores.mutable.group.iter().enumerate() {
            if group.scheduled.load(Ordering::Relaxed) {
                LocalMutableStore::flush_delayed(mutable.clone(), index, 0).await;
                swept[1] += 1;
            }
        }
        Reply::Ran {
            loads: self.loads_since(before),
            outcomes: swept.map(Outcome::Swept).to_vec(),
        }
    }

    fn kept(&self) -> &Stores {
        self.kept
            .as_ref()
            .expect("a flush outside a call is of stores a call kept")
    }
}

/// A store client's stores.
struct Stores {
    immutable: Arc<LocalImmutableStore>,
    mutable: Arc<LocalMutableStore>,
}

impl Stores {
    async fn run(&self, op: Op) -> Outcome {
        let immutable: Arc<dyn ImmutableStore> = self.immutable.clone();
        let mutable: Arc<dyn MutableStore> = self.mutable.clone();
        match op {
            Op::Put {
                key,
                partition: number,
                durable,
            } => {
                let payload = payload(key);
                let fragment = Fragment {
                    flags: if durable {
                        (FragmentFlags::PayloadStoredLocal | FragmentFlags::PayloadStoredDurable)
                            .bits()
                    } else {
                        FragmentFlags::PayloadStoredLocal.bits()
                    },
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                };
                let put = immutable
                    .put(
                        partition(number),
                        address(key),
                        fragment,
                        Some(Bytes::from(payload)),
                        false,
                    )
                    .await;
                done(put)
            }
            Op::Get {
                key,
                partition: number,
            } => match immutable.get(partition(number), address(key)).await {
                Ok(read) if read.payload.as_deref() == Some(payload(key).as_slice()) => Found,
                Ok(_) => Outcome::Wrong,
                Err(err) => missing(err),
            },
            Op::Query {
                key,
                partition: number,
            } => match query_one(&immutable, partition(number), address(key)).await {
                Ok(found) => matched(found.match_made, found.partition),
                Err(err) => missing(err),
            },
            Op::Metadata {
                key,
                partition: number,
            } => match immutable
                .get_metadata(partition(number), address(key))
                .await
            {
                Ok(found) => matched(found.match_made, found.partition),
                Err(err) => missing(err),
            },
            Op::Copy { key, from, to } => {
                let copy = immutable
                    .copy(
                        partition(from),
                        address(key),
                        partition(to),
                        Context::from([0; 16]),
                        CopyBehavior::default(),
                    )
                    .await;
                done(copy)
            }
            Op::Obliterate {
                key,
                partition: number,
            } => done(
                immutable
                    .obliterate(
                        partition(number),
                        address(key),
                        Arc::new(StoreObliterateStats::default()),
                    )
                    .await,
            ),
            Op::Evict => match immutable.evict(1, false, None).await {
                Ok(evicted) => Outcome::Evicted(evicted),
                Err(err) => Outcome::Failed(err.to_string()),
            },
            Op::Compact => {
                let mut at = None;
                loop {
                    match immutable.clone().compact(1, at, false, None).await {
                        Ok(Some(next)) => at = Some(next),
                        Ok(None) => return Done,
                        Err(err) => return Outcome::Failed(err.to_string()),
                    }
                }
            }
            Op::Level { key } => Outcome::Level(
                self.immutable.group[usize::from(address(key).hash.data()[0])]
                    .bucket_count
                    .load(Ordering::Relaxed),
            ),
            Op::Store { key, value } => done(
                mutable
                    .store(
                        partition(FIRST),
                        mutable_key(key),
                        value_hash(value),
                        KeyType::Untyped,
                    )
                    .await,
            ),
            Op::Load { key } => {
                match mutable
                    .load(partition(FIRST), mutable_key(key), KeyType::Untyped)
                    .await
                {
                    Ok(value) => value_of(value).map_or_else(
                        || Outcome::Failed(format!("an unknown value {value}")),
                        Outcome::Value,
                    ),
                    Err(err) => missing(err),
                }
            }
            Op::Swap {
                key,
                expected,
                value,
            } => {
                let swapped = mutable
                    .compare_and_swap(
                        partition(FIRST),
                        mutable_key(key),
                        value_hash(expected),
                        value_hash(value),
                        KeyType::Untyped,
                    )
                    .await;
                match swapped {
                    Ok(previous) if previous == value_hash(expected) => Done,
                    Ok(previous) => value_of(previous).map_or_else(
                        || Outcome::Failed(format!("an unknown value {previous}")),
                        |current| Outcome::Refused { current },
                    ),
                    Err(err) => Outcome::Failed(err.to_string()),
                }
            }
        }
    }

    /// Flushes both stores, as a call ends.
    async fn flush(&self) -> [Outcome; 2] {
        [
            done(ImmutableStore::flush(self.immutable.clone(), false).await),
            done(MutableStore::flush(self.mutable.clone(), false).await),
        ]
    }
}

fn done(result: Result<(), StoreError>) -> Outcome {
    match result {
        Ok(()) => Done,
        Err(err) => Outcome::Failed(err.to_string()),
    }
}

/// [`Absent`] for an error saying nothing was there, and [`Outcome::Failed`] for any other.
fn missing(err: StoreError) -> Outcome {
    match err {
        StoreError::AddressNotFound(_) | StoreError::PayloadNotFound(_) => Absent,
        err => Outcome::Failed(err.to_string()),
    }
}

fn matched(level: StoreMatch, found_in: Partition) -> Outcome {
    let level = match level {
        StoreMatch::MatchNone => return Absent,
        StoreMatch::MatchHash => Match::Hash,
        StoreMatch::MatchPartition => Match::Partition,
        StoreMatch::MatchFull => Match::Full,
    };
    Outcome::Matched {
        level,
        partition: found_in.data()[0],
    }
}

/// Partition `number`.
fn partition(number: u8) -> Partition {
    Partition::from([number; 16])
}

/// The payload a key names: bytes of its own for every key.
fn payload(key: u32) -> Vec<u8> {
    format!("store retention payload {key}").into_bytes()
}

/// The address of the key's payload, under no context.
fn address(key: u32) -> Address {
    Address {
        hash: hash_slice(&payload(key)),
        context: Context::from([0; 16]),
    }
}

/// `COUNT` keys whose payloads fall in one group of the immutable store.
fn keys_in_one_group<const COUNT: usize>() -> [u32; COUNT] {
    let mut by_group: HashMap<u8, Vec<u32>> = HashMap::new();
    for key in 1.. {
        let keys = by_group.entry(address(key).hash.data()[0]).or_default();
        keys.push(key);
        if let Ok(keys) = <[u32; COUNT]>::try_from(keys.as_slice()) {
            return keys;
        }
    }
    unreachable!("some group fills before the keys run out")
}

/// The mutable key a key names.
fn mutable_key(key: u32) -> Hash {
    let mut bytes = [0x6b; 32];
    bytes[8..12].copy_from_slice(&key.to_le_bytes());
    Hash::from(bytes)
}

/// The hash stored for the mutable value `value`.
fn value_hash(value: u32) -> Hash {
    let mut bytes = [0xa5; 32];
    bytes[..4].copy_from_slice(&value.to_le_bytes());
    Hash::from(bytes)
}

/// The value `hash` stores, if it is one [`value_hash`] makes.
fn value_of(hash: Hash) -> Option<u32> {
    let value = u32::from_le_bytes(hash.data()[..4].try_into().expect("four bytes"));
    (value_hash(value) == hash).then_some(value)
}

fn put(key: u32) -> Op {
    Op::Put {
        key,
        partition: FIRST,
        durable: false,
    }
}

fn put_durable(key: u32) -> Op {
    Op::Put {
        key,
        partition: FIRST,
        durable: true,
    }
}

fn get(key: u32) -> Op {
    Op::Get {
        key,
        partition: FIRST,
    }
}

fn found_in(partition: u8) -> Outcome {
    Outcome::Matched {
        level: Match::Full,
        partition,
    }
}

/// **A kept store reloads once to see what another process put, and not again after.**
#[test]
fn a_kept_store_reloads_once_to_see_what_another_process_put() {
    across_processes(
        "a_kept_store_reloads_once_to_see_what_another_process_put",
        |a, b, c| {
            let x = 1;
            b.call("opens its stores", &[get(x)], [1, 1], &[Absent]);
            a.call("puts x", &[put(x)], [1, 1], &[Done]);
            b.call("reads x after A put it", &[get(x)], [1, 0], &[Found]);
            b.call("reads x again", &[get(x)], [0, 0], &[Found]);
            c.call("reads x", &[get(x)], [1, 1], &[Found]);
        },
    );
}

/// **A kept store serves no payload another process obliterated.**
#[test]
fn a_kept_store_serves_no_payload_another_process_obliterated() {
    across_processes(
        "a_kept_store_serves_no_payload_another_process_obliterated",
        |a, b, c| {
            let x = 1;
            let obliterate = Op::Obliterate {
                key: x,
                partition: FIRST,
            };
            a.call("puts x", &[put(x)], [1, 1], &[Done]);
            b.call("reads x", &[get(x)], [1, 1], &[Found]);
            a.call("obliterates x", &[obliterate], [0, 0], &[Done]);
            b.call(
                "reads x after A obliterated it",
                &[get(x)],
                [1, 0],
                &[Absent],
            );
            c.call("reads x", &[get(x)], [1, 1], &[Absent]);
        },
    );
}

/// **A kept store finds a payload where another process copied it, and not where it obliterated
/// it.**
#[test]
fn a_kept_store_finds_a_payload_where_another_process_left_it() {
    across_processes(
        "a_kept_store_finds_a_payload_where_another_process_left_it",
        |a, b, c| {
            let x = 1;
            let query = |partition| Op::Query { key: x, partition };
            let metadata = |partition| Op::Metadata { key: x, partition };
            let moved = [
                Op::Copy {
                    key: x,
                    from: FIRST,
                    to: SECOND,
                },
                Op::Obliterate {
                    key: x,
                    partition: FIRST,
                },
            ];
            let looks = [
                query(FIRST),
                query(SECOND),
                metadata(FIRST),
                metadata(SECOND),
            ];
            let found = [Absent, found_in(SECOND), Absent, found_in(SECOND)];
            a.call("puts x in the first partition", &[put(x)], [1, 1], &[Done]);
            b.call(
                "finds x in the first partition",
                &[query(FIRST), metadata(FIRST)],
                [1, 1],
                &[found_in(FIRST), found_in(FIRST)],
            );
            a.call(
                "copies x to the second partition and obliterates it in the first",
                &moved,
                [0, 0],
                &[Done, Done],
            );
            b.call(
                "finds x in the second partition alone",
                &looks,
                [1, 0],
                &found,
            );
            c.call(
                "finds x in the second partition alone",
                &looks,
                [1, 1],
                &found,
            );
        },
    );
}

/// **Another process's unflushed change keeps that process's claims until it flushes**, so a call
/// waits for the flush and then sees the change.
#[test]
fn a_call_waits_for_another_process_to_flush_its_change() {
    across_processes(
        "a_call_waits_for_another_process_to_flush_its_change",
        |a, b, c| {
            let (x, k) = (1, 1);
            let reads = [get(x), Op::Load { key: k }];
            let writes = [put(x), Op::Store { key: k, value: 1 }];
            b.call("opens its stores", &reads, [1, 1], &[Absent, Absent]);
            a.call_unflushed(
                "puts x and stores k without flushing",
                &writes,
                [1, 1],
                &[Done, Done],
            );
            b.start_probed_call("starts reading x and k", &reads, [true, true]);
            a.flush("flushes");
            b.expect(
                "reads x and k once A flushed",
                [1, 1],
                &[Found, Outcome::Value(1)],
            );
            c.call("reads x and k", &reads, [1, 1], &[Found, Outcome::Value(1)]);
        },
    );
}

/// **Another process's unflushed change keeps that process's claims until its delayed flush
/// writes it**, so a call waits for that flush and then sees the change. The flush writes a new
/// group's level marker before its bucket, which the call would otherwise read at another level.
#[test]
fn a_call_waits_for_another_process_s_delayed_flush_of_its_change() {
    across_processes(
        "a_call_waits_for_another_process_s_delayed_flush_of_its_change",
        |a, b, c| {
            let (x, k) = (1, 1);
            let reads = [get(x), Op::Load { key: k }];
            let writes = [put(x), Op::Store { key: k, value: 1 }];
            b.call("opens its stores", &reads, [1, 1], &[Absent, Absent]);
            a.call_unflushed(
                "puts x and stores k without flushing",
                &writes,
                [1, 1],
                &[Done, Done],
            );
            b.start_probed_call("starts reading x and k", &reads, [true, true]);
            a.sweep("runs the delayed flush each store has due", [1, 1]);
            b.expect(
                "reads x and k once A's delayed flushes ran",
                [1, 1],
                &[Found, Outcome::Value(1)],
            );
            c.call("reads x and k", &reads, [1, 1], &[Found, Outcome::Value(1)]);
        },
    );
}

/// **Another process's unflushed change to the mutable store alone keeps that process's claims on
/// both stores**, so a process opening the stores waits for the immutable store, which every call
/// claims first. Holding the immutable store while it waited for the mutable one, it would keep the
/// other process's next call from claiming the immutable store, and neither would go on.
#[test]
fn a_mutable_change_alone_keeps_another_process_s_claims_on_both_stores() {
    across_processes(
        "a_mutable_change_alone_keeps_another_process_s_claims_on_both_stores",
        |a, _, c| {
            let k = 1;
            let load = Op::Load { key: k };
            a.call_unflushed(
                "stores 1 for k without flushing",
                &[Op::Store { key: k, value: 1 }],
                [1, 1],
                &[Done],
            );
            c.start_probed_call("starts loading k", &[load], [true, true]);
            a.call(
                "stores 2 for k",
                &[Op::Store { key: k, value: 2 }],
                [0, 0],
                &[Done],
            );
            c.expect("loads k once A flushed", [1, 1], &[Outcome::Value(2)]);
        },
    );
}

/// **A kept store reads a payload where another process's compaction moved it**, out of a packfile
/// the compaction truncated.
#[test]
fn a_kept_store_reads_what_another_process_compacted() {
    across_processes(
        "a_kept_store_reads_what_another_process_compacted",
        |a, b, c| {
            let [y, x] = keys_in_one_group();
            let reads = [get(x), get(y)];
            a.call(
                "puts y durably and then x in one packfile",
                &[put_durable(y), put(x)],
                [1, 1],
                &[Done, Done],
            );
            b.call("reads x", &[get(x)], [1, 1], &[Found]);
            a.call("compacts, evicting y", &[Op::Compact], [0, 0], &[Done]);
            b.call("reads x where A moved it", &reads, [1, 0], &[Found, Absent]);
            c.call("reads x and y", &reads, [1, 1], &[Found, Absent]);
        },
    );
}

/// **A kept store serves no payload another process evicted.**
#[test]
fn a_kept_store_serves_no_payload_another_process_evicted() {
    across_processes(
        "a_kept_store_serves_no_payload_another_process_evicted",
        |a, b, c| {
            let (x, y, z) = (1, 2, 3);
            let reads = [get(x), get(y), get(z)];
            a.call(
                "puts x and y durably and z locally",
                &[put_durable(x), put_durable(y), put(z)],
                [1, 1],
                &[Done, Done, Done],
            );
            b.call("reads x and z", &[get(x), get(z)], [1, 1], &[Found, Found]);
            a.call("evicts", &[Op::Evict], [0, 0], &[Outcome::Evicted(2)]);
            b.call(
                "reads x, y and z after A evicted",
                &reads,
                [1, 0],
                &[Absent, Absent, Found],
            );
            c.call("reads x, y and z", &reads, [1, 1], &[Absent, Absent, Found]);
        },
    );
}

/// **A kept store reads a group another process fanned out at the group's new level.**
#[test]
fn a_kept_store_reads_a_group_another_process_fanned_out() {
    across_processes(
        "a_kept_store_reads_a_group_another_process_fanned_out",
        |a, b, c| {
            let [w, x, y, z] = keys_in_one_group();
            let reads = [get(w), get(x), get(y), get(z), Op::Level { key: w }];
            let found = [Found, Found, Found, Found, Outcome::Level(32)];
            a.call("puts w", &[put(w)], [1, 1], &[Done]);
            b.call(
                "reads w in a group of one bucket",
                &[get(w), Op::Level { key: w }],
                [1, 1],
                &[Found, Outcome::Level(1)],
            );
            a.call(
                "puts x, y and z in w's group, fanning it out",
                &[put(x), put(y), put(z)],
                [0, 0],
                &[Done, Done, Done],
            );
            b.call("reads the group at its new level", &reads, [1, 0], &found);
            c.call("reads the group", &reads, [1, 1], &found);
        },
    );
}

/// **A kept store loads the value another process swapped in, and a swap from the value it held
/// before fails.**
#[test]
fn a_kept_store_loads_the_value_another_process_swapped_in() {
    across_processes(
        "a_kept_store_loads_the_value_another_process_swapped_in",
        |a, b, c| {
            let k = 1;
            let load = Op::Load { key: k };
            a.call(
                "stores 1 for k",
                &[Op::Store { key: k, value: 1 }],
                [1, 1],
                &[Done],
            );
            b.call("loads k", &[load], [1, 1], &[Outcome::Value(1)]);
            a.call(
                "swaps k from 1 to 2",
                &[Op::Swap {
                    key: k,
                    expected: 1,
                    value: 2,
                }],
                [0, 0],
                &[Done],
            );
            b.call(
                "loads k, and swaps it from the 1 it held before",
                &[
                    load,
                    Op::Swap {
                        key: k,
                        expected: 1,
                        value: 3,
                    },
                    load,
                ],
                [0, 1],
                &[
                    Outcome::Value(2),
                    Outcome::Refused { current: 2 },
                    Outcome::Value(2),
                ],
            );
            c.call("loads k", &[load], [1, 1], &[Outcome::Value(2)]);
        },
    );
}
