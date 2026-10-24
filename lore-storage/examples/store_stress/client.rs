// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A store client: a process using the immutable and the mutable store of one directory in calls,
//! as a client process does. A call claims both stores, runs its operations, flushes them unless
//! told not to, and lets its claims go. A client that keeps its stores reuses them from call to
//! call, reusing their state across holds of their locks while their epochs are unchanged; one that
//! does not opens them afresh for each call, its claims joining the ones they were opened under.
//!
//! `client <dir> [--keeps]` serves the requests `retention` sends it, one per line on stdin, with
//! one reply per line on stdout. Each reply names how many times each store loaded while the
//! request ran, from its directory's load count, and what each operation found. Keys are the
//! keyspace's, see [`crate::model`], and mutable keys [`crate::mutable`]'s.
//!
//! - `call <op>...` runs a call and answers `ran <loads> <loads> <outcome>...`.
//! - `unflushed <op>...` runs a call that does not flush, and answers the same.
//! - `cancel <point> <op>...` runs a call whose flush its caller drops once it reaches the pause
//!   point `<point>`, see [`pause_point`], and answers the same, or `unreached` for a flush that
//!   ended before it reached the point. What the store runs on of the flush stays held there until
//!   the next request.
//! - `probed <op>...` answers `probed <busy> <busy>`, whether another process holds each store's
//!   flock, before it claims the stores, then runs the call and answers as `call` does.
//! - `hold <op>...` runs the operations of a call and answers as `call` does, then keeps the call
//!   open until the process is killed.
//! - `pause <point> <op>...` runs the operations of a call and its flush until they reach the
//!   pause point `<point>`, answers `paused` once there, or `unreached` if they ended first, and
//!   stays there until the process is killed.
//! - `flush` flushes the kept stores outside any call, and `sweep` runs their delayed flushes that
//!   are due, as they run once their delay is up.

use std::fmt;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_storage::CallerBuffer;
use lore_storage::ImmutableStore;
use lore_storage::ImmutableStoreSettings;
use lore_storage::LocalImmutableStore;
use lore_storage::LocalMutableStore;
use lore_storage::MutableStore;
use lore_storage::PayloadRead;
use lore_storage::StoreError;
use lore_storage::StoreMatch;
use lore_storage::StoreObliterateStats;
use lore_storage::immutable_store::CopyBehavior;
use lore_storage::immutable_store::query_one;
use lore_storage::local::immutable_store::ImmutableStoreGroup;
use lore_storage::local::store_lock::StoreGuard;
use lore_storage::local::store_lock::StoreLock;
use lore_storage::test_util::PausePoint;

use crate::args::Args;
use crate::journal::Store;
use crate::model::Config;
use crate::model::Served;
use crate::mutable;

/// Seconds a scripted client's delayed flush waits: longer than any script, so that what a call
/// leaves unflushed is written only by a flush or a sweep the script asks for.
const FLUSH_DELAY_SECONDS: u64 = 60 * 60;

/// Seconds a request may run before the client counts itself hung and exits, so that the script
/// waiting on its reply reads its end instead.
const HANG_SECONDS: u64 = 60;

/// The pause point `name` names, in lower case, where a `pause` or `cancel` request stops a call.
fn pause_point(name: &str) -> Result<PausePoint, String> {
    const POINTS: [(&str, PausePoint); 12] = [
        ("reserved", PausePoint::Reserved),
        ("written", PausePoint::Written),
        ("located", PausePoint::Located),
        ("selected", PausePoint::Selected),
        ("stopped", PausePoint::Stopped),
        ("serializing", PausePoint::Serializing),
        ("syncing", PausePoint::Syncing),
        ("committing", PausePoint::Committing),
        ("evicting", PausePoint::Evicting),
        ("advanced", PausePoint::Advanced),
        ("scanning", PausePoint::Scanning),
        ("listing", PausePoint::Listing),
    ];
    POINTS
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, point)| *point)
        .ok_or_else(|| format!("no pause point {name}"))
}

