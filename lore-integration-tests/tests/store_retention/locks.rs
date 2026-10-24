// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The cross-process locks a call takes: the immutable store's, the mutable store's and the
//! repository's, always in that order, so that processes running calls on one repository, or on
//! repositories over one shared store, never wait on each other for good.
//!
//! The order tests take one of the locks in another process's place, through a descriptor of the
//! test's own, which a flock excludes as it excludes another process. A call waiting for that lock
//! must hold every lock before it in the order and none after it, whether its process kept
//! nothing of its stores from the call before or kept only the immutable store. The tests across
//! processes run calls in several copies of this binary on one store at once, some retaining
//! their stores, and fail if a copy does not finish.

use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use lore::revision;
use lore_base::fs::lock::FSLock;
use lore_base::runtime::runtime_flush_guarded;
use lore_base::test_util::TempDir;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreString;
use lore_storage::ImmutableStore;
use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
use lore_storage::local::store_lock::StoreGuard;
use rand::Rng as _;
use rand::SeedableRng;
use rand::rngs::StdRng;
use tokio::time::timeout;

/// Names, for a copy running calls across processes, the directory it works in, which the test
/// removes.
const CHILD_DIR: &str = "LORE_STORE_RETENTION_TEST_DIR";

/// Names, for a copy running calls across processes, its part: `setup`, or its index as a caller.
const CHILD_PART: &str = "LORE_STORE_RETENTION_TEST_PART";

/// Seeds the mixes of calls where set, so a run can be repeated.
const SEED: &str = "LORE_STORE_RETENTION_TEST_SEED";

/// How long a call is given to take the locks before the one it waits for, and a process to let
/// its stores go once its calls have ended.
const SETTLE: Duration = Duration::from_secs(5);

/// How long a waiting call is watched for a lock after the one it waits for, from when it holds
/// the locks before that one, or from its start where there are none.
const WATCH: Duration = Duration::from_secs(1);

/// How long each copy runs calls in the tests across processes.
const CALLS_FOR: Duration = Duration::from_secs(8);

/// How many copies run calls at once in the tests across processes.
const CALLERS: usize = 6;

/// The locks a call takes, in the order it takes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lock {
    Immutable,
    Mutable,
    Repository,
}

const ORDER: [Lock; 3] = [Lock::Immutable, Lock::Mutable, Lock::Repository];

/// What backs a repository's stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backing {
    /// Stores of its own, under its `.lore`.
    Own,
    /// A shared store, which other repositories use too.
    Shared,
}

/// What a call does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call {
    /// `lore history`, which reads.
    Read,
    /// A call that holds the write token and writes nothing.
    Write,
    /// A storage handle opened, written through and closed.
    Handle,
}

/// Creates a shared store at `path`, kept out of the global config.
async fn create_shared_store(path: &Path) {
    let status = lore::shared_store::create(
        LoreGlobalArgs {
            offline: 1,
            ..LoreGlobalArgs::default()
        },
        lore::shared_store::LoreSharedStoreCreateArgs {
            remote_url: super::REMOTE_URL.into(),
            path: LoreString::from(path.to_str().expect("a UTF-8 path")),
            make_default: 0,
        },
        None,
    )
    .await;
    assert_eq!(status, 0, "creating the shared store at {}", path.display());
}

/// Creates an offline repository at `path` with one revision, over the shared store at `shared`
/// or with stores of its own. Its calls, and the flushes they leave behind, have ended when this
/// returns.
async fn create_with_revision(path: &Path, shared: Option<&Path>) {
    std::fs::create_dir_all(path).expect("creates the repository directory");
    super::create_repository_over(path, shared).await;
    commit_file(path, 0).await;
    runtime_flush_guarded().await;
}

/// Writes a file of its own for `round` in `repository`, and stages and commits it.
async fn commit_file(repository: &Path, round: usize) {
    std::fs::write(
        repository.join(format!("file-{round}.txt")),
        format!("round {round}"),
    )
    .expect("writes a file");
    super::stage(repository).await;
    super::commit(repository).await;
}

/// Runs `lore history` against `repository`, asking for a keep-alive of
/// [`super::ENDING_WINDOW_SECONDS`] if `keep_alive`.
async fn history(repository: &Path, keep_alive: bool) {
    let status = revision::history(
        super::windowed_globals(repository, keep_alive, super::ENDING_WINDOW_SECONDS),
        revision::LoreRevisionHistoryArgs::default(),
        None,
    )
    .await;
    assert_eq!(status, 0, "lore history on {}", repository.display());
}

