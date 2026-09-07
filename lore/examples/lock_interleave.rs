// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Concurrency probe for the store and repository locks.
//!
//! Holds a `lore_storage_open` handle open — what a long-running client does — and
//! interleaves repository commands with it. That is the pairing that exercises
//! containment: the storage API takes store locks and no repository lock, while a
//! repository command takes both, so if the two are ever acquired in opposite orders
//! this is where it shows. `FSLock`'s wait is unbounded, so an inversion is a hang
//! that never ends rather than a slow call — the probe either completes or it does
//! not return.
//!
//! Run several against one repository at once. Each instance holds its store claims
//! from open to close, so instances take turns on the stores; the probe passes when
//! every instance completes.
//!
//! ```sh
//! cargo run --release --example lock_interleave -- <repository> <rounds>
//! ```
//!
//! Exits non-zero if any call fails, so a caller can tell a hang (no exit) from
//! an error (an exit that says so).

use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreString;

/// Globals every call in this example shares: local, offline, nothing to fetch.
fn globals(repository: &str) -> LoreGlobalArgs {
    LoreGlobalArgs {
        repository_path: LoreString::from(repository),
        offline: 1,
        local: 1,
        // On, so stores stay alive between commands as retained memory with no claim,
        // which is the state a long-running client is in.
        store_keep_alive: 1,
        store_keep_alive_seconds: 2,
        ..LoreGlobalArgs::default()
    }
}

/// A status that reports the revision and touches nothing else.
fn status_args() -> lore::repository::LoreRepositoryStatusArgs {
    lore::repository::LoreRepositoryStatusArgs {
        staged: 0,
        scan: 0,
        check_dirty: 0,
        reset: 0,
        sync_point: 0,
        revision_only: 1,
        count: 0,
        paths: lore_revision::interface::LoreArray::default(),
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(repository) = args.next() else {
        eprintln!("usage: lock_interleave <repository> [rounds]");
        return std::process::ExitCode::FAILURE;
    };
    let rounds: usize = args.next().and_then(|arg| arg.parse().ok()).unwrap_or(4);

    // A repository command **before** the handle is opened, then commands while it
    // is open: the shape of a long-running client, which `tome-mcp --index` has — a
    // summary, then a content handle held for the whole run. A path that took the
    // repository lock outside the store locks would show here as a hang.
    let first = lore::repository::status(globals(&repository), status_args(), None).await;
    if first != 0 {
        eprintln!("opening status failed: {first}");
        return std::process::ExitCode::FAILURE;
    }

    // The handle a long-running client keeps, holding both store claims for as long
    // as it is open.
    let opened: std::sync::Arc<std::sync::Mutex<Option<u64>>> = std::sync::Arc::default();
    let sink = std::sync::Arc::clone(&opened);
    let code = lore::storage::open::open(
        globals(&repository),
        lore::storage::open::LoreStorageOpenArgs {
            repository_path: LoreString::from(repository.as_str()),
            in_memory: 0,
            ..Default::default()
        },
        Some(Box::new(move |event: &lore::interface::LoreEvent| {
            if let lore::interface::LoreEvent::StorageOpened(data) = event
                && let Ok(mut slot) = sink.lock()
            {
                *slot = Some(data.handle_id);
            }
        })),
    )
    .await;
    if code != 0 {
        eprintln!("storage open failed: {code}");
        return std::process::ExitCode::FAILURE;
    }
    let handle = opened
        .lock()
        .ok()
        .and_then(|slot| *slot)
        .unwrap_or_default();

    // Repository commands, while that handle stays open.
    for round in 0..rounds {
        let status = lore::repository::status(globals(&repository), status_args(), None).await;
        if status != 0 {
            eprintln!("status failed in round {round}: {status}");
            return std::process::ExitCode::FAILURE;
        }
        let history = lore::revision::history(
            globals(&repository),
            lore::revision::LoreRevisionHistoryArgs {
                length: 16,
                ..Default::default()
            },
            None,
        )
        .await;
        if history != 0 {
            eprintln!("history failed in round {round}: {history}");
            return std::process::ExitCode::FAILURE;
        }
    }

    let closed = lore::storage::close::close(
        globals(&repository),
        lore::storage::close::LoreStorageCloseArgs {
            handle: lore::storage::handle::LoreStore { handle_id: handle },
        },
        None,
    )
    .await;
    lore::shutdown();
    if closed != 0 {
        eprintln!("storage close failed: {closed}");
        return std::process::ExitCode::FAILURE;
    }
    println!("ok");
    std::process::ExitCode::SUCCESS
}