/// Both stores of a directory.
#[derive(Clone)]
pub struct Stores {
    pub immutable: Arc<LocalImmutableStore>,
    pub mutable: Arc<LocalMutableStore>,
}

/// The claims of a call, or the ones its stores were opened under: the immutable store's, then
/// the mutable store's.
pub type Claims = [Option<StoreGuard>; 2];

impl Stores {
    /// Open the stores of `dir`, reusing their state across holds when `keeps`, with the claims
    /// they were opened under. With `flush_background`, the immutable store's durable writes
    /// schedule its delayed flush too, as a service's do.
    pub async fn open(
        dir: &Path,
        config: &Config,
        keeps: bool,
        flush_delay: u64,
        flush_background: bool,
    ) -> Result<(Stores, Claims), String> {
        let (immutable, immutable_opening) = LocalImmutableStore::new_claimed(
            Some(dir.to_path_buf()),
            ImmutableStoreSettings {
                reuse_state_across_holds: keeps,
                ..config.settings(flush_background, flush_delay)
            },
        )
        .await
        .map_err(|err| format!("open the immutable store: {err}"))?;
        let (mutable, mutable_opening) = LocalMutableStore::new_claimed(
            Some(dir),
            config.mutable_settings(flush_delay, keeps),
            immutable.clone(),
        )
        .await
        .map_err(|err| format!("open the mutable store: {err}"))?;
        let stores = Stores {
            immutable,
            mutable: Arc::new(mutable),
        };
        Ok((stores, [immutable_opening, mutable_opening]))
    }

    /// Claim both stores for a call.
    pub async fn claim(&self) -> Result<Claims, String> {
        let immutable = ImmutableStore::hold_for_command(self.immutable.clone())
            .await
            .map_err(|err| format!("claim the immutable store: {err}"))?;
        let mutable = MutableStore::hold_for_command(self.mutable.clone())
            .await
            .map_err(|err| format!("claim the mutable store: {err}"))?;
        Ok([immutable, mutable])
    }

    /// Flush `store`, syncing what it writes when `sync`.
    pub async fn flush_store(&self, store: Store, sync: bool) -> Result<(), StoreError> {
        match store {
            Store::Immutable => ImmutableStore::flush(self.immutable.clone(), sync).await,
            Store::Mutable => MutableStore::flush(self.mutable.clone(), sync).await,
        }
    }

    /// Flush the immutable store, then the mutable store.
    pub async fn flush(&self) -> [Result<(), StoreError>; 2] {
        [
            self.flush_store(Store::Immutable, false).await,
            self.flush_store(Store::Mutable, false).await,
        ]
    }

    /// Run the delayed flush of each group that has one due, the immutable store's then the
    /// mutable store's, and return how many each ran.
    pub async fn sweep(&self) -> [usize; 2] {
        let mut swept = [0; 2];
        let immutable = Arc::downgrade(&self.immutable);
        for (index, group) in self.immutable.group.iter().enumerate() {
            if group.scheduled.load(Ordering::Relaxed) {
                ImmutableStoreGroup::flush_delayed(immutable.clone(), index, 0).await;
                swept[0] += 1;
            }
        }
        let mutable = Arc::downgrade(&self.mutable);
        for (index, group) in self.mutable.group.iter().enumerate() {
            if group.scheduled.load(Ordering::Relaxed) {
                LocalMutableStore::flush_delayed(mutable.clone(), index, 0).await;
                swept[1] += 1;
            }
        }
        swept
    }
}

/// How many times each store of a directory loaded in this process. Locks on the immutable and the
/// mutable store directory, which take no flock of their own, kept for the process's life so that
/// each directory's load count lasts across the store objects opened on it, and the lock file of
/// each.
pub struct Loads([Arc<StoreLock>; 2], [PathBuf; 2]);

