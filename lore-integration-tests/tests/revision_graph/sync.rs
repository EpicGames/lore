// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `branch_sync` against an in-process Lore Server: fast-forwards to the remote latest and the
//! branch recorded in sync, divergent and forced syncs, expectations, number and hash targets,
//! bringing a branch in from the remote, and the synced event a failure reports.

use std::sync::atomic::Ordering;
use std::time::Duration;

use lore::revision_graph::LoreRevisionIdentifier;
use lore::revision_graph::branch_sync::LoreRevisionGraphBranchSyncOptions;
use lore_base::error::BranchAdvanced;
use lore_base::error::BranchAlreadyExists;
use lore_base::error::BranchNotFound;
use lore_base::error::Divergent;
use lore_base::error::InvalidArguments;
use lore_base::error::RevisionNotFound;
use lore_base::types::BranchId;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::metadata::Metadata;
use lore_revision::test_util::write_state_revision;

use super::support::*;

/// The options of a sync with neither `force` nor an expectation.
fn plain() -> LoreRevisionGraphBranchSyncOptions {
    LoreRevisionGraphBranchSyncOptions::default()
}

/// The options of a forced sync.
fn forced() -> LoreRevisionGraphBranchSyncOptions {
    LoreRevisionGraphBranchSyncOptions {
        force: 1,
        ..Default::default()
    }
}

/// The all-zero target: the latest of the branch the call syncs.
fn no_target() -> LoreRevisionIdentifier {
    LoreRevisionIdentifier::default()
}

/// The status and events of a no-flag sync of `branch` on `f`'s handle to `target` with `options`.
async fn sync(
    f: &Fixture,
    branch: BranchId,
    target: LoreRevisionIdentifier,
    options: LoreRevisionGraphBranchSyncOptions,
) -> (i32, Vec<lore_revision::event::LoreEvent>) {
    sync_branch(f, f.store, graph_globals(), branch, target, options).await
}

/// The id the local store maps `name` to, zero where it maps none.
async fn mapped(f: &Fixture, name: &str) -> Context {
    match in_client(branch::load_name_to_id_local(local_writer(f), name)).await {
        Ok(id) => id,
        Err(err) if err.is_branch_not_found() => Context::default(),
        Err(err) => panic!("loading the mapping of {name} failed: {err}"),
    }
}

/// Writes in the local store of `f`'s handle alone a revision on `branch` from `parent`, numbered
/// `number`, whose metadata names the branch, and returns its hash.
async fn local_only_revision(f: &Fixture, branch: BranchId, parent: Hash, number: u64) -> Hash {
    let writer = local_writer(f);
    in_client(async {
        let mut metadata = Metadata::new();
        metadata.set_branch(branch).expect("sets the branch");
        let metadata = metadata
            .serialize(writer.clone())
            .await
            .expect("writes the metadata");
        write_state_revision(&writer, parent, number, metadata).await
    })
    .await
}

/// A sync whose local latest lags the remote latest another handle published fast-forwards to it,
/// and records the branch in sync with the remote: the latest status cleared and the last
/// synchronized revision the target.
#[tokio::test]
async fn a_lagging_local_latest_fast_forwards_to_the_remote_latest_and_records_in_sync() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let published = publish_from_another_handle(&f, f.default_branch, seeded).await;

    let (status, events) = sync(&f, f.default_branch, no_target(), plain()).await;

    assert_eq!(status, 0);
    assert_eq!(synced(&events), [(seeded, published)]);
    assert_eq!(server_latest(&f, f.default_branch).await, published);
    assert_eq!(
        local_value(&f, branch::LATEST, f.default_branch).await,
        Some(published)
    );
    assert_eq!(
        local_value(&f, branch::LATEST_STATUS, f.default_branch).await,
        None
    );
    assert_eq!(
        local_value(&f, branch::LAST_SYNC, f.default_branch).await,
        Some(published)
    );
    f.close().await;
}

