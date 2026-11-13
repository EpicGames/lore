// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A storage handle held open while repository commands run against the same repository,
//! in several processes at once.
//!
//! The storage API takes store locks and no repository lock; a repository command takes
//! both. If the two were ever acquired in opposite orders this is where it would show, and
//! because the lock wait has no timeout it would show as a process that never returns. The
//! probe therefore either completes or hangs, so the test runs it under a deadline, in
//! several copies of this binary against one repository.

use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use lore::repository;
use lore::storage::close;
use lore::storage::handle::LoreStore;
use lore::storage::open;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreString;
use lore_revision::repository::LoreSharedStoreMode;

use crate::call::test_support::opened_handle_id;
use crate::call::test_support::recording_callback;

/// Names, for a child process, the repository to run the probe against.
const CHILD_REPOSITORY: &str = "LORE_LOCK_INTERLEAVE_REPOSITORY";

/// Processes run against one repository at once.
const INSTANCES: usize = 6;

/// Times the set of processes is run, one set after another. The hang is a matter of
/// timing, so one set is weak evidence: the order violation this probes showed in 6 of
/// 15 sets of 6 processes and 4 rounds each; the counts here run some thousands of
/// commands through the locks in a few seconds.
const TRIALS: usize = 10;

/// Repository commands each process interleaves with its open handle.
const ROUNDS: usize = 64;

/// How long one set of processes gets before it is counted as hung.
const DEADLINE: Duration = Duration::from_secs(120);

/// The harness name of the test below, as a child process is told to run it.
const TEST: &str = "storage::lock_interleave::a_storage_handle_held_open_does_not_deadlock_repository_commands_across_processes";

/// Globals every call shares: local, offline, with stores kept alive between commands. The
/// child processes retain stores (`LORE_STORE_RETAIN`), so what they keep is memory with no
/// claim, which is the state a long-running client is in.
fn globals(repository: &Path) -> LoreGlobalArgs {
    LoreGlobalArgs {
        repository_path: LoreString::from(repository.to_str().expect("a UTF-8 path")),
        offline: 1,
        local: 1,
        store_keep_alive: 1,
        store_keep_alive_seconds: 2,
        ..LoreGlobalArgs::default()
    }
}

/// A status that reports the revision and touches nothing else.
fn status_args() -> repository::LoreRepositoryStatusArgs {
    repository::LoreRepositoryStatusArgs {
        staged: 0,
        scan: 0,
        check_dirty: 0,
        reset: 0,
        sync_point: 0,
        revision_only: 1,
        count: 0,
        paths: LoreArray::default(),
    }
}

/// Creates an offline repository at `path` with a store of its own.
async fn create_repository(path: &Path) {
    let status = repository::create(
        globals(path),
        repository::LoreRepositoryCreateArgs {
            repository_url: "lore://localhost/lock-interleave".into(),
            description: LoreString::default(),
            id: LoreString::default(),
            use_shared_store: LoreSharedStoreMode::Disabled,
            shared_store_path: LoreString::default(),
            vfs: Default::default(),
        },
        None,
    )
    .await;
    assert_eq!(status, 0, "creating the repository");
}

/// One repository command before the handle is opened, the handle opened and held, `rounds`
/// of repository commands while it is open, then the handle closed. The shape of a
/// long-running client: a summary, then a content handle held for the whole run.
async fn interleave(repository: &Path, rounds: usize) -> Result<(), String> {
    let status = repository::status(globals(repository), status_args(), None).await;
    if status != 0 {
        return Err(format!("opening status failed: {status}"));
    }

    let (events, callback) = recording_callback();
    let code = open::open(
        globals(repository),
        open::LoreStorageOpenArgs {
            repository_path: LoreString::from(repository.to_str().expect("a UTF-8 path")),
            in_memory: 0,
            ..Default::default()
        },
        callback,
    )
    .await;
    if code != 0 {
        return Err(format!("storage open failed: {code}"));
    }
    let handle = events
        .lock()
        .ok()
        .and_then(|events| opened_handle_id(&events))
        .ok_or_else(|| "storage open emitted no handle".to_owned())?;

    for round in 0..rounds {
        let status = repository::status(globals(repository), status_args(), None).await;
        if status != 0 {
            return Err(format!("status failed in round {round}: {status}"));
        }
        let history = lore::revision::history(
            globals(repository),
            lore::revision::LoreRevisionHistoryArgs {
                length: 16,
                ..Default::default()
            },
            None,
        )
        .await;
        if history != 0 {
            return Err(format!("history failed in round {round}: {history}"));
        }
    }

    let closed = close::close(
        globals(repository),
        close::LoreStorageCloseArgs {
            handle: LoreStore { handle_id: handle },
            ..Default::default()
        },
        None,
    )
    .await;
    if closed != 0 {
        return Err(format!("storage close failed: {closed}"));
    }
    Ok(())
}

