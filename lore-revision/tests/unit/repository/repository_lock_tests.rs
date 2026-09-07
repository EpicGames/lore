// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_revision::repository::*;
use lore_storage::ImmutableStore;
use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;

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

    let (created, left_over) =
        get_or_create_repository_lock(dot_path.clone(), claim(store.clone()).await)
            .await
            .expect("creates");
    assert!(
        left_over.is_empty(),
        "the creator's claims went into the holder, so nothing comes back"
    );

    let (joined, kept) =
        get_or_create_repository_lock(dot_path.clone(), claim(store.clone()).await)
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
    )
    .await
    .expect("creates");
    let (second, _) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        get_or_create_repository_lock(
            link.join(".lore"),
            lore_storage::local::store_lock::StoreHold::default(),
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
            .expect("creates");
    let again =
        create_immutable_store_at_path(link.clone(), ImmutableStoreCreateOptions::none(), false)
            .await
            .expect("reaches it");
    assert!(
        Arc::ptr_eq(&immutable, &again),
        "one directory, one immutable store"
    );

    let mutable = create_mutable_store_at_path(real, immutable.clone())
        .await
        .expect("creates");
    let again = create_mutable_store_at_path(link, immutable)
        .await
        .expect("reaches it");
    assert!(
        Arc::ptr_eq(&mutable, &again),
        "one directory, one mutable store"
    );
}