/// A branch without a local latest fast-forwards to the remote latest.
#[tokio::test]
async fn a_zero_local_latest_fast_forwards() {
    let f = fixture().await;
    let published = publish_from_another_handle(&f, f.default_branch, Hash::default()).await;

    let (status, events) = sync(&f, f.default_branch, no_target(), plain()).await;

    assert_eq!(status, 0);
    assert_eq!(synced(&events), [(Hash::default(), published)]);
    assert_eq!(
        local_value(&f, branch::LATEST, f.default_branch).await,
        Some(published)
    );
    f.close().await;
}

/// A local latest that holds a revision the remote latest lacks fails with `Divergent` and stays,
/// and `force` moves it to the remote latest.
#[tokio::test]
async fn local_revisions_the_remote_lacks_fail_divergent_unless_force() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let committed = commit_on(f.store, f.repository, f.default_branch, seeded).await;

    let (status, _) = sync(&f, f.default_branch, no_target(), plain()).await;
    assert_eq!(status, Divergent::FFI_CODE);
    assert_eq!(
        local_value(&f, branch::LATEST, f.default_branch).await,
        Some(committed)
    );

    let (status, events) = sync(&f, f.default_branch, no_target(), forced()).await;
    assert_eq!(status, 0);
    assert_eq!(synced(&events), [(committed, seeded)]);
    assert_eq!(
        local_value(&f, branch::LATEST, f.default_branch).await,
        Some(seeded)
    );
    f.close().await;
}

/// An expectation other than the local latest fails with `BranchAdvanced` and leaves the local
/// latest.
#[tokio::test]
async fn expected_local_latest_blocks_a_moved_latest() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    publish_from_another_handle(&f, f.default_branch, seeded).await;
    let options = LoreRevisionGraphBranchSyncOptions {
        expected_local_latest: Hash::from([9; 32]),
        ..Default::default()
    };

    let (status, events) = sync(&f, f.default_branch, no_target(), options).await;

    assert_eq!(status, BranchAdvanced::FFI_CODE);
    assert_eq!(synced(&events), [(seeded, seeded)]);
    assert_eq!(
        local_value(&f, branch::LATEST, f.default_branch).await,
        Some(seeded)
    );
    f.close().await;
}

/// A hash target that is the remote latest records the branch in sync with the remote.
#[tokio::test]
async fn a_target_equal_to_the_remote_latest_records_in_sync() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let published = publish_from_another_handle(&f, f.default_branch, seeded).await;

    let (status, _) = sync(&f, f.default_branch, by_hash(published), plain()).await;

    assert_eq!(status, 0);
    assert_eq!(
        local_value(&f, branch::LATEST, f.default_branch).await,
        Some(published)
    );
    assert_eq!(
        local_value(&f, branch::LATEST_STATUS, f.default_branch).await,
        None
    );
    assert_eq!(
        local_value(&f, branch::LAST_SYNC, f.default_branch).await,
        Some(published)
    );
    f.close().await;
}

/// A revision number names the revision the remote's revision list numbers so.
#[tokio::test]
async fn a_number_target_resolves_on_the_remote() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let published = publish_from_another_handle(&f, f.default_branch, seeded).await;

    let (status, events) = sync(&f, f.default_branch, numbered(f.default_branch, 2), plain()).await;

    assert_eq!(status, 0);
    assert_eq!(synced(&events), [(seeded, published)]);
    f.close().await;
}

/// A number target below the remote latest names a revision on the remote's history line for the
/// branch, so the sync records the local latest convergent, and stores no last synchronized
/// revision, which only a target that is the remote latest records.
#[tokio::test]
async fn a_number_target_below_the_remote_latest_records_the_latest_convergent() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let first = publish_from_another_handle(&f, f.default_branch, seeded).await;
    publish_from_another_handle(&f, f.default_branch, first).await;
    let last_sync = local_value(&f, branch::LAST_SYNC, f.default_branch).await;

    let (status, events) = sync(&f, f.default_branch, numbered(f.default_branch, 2), plain()).await;

    assert_eq!(status, 0);
    assert_eq!(synced(&events), [(seeded, first)]);
    assert_eq!(
        local_value(&f, branch::LATEST_STATUS, f.default_branch).await,
        None
    );
    assert_eq!(
        local_value(&f, branch::LAST_SYNC, f.default_branch).await,
        last_sync
    );
    f.close().await;
}