/// Runs `call` against `repository`, asking for no keep-alive.
async fn run(call: Call, repository: &Path) {
    match call {
        Call::Read => history(repository, false).await,
        Call::Write => {
            super::call(repository, false, super::WINDOW_SECONDS).await;
        }
        Call::Handle => {
            let store = super::open_handle(repository).await;
            let payload = format!("through a handle at {:?}", Instant::now());
            super::put(repository, store, payload.as_bytes()).await;
            super::close_handle(repository, store).await;
        }
    }
}

/// The immutable store calls against `repository` open, with the claim it was opened under,
/// `None` when this process kept it from before.
async fn open_immutable(repository: &Path) -> (Arc<dyn ImmutableStore>, Option<StoreGuard>) {
    let dot = lore_revision::repository::get_dot_lore_path(repository).expect("a repository");
    let config = lore_revision::repository::load_repository_config(repository).expect("its config");
    lore_revision::repository::create_client_immutable_store(
        &config,
        &dot,
        ImmutableStoreCreateOptions::none(),
        false,
    )
    .await
    .expect("opens the immutable store")
}

/// The lock files of a repository and of its stores.
struct LockFiles {
    /// The repository's `.lore` directory, whose lock is the repository's.
    dot: PathBuf,
    /// The directory holding the immutable and the mutable store: `.lore`, or a shared store.
    stores: PathBuf,
}

impl LockFiles {
    async fn of(repository: &Path) -> Self {
        let dot = lore_revision::repository::get_dot_lore_path(repository).expect("a repository");
        let config =
            lore_revision::repository::load_repository_config(repository).expect("its config");
        let stores = lore_revision::shared_store::get_shared_store_path_for_repo(&config)
            .await
            .expect("resolves the store directory")
            .unwrap_or_else(|| dot.clone());
        Self { dot, stores }
    }

    fn path(&self, lock: Lock) -> PathBuf {
        match lock {
            Lock::Immutable => self.stores.join("immutable").join("lock"),
            Lock::Mutable => self.stores.join("mutable").join("lock"),
            Lock::Repository => self.dot.join("lock"),
        }
    }

    /// Takes `lock` in another process's place, waiting for it.
    async fn take(&self, lock: Lock) -> FSLock {
        FSLock::acquire_exact_path(&self.path(lock))
            .await
            .unwrap_or_else(|err| panic!("takes the {lock:?} lock: {err}"))
    }

    /// Takes `lock` in another process's place if nothing else holds it.
    async fn try_take(&self, lock: Lock) -> Option<FSLock> {
        FSLock::try_acquire_exact_path(&self.path(lock))
            .await
            .unwrap_or_else(|err| panic!("tries the {lock:?} lock: {err}"))
    }

    /// The first lock of `locks` that anything else holds, this process's calls included.
    async fn first_held(&self, locks: &[Lock]) -> Option<Lock> {
        for &lock in locks {
            if self.try_take(lock).await.is_none() {
                return Some(lock);
            }
        }
        None
    }

    /// Whether anything else holds every lock of `locks`, this process's calls included.
    async fn all_held(&self, locks: &[Lock]) -> bool {
        for &lock in locks {
            if self.try_take(lock).await.is_some() {
                return false;
            }
        }
        true
    }
}

/// A repository in `dir`, backed as `backing`.
async fn test_repository(dir: &Path, backing: Backing) -> PathBuf {
    let repository = dir.join("repository");
    match backing {
        Backing::Own => create_with_revision(&repository, None).await,
        Backing::Shared => {
            let shared = dir.join("shared");
            create_shared_store(&shared).await;
            create_with_revision(&repository, Some(&shared)).await;
        }
    }
    repository
}

/// Runs the test named `test` in a copy of this binary that retains no stores: each kind of call
/// that takes `blocked`, finding nothing kept and finding only the immutable store kept, waits for
/// `blocked` while another process holds it, holding every lock before it in [`ORDER`] and none
/// after it, and ends once that process lets go. A storage handle takes no repository lock.
async fn each_call_waits_for(test: &str, blocked: Lock, backing: Backing) {
    let test = super::harness_name(module_path!(), test);
    super::in_child(&test, false, || async move {
        let dir = TempDir::new("lore-store-locks-");
        let repository = test_repository(dir.path(), backing).await;
        let locks = LockFiles::of(&repository).await;
        let calls: &[Call] = match blocked {
            Lock::Repository => &[Call::Read, Call::Write],
            _ => &[Call::Read, Call::Write, Call::Handle],
        };
        for &call in calls {
            for keeps_immutable in [false, true] {
                waits_for(&repository, &locks, blocked, call, keeps_immutable).await;
            }
        }
    })
    .await;
}

