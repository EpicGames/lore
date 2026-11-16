// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `branch_metadata_get` and `branch_metadata_set` against Lore Server: the entries each location
//! reports, the remote first; a set that edits each location's own metadata; a set at the remote
//! alone; an address the remote lacks; metadata over one fragment; a `protect` that refuses an
//! archive at the remote; a branch archived at both locations; and at the remote, a swap another
//! writer overtakes and a metadata key that goes before the swap.

use std::sync::Arc;

use lore_base::error::AddressNotFound;
use lore_base::error::BranchNotFound;
use lore_base::types::BranchId;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::event::LoreEvent;
use lore_revision::event::LoreMetadataEventData;
use lore_revision::interface::LoreBinary;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreMetadata;
use lore_revision::metadata::Metadata;
use lore_revision::repository::RepositoryContext;
use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;

use super::support::*;
use crate::common::net_common::put_content;

/// A record as a test compares it: its location, key and value.
type Reported = (LoreBranchLocation, String, LoreMetadata);

/// The location, key and value of each branch metadata record among `events`, in order.
fn reported(events: &[LoreEvent]) -> Vec<Reported> {
    branch_metadata_records(events)
        .into_iter()
        .map(|record| {
            (
                record.location,
                record.key.as_str().to_owned(),
                record.value.clone(),
            )
        })
        .collect()
}

/// Each entry of `metadata` as a record at `location` reports it, in stored order.
fn entries_at(location: LoreBranchLocation, metadata: &Metadata) -> Vec<Reported> {
    let mut entries = Vec::new();
    metadata.walk(|key, value, value_type| {
        let key = std::str::from_utf8(key).expect("a UTF-8 key");
        let entry =
            LoreMetadataEventData::new(key, value, value_type).expect("a value of its type");
        entries.push((location, key.to_owned(), entry.value));
    });
    entries
}

/// The order [`event_order`] gives `records` records at `location` and its location complete
/// event.
fn records_then_complete(
    location: LoreBranchLocation,
    records: usize,
) -> Vec<(LoreBranchLocation, bool)> {
    let mut order = vec![(location, false); records];
    order.push((location, true));
    order
}

/// The string value `metadata` holds under each of `keys`, `None` where it holds none.
fn strings<'a>(metadata: &'a Metadata, keys: &[&str]) -> Vec<Option<&'a str>> {
    keys.iter()
        .map(|key| metadata.get_string(key).ok())
        .collect()
}

/// Sets the String `value` under `key` in the metadata of `branch` that the stores of `context`
/// hold, with a swap of its metadata key from the value read. Panics unless the stores hold
/// metadata for the branch and the swap wins.
async fn store_entry(context: Arc<RepositoryContext>, branch: BranchId, key: &str, value: &str) {
    let (expected, updated) = metadata_with_entry(&context, branch, key, value).await;
    let held = branch::mutable_try_store(context, branch::METADATA, branch, expected, updated)
        .await
        .expect("swaps the metadata key");
    assert_eq!(held, expected, "the swap of the metadata key wins");
}