/// A hash target below the remote latest whose number on the branch the remote's revision list
/// answers with that hash is on the remote's history line, so the sync records the local latest
/// convergent.
#[tokio::test]
async fn a_hash_target_the_remote_numbers_so_records_the_latest_convergent() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let first = publish_from_another_handle(&f, f.default_branch, seeded).await;
    publish_from_another_handle(&f, f.default_branch, first).await;
    let last_sync = local_value(&f, branch::LAST_SYNC, f.default_branch).await;

    let (status, events) = sync(&f, f.default_branch, by_hash(first), plain()).await;

    assert_eq!(status, 0);
    assert_eq!(synced(&events), [(seeded, first)]);
    assert_eq!(
        local_value(&f, branch::LATEST_STATUS, f.default_branch).await,
        None
    );
    assert_eq!(
        local_value(&f, branch::LAST_SYNC, f.default_branch).await,
        last_sync
    );
    f.close().await;
}

/// A hash target the remote holds off its history line for the branch, a revision on the branch
/// whose number the remote's revision list answers with another revision, leaves the local latest
/// divergent.
#[tokio::test]
async fn a_hash_target_off_the_remotes_line_records_the_latest_divergent() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    publish_from_another_handle(&f, f.default_branch, seeded).await;
    let off_line = commit_from_another_handle(&f, f.default_branch, seeded).await;

    let (status, events) = sync(&f, f.default_branch, by_hash(off_line), plain()).await;

    assert_eq!(status, 0);
    assert_eq!(synced(&events), [(seeded, off_line)]);
    assert_eq!(
        local_value(&f, branch::LATEST_STATUS, f.default_branch).await,
        Some(off_line)
    );
    f.close().await;
}

/// A forced sync to the branch point a branch starts from, where the remote latest of the branch is
/// another revision, records the local latest convergent: the remote's history line for the branch
/// reaches its branch point.
#[tokio::test]
async fn a_forced_sync_to_the_branch_point_records_the_latest_convergent() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let feature = create_branch_at_both_locations(
        &f,
        "feature",
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await;
    publish_from_another_handle(&f, feature, seeded).await;
    let committed = commit_on(f.store, f.repository, feature, seeded).await;
    let last_sync = local_value(&f, branch::LAST_SYNC, feature).await;

    let (status, events) = sync(&f, feature, by_hash(seeded), forced()).await;

    assert_eq!(status, 0);
    assert_eq!(synced(&events), [(committed, seeded)]);
    assert_eq!(local_value(&f, branch::LATEST_STATUS, feature).await, None);
    assert_eq!(local_value(&f, branch::LAST_SYNC, feature).await, last_sync);
    f.close().await;
}

/// A forced sync to a revision number looked up in the local store records the local latest
/// divergent. The same sync at the remote, whose revision list numbers the local latest so, clears
/// the status, and stores no last synchronized revision and no local latest.
#[tokio::test]
async fn a_number_target_at_the_local_latest_clears_a_divergent_status_at_the_remote() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let published = publish_from_another_handle(&f, f.default_branch, seeded).await;
    let (status, _) = sync(&f, f.default_branch, no_target(), plain()).await;
    assert_eq!(status, 0);
    let (status, _) = sync_branch(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Local),
        f.default_branch,
        numbered(f.default_branch, 1),
        forced(),
    )
    .await;
    assert_eq!(status, 0);
    assert_eq!(
        local_value(&f, branch::LATEST_STATUS, f.default_branch).await,
        Some(seeded)
    );

    let (status, events) = sync(&f, f.default_branch, numbered(f.default_branch, 1), plain()).await;

    assert_eq!(status, 0);
    assert_eq!(synced(&events), [(seeded, seeded)]);
    assert_eq!(
        local_value(&f, branch::LATEST_STATUS, f.default_branch).await,
        None
    );
    assert_eq!(
        local_value(&f, branch::LAST_SYNC, f.default_branch).await,
        Some(published)
    );
    f.close().await;
}