impl Loads {
    pub fn new(dir: &Path) -> Result<Loads, String> {
        let lock = |store: &str| {
            let directory = dir.join(store);
            std::fs::create_dir_all(&directory)
                .and_then(|()| StoreLock::new(&directory, false))
                .map_err(|err| format!("lock {}: {err}", directory.display()))
        };
        Ok(Loads(
            [lock("immutable")?, lock("mutable")?],
            ["immutable", "mutable"].map(|store| dir.join(store).join("lock")),
        ))
    }

    pub fn now(&self) -> [usize; 2] {
        self.0.each_ref().map(|lock| lock.loads())
    }

    /// The loads since the count was `before`.
    pub fn since(&self, before: [usize; 2]) -> [usize; 2] {
        let now = self.now();
        [now[0] - before[0], now[1] - before[1]]
    }

    /// Whether another process holds each store's flock. Tried on the lock file itself: an
    /// acquisition on the mutable store takes the immutable store's flock first, so it would find
    /// the mutable store busy whenever the immutable one is. A flock this process holds counts as
    /// free: a call joins it, and a second lock on its file would fail against it.
    async fn busy(&self) -> Result<[bool; 2], String> {
        let mut busy = [false; 2];
        for ((lock, file), busy) in self.0.iter().zip(&self.1).zip(&mut busy) {
            if lock.is_held() {
                continue;
            }
            *busy = lore_base::fs::lock::FSLock::try_acquire_exact_path(file)
                .await
                .map_err(|err| err.to_string())?
                .is_none();
        }
        Ok(busy)
    }
}

/// One operation of a call. `K` is a key of the keyspace, `V` and `W` variants, `I` a mutable key,
/// and `N` a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// `put:K:V`: the key's payload under the variant, as `work` stores it, a list after its
    /// children.
    Put(u64, u32),
    /// `get:K:V`
    Get(u64, u32),
    /// `getinto:K:V`
    GetInto(u64, u32),
    /// `query:K:V`
    Query(u64, u32),
    /// `meta:K:V`
    Metadata(u64, u32),
    /// `copy:K:V:W`: from the variant `V` to the variant `W`.
    Copy(u64, u32, u32),
    /// `ob:K:V`
    Obliterate(u64, u32),
    /// `evict`: everything eviction may take.
    Evict,
    /// `compact`: as far as compaction goes.
    Compact,
    /// `loadindex`
    LoadIndex,
    /// `verify`: every payload against its hash.
    Verify,
    /// `level:K`: how many buckets the key's group has.
    Level(u64),
    /// `mstore:I:N`
    Store(u64, u64),
    /// `mload:I`
    Load(u64),
    /// `swap:I:N:N`: from the first value to the second.
    Swap(u64, u64, u64),
    /// `mlist`
    List,
    /// `flush`: both stores.
    Flush,
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Op::Put(key, variant) => write!(f, "put:{key}:{variant}"),
            Op::Get(key, variant) => write!(f, "get:{key}:{variant}"),
            Op::GetInto(key, variant) => write!(f, "getinto:{key}:{variant}"),
            Op::Query(key, variant) => write!(f, "query:{key}:{variant}"),
            Op::Metadata(key, variant) => write!(f, "meta:{key}:{variant}"),
            Op::Copy(key, from, to) => write!(f, "copy:{key}:{from}:{to}"),
            Op::Obliterate(key, variant) => write!(f, "ob:{key}:{variant}"),
            Op::Evict => write!(f, "evict"),
            Op::Compact => write!(f, "compact"),
            Op::LoadIndex => write!(f, "loadindex"),
            Op::Verify => write!(f, "verify"),
            Op::Level(key) => write!(f, "level:{key}"),
            Op::Store(index, value) => write!(f, "mstore:{index}:{value}"),
            Op::Load(index) => write!(f, "mload:{index}"),
            Op::Swap(index, from, to) => write!(f, "swap:{index}:{from}:{to}"),
            Op::List => write!(f, "mlist"),
            Op::Flush => write!(f, "flush"),
        }
    }
}

impl FromStr for Op {
    type Err = String;