/// A branch both locations hold, created from the default branch once [`seed_main`] committed
/// and published a revision on it.
async fn seeded_branch(f: &Fixture, name: &str) -> BranchId {
    seed_main(f).await;
    create_branch_at_both_locations(
        f,
        name,
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await
}

/// Each location reports its own metadata, one record per entry in stored order, the remote's
/// first, so an entry written at one location alone is reported there alone. Each location's
/// complete event follows its records and counts them.
#[tokio::test]
async fn get_reports_each_locations_entries_remote_first() {
    let f = fixture().await;
    let feature = seeded_branch(&f, "feature").await;
    on_server(&f, async |context| {
        store_entry(context, feature, "owner", "server").await;
    })
    .await;
    in_client(store_entry(local_writer(&f), feature, "reviewer", "local")).await;

    let (status, events) = get_branch_metadata(&f, f.store, graph_globals(), feature, "").await;

    let server = server_branch_metadata(&f, feature).await;
    let local = local_branch_metadata(&f, feature).await;
    let at_remote = entries_at(LoreBranchLocation::Remote, &server);
    let at_local = entries_at(LoreBranchLocation::Local, &local);
    assert_eq!(status, 0);
    assert_eq!(
        strings(&server, &["owner", "reviewer"]),
        [Some("server"), None]
    );
    assert_eq!(
        strings(&local, &["owner", "reviewer"]),
        [None, Some("local")]
    );
    assert_eq!(reported(&events), [&at_remote[..], &at_local[..]].concat());
    assert_eq!(
        event_order(&events),
        [
            records_then_complete(LoreBranchLocation::Remote, at_remote.len()),
            records_then_complete(LoreBranchLocation::Local, at_local.len()),
        ]
        .concat()
    );
    assert_eq!(
        location_outcomes(&events),
        [
            (LoreBranchLocation::Remote, at_remote.len() as u64, 0),
            (LoreBranchLocation::Local, at_local.len() as u64, 0),
        ]
    );
    f.close().await;
}

/// A set edits each location's own metadata: it sets one entry and removes another at both, and
/// an entry the local store alone holds stays there alone. Each location then reports the
/// metadata it holds.
#[tokio::test]
async fn set_edits_both_locations_and_each_reports_its_metadata() {
    let f = fixture().await;
    let feature = seeded_branch(&f, "feature").await;
    on_server(&f, async |context| {
        store_entry(context, feature, "stale", "server").await;
    })
    .await;
    in_client(async {
        store_entry(local_writer(&f), feature, "stale", "local").await;
        store_entry(local_writer(&f), feature, "local-only", "kept").await;
    })
    .await;

    let (status, events) = set_branch_metadata(
        &f,
        f.store,
        graph_globals(),
        feature,
        vec![("owner", string("ci"))],
        &["stale"],
    )
    .await;

    let server = server_branch_metadata(&f, feature).await;
    let local = local_branch_metadata(&f, feature).await;
    let at_remote = entries_at(LoreBranchLocation::Remote, &server);
    let at_local = entries_at(LoreBranchLocation::Local, &local);
    let keys = ["owner", "stale", "local-only"];
    assert_eq!(status, 0);
    assert_eq!(strings(&server, &keys), [Some("ci"), None, None]);
    assert_eq!(strings(&local, &keys), [Some("ci"), None, Some("kept")]);
    assert_eq!(reported(&events), [&at_remote[..], &at_local[..]].concat());
    assert_eq!(
        location_outcomes(&events),
        [
            (LoreBranchLocation::Remote, at_remote.len() as u64, 0),
            (LoreBranchLocation::Local, at_local.len() as u64, 0),
        ]
    );
    f.close().await;
}

/// A set with the `remote` flag edits the remote alone: the server holds the edit and the remote
/// reports the metadata it holds, while the local store's metadata stays as it was.
#[tokio::test]
async fn a_set_with_the_remote_flag_edits_the_remote_alone() {
    let f = fixture().await;
    let feature = seeded_branch(&f, "feature").await;
    let local_before = entries_at(
        LoreBranchLocation::Local,
        &local_branch_metadata(&f, feature).await,
    );

    let (status, events) = set_branch_metadata(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Remote),
        feature,
        vec![("owner", string("ci"))],
        &[],
    )
    .await;

    let server = server_branch_metadata(&f, feature).await;
    let at_remote = entries_at(LoreBranchLocation::Remote, &server);
    assert_eq!(status, 0);
    assert_eq!(strings(&server, &["owner"]), [Some("ci")]);
    assert_eq!(
        entries_at(
            LoreBranchLocation::Local,
            &local_branch_metadata(&f, feature).await
        ),
        local_before
    );
    assert_eq!(reported(&events), at_remote);
    assert_eq!(
        location_outcomes(&events),
        [(LoreBranchLocation::Remote, at_remote.len() as u64, 0)]
    );
    f.close().await;
}