/// A hash target at a divergent local latest that the remote's revision list numbers so clears the
/// status, and stores no last synchronized revision.
#[tokio::test]
async fn a_hash_target_at_the_local_latest_the_remote_numbers_so_clears_a_divergent_status() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    publish_from_another_handle(&f, f.default_branch, seeded).await;
    record_divergent(&f, f.default_branch, seeded).await;
    let last_sync = local_value(&f, branch::LAST_SYNC, f.default_branch).await;

    let (status, events) = sync(&f, f.default_branch, by_hash(seeded), plain()).await;

    assert_eq!(status, 0);
    assert_eq!(synced(&events), [(seeded, seeded)]);
    assert_eq!(
        local_value(&f, branch::LATEST_STATUS, f.default_branch).await,
        None
    );
    assert_eq!(
        local_value(&f, branch::LAST_SYNC, f.default_branch).await,
        last_sync
    );
    f.close().await;
}

/// A target at a divergent local latest that is the branch point the remote's record of the branch
/// starts from clears the status.
#[tokio::test]
async fn a_branch_point_target_at_the_local_latest_clears_a_divergent_status() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let feature = create_branch_at_both_locations(
        &f,
        "feature",
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await;
    publish_from_another_handle(&f, feature, seeded).await;
    record_divergent(&f, feature, seeded).await;

    let (status, events) = sync(&f, feature, by_hash(seeded), plain()).await;

    assert_eq!(status, 0);
    assert_eq!(synced(&events), [(seeded, seeded)]);
    assert_eq!(local_value(&f, branch::LATEST_STATUS, feature).await, None);
    f.close().await;
}

/// A target at a divergent local latest that the remote doesn't confirm on its history line for
/// the branch, a hash its revision list numbers with another revision, or a number looked up in the
/// local store, leaves the status divergent.
#[tokio::test]
async fn a_target_at_the_local_latest_off_the_remotes_line_keeps_a_divergent_status() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    publish_from_another_handle(&f, f.default_branch, seeded).await;
    let off_line = commit_from_another_handle(&f, f.default_branch, seeded).await;
    let (status, _) = sync(&f, f.default_branch, by_hash(off_line), plain()).await;
    assert_eq!(status, 0);

    let (by_hash_status, _) = sync(&f, f.default_branch, by_hash(off_line), plain()).await;
    let (local_status, _) = sync_branch(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Local),
        f.default_branch,
        numbered(f.default_branch, 2),
        plain(),
    )
    .await;

    assert_eq!((by_hash_status, local_status), (0, 0));
    assert_eq!(
        local_value(&f, branch::LATEST_STATUS, f.default_branch).await,
        Some(off_line)
    );
    f.close().await;
}

/// A branch the remote alone holds is brought in: its metadata key, its name's mapping and its
/// latest are set in the local store, and a local branch resolve finds it.
#[tokio::test]
async fn bring_in_copies_metadata_and_claims_the_name() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let feature = create_remote_branch(&f, "feature", f.default_branch, CREATOR).await;

    let (status, events) = sync(&f, feature, no_target(), plain()).await;

    assert_eq!(status, 0);
    assert_eq!(synced(&events), [(Hash::default(), seeded)]);
    assert_eq!(
        local_value(&f, branch::METADATA, feature).await,
        Some(server_record(&f, feature).await.metadata)
    );
    assert_eq!(mapped(&f, "feature").await, feature);
    assert_eq!(local_value(&f, branch::LATEST, feature).await, Some(seeded));
    let (status, _) = resolve_branch(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Local),
        "feature",
    )
    .await;
    assert_eq!(status, 0);
    f.close().await;
}