/// [`each_call_waits_for`] for one kind of call, with only the immutable store kept from before
/// if `keeps_immutable`, as a read call's closing flush keeps it.
async fn waits_for(
    repository: &Path,
    locks: &LockFiles,
    blocked: Lock,
    call: Call,
    keeps_immutable: bool,
) {
    let kept = if keeps_immutable {
        "only the immutable store"
    } else {
        "nothing"
    };
    let case = format!("a {call:?} call finding {kept} kept, waiting for the {blocked:?} lock");

    let (immutable, opening) = open_immutable(repository).await;
    assert!(
        opening.is_some(),
        "{case}: the calls before it left the immutable store kept"
    );
    drop(opening);
    let immutable = keeps_immutable.then_some(immutable);

    let position = ORDER
        .iter()
        .position(|lock| *lock == blocked)
        .expect("in the order");
    let (before, after) = (&ORDER[..position], &ORDER[position + 1..]);

    let other = locks.take(blocked).await;
    let ended = AtomicBool::new(false);
    let calling = async {
        run(call, repository).await;
        ended.store(true, Ordering::Release);
    };
    let watching = async {
        let reached = timeout(SETTLE, super::until(async || locks.all_held(before).await))
            .await
            .is_ok();
        let mut overtaken = None;
        if !after.is_empty() {
            let _ = timeout(
                WATCH,
                super::until(async || {
                    overtaken = locks.first_held(after).await;
                    overtaken.is_some()
                }),
            )
            .await;
        }
        let waited = !ended.load(Ordering::Acquire);
        drop(other);
        (reached, overtaken, waited)
    };
    let ((), (reached, overtaken, waited)) = tokio::join!(calling, watching);
    drop(immutable);
    runtime_flush_guarded().await;

    assert!(waited, "{case}: the call waits while the lock is held");
    assert!(
        reached,
        "{case}: the call holds every lock before the one it waits for, {before:?}"
    );
    assert_eq!(
        overtaken, None,
        "{case}: the call holds no lock after the one it waits for, against a process holding \
         that one and waiting for it"
    );
}

/// **A call waiting for the immutable store holds no other lock**, so the process holding the
/// immutable store can take the mutable store and the repository after it, as every call does,
/// whether the call's process kept nothing or kept only the immutable store, which leaves the
/// call to open the mutable store before it claims the immutable store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_waiting_for_the_immutable_store_holds_no_other_lock() {
    each_call_waits_for(
        "a_call_waiting_for_the_immutable_store_holds_no_other_lock",
        Lock::Immutable,
        Backing::Own,
    )
    .await;
}

/// **A call on a shared store waiting for its immutable store holds no other lock**, as on
/// stores of the repository's own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_on_a_shared_store_waiting_for_its_immutable_store_holds_no_other_lock() {
    each_call_waits_for(
        "a_call_on_a_shared_store_waiting_for_its_immutable_store_holds_no_other_lock",
        Lock::Immutable,
        Backing::Shared,
    )
    .await;
}

/// **A call waiting for the mutable store holds the immutable store and not the repository.**
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_waiting_for_the_mutable_store_holds_the_immutable_store_alone() {
    each_call_waits_for(
        "a_call_waiting_for_the_mutable_store_holds_the_immutable_store_alone",
        Lock::Mutable,
        Backing::Own,
    )
    .await;
}

/// **A call waiting for the repository holds both stores**, the repository's lock being taken
/// inside them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_waiting_for_the_repository_holds_both_stores() {
    each_call_waits_for(
        "a_call_waiting_for_the_repository_holds_both_stores",
        Lock::Repository,
        Backing::Own,
    )
    .await;
}

/// The generator of a mix of calls, `stream` of the seed [`SEED`] names, or of one drawn at
/// random. The seed goes to stderr, which a copy's test harness does not capture, so a copy killed
/// as hung still reports it, in one write, so lines from copies running at once do not interleave.
fn mix_rng(stream: u64) -> StdRng {
    let seed = std::env::var(SEED)
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or_else(rand::random::<u64>);
    let line = format!("{SEED}={seed}, stream {stream}\n");
    let _ = std::io::stderr().write_all(line.as_bytes());
    StdRng::seed_from_u64(seed.wrapping_add(stream))
}