/// **A storage handle held open does not deadlock repository commands, across processes.**
///
/// Six copies of this binary run the probe against one repository at once, each holding
/// a storage handle open while it runs repository commands, which is the pairing that
/// exercises the containment of the repository lock within the store locks. The set is
/// run ten times over. Every copy must complete before the deadline; one that does not
/// is a hang, which is what the lock order violation looks like. Run on demand:
///
/// ```sh
/// cargo test -p lore --test unit -- --ignored --exact \
///     storage::lock_interleave::a_storage_handle_held_open_does_not_deadlock_repository_commands_across_processes
/// ```
///
/// A copy of this binary told which repository to use runs the probe once and returns;
/// the copy that is told nothing creates the repository and runs the others.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "slow: runs several copies of this binary against one repository; run on demand with --ignored"]
async fn a_storage_handle_held_open_does_not_deadlock_repository_commands_across_processes() {
    if let Ok(repository) = std::env::var(CHILD_REPOSITORY) {
        interleave(Path::new(&repository), ROUNDS)
            .await
            .expect("the probe completes in a child process");
        return;
    }

    let dir = lore_base::test_util::TempDir::new("lore-lock-interleave-");
    create_repository(dir.path()).await;
    interleave(dir.path(), 2)
        .await
        .expect("the probe completes in this process");

    let binary = std::env::current_exe().expect("locate this binary");
    for trial in 0..TRIALS {
        run_processes_once(&binary, dir.path(), trial).await;
    }
}

/// Runs [`INSTANCES`] copies of `binary` against `repository` at once and waits for every
/// one, failing the test if one does not complete before [`DEADLINE`] or reports a failed
/// call.
async fn run_processes_once(binary: &Path, repository: &Path, trial: usize) {
    let mut running: Vec<std::process::Child> = (0..INSTANCES)
        .map(|_| {
            // The harness summary comes back on stdout, and is read once the child has
            // exited: it is a few lines, well within the pipe.
            Command::new(binary)
                .args([TEST, "--exact", "--ignored"])
                .env(CHILD_REPOSITORY, repository)
                .env("LORE_STORE_RETAIN", "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("spawn a copy of this binary")
        })
        .collect();

    let deadline = Instant::now() + DEADLINE;
    let mut failed = 0;
    while !running.is_empty() {
        let mut still_running = Vec::with_capacity(running.len());
        for mut child in running {
            match child.try_wait().expect("poll a child process") {
                Some(status) => {
                    // A harness given a name that matches nothing exits zero having run
                    // nothing, so the summary has to say the probe ran.
                    let mut summary = String::new();
                    if let Some(mut stdout) = child.stdout.take() {
                        let _ = stdout.read_to_string(&mut summary);
                    }
                    if !status.success() || !summary.contains("1 passed") {
                        eprintln!(
                            "trial {trial}: a child process did not complete the probe: \
                             {status}\n{summary}"
                        );
                        failed += 1;
                    }
                }
                None => still_running.push(child),
            }
        }
        running = still_running;
        if running.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            for child in &mut running {
                let _ = child.kill();
            }
            panic!(
                "trial {trial}: {} of {INSTANCES} processes had not completed after \
                 {DEADLINE:?}: a repository command and a storage handle are waiting on each \
                 other's locks",
                running.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        failed, 0,
        "trial {trial}: {failed} of {INSTANCES} processes reported a failed call"
    );
}
