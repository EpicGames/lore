// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `branch_archive` against Lore Server: one record per location archived, the remote first, each
//! followed by its location complete event; an archived branch still resolving by its id; a retry
//! after a remote-only archive; a refusal in the local store after the remote archived; the
//! remote's protection and default-branch pre-checks, for an archived branch too; a delete the
//! remote refuses; a stopped restore the remote's delete settles; and the live-instance refusal and the
//! location flags that skip it.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use lore_base::error::DeleteCurrent;
use lore_base::error::DeleteDefault;
use lore_base::error::DeleteProtected;
use lore_base::error::NotAuthorized;
use lore_base::test_util::TempDir;
use lore_base::types::BranchId;
use lore_revision::branch;
use lore_revision::interface::LoreBranchLocation;

use super::support::*;

/// Creates a branch named `name` at both locations from the seeded default branch, through
/// `branch_create` with no flag, and returns its id.
async fn branch_at_both(f: &Fixture, name: &str) -> BranchId {
    let (status, events) =
        create_branch(f, f.store, graph_globals(), name, latest(f.default_branch)).await;
    assert_eq!(status, 0, "creates {name} at both locations");
    branch_records(&events)[0].branch
}

/// Whether a live branch holds `name` at the server.
async fn named_at_server(f: &Fixture, name: &str) -> bool {
    in_client(async {
        connection_for(&f.url, f.repository)
            .await
            .revision(f.repository)
            .await
            .expect("reaches the revision service")
            .branch_query(None, Some(name))
            .await
    })
    .await
    .is_ok()
}

/// The archived flag of each branch record among `events` with its location, in order.
fn archived_records(events: &[lore_revision::event::LoreEvent]) -> Vec<(LoreBranchLocation, u8)> {
    branch_records(events)
        .into_iter()
        .map(|record| (record.location, record.archived))
        .collect()
}

/// An archive with no flag archives at the remote, then in the local store, each record with
/// `archived` set and followed by its complete event. The server's name no longer names the
/// branch, its query by id shows it deleted, and its metadata stays.
#[tokio::test]
async fn archives_at_both_locations_remote_first() {
    let f = fixture().await;
    seed_main(&f).await;
    let feature = branch_at_both(&f, "feature").await;

    let (status, events) = archive_branch(&f, f.store, graph_globals(), feature).await;

    assert_eq!(status, 0);
    assert_eq!(
        event_order(&events),
        [
            (LoreBranchLocation::Remote, false),
            (LoreBranchLocation::Remote, true),
            (LoreBranchLocation::Local, false),
            (LoreBranchLocation::Local, true),
        ]
    );
    assert_eq!(
        archived_records(&events),
        [
            (LoreBranchLocation::Remote, 1),
            (LoreBranchLocation::Local, 1)
        ]
    );
    assert_eq!(
        location_outcomes(&events),
        [
            (LoreBranchLocation::Remote, 1, 0),
            (LoreBranchLocation::Local, 1, 0),
        ]
    );
    assert!(!named_at_server(&f, "feature").await);
    let deleted = in_client(async {
        connection_for(&f.url, f.repository)
            .await
            .revision(f.repository)
            .await
            .expect("reaches the revision service")
            .branch_query(Some(feature), None)
            .await
    })
    .await
    .expect("the server keeps the branch's record")
    .deleted;
    assert!(deleted);
    server_branch_metadata(&f, feature).await;
    f.close().await;
}

/// After an archive at both locations, the branch's latest still resolves by its id at both.
#[tokio::test]
async fn an_archived_branch_still_resolves_by_id() {
    let f = fixture().await;
    seed_main(&f).await;
    let feature = branch_at_both(&f, "feature").await;
    let (status, _) = archive_branch(&f, f.store, graph_globals(), feature).await;
    assert_eq!(status, 0);

    let (status, events) = resolve_revision(&f, f.store, graph_globals(), latest(feature)).await;

    assert_eq!(status, 0);
    assert_eq!(revision_records(&events).len(), 2);
    f.close().await;
}

/// An archive with `remote`, then one with no flag: the second reports the remote's archived
/// record, then archives in the local store, whose name no longer maps.
#[tokio::test]
async fn a_retry_after_a_remote_only_archive_completes_both_locations() {
    let f = fixture().await;
    seed_main(&f).await;
    let feature = branch_at_both(&f, "feature").await;
    let (status, _) = archive_branch(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Remote),
        feature,
    )
    .await;
    assert_eq!(status, 0);

    let (status, events) = archive_branch(&f, f.store, graph_globals(), feature).await;

    assert_eq!(status, 0);
    assert_eq!(
        archived_records(&events),
        [
            (LoreBranchLocation::Remote, 1),
            (LoreBranchLocation::Local, 1)
        ]
    );
    assert_eq!(
        location_outcomes(&events),
        [
            (LoreBranchLocation::Remote, 1, 0),
            (LoreBranchLocation::Local, 1, 0),
        ]
    );
    let (status, _) = resolve_branch(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Local),
        "feature",
    )
    .await;
    assert_ne!(status, 0, "the local name no longer maps");
    f.close().await;
}

