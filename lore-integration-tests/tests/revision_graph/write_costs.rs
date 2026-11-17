// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What `branch_create` reads and writes in the local store in one call with no location flag,
//! counted on a handle over the fixture handle's local stores: its pre-checks' loads and reads,
//! then exactly the record it creates.

use std::sync::atomic::Ordering;

use lore_base::error::InvalidArguments;
use lore_revision::branch;

use super::counting::ByKeyType;
use super::counting::LocalCalls;
use super::counting::counted;
use super::counting::open_counted_handle;
use super::support::*;
use crate::common::net_common::close_handle;

/// A create from a branch the local store holds with metadata, at both locations, loads the
/// name's mapping, the latest of the id the name derives, the parent's metadata key and latest, and
/// reads the parent's metadata blob. It then writes exactly its record: its metadata blob, swapped
/// into the metadata key from zero, the name's mapping and the latest, each with a compare-and-swap,
/// and the latest's status and history: a load of the history key, and a stored history blob the
/// key then names. Each blob write queries the store before it puts. The parent's latest is a
/// revision committed on it, so no history blob of another branch holds the new branch's entry.
#[tokio::test]
async fn branch_create_local_writes_exactly_its_record() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let parent = create_branch_at_both_locations(
        &f,
        "parent",
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await;
    let committed = commit_on(f.store, f.repository, parent, seeded).await;
    publish(&f, parent, committed).await;

    let (status, _, calls) = counted(&f, async |store| {
        create_branch(&f, store, graph_globals(), "feature", latest(parent)).await
    })
    .await;

    assert_eq!(status, 0);
    assert_eq!(
        calls,
        LocalCalls {
            loads: ByKeyType {
                branch_id: 1,
                branch_metadata: 1,
                branch_latest_pointer: 2,
                untyped: 1,
                ..Default::default()
            },
            swaps: ByKeyType {
                branch_id: 1,
                branch_metadata: 1,
                branch_latest_pointer: 1,
                ..Default::default()
            },
            gets: 1,
            queries: 2,
            puts: 2,
            others: 2,
            ..Default::default()
        }
    );
    f.close().await;
}

/// A create with an invalid name fails before any read: no call on either local store, no read on
/// the server's store, and no location complete event.
#[tokio::test]
async fn branch_create_with_an_invalid_name_touches_no_store() {
    let f = fixture().await;
    let (store, mutable, immutable) = open_counted_handle(&f, None, None);
    let server_gets = f.server_gets.load(Ordering::SeqCst);

    let (status, events) =
        create_branch(&f, store, graph_globals(), "", latest(f.default_branch)).await;
    close_handle(store).await;

    assert_eq!(status, InvalidArguments::FFI_CODE);
    assert_eq!((mutable.total(), immutable.total()), (0, 0));
    assert_eq!(f.server_gets.load(Ordering::SeqCst), server_gets);
    assert!(location_outcomes(&events).is_empty());
    f.close().await;
}
