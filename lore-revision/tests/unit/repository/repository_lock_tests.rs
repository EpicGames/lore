// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::repository::*;
use lore_storage::ImmutableStore;
use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;

use crate::fs::filesystem_provider::setup_test_execution;

/// **A caller that joins a cached repository lock keeps its own store claims.**
///
/// The holder in the cache owns whichever claims its *creator* supplied, on
/// whichever store objects that command was using. Handing back a cached holder
/// while dropping this caller's claims would leave it holding the repository flock
/// with nothing of its own underneath — the arrangement the containment exists to
/// rule out, arrived at through the cache rather than through a missing argument.
#[tokio::test]
async fn joining_a_cached_repository_lock_returns_the_callers_claims() {
    let dir = lore_base::test_util::TempDir::new("lore-repo-lock-cache-");
    let dot_path = dir.path().join(".lore");
    std::fs::create_dir_all(&dot_path).expect("dot path");

    let store = lore_storage::local::immutable_store::LocalImmutableStore::new(
        Some(dir.path().to_path_buf()),
        lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
    )
    .await
    .expect("store");

    let claim = |store: Arc<lore_storage::local::immutable_store::LocalImmutableStore>| async move {
        let guard = store
            .hold_for_command()
            .await
            .expect("claim")
            .expect("a disk-backed store claims something");
        lore_storage::local::store_lock::StoreHold::new(vec![guard])
    };

    let (created, left_over) = get_or_create_repository_lock(
        dot_path.clone(),
        claim(store.clone()).await,
        true,
        LockSharing::Shared,
    )
    .await
    .expect("creates");
    assert!(
        left_over.is_empty(),
        "the creator's claims went into the holder, so nothing comes back"
    );

    let (joined, kept) = get_or_create_repository_lock(
        dot_path.clone(),
        claim(store.clone()).await,
        true,
        LockSharing::Shared,
    )
    .await
    .expect("joins");
    assert!(
        Arc::ptr_eq(&created, &joined),
        "the second caller must join the cached holder, not make another"
    );
    assert_eq!(
        kept.len(),
        1,
        "and must get its own claims back rather than have them dropped"
    );
}

/// **Another spelling of a repository joins the lock held for it.** A second holder for
/// one directory takes a second flock on it, which excludes the first inside this very
/// process — so the second command would wait for the first to finish.
#[cfg(unix)]
#[tokio::test]
async fn another_spelling_of_a_repository_joins_its_lock() {
    let dir = lore_base::test_util::TempDir::new("lore-repo-lock-spelling-");
    let dot_path = dir.path().join("repo").join(".lore");
    std::fs::create_dir_all(&dot_path).expect("dot path");
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(dir.path().join("repo"), &link).expect("links");

    let (first, _) = get_or_create_repository_lock(
        dot_path,
        lore_storage::local::store_lock::StoreHold::default(),
        true,
        LockSharing::Shared,
    )
    .await
    .expect("creates");
    let (second, _) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        get_or_create_repository_lock(
            link.join(".lore"),
            lore_storage::local::store_lock::StoreHold::default(),
            true,
            LockSharing::Shared,
        ),
    )
    .await
    .expect("does not wait on the flock the first spelling took")
    .expect("joins");
    assert!(Arc::ptr_eq(&first, &second), "one directory, one holder");
}

/// **Another spelling of a store directory reaches the same stores.** Two objects on one
/// directory keep separate in-memory state over one flock, so neither learns of the
/// other's writes while that flock stays held.
#[cfg(unix)]
#[tokio::test]
async fn another_spelling_of_a_store_directory_reaches_the_same_stores() {
    let dir = lore_base::test_util::TempDir::new("lore-store-cache-spelling-");
    let real = dir.path().join("real");
    std::fs::create_dir_all(&real).expect("store root");
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).expect("links");

    let immutable =
        create_immutable_store_at_path(real.clone(), ImmutableStoreCreateOptions::none(), false)
            .await
            .expect("creates")
            .0;
    let again =
        create_immutable_store_at_path(link.clone(), ImmutableStoreCreateOptions::none(), false)
            .await
            .expect("reaches it")
            .0;
    assert!(
        Arc::ptr_eq(&immutable, &again),
        "one directory, one immutable store"
    );

    let mutable = create_mutable_store_at_path(real, immutable.clone())
        .await
        .expect("creates")
        .0;
    let again = create_mutable_store_at_path(link, immutable)
        .await
        .expect("reaches it")
        .0;
    assert!(
        Arc::ptr_eq(&mutable, &again),
        "one directory, one mutable store"
    );
}

/// Whether `future`, a caller of the lock of the Lore directory at `dot_path`, waits at the gate
/// once polled: it is pending, and counted by `waiting`, the gate's count of the callers of its
/// kind waiting there, which drops the caller once it is admitted.
async fn waits_at_gate(
    future: std::pin::Pin<&mut impl std::future::Future>,
    dot_path: &std::path::Path,
    waiting: fn(&std::path::Path) -> usize,
) -> bool {
    futures::poll!(future).is_pending() && waiting(dot_path) == 1
}

/// The lock of the Lore directory at `dot_path`, as a command that shares it takes it.
fn shared(
    dot_path: &std::path::Path,
) -> impl std::future::Future<
    Output = Result<
        (
            Arc<RepositoryLock>,
            lore_storage::local::store_lock::StoreHold,
        ),
        RepositoryError,
    >,
> {
    get_or_create_repository_lock(
        dot_path.to_path_buf(),
        lore_storage::local::store_lock::StoreHold::default(),
        true,
        LockSharing::Shared,
    )
}