    fn from_str(text: &str) -> Result<Op, String> {
        let mut fields = text.split(':');
        let name = fields.next().unwrap_or_default();
        let numbers: Vec<u64> = fields
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(|err: std::num::ParseIntError| format!("{text}: {err}"))?;
        let op = match (name, numbers.as_slice()) {
            ("put", &[key, variant]) => Op::Put(key, variant as u32),
            ("get", &[key, variant]) => Op::Get(key, variant as u32),
            ("getinto", &[key, variant]) => Op::GetInto(key, variant as u32),
            ("query", &[key, variant]) => Op::Query(key, variant as u32),
            ("meta", &[key, variant]) => Op::Metadata(key, variant as u32),
            ("copy", &[key, from, to]) => Op::Copy(key, from as u32, to as u32),
            ("ob", &[key, variant]) => Op::Obliterate(key, variant as u32),
            ("evict", []) => Op::Evict,
            ("compact", []) => Op::Compact,
            ("loadindex", []) => Op::LoadIndex,
            ("verify", []) => Op::Verify,
            ("level", &[key]) => Op::Level(key),
            ("mstore", &[index, value]) => Op::Store(index, value),
            ("mload", &[index]) => Op::Load(index),
            ("swap", &[index, from, to]) => Op::Swap(index, from, to),
            ("mlist", []) => Op::List,
            ("flush", []) => Op::Flush,
            _ => return Err(format!("no operation {text}")),
        };
        Ok(op)
    }
}

/// What an operation found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// `done`: the operation did what it was asked.
    Done,
    /// `found`: the payload, with the key's bytes.
    Found,
    /// `wrong:<served>`: the payload, with other bytes.
    Wrong(Served),
    /// `absent`: nothing.
    Absent,
    /// `<level>:<partition>`: the payload, matched at `level`, in the partition numbered so.
    Matched(StoreMatch, u8),
    /// `evicted:<n>`
    Evicted(usize),
    /// `level:<n>`
    Level(usize),
    /// `swept:<n>`
    Swept(usize),
    /// `value:<n>`
    Value(u64),
    /// `refused:<n>`: a swap found this value instead, and changed nothing.
    Refused(u64),
    /// `listed:<i>=<n>,...`: every mutable key listed, with its value.
    Listed(Vec<(u64, u64)>),
    /// `failed:<message>`
    Failed(String),
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Done => write!(f, "done"),
            Outcome::Found => write!(f, "found"),
            Outcome::Wrong(served) => write!(f, "wrong:{served}"),
            Outcome::Absent => write!(f, "absent"),
            Outcome::Matched(level, partition) => {
                let level = match level {
                    StoreMatch::MatchFull => "full",
                    StoreMatch::MatchPartition => "partition",
                    _ => "hash",
                };
                write!(f, "{level}:{partition}")
            }
            Outcome::Evicted(count) => write!(f, "evicted:{count}"),
            Outcome::Level(count) => write!(f, "level:{count}"),
            Outcome::Swept(count) => write!(f, "swept:{count}"),
            Outcome::Value(value) => write!(f, "value:{value}"),
            Outcome::Refused(value) => write!(f, "refused:{value}"),
            Outcome::Listed(pairs) => {
                let pairs: Vec<String> = pairs
                    .iter()
                    .map(|(index, value)| format!("{index}={value}"))
                    .collect();
                write!(f, "listed:{}", pairs.join(","))
            }
            Outcome::Failed(message) => write!(f, "failed:{}", message.replace(' ', "_")),
        }
    }
}

/// The reply to a request that ran: the loads, then the outcomes.
pub fn ran(loads: [usize; 2], outcomes: &[Outcome]) -> String {
    let mut reply = format!("ran {} {}", loads[0], loads[1]);
    for outcome in outcomes {
        reply.push_str(&format!(" {outcome}"));
    }
    reply
}

pub async fn main(args: &Args) -> u8 {
    let Some(dir) = args.dir() else {
        eprintln!("client needs a store directory");
        return 2;
    };
    match serve(&dir, args.flag("keeps")).await {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("client failed: {err}");
            3
        }
    }
}