/// A branch archived in the local store alone and live on the remote is brought in again: its
/// name maps to it, its metadata key holds the remote's hash, and a local branch resolve finds it.
#[tokio::test]
async fn bring_in_restores_a_missing_name_mapping_when_metadata_is_present() {
    let f = fixture().await;
    seed_main(&f).await;
    let feature = create_branch_at_both_locations(
        &f,
        "feature",
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await;
    archive_locally(&f, feature).await;

    let (status, _) = sync(&f, feature, no_target(), plain()).await;

    assert_eq!(status, 0);
    assert_eq!(mapped(&f, "feature").await, feature);
    assert_eq!(
        local_value(&f, branch::METADATA, feature).await,
        Some(server_record(&f, feature).await.metadata)
    );
    let (status, _) = resolve_branch(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Local),
        "feature",
    )
    .await;
    assert_eq!(status, 0);
    f.close().await;
}

/// Bringing in a remote branch whose name a different live local branch holds fails with
/// `BranchAlreadyExists`, and leaves no metadata key of the remote branch in the local store.
#[tokio::test]
async fn bring_in_fails_branch_already_exists_when_a_live_local_branch_holds_the_name() {
    let f = fixture().await;
    seed_main(&f).await;
    let local = create_local_branch(&f, "x", f.default_branch, CREATOR).await;
    let remote = create_remote_branch(&f, "x", f.default_branch, CREATOR).await;

    let (status, events) = sync(&f, remote, no_target(), plain()).await;

    assert_eq!(status, BranchAlreadyExists::FFI_CODE);
    assert!(synced(&events).is_empty());
    assert_eq!(local_value(&f, branch::METADATA, remote).await, None);
    assert_eq!(mapped(&f, "x").await, local);
    f.close().await;
}

