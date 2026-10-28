// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_revision::revision::ResolveSearchLocation;
use lore_revision::revision::resolve_boxed;

use crate::helper::setup_test_execution;
use crate::paged_remote::Anchor;
use crate::paged_remote::PagedRemote;
use crate::paged_remote::client_and_writer;

fn signature(number: u64) -> Hash {
    let mut bytes = [0xA5; 32];
    bytes[..8].copy_from_slice(&number.to_le_bytes());
    Hash::from(bytes)
}

/// Resolves `<branch>@<number>` against a remote whose page for `number` runs newest to
/// oldest from `first`, returning the revision resolved and how many revision lists the remote
/// was asked for.
async fn resolve_number_from_page(number: u64, first: u64) -> (Hash, usize) {
    let branch = BranchId::from([7; 16]);
    let page = (1..=first)
        .rev()
        .take(100)
        .map(|number| (number, signature(number)))
        .collect();
    let remote = Arc::new(PagedRemote::new(branch, signature(first)).page(
        Anchor::Number(number),
        page,
        Hash::default(),
    ));
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
    (resolved, remote.lists())
}

/// A page runs newest to oldest, so the revision asked for is first in a page the remote
/// walked from it.
#[tokio::test]
async fn a_number_first_in_the_remote_page_resolves_from_that_page() {
    let (resolved, lists) = resolve_number_from_page(8_500, 8_500).await;
    assert_eq!(resolved, signature(8_500));
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
        let (resolved, lists) = resolve_number_from_page(8_500, first).await;
        assert_eq!(resolved, signature(8_500), "page from {first}");
        assert_eq!(lists, 1, "page from {first}");
    }
}