/// Serve requests on the stores of `dir`, keeping them between calls when `keeps`, until the
/// requests end.
#[expect(
    clippy::exit,
    reason = "a hung client leaves at once, so the script waiting on its reply reads its end"
)]
async fn serve(dir: &Path, keeps: bool) -> Result<(), String> {
    let config = Config::load(dir).map_err(|err| err.to_string())?;
    let mut client = Client {
        dir: dir.to_path_buf(),
        config,
        keeps,
        loads: Loads::new(dir)?,
        kept: None,
        held: None,
    };
    let (lines, mut requests) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                return;
            }
        }
    });
    while let Some(line) = requests.recv().await {
        let handled =
            tokio::time::timeout(Duration::from_secs(HANG_SECONDS), client.handle(&line)).await;
        let Ok(result) = handled else {
            eprintln!("client counts itself hung on {line}");
            std::process::exit(2);
        };
        result?;
    }
    Ok(())
}

/// Write `line` as a reply.
fn answer(line: &str) -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}")
        .and_then(|()| out.flush())
        .map_err(|err| err.to_string())
}

/// A scripted client's state.
struct Client {
    dir: PathBuf,
    config: Config,
    keeps: bool,
    loads: Loads,
    /// The stores the last call kept, where the client keeps them.
    kept: Option<Stores>,
    /// The pause point a `cancel` request left a flush held at, released at the next request.
    held: Option<Arc<lore_storage::test_util::Pause>>,
}

impl Client {
    /// Serve the request `line`, see the module documentation.
    async fn handle(&mut self, line: &str) -> Result<(), String> {
        if let Some(pause) = self.held.take() {
            pause.resume.notify_one();
        }
        let mut words = line.split_whitespace();
        let request = words.next().unwrap_or_default();
        let point = match request {
            "pause" | "cancel" => Some(pause_point(words.next().unwrap_or_default())?),
            _ => None,
        };
        let ops: Vec<Op> = words.map(str::parse).collect::<Result<_, _>>()?;
        match (request, point) {
            ("call" | "unflushed" | "probed" | "cancel", _) => {
                if request == "probed" {
                    let busy = self.loads.busy().await?;
                    answer(&format!("probed {} {}", busy[0], busy[1]))?;
                }
                let before = self.loads.now();
                let (stores, claims) = self.begin().await?;
                let outcomes = self.run_all(&stores, &ops).await;
                if let Some(point) = point {
                    let pause = lore_storage::test_util::pause_at(point);
                    tokio::select! {
                        biased;
                        () = pause.reached.notified() => {}
                        _ = stores.flush() => {
                            pause.resume.notify_one();
                            return answer("unreached");
                        }
                    }
                    self.held = Some(pause);
                } else if request != "unflushed" {
                    for flushed in stores.flush().await {
                        flushed.map_err(|err| format!("the call's flush failed: {err}"))?;
                    }
                }
                drop(claims);
                if self.keeps {
                    self.kept = Some(stores);
                }
                answer(&ran(self.loads.since(before), &outcomes))
            }
            ("hold", _) => {
                let before = self.loads.now();
                let (stores, _claims) = self.begin().await?;
                let outcomes = self.run_all(&stores, &ops).await;
                answer(&ran(self.loads.since(before), &outcomes))?;
                std::future::pending().await
            }
            ("pause", Some(point)) => {
                let (stores, _claims) = self.begin().await?;
                let pause = lore_storage::test_util::pause_at(point);
                // Held, not dropped, once at the step, so that nothing it started goes on or
                // unwinds before the process is killed.
                let mut work = std::pin::pin!(async {
                    self.run_all(&stores, &ops).await;
                    stores.flush().await
                });
                tokio::select! {
                    biased;
                    () = pause.reached.notified() => {}
                    _ = &mut work => {
                        pause.resume.notify_one();
                        return answer("unreached");
                    }
                }
                answer("paused")?;
                std::future::pending().await
            }
            ("flush" | "sweep", _) => {
                let before = self.loads.now();
                let stores = self
                    .kept
                    .as_ref()
                    .ok_or_else(|| format!("{request} outside a call needs kept stores"))?;
                let outcomes: Vec<Outcome> = match request {
                    "flush" => stores.flush().await.into_iter().map(done).collect(),
                    _ => stores.sweep().await.map(Outcome::Swept).to_vec(),
                };
                answer(&ran(self.loads.since(before), &outcomes))
            }
            _ => Err(format!("no request {request}")),
        }
    }