/// A branch protected in the local store alone archives at the remote, then fails in the local
/// store: the remote's record, then the local store's complete event with `DeleteProtected`.
#[tokio::test]
async fn a_local_refusal_after_a_remote_archive_reports_the_local_location() {
    let f = fixture().await;
    seed_main(&f).await;
    let feature = branch_at_both(&f, "feature").await;
    in_client(branch::protect(local_writer(&f), feature))
        .await
        .expect("protects the branch in the local store");

    let (status, events) = archive_branch(&f, f.store, graph_globals(), feature).await;

    assert_eq!(status, DeleteProtected::FFI_CODE);
    assert_eq!(archived_records(&events), [(LoreBranchLocation::Remote, 1)]);
    assert_eq!(
        location_outcomes(&events),
        [
            (LoreBranchLocation::Remote, 1, 0),
            (LoreBranchLocation::Local, 0, DeleteProtected::FFI_CODE),
        ]
    );
    f.close().await;
}

/// A branch protected at the server fails with `DeleteProtected` before the remote's delete: no
/// location complete event, and the branch stays live at both locations.
#[tokio::test]
async fn a_protected_remote_branch_fails_delete_protected_before_the_rpc() {
    let f = fixture().await;
    seed_main(&f).await;
    let feature = branch_at_both(&f, "feature").await;
    on_server(&f, async |context| branch::protect(context, feature).await)
        .await
        .expect("protects the branch on the server");

    let (status, events) = archive_branch(&f, f.store, graph_globals(), feature).await;

    assert_eq!(status, DeleteProtected::FFI_CODE);
    assert!(location_completes(&events).is_empty());
    assert!(named_at_server(&f, "feature").await);
    let (status, _) = resolve_branch(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Local),
        "feature",
    )
    .await;
    assert_eq!(status, 0, "the local branch is untouched");
    f.close().await;
}

/// The repository's default branch fails with `DeleteDefault` before the remote's delete.
#[tokio::test]
async fn the_default_branch_fails_delete_default() {
    let f = fixture().await;
    seed_main(&f).await;

    let (status, events) = archive_branch(&f, f.store, graph_globals(), f.default_branch).await;

    assert_eq!(status, DeleteDefault::FFI_CODE);
    assert!(location_completes(&events).is_empty());
    assert!(named_at_server(&f, DEFAULT_BRANCH_NAME).await);
    f.close().await;
}

/// A branch the server holds with an empty stack, which isn't the default branch, fails with
/// `DeleteDefault` before the remote's delete, as the local archive refuses one.
#[tokio::test]
async fn a_remote_branch_with_an_empty_stack_fails_delete_default() {
    let f = fixture().await;
    let rootless = BranchId::from(uuid::Uuid::now_v7());
    on_server(&f, async |context| {
        branch::create(
            context,
            &lore_server::grpc::get_write_token(),
            rootless,
            "rootless",
            branch::default_category(),
            CREATOR,
            lore_revision::util::time::timestamp(),
            Vec::new(),
            false,
            false,
        )
        .await
    })
    .await
    .expect("creates the branch on the server");

    let (status, events) = archive_branch(&f, f.store, graph_globals(), rootless).await;

    assert_eq!(status, DeleteDefault::FFI_CODE);
    assert!(location_completes(&events).is_empty());
    f.close().await;
}

/// A live instance on a branch at both locations refuses the archive before either location
/// changes: its instance record alone, `DeleteCurrent`, and the server still names the branch.
/// Once the instance's directory is gone, the archive succeeds at both locations.
#[tokio::test]
async fn a_live_instance_fails_delete_current_before_either_location_changes() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let feature = branch_at_both(&f, "feature").await;
    let dir = TempDir::new("graph-archive-live-instance-");
    let instance = register_instance_at(&f, feature, seeded, &dir).await;

    let (status, events) = archive_branch(&f, f.store, graph_globals(), feature).await;

    assert_eq!(status, DeleteCurrent::FFI_CODE);
    let instances: Vec<_> = instance_records(&events)
        .into_iter()
        .map(|record| record.instance_id)
        .collect();
    assert_eq!(instances, [instance]);
    assert!(branch_records(&events).is_empty());
    assert!(location_completes(&events).is_empty());
    assert!(named_at_server(&f, "feature").await);

    drop(dir);
    let (status, events) = archive_branch(&f, f.store, graph_globals(), feature).await;

    assert_eq!(status, 0);
    assert_eq!(
        archived_records(&events),
        [
            (LoreBranchLocation::Remote, 1),
            (LoreBranchLocation::Local, 1)
        ]
    );
    f.close().await;
}

