// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::revision::ResolveSearchLocation;
use lore_revision::revision::resolve_boxed;

use crate::branch::write_line;
use crate::helper::setup_test_execution;
use crate::paged_remote::Anchor;
use crate::paged_remote::PagedRemote;
use crate::paged_remote::client_and_writer;
use crate::paged_remote::remote_revision;
use crate::paged_remote::stored_locally;
use crate::paged_remote::without_its_own_state;

fn signature(number: u64) -> Hash {
    let mut bytes = [0xA5; 32];
    bytes[..8].copy_from_slice(&number.to_le_bytes());
    Hash::from(bytes)
}

/// Resolves `<branch>@<number>` against a remote whose page for `number` runs newest to
/// oldest from `first`, and carries the state of revision `number`. Returns the revision
/// resolved, revision `number`, and how many revision lists the remote was asked for.
async fn resolve_number_from_page(number: u64, first: u64) -> (Hash, Hash, usize) {
    let branch = BranchId::from([7; 16]);
    let target = remote_revision(number, Hash::default(), Hash::from([0xA5; 32])).await;
    let page = (1..=first)
        .rev()
        .take(100)
        .map(|listed| {
            if listed == number {
                (listed, target.signature)
            } else {
                (listed, signature(listed))
            }
        })
        .collect();
    let remote = Arc::new(
        PagedRemote::new(branch, signature(first))
            .holding([&target])
            .page(Anchor::Number(number), page, Hash::default()),
    );
    let (repository, _writer) = client_and_writer(remote.clone()).await;
    let resolved = LORE_CONTEXT
        .scope(
            setup_test_execution(),
            resolve_boxed(
                repository,
                format!("{branch}@{number}"),
                ResolveSearchLocation::Remote,
            ),
        )
        .await
        .expect("the remote's page names the revision");
    (resolved, target.signature, remote.lists())
}

/// A page runs newest to oldest, so the revision asked for is first in a page the remote
/// walked from it.
#[tokio::test]
async fn a_number_first_in_the_remote_page_resolves_from_that_page() {
    let (resolved, target, lists) = resolve_number_from_page(8_500, 8_500).await;
    assert_eq!(resolved, target);
    assert_eq!(
        lists, 1,
        "the page answering the number is the only list asked for"
    );
}

/// A page the remote serves from a cached segment holds the revision asked for at any
/// position.
#[tokio::test]
async fn a_number_inside_the_remote_page_resolves_from_that_page() {
    for first in [8_505, 8_550, 8_599] {
        let (resolved, target, lists) = resolve_number_from_page(8_500, first).await;
        assert_eq!(resolved, target, "page from {first}");
        assert_eq!(lists, 1, "page from {first}");
    }
}

/// The number a page lists beside an item picks it, and the item's own state confirms it. An item
/// listed with the number whose state is missing, cut short, or another revision's is not taken:
/// the lookup finds the revision through the search instead. Another revision's state here carries
/// the number asked for.
#[tokio::test]
async fn a_page_item_without_its_own_state_resolves_through_the_search() {
    let branch = BranchId::from([7; 16]);
    let first = remote_revision(1, Hash::default(), Hash::from([0xA0; 32])).await;
    let second = remote_revision(2, first.signature, Hash::from([0xA0; 32])).await;
    let third = remote_revision(3, second.signature, Hash::from([0xA0; 32])).await;
    let impostor = remote_revision(2, first.signature, Hash::from([0xB0; 32])).await;
    for (kind, listed) in without_its_own_state(&impostor, &second) {
        let remote = Arc::new(
            PagedRemote::new(branch, third.signature)
                .holding([&first, &second, &third])
                .holding(listed.iter())
                .page(
                    Anchor::Number(2),
                    vec![
                        (3, third.signature),
                        (2, impostor.signature),
                        (1, first.signature),
                    ],
                    Hash::default(),
                )
                .page(
                    Anchor::Signature(third.signature),
                    vec![
                        (3, third.signature),
                        (2, second.signature),
                        (1, first.signature),
                    ],
                    Hash::default(),
                )
                .page(
                    Anchor::Signature(second.signature),
                    vec![(2, second.signature), (1, first.signature)],
                    Hash::default(),
                ),
        );
        let (repository, _writer) = client_and_writer(remote).await;

        let resolved = LORE_CONTEXT
            .scope(
                setup_test_execution(),
                resolve_boxed(
                    repository,
                    format!("{branch}@2"),
                    ResolveSearchLocation::Remote,
                ),
            )
            .await
            .expect("the search finds revision 2");

        assert_eq!(resolved, second.signature, "{kind} state");
    }
}