/// Runs a random mix of calls against `repository` until `stop` says to, each of which must
/// succeed: reads, writes, storage handles, reads asking for a keep-alive, calls at once in this
/// process, and commits of a file of its own where `commits`. Now and then it keeps the immutable
/// store and not the mutable store between calls, as a read call's closing flush does, holds a
/// storage handle open across calls, and pauses between calls, as a process with nothing to do
/// does. A keep-alive is asked for once the last one's window has ended and a second more, since
/// windows that overlap would keep both stores for the whole run.
async fn run_calls(repository: &Path, rng: &mut StdRng, commits: bool, stop: impl Fn() -> bool) {
    let mut kept = None;
    let mut held_open = None;
    let mut round = 1;
    let mut keep_alive_after = Instant::now();
    while !stop() {
        match rng.random_range(0..8) {
            0 => kept = Some(open_immutable(repository).await.0),
            1 => kept = None,
            2 if held_open.is_none() => held_open = Some(super::open_handle(repository).await),
            3 => {
                if let Some(store) = held_open.take() {
                    super::close_handle(repository, store).await;
                }
            }
            _ => {}
        }
        match rng.random_range(0..16) {
            11 if Instant::now() >= keep_alive_after => {
                history(repository, true).await;
                keep_alive_after =
                    Instant::now() + Duration::from_secs(super::ENDING_WINDOW_SECONDS + 1);
            }
            0..=4 | 11 => run(Call::Read, repository).await,
            5..=7 => run(Call::Write, repository).await,
            8..=10 => run(Call::Handle, repository).await,
            12 | 13 => {
                tokio::join!(run(Call::Read, repository), run(Call::Write, repository));
            }
            _ => {
                tokio::join!(run(Call::Read, repository), run(Call::Handle, repository));
            }
        }
        if commits && rng.random_range(0..4) == 0 {
            commit_file(repository, round).await;
        }
        if rng.random_range(0..4) == 0 {
            tokio::time::sleep(Duration::from_millis(rng.random_range(0..20))).await;
        }
        round += 1;
    }
    if let Some(store) = held_open {
        super::close_handle(repository, store).await;
    }
    drop(kept);
    runtime_flush_guarded().await;
}