/// With `remote`, a live instance doesn't refuse the archive, since nothing local changes.
#[tokio::test]
async fn the_remote_flag_skips_the_instance_check() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let feature = branch_at_both(&f, "feature").await;
    let dir = TempDir::new("graph-archive-remote-flag-");
    register_instance_at(&f, feature, seeded, &dir).await;

    let (status, events) = archive_branch(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Remote),
        feature,
    )
    .await;

    assert_eq!(status, 0);
    assert!(instance_records(&events).is_empty());
    assert_eq!(archived_records(&events), [(LoreBranchLocation::Remote, 1)]);
    f.close().await;
}

/// A live instance registered on a branch the local store lacks doesn't refuse: the archive runs
/// at the remote, sends no instance record, and the local store completes with no records.
#[tokio::test]
async fn an_instance_on_a_branch_absent_locally_does_not_block_the_remote_archive() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let remote_only = create_remote_branch(&f, "remote-only", f.default_branch, CREATOR).await;
    let dir = TempDir::new("graph-archive-absent-locally-");
    register_instance_at(&f, remote_only, seeded, &dir).await;

    let (status, events) = archive_branch(&f, f.store, graph_globals(), remote_only).await;

    assert_eq!(status, 0);
    assert!(instance_records(&events).is_empty());
    assert_eq!(archived_records(&events), [(LoreBranchLocation::Remote, 1)]);
    assert_eq!(
        location_outcomes(&events),
        [
            (LoreBranchLocation::Remote, 1, 0),
            (LoreBranchLocation::Local, 0, 0),
        ]
    );
    f.close().await;
}

/// A delete the remote refuses for want of write access, after the read-only checks passed, is
/// read with the checks again, which attribute nothing: the remote's complete event carries
/// `NotAuthorized`, no local complete event follows, and the server still names the branch.
#[tokio::test]
async fn a_delete_the_remote_refuses_reports_its_refusal_at_the_remote() {
    let armed = Arc::new(AtomicBool::new(false));
    let f = fixture_with_authorizer(Arc::new(ReadOnly(armed.clone()))).await;
    seed_main(&f).await;
    let feature = branch_at_both(&f, "feature").await;
    armed.store(true, Ordering::SeqCst);

    let (status, events) = archive_branch(&f, f.store, graph_globals(), feature).await;

    assert_eq!(status, NotAuthorized::FFI_CODE);
    assert_eq!(
        location_outcomes(&events),
        [(LoreBranchLocation::Remote, 0, NotAuthorized::FFI_CODE)]
    );
    assert!(branch_records(&events).is_empty());
    assert!(named_at_server(&f, "feature").await);
    f.close().await;
}

/// A branch the server has archived, whose metadata records a stopped restore, is archived again
/// at the remote: the server's delete settles the restore, which leaves its name released.
#[tokio::test]
async fn a_remote_archive_settles_a_stopped_restore_the_server_holds() {
    let f = fixture().await;
    seed_main(&f).await;
    let feature = branch_at_both(&f, "feature").await;
    archive_on_server(&f, feature).await;
    on_server(&f, async |context| {
        let (_, written) = metadata_with_entry(&context, feature, branch::RESTORING, "bee").await;
        branch::mutable_store_metadata(context, feature, written).await
    })
    .await
    .expect("records the stopped restore on the server");

    let (status, events) = archive_branch(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Remote),
        feature,
    )
    .await;

    assert_eq!(status, 0);
    assert_eq!(archived_records(&events), [(LoreBranchLocation::Remote, 1)]);
    let settled = server_branch_metadata(&f, feature).await;
    assert!(
        settled.get_string(branch::RESTORING).is_err(),
        "the restore is settled"
    );
    assert_eq!(settled.get_string(branch::RELEASED).ok(), Some("bee"));
    f.close().await;
}

/// A branch archived at the server and protected there since fails with `DeleteProtected` at the
/// remote, as the archive core and the server's delete check protection before they find the
/// branch archived.
#[tokio::test]
async fn an_archived_branch_protected_at_the_remote_fails_delete_protected() {
    let f = fixture().await;
    seed_main(&f).await;
    let feature = branch_at_both(&f, "feature").await;
    archive_on_server(&f, feature).await;
    on_server(&f, async |context| branch::protect(context, feature).await)
        .await
        .expect("protects the archived branch on the server");

    let (status, events) = archive_branch(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Remote),
        feature,
    )
    .await;

    assert_eq!(status, DeleteProtected::FFI_CODE);
    assert!(location_completes(&events).is_empty());
    f.close().await;
}