/// The lookup caches the states of the remote's page for the number in the local immutable store,
/// in a context that cannot write the mutable store.
#[tokio::test]
async fn a_lookup_without_a_write_token_caches_the_page_states_locally() {
    let branch = BranchId::from([7; 16]);
    let mut revisions = Vec::new();
    let mut parent = Hash::default();
    for number in 1..=3 {
        let revision = remote_revision(number, parent, Hash::from([0xA0; 32])).await;
        parent = revision.signature;
        revisions.push(revision);
    }
    let line: Vec<Hash> = revisions
        .iter()
        .map(|revision| revision.signature)
        .collect();
    let remote = Arc::new(PagedRemote::new(branch, line[2]).holding(&revisions).page(
        Anchor::Number(2),
        vec![(3, line[2]), (2, line[1]), (1, line[0])],
        Hash::default(),
    ));
    let (repository, _writer) = client_and_writer(remote).await;
    assert!(repository.try_write_token().is_none());
    let stored = || stored_locally(&repository, &line);
    let before = LORE_CONTEXT.scope(setup_test_execution(), stored()).await;

    let resolved = LORE_CONTEXT
        .scope(
            setup_test_execution(),
            resolve_boxed(
                repository.clone(),
                format!("{branch}@2"),
                ResolveSearchLocation::Remote,
            ),
        )
        .await
        .expect("the remote's page names the revision");

    let after = LORE_CONTEXT.scope(setup_test_execution(), stored()).await;
    assert_eq!(resolved, line[1]);
    assert_eq!(before, [false; 3]);
    assert_eq!(after, [true; 3]);
}

/// A branch's latest is the local one when the walk between the two latests stops: here the
/// remote answers each revision list with the revision asked for alone, as its own next page,
/// so no round after the first adds a revision.
#[tokio::test]
async fn a_lookup_whose_walk_stops_falls_back_to_the_local_latest() {
    let branch = BranchId::from([7; 16]);
    let base_revision = remote_revision(1, Hash::default(), Hash::from([0xA0; 32])).await;
    let latest_revision = remote_revision(2, base_revision.signature, Hash::from([0xA0; 32])).await;
    let base = base_revision.signature;
    let remote_latest = latest_revision.signature;
    let remote = Arc::new(
        PagedRemote::new(branch, remote_latest)
            .holding([&base_revision, &latest_revision])
            .page(
                Anchor::Signature(remote_latest),
                vec![(2, remote_latest)],
                remote_latest,
            )
            .page(Anchor::Signature(base), vec![(1, base)], base),
    );
    let (repository, writer) = client_and_writer(remote).await;
    let local_latest = write_line(&writer, base, 2, 1).await[0];
    let store = branch::store_latest(
        writer,
        branch,
        Hash::default(),
        local_latest,
        BranchLatestStatus::Divergent,
    );
    LORE_CONTEXT
        .scope(setup_test_execution(), store)
        .await
        .expect("storing the local latest");

    let lookup = resolve_boxed(
        repository,
        format!("{branch}@latest"),
        ResolveSearchLocation::RemoteOrLocal,
    );
    let resolved = tokio::time::timeout(
        Duration::from_secs(1),
        LORE_CONTEXT.scope(setup_test_execution(), lookup),
    )
    .await
    .expect("the lookup ends within a second")
    .expect("the local latest answers the lookup");

    assert_eq!(resolved, local_latest);
}