/// Bringing in a branch whose local metadata records a stopped restore, under a name that maps to
/// it, settles the restore: that name maps to nothing, the metadata key holds the remote's hash,
/// and the branch's own name maps to it.
#[tokio::test]
async fn bring_in_settles_a_stopped_local_restore() {
    let f = fixture().await;
    seed_main(&f).await;
    let feature = create_branch_at_both_locations(
        &f,
        "feature",
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await;
    archive_locally(&f, feature).await;
    let writer = local_writer(&f);
    in_client(async {
        let (archived, mut metadata) = branch::local_metadata(writer.clone(), feature)
            .await
            .expect("reads the local metadata")
            .expect("the local store holds the metadata");
        metadata
            .set_string(branch::RESTORING, "bee")
            .expect("records the stopped restore");
        let stopped = metadata
            .serialize(writer.clone())
            .await
            .expect("writes the metadata");
        branch::mutable_try_store(writer.clone(), branch::METADATA, feature, archived, stopped)
            .await
            .expect("swaps in the stopped restore");
        branch::claim_name(writer.clone(), "bee", Context::default(), feature)
            .await
            .expect("the stopped restore claimed its name");
    })
    .await;

    let (status, _) = sync(&f, feature, no_target(), plain()).await;

    assert_eq!(status, 0);
    assert_eq!(mapped(&f, "bee").await, Context::default());
    assert_eq!(mapped(&f, "feature").await, feature);
    assert_eq!(
        local_value(&f, branch::METADATA, feature).await,
        Some(server_record(&f, feature).await.metadata)
    );
    f.close().await;
}

/// A server branch whose id is the one its name derives, of which the local store holds a latest
/// and neither metadata nor a mapping, is brought in and its name maps to it.
#[tokio::test]
async fn bring_in_claims_the_name_of_a_branch_whose_id_is_its_derived_id() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let derived = branch::fallback_id("x");
    create_remote_branch_as(
        &f,
        derived,
        "x",
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await;
    in_client(branch::mutable_try_store(
        local_writer(&f),
        branch::LATEST,
        derived,
        Hash::default(),
        seeded,
    ))
    .await
    .expect("stores the local latest");

    let (status, _) = sync(&f, derived, no_target(), plain()).await;

    assert_eq!(status, 0);
    assert_eq!(mapped(&f, "x").await, derived);
    f.close().await;
}

/// A store that committed on a branch whose metadata it lacks holds a local latest the sync reads
/// after it brings the branch in: a remote latest that doesn't descend from it fails with
/// `Divergent`, and the synced event reports it.
#[tokio::test]
async fn a_sync_on_a_store_with_a_latest_but_no_metadata_reads_that_latest() {
    let f = fixture().await;
    let published = publish_from_another_handle(&f, f.default_branch, Hash::default()).await;
    let committed = commit_on(f.store, f.repository, f.default_branch, Hash::default()).await;

    let (status, events) = sync(&f, f.default_branch, by_hash(published), plain()).await;

    assert_eq!(status, Divergent::FFI_CODE);
    assert_eq!(synced(&events), [(committed, committed)]);
    f.close().await;
}

/// A branch archived in the local store, whose name a new local branch holds, fails with
/// `BranchAlreadyExists` when a sync would bring it in.
#[tokio::test]
async fn a_sync_whose_name_another_live_local_branch_holds_fails_branch_already_exists() {
    let f = fixture().await;
    seed_main(&f).await;
    let archived = create_branch_at_both_locations(
        &f,
        "b",
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await;
    archive_locally(&f, archived).await;
    let holder = create_local_branch(&f, "b", f.default_branch, CREATOR).await;

    let (status, _) = sync(&f, archived, no_target(), plain()).await;

    assert_eq!(status, BranchAlreadyExists::FFI_CODE);
    assert_eq!(mapped(&f, "b").await, holder);
    f.close().await;
}

/// A sync of a branch archived at both locations claims no name for it, so a local branch resolve
/// of its name finds no branch.
#[tokio::test]
async fn syncing_a_branch_archived_at_both_locations_leaves_its_name_unclaimed() {
    let f = fixture().await;
    seed_main(&f).await;
    let (status, events) = create_branch(
        &f,
        f.store,
        graph_globals(),
        "feature",
        latest(f.default_branch),
    )
    .await;
    assert_eq!(status, 0);
    let feature = branch_records(&events)[0].branch;
    let (status, _) = archive_branch(&f, f.store, graph_globals(), feature).await;
    assert_eq!(status, 0);

    let (status, _) = sync(&f, feature, no_target(), plain()).await;

    assert_eq!(status, 0);
    assert_eq!(mapped(&f, "feature").await, Context::default());
    let (status, _) = resolve_branch(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Local),
        "feature",
    )
    .await;
    assert_eq!(status, BranchNotFound::FFI_CODE);
    f.close().await;
}

/// A hash target the local store holds and the remote doesn't fails with `RevisionNotFound`, since
/// the call looks the target up at the remote.
#[tokio::test]
async fn a_hash_target_the_remote_lacks_fails_revision_not_found() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let local = local_only_revision(&f, f.default_branch, seeded, 2).await;

    let (status, _) = sync(&f, f.default_branch, by_hash(local), plain()).await;

    assert_eq!(status, RevisionNotFound::FFI_CODE);
    f.close().await;
}

/// A sync with no target of a branch whose local latest is the remote latest stores that latest as
/// the last synchronized revision.
#[tokio::test]
async fn a_sync_to_an_equal_remote_latest_stores_last_sync_when_convergent() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let (status, events) = create_branch(
        &f,
        f.store,
        graph_globals(),
        "feature",
        latest(f.default_branch),
    )
    .await;
    assert_eq!(status, 0);
    let feature = branch_records(&events)[0].branch;

    let (status, events) = sync(&f, feature, no_target(), plain()).await;

    assert_eq!(status, 0);
    assert_eq!(synced(&events), [(seeded, seeded)]);
    assert_eq!(
        local_value(&f, branch::LAST_SYNC, feature).await,
        Some(seeded)
    );
    f.close().await;
}