    /// The stores of a call, claimed: the kept ones, or ones opened afresh, whose opening claims
    /// the call's claims join.
    async fn begin(&mut self) -> Result<(Stores, Claims), String> {
        let (stores, openings) = if let Some(stores) = self.kept.take() {
            (stores, None)
        } else {
            let (stores, openings) = Stores::open(
                &self.dir,
                &self.config,
                self.keeps,
                FLUSH_DELAY_SECONDS,
                false,
            )
            .await?;
            (stores, Some(openings))
        };
        let claims = stores.claim().await?;
        drop(openings);
        Ok((stores, claims))
    }

    async fn run_all(&self, stores: &Stores, ops: &[Op]) -> Vec<Outcome> {
        let mut outcomes = Vec::with_capacity(ops.len());
        for op in ops {
            outcomes.push(self.run(stores, *op).await);
        }
        outcomes
    }

    async fn run(&self, stores: &Stores, op: Op) -> Outcome {
        let config = &self.config;
        let immutable: Arc<dyn ImmutableStore> = stores.immutable.clone();
        let mutable: Arc<dyn MutableStore> = stores.mutable.clone();
        match op {
            Op::Put(key, variant) => {
                let mut keys = match config.is_list(key) {
                    true => config.children(key),
                    false => vec![],
                };
                keys.push(key);
                for key in keys {
                    let put = immutable
                        .clone()
                        .put(
                            config.partition(variant),
                            config.address(key, variant),
                            config.fragment(key, variant),
                            Some(config.payload(key)),
                            false,
                        )
                        .await;
                    if put.is_err() {
                        return done(put);
                    }
                }
                Outcome::Done
            }
            Op::Get(key, variant) => {
                let got = immutable
                    .get(config.partition(variant), config.address(key, variant))
                    .await;
                match got {
                    Ok(data) => match data.payload {
                        Some(payload) => self.payload(key, &payload),
                        None => Outcome::Failed("no payload".to_string()),
                    },
                    Err(err) => missing(err),
                }
            }
            Op::GetInto(key, variant) => {
                let mut buffer = vec![0u8; config.payload_len(key)];
                // SAFETY: `buffer` outlives the read, which is awaited to its end, and nothing
                // else touches it meanwhile.
                let mut destination =
                    unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
                let read = immutable
                    .get_into(
                        config.partition(variant),
                        config.address(key, variant),
                        &mut destination,
                    )
                    .await;
                drop(destination);
                match read {
                    Ok((fragment, PayloadRead::IntoBuffer)) => {
                        let read = (fragment.size_payload as usize).min(buffer.len());
                        self.payload(key, &buffer[..read])
                    }
                    Ok((_, PayloadRead::Returned(bytes))) => self.payload(key, &bytes),
                    Err(err) => missing(err),
                }
            }
            Op::Query(key, variant) => {
                match query_one(
                    &immutable,
                    config.partition(variant),
                    config.address(key, variant),
                )
                .await
                {
                    Ok(found) => matched(found.match_made, found.partition),
                    Err(err) => missing(err),
                }
            }
            Op::Metadata(key, variant) => {
                match immutable
                    .get_metadata(config.partition(variant), config.address(key, variant))
                    .await
                {
                    Ok(found) => matched(found.match_made, found.partition),
                    Err(err) => missing(err),
                }
            }
            Op::Copy(key, from, to) => done(
                immutable
                    .copy(
                        config.partition(from),
                        config.address(key, from),
                        config.partition(to),
                        config.context(to),
                        CopyBehavior {
                            durable: config.evictable(to) && !config.server,
                            do_not_replicate: false,
                        },
                    )
                    .await,
            ),
            Op::Obliterate(key, variant) => done(
                immutable
                    .obliterate(
                        config.partition(variant),
                        config.address(key, variant),
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
                        Ok(None) => return Outcome::Done,
                        Err(err) => return Outcome::Failed(err.to_string()),
                    }
                }
            }
            Op::LoadIndex => done(immutable.load_index().await),
            Op::Verify => done(immutable.verify(false).await),
            Op::Level(key) => Outcome::Level(
                stores.immutable.group[usize::from(config.hash(key).data()[0])]
                    .bucket_count
                    .load(Ordering::Relaxed),
            ),
            Op::Store(index, value) => done(
                mutable
                    .store(
                        mutable::partition(),
                        mutable::key(index),
                        mutable::value(index, value),
                        mutable::KEY_TYPE,
                    )
                    .await,
            ),
            Op::Load(index) => {
                let loaded = mutable
                    .load(mutable::partition(), mutable::key(index), mutable::KEY_TYPE)
                    .await;
                match mutable::Read::of(index, &loaded) {
                    Ok(mutable::Read::Is(value)) => Outcome::Value(value),
                    Ok(mutable::Read::Absent) => Outcome::Absent,
                    Ok(mutable::Read::Foreign) => Outcome::Failed("a foreign value".to_string()),
                    Err(err) => Outcome::Failed(err),
                }
            }
            Op::Swap(index, from, to) => {
                let expected = mutable::value(index, from);
                let swapped = mutable
                    .compare_and_swap(
                        mutable::partition(),
                        mutable::key(index),
                        expected,
                        mutable::value(index, to),
                        mutable::KEY_TYPE,
                    )
                    .await;
                match swapped {
                    Ok(previous) if previous == expected => Outcome::Done,
                    Ok(previous) => match mutable::value_of(index, &previous) {
                        Some(current) => Outcome::Refused(current),
                        None => Outcome::Failed("a foreign value".to_string()),
                    },
                    Err(err) => Outcome::Failed(err.to_string()),
                }
            }
            Op::List => match mutable::list(&mutable).await {
                Ok(pairs) => {
                    let mut listed: Vec<(u64, u64)> = pairs
                        .iter()
                        .map(|(hash, stored)| {
                            let index = mutable::index_of(hash).unwrap_or(u64::MAX);
                            (index, mutable::value_of(index, stored).unwrap_or(u64::MAX))
                        })
                        .collect();
                    listed.sort_unstable();
                    Outcome::Listed(listed)
                }
                Err(err) => Outcome::Failed(err),
            },
            Op::Flush => match stores.flush().await {
                [Ok(()), Ok(())] => Outcome::Done,
                [Err(err), _] | [_, Err(err)] => Outcome::Failed(err.to_string()),
            },
        }
    }

    /// [`Outcome::Found`] for the bytes of `key`, and [`Outcome::Wrong`] for others.
    fn payload(&self, key: u64, bytes: &[u8]) -> Outcome {
        match self.config.classify(key, bytes) {
            Served::Correct => Outcome::Found,
            served => Outcome::Wrong(served),
        }
    }
}

fn done(result: Result<(), StoreError>) -> Outcome {
    match result {
        Ok(()) => Outcome::Done,
        Err(err) => missing(err),
    }
}

/// [`Outcome::Absent`] for an error saying nothing was there, and [`Outcome::Failed`] for any
/// other.
fn missing(err: StoreError) -> Outcome {
    match err {
        StoreError::AddressNotFound(_) | StoreError::PayloadNotFound(_) => Outcome::Absent,
        err => Outcome::Failed(err.to_string()),
    }
}

/// What a lookup that matched at `level` in `partition` found, by the partition's number.
fn matched(level: StoreMatch, partition: lore_storage::Partition) -> Outcome {
    match level {
        StoreMatch::MatchNone => Outcome::Absent,
        level => Outcome::Matched(level, partition.data()[0].wrapping_sub(0x40)),
    }
}