/// **A process holds the mutable store only while it holds the immutable store**, through every
/// kind of call and what each leaves behind: closing flushes, keep-alives, storage handles held
/// open and closed, calls at once, and only the immutable store kept between calls. A probe takes
/// the immutable store's lock whenever it is free, as another process starting a call does, and
/// tries the mutable store's while it holds it: held then, it is held by a process that will take
/// the immutable store's after it, and waits for the probe. Once the calls have ended the process
/// holds neither store, so no other process is kept out for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_process_holds_the_mutable_store_only_inside_the_immutable_store() {
    let test = super::harness_name(
        module_path!(),
        "a_process_holds_the_mutable_store_only_inside_the_immutable_store",
    );
    super::in_child(&test, false, || async {
        let dir = TempDir::new("lore-store-locks-probe-");
        let repository = test_repository(dir.path(), Backing::Own).await;
        let locks = LockFiles::of(&repository).await;
        let mut rng = mix_rng(0);

        let stop = AtomicBool::new(false);
        let probes = AtomicUsize::new(0);
        let calling = async {
            let started = Instant::now();
            run_calls(&repository, &mut rng, true, || {
                started.elapsed() >= CALLS_FOR
            })
            .await;
            stop.store(true, Ordering::Release);
        };
        let probing = async {
            while !stop.load(Ordering::Acquire) {
                if let Some(immutable) = locks.try_take(Lock::Immutable).await {
                    probes.fetch_add(1, Ordering::Relaxed);
                    let mutable_held = locks.first_held(&[Lock::Mutable]).await.is_some();
                    drop(immutable);
                    if mutable_held {
                        return true;
                    }
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            false
        };
        let ((), outside) = tokio::join!(calling, probing);
        let stores = [Lock::Immutable, Lock::Mutable];
        let window = Duration::from_secs(super::ENDING_WINDOW_SECONDS);
        assert!(
            timeout(
                window + SETTLE,
                super::until(async || locks.first_held(&stores).await.is_none())
            )
            .await
            .is_ok(),
            "the process lets both stores go once its calls and keep-alives have ended"
        );
        assert!(
            probes.load(Ordering::Relaxed) > 0,
            "the probe found the immutable store free at times"
        );
        assert!(
            !outside,
            "the mutable store was held while the immutable store was free"
        );
    })
    .await;
}

/// The repository caller `index` runs its calls against in `dir`: one repository for every
/// caller, or one of its own over the shared store.
fn caller_repository(dir: &Path, backing: Backing, index: usize) -> PathBuf {
    match backing {
        Backing::Own => dir.join("repository"),
        Backing::Shared => dir.join(format!("instance-{index}")),
    }
}

/// Sets up `backing` in `dir` for [`CALLERS`] callers.
async fn set_up(dir: &Path, backing: Backing) {
    match backing {
        Backing::Own => create_with_revision(&caller_repository(dir, backing, 0), None).await,
        Backing::Shared => {
            let shared = dir.join("shared");
            create_shared_store(&shared).await;
            for index in 0..CALLERS {
                create_with_revision(&caller_repository(dir, backing, index), Some(&shared)).await;
            }
        }
    }
}

/// Runs the test named `test` across processes: one copy of this binary sets up `backing`, then
/// [`CALLERS`] copies run calls on it at once for [`CALLS_FOR`], every other one retaining its
/// stores. Each copy commits to a repository of its own over a shared store; over one repository
/// none commits, since all share one working tree. Fails unless every copy finishes and passes.
async fn across_processes(test: &str, backing: Backing) {
    if let (Some(done), Ok(part)) = (
        std::env::var_os(super::CHILD_DONE),
        std::env::var(CHILD_PART),
    ) {
        let dir =
            PathBuf::from(std::env::var_os(CHILD_DIR).expect("a copy is given its directory"));
        if part == "setup" {
            set_up(&dir, backing).await;
        } else {
            let index: usize = part.parse().expect("a caller index");
            let started = Instant::now();
            run_calls(
                &caller_repository(&dir, backing, index),
                &mut mix_rng(index as u64),
                backing == Backing::Shared,
                || started.elapsed() >= CALLS_FOR,
            )
            .await;
        }
        std::fs::write(done, b"").expect("marks the part passed");
        return;
    }

    let test = super::harness_name(module_path!(), test);
    let work = TempDir::new("lore-store-locks-across-");
    let sandbox = work.path().join("sandbox");
    std::fs::create_dir_all(&sandbox).expect("creates the sandbox");
    let spawn = |part: String, retain: bool| {
        let done = work.path().join(format!("{part}.done"));
        let mut copy = super::copy_of_this_binary(&test, &sandbox);
        copy.env(super::CHILD_DONE, &done)
            .env(CHILD_DIR, work.path())
            .env(CHILD_PART, &part);
        if retain {
            copy.env("LORE_STORE_RETAIN", "1");
        }
        (copy.spawn().expect("spawn a copy of this binary"), done)
    };

    let (setup, done) = spawn("setup".to_string(), false);
    let (status, report) = super::wait_for_copy(setup).await;
    assert!(
        status.success() && done.exists(),
        "setting up failed: {status}\n{report}"
    );

    let mut callers = tokio::task::JoinSet::new();
    for index in 0..CALLERS {
        let (child, done) = spawn(index.to_string(), index % 2 == 0);
        lore_base::lore_spawn!(callers, async move {
            let (status, report) = super::wait_for_copy(child).await;
            (index, status, report, done.exists())
        });
    }
    for (index, status, report, done) in callers.join_all().await {
        assert!(
            status.success() && done,
            "caller {index} did not finish its calls: {status}\n{report}"
        );
    }
}

/// **Processes running calls on one repository at once all finish**, some retaining their
/// stores and some not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn processes_running_calls_on_one_repository_all_finish() {
    across_processes(
        "processes_running_calls_on_one_repository_all_finish",
        Backing::Own,
    )
    .await;
}

/// **Processes running calls on repositories over one shared store at once all finish**, each on
/// a repository of its own, as clones made with `--use-shared-store` are.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn processes_running_calls_on_repositories_over_one_shared_store_all_finish() {
    across_processes(
        "processes_running_calls_on_repositories_over_one_shared_store_all_finish",
        Backing::Shared,
    )
    .await;
}