/// A sync with `local` reads nothing from the remote.
#[tokio::test]
async fn a_local_sync_reads_nothing_from_the_remote() {
    let f = fixture().await;
    seed_main(&f).await;
    let feature = create_branch_at_both_locations(
        &f,
        "feature",
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await;
    let gets = f.server_gets.load(Ordering::SeqCst);

    let (status, events) = sync_branch(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Local),
        feature,
        no_target(),
        plain(),
    )
    .await;

    assert_eq!(status, 0);
    assert_eq!(synced(&events).len(), 1);
    assert_eq!(f.server_gets.load(Ordering::SeqCst), gets);
    f.close().await;
}

/// A sync with no target of a branch whose local latest is the remote latest and whose latest
/// status is divergent clears the status and stores the last synchronized revision, and writes no
/// local latest.
#[tokio::test]
async fn a_sync_to_an_equal_remote_latest_clears_a_divergent_status() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    record_divergent(&f, f.default_branch, seeded).await;

    let (status, events) = sync(&f, f.default_branch, no_target(), plain()).await;

    assert_eq!(status, 0);
    assert_eq!(synced(&events), [(seeded, seeded)]);
    assert_eq!(
        local_value(&f, branch::LATEST_STATUS, f.default_branch).await,
        None
    );
    assert_eq!(
        local_value(&f, branch::LAST_SYNC, f.default_branch).await,
        Some(seeded)
    );
    f.close().await;
}

/// Without a remote, a walk whose lines both stop at revisions the local store lacks fails with
/// `RevisionNotFound` rather than looping.
#[tokio::test]
async fn a_local_walk_that_runs_out_of_revisions_fails_revision_not_found_without_looping() {
    let f = fixture_without_remote().await;
    let feature = BranchId::from(uuid::Uuid::now_v7());
    let previous = local_only_revision(&f, feature, Hash::from([0xa1; 32]), 5).await;
    let target = local_only_revision(&f, feature, Hash::from([0xb2; 32]), 9).await;
    in_client(branch::create_record(
        local_writer(&f),
        feature,
        "feature",
        branch::default_category(),
        CREATOR,
        lore_revision::util::time::timestamp(),
        Vec::new(),
        previous,
        BranchLatestStatus::Divergent,
        Context::default(),
    ))
    .await
    .expect("creates the branch");

    let (status, _) = tokio::time::timeout(
        Duration::from_secs(5),
        sync(&f, feature, by_hash(target), plain()),
    )
    .await
    .expect("the sync ends within 5 seconds");

    assert_eq!(status, RevisionNotFound::FFI_CODE);
    f.close().await;
}

/// A hash target at the remote on another branch, and not the synced branch's branch point, fails
/// with `InvalidArguments`.
#[tokio::test]
async fn a_remote_target_outside_the_branch_fails_invalid_arguments() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let feature = create_branch_at_both_locations(
        &f,
        "feature",
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await;
    let other = publish_from_another_handle(&f, f.default_branch, seeded).await;

    let (status, _) = sync(&f, feature, by_hash(other), plain()).await;

    assert_eq!(status, InvalidArguments::FFI_CODE);
    f.close().await;
}

/// A branch the local store holds and the remote doesn't fails with `BranchNotFound`, after one
/// synced event that reports its local latest.
#[tokio::test]
async fn a_branch_absent_at_the_remote_fails_branch_not_found_after_reporting_the_local_latest() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let local = create_local_branch(&f, "local", f.default_branch, CREATOR).await;

    let (status, events) = sync(&f, local, no_target(), plain()).await;

    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert_eq!(synced(&events), [(seeded, seeded)]);
    f.close().await;
}

/// A divergent sync reports the unmoved local latest as both.
#[tokio::test]
async fn a_divergent_sync_reports_the_observed_latest() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let committed = commit_on(f.store, f.repository, f.default_branch, seeded).await;

    let (status, events) = sync(&f, f.default_branch, no_target(), plain()).await;

    assert_eq!(status, Divergent::FFI_CODE);
    assert_eq!(synced(&events), [(committed, committed)]);
    f.close().await;
}