/// An address whose payload the local store alone holds fails the remote with `AddressNotFound`
/// before its swap, so the server's metadata stays as it was. The failure stops the call before
/// the local store, whose metadata stays as it was too.
#[tokio::test]
async fn an_address_the_remote_lacks_fails_the_remote_before_the_swap() {
    let f = fixture().await;
    let feature = seeded_branch(&f, "feature").await;
    let address = put_content(
        f.store,
        f.repository,
        Context::default(),
        b"held in the local store alone",
        0,
        0,
    )
    .await;
    let server_before = entries_at(
        LoreBranchLocation::Remote,
        &server_branch_metadata(&f, feature).await,
    );
    let local_before = entries_at(
        LoreBranchLocation::Local,
        &local_branch_metadata(&f, feature).await,
    );

    let (status, events) = set_branch_metadata(
        &f,
        f.store,
        graph_globals(),
        feature,
        vec![("payload", LoreMetadata::Address(address))],
        &[],
    )
    .await;

    assert_eq!(status, AddressNotFound::FFI_CODE);
    assert_eq!(
        location_outcomes(&events),
        [(LoreBranchLocation::Remote, 0, AddressNotFound::FFI_CODE)]
    );
    assert_eq!(
        entries_at(
            LoreBranchLocation::Remote,
            &server_branch_metadata(&f, feature).await
        ),
        server_before
    );
    assert_eq!(
        entries_at(
            LoreBranchLocation::Local,
            &local_branch_metadata(&f, feature).await
        ),
        local_before
    );
    f.close().await;
}

/// A Binary value of 300 KiB makes metadata over the 256 KiB of one fragment, which the set
/// writes at the remote in fragments: the server holds the value, as does the local store.
#[tokio::test]
async fn a_set_over_one_fragment_reaches_the_server() {
    let f = fixture().await;
    let feature = seeded_branch(&f, "feature").await;
    let mut value = vec![0; 300 * 1024];
    StdRng::seed_from_u64(0x5EED).fill(&mut value[..]);

    let (status, events) = set_branch_metadata(
        &f,
        f.store,
        graph_globals(),
        feature,
        vec![(
            "large",
            LoreMetadata::Binary(LoreBinary::from_bytes(&value)),
        )],
        &[],
    )
    .await;

    let server = server_branch_metadata(&f, feature).await;
    let local = local_branch_metadata(&f, feature).await;
    assert_eq!(status, 0);
    assert!(
        server.get_binary("large").ok() == Some(&value[..]),
        "the server holds the value"
    );
    assert!(
        local.get_binary("large").ok() == Some(&value[..]),
        "the local store holds the value"
    );
    assert_eq!(
        location_outcomes(&events),
        [
            (
                LoreBranchLocation::Remote,
                entries_at(LoreBranchLocation::Remote, &server).len() as u64,
                0
            ),
            (
                LoreBranchLocation::Local,
                entries_at(LoreBranchLocation::Local, &local).len() as u64,
                0
            ),
        ]
    );
    f.close().await;
}

/// `protect` set through the verb at the remote protects the branch there: an archive on the
/// server with `branch::delete`, the core Lore Server's branch delete runs, is refused as the
/// archive of a protected branch.
#[tokio::test]
async fn protect_set_through_the_verb_refuses_an_archive_at_the_remote() {
    let f = fixture().await;
    seed_main(&f).await;
    let guarded = create_remote_branch(&f, "guarded", f.default_branch, CREATOR).await;

    let (status, _) = set_branch_metadata(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Remote),
        guarded,
        vec![(branch::PROTECT, LoreMetadata::Boolean(1))],
        &[],
    )
    .await;
    let archived = on_server(&f, async |context| branch::delete(context, guarded).await).await;

    assert_eq!(status, 0);
    assert!(
        archived
            .as_ref()
            .is_err_and(|err| err.is_delete_protected()),
        "expected DeleteProtected, got {archived:?}"
    );
    f.close().await;
}