/// The lock of the Lore directory at `dot_path`, as a command that holds it alone takes it.
fn exclusive(
    dot_path: &std::path::Path,
) -> impl std::future::Future<
    Output = Result<
        (
            Arc<RepositoryLock>,
            lore_storage::local::store_lock::StoreHold,
        ),
        RepositoryError,
    >,
> {
    get_or_create_repository_lock(
        dot_path.to_path_buf(),
        lore_storage::local::store_lock::StoreHold::default(),
        true,
        LockSharing::Exclusive,
    )
}

/// A Lore directory of its own for a lock test, under `root`.
fn lock_directory(root: &lore_base::test_util::TempDir) -> std::path::PathBuf {
    let dot_path = root.path().join(".lore");
    std::fs::create_dir_all(&dot_path).expect("dot path");
    dot_path
}

/// **An exclusive holder shares with no other command of its process.** It waits for the
/// shared holder to be released, and a command that comes while it is held waits for it, rather
/// than joining it as commands join a shared holder.
#[tokio::test]
async fn an_exclusive_repository_lock_waits_for_and_keeps_out_shared_holders() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let dir = lore_base::test_util::TempDir::new("lore-repo-lock-exclusive-");
            let dot_path = lock_directory(&dir);

            let (first, _) = shared(&dot_path).await.expect("creates");
            let mut alone = std::pin::pin!(exclusive(&dot_path));
            assert!(
                waits_at_gate(alone.as_mut(), &dot_path, exclusive_callers_waiting).await,
                "the exclusive holder must wait while a shared one is held"
            );

            drop(first);
            let (alone, _) = tokio::time::timeout(std::time::Duration::from_secs(5), alone)
                .await
                .expect("is admitted once the shared holder is released")
                .expect("creates");

            let mut later = std::pin::pin!(shared(&dot_path));
            assert!(
                waits_at_gate(later.as_mut(), &dot_path, shared_callers_waiting).await,
                "a shared holder must wait while the exclusive one is held"
            );

            drop(alone);
            tokio::time::timeout(std::time::Duration::from_secs(5), later)
                .await
                .expect("is admitted once the exclusive holder is released")
                .expect("creates");
        })
        .await;
}

/// Commands that keep a shared holder alive by overlapping do not keep a caller waiting to hold
/// the lock alone out for good: once it waits, the shared holder admits no more commands, and
/// the next one waits behind it.
#[tokio::test]
async fn a_waiting_exclusive_caller_is_not_starved_by_overlapping_shared_holders() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let dir = lore_base::test_util::TempDir::new("lore-repo-lock-starve-");
            let dot_path = lock_directory(&dir);

            let (first, _) = shared(&dot_path).await.expect("creates");
            let mut alone = std::pin::pin!(exclusive(&dot_path));
            assert!(
                waits_at_gate(alone.as_mut(), &dot_path, exclusive_callers_waiting).await,
                "the exclusive caller waits"
            );

            // A command that would overlap the first and keep the holder alive.
            let mut second = std::pin::pin!(shared(&dot_path));
            assert!(
                waits_at_gate(second.as_mut(), &dot_path, shared_callers_waiting).await,
                "a shared command that comes while the exclusive caller waits joins no holder"
            );

            drop(first);
            let (alone, _) = tokio::time::timeout(std::time::Duration::from_secs(5), alone)
                .await
                .expect("is admitted once the holder it waited for is released")
                .expect("creates");
            assert!(
                waits_at_gate(second.as_mut(), &dot_path, shared_callers_waiting).await,
                "the shared command waits for the exclusive holder"
            );

            drop(alone);
            tokio::time::timeout(std::time::Duration::from_secs(5), second)
                .await
                .expect("is admitted once the exclusive holder is released")
                .expect("creates");
        })
        .await;
}

/// A caller that gives up waiting to hold the lock alone, as a command dropped while it waits
/// does, is no longer counted as waiting: shared commands that came while it waited join the
/// holder it waited for, and later ones join it at once.
#[tokio::test]
async fn an_exclusive_caller_dropped_while_it_waits_lets_shared_callers_join() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let dir = lore_base::test_util::TempDir::new("lore-repo-lock-cancel-");
            let dot_path = lock_directory(&dir);

            let (first, _) = shared(&dot_path).await.expect("creates");
            let mut alone = Box::pin(exclusive(&dot_path));
            assert!(
                waits_at_gate(alone.as_mut(), &dot_path, exclusive_callers_waiting).await,
                "the exclusive caller waits"
            );
            assert_eq!(exclusive_callers_waiting(&dot_path), 1);
            let mut queued = std::pin::pin!(shared(&dot_path));
            assert!(
                waits_at_gate(queued.as_mut(), &dot_path, shared_callers_waiting).await,
                "a shared caller that comes meanwhile queues behind the exclusive one"
            );

            drop(alone);

            assert_eq!(exclusive_callers_waiting(&dot_path), 0);
            let (joined, _) = tokio::time::timeout(std::time::Duration::from_secs(5), queued)
                .await
                .expect("the queued caller is admitted once the exclusive one gives up")
                .expect("joins");
            assert!(
                Arc::ptr_eq(&first, &joined),
                "the queued caller joins the holder alive"
            );
            let (later, _) = futures::FutureExt::now_or_never(shared(&dot_path))
                .expect("a later caller does not wait")
                .expect("joins");
            assert!(
                Arc::ptr_eq(&first, &later),
                "a later caller joins it at once"
            );
        })
        .await;
}