/// A branch archived at both locations reads and edits like a live one: get reports each
/// location's entries, its archive entry included, and set edits each location's metadata, which
/// keeps the archive entry.
#[tokio::test]
async fn an_archived_branch_reads_and_edits_at_both_locations() {
    let f = fixture().await;
    let retired = seeded_branch(&f, "retired").await;
    archive_on_server(&f, retired).await;
    archive_locally(&f, retired).await;
    let archived = [
        entries_at(
            LoreBranchLocation::Remote,
            &server_branch_metadata(&f, retired).await,
        ),
        entries_at(
            LoreBranchLocation::Local,
            &local_branch_metadata(&f, retired).await,
        ),
    ]
    .concat();

    let (get_status, got) = get_branch_metadata(&f, f.store, graph_globals(), retired, "").await;
    let (set_status, set) = set_branch_metadata(
        &f,
        f.store,
        graph_globals(),
        retired,
        vec![("owner", string("ci"))],
        &[],
    )
    .await;

    let server = server_branch_metadata(&f, retired).await;
    let local = local_branch_metadata(&f, retired).await;
    assert_eq!((get_status, set_status), (0, 0));
    assert_eq!(reported(&got), archived);
    assert_eq!(
        reported(&set),
        [
            entries_at(LoreBranchLocation::Remote, &server),
            entries_at(LoreBranchLocation::Local, &local),
        ]
        .concat()
    );
    for metadata in [&server, &local] {
        assert!(metadata.get_typed(branch::ARCHIVED).is_ok());
        assert_eq!(strings(metadata, &["owner"]), [Some("ci")]);
    }
    f.close().await;
}

/// A swap at the remote that another writer overtakes after the verb's read, as the server's store
/// takes that writer's metadata once it answers the verb's load of the key, makes the verb read the
/// metadata the server answers the swap with and apply its edit to that: the server then holds both
/// writers' entries, which the remote reports.
#[tokio::test]
async fn a_lost_remote_swap_keeps_the_other_writers_entries() {
    let f = fixture().await;
    seed_main(&f).await;
    let feature = create_remote_branch(&f, "feature", f.default_branch, CREATOR).await;
    let (_, other) = on_server(&f, async |context| {
        metadata_with_entry(&context, feature, "reviewer", "other").await
    })
    .await;
    overtake_on_server(&f, feature, other);

    let (status, events) = set_branch_metadata(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Remote),
        feature,
        vec![("owner", string("ci"))],
        &[],
    )
    .await;

    let server = server_branch_metadata(&f, feature).await;
    let at_remote = entries_at(LoreBranchLocation::Remote, &server);
    assert_eq!(status, 0);
    assert_eq!(
        strings(&server, &["owner", "reviewer"]),
        [Some("ci"), Some("other")]
    );
    assert_eq!(reported(&events), at_remote);
    assert_eq!(
        location_outcomes(&events),
        [(LoreBranchLocation::Remote, at_remote.len() as u64, 0)]
    );
    f.close().await;
}

/// A metadata key the server loses after the verb's read, as its store clears the key once it
/// answers the verb's load, makes the server answer the swap with `NotFound`, which the verb's
/// second metadata get confirms as a miss: the remote reports no record and no error, the call
/// fails `BranchNotFound`, and the key stays clear.
#[tokio::test]
async fn a_remote_metadata_key_cleared_before_the_swap_is_a_miss() {
    let f = fixture().await;
    seed_main(&f).await;
    let feature = create_remote_branch(&f, "feature", f.default_branch, CREATOR).await;
    overtake_on_server(&f, feature, Hash::default());

    let (status, events) = set_branch_metadata(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Remote),
        feature,
        vec![("owner", string("ci"))],
        &[],
    )
    .await;

    let held = on_server(&f, async |context| {
        branch::mutable_value(context, branch::METADATA, feature).await
    })
    .await
    .expect("reads the metadata key on the server");
    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert_eq!(
        location_outcomes(&events),
        [(LoreBranchLocation::Remote, 0, 0)]
    );
    assert_eq!(held, None);
    f.close().await;
}
