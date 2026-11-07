// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

mod batch_load_history {
    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Address;
    use lore_base::types::BranchId;
    use lore_base::types::Hash;
    use lore_revision::find::batch_load_history;
    use lore_revision::immutable::is_stored_local;

    use crate::branch::write_line;
    use crate::helper::setup_test_execution;
    use crate::immutable::serving::Serving;
    use crate::immutable::serving::remote_copy;
    use crate::immutable::serving::serving_remote;
    use crate::paged_remote::Anchor;
    use crate::paged_remote::PagedRemote;
    use crate::paged_remote::above_unverified;
    use crate::paged_remote::branched_from_cached_segment;
    use crate::paged_remote::client_on;
    use crate::paged_remote::client_on_storage;
    use crate::paged_remote::client_stores;
    use crate::paged_remote::remote_revision;
    use crate::paged_remote::stored_locally;

    /// A batch caches the states the remote's page carries in the local immutable store, in a
    /// context that cannot write the mutable store: those of the revisions on the line, and those
    /// the page lists above the revision asked for.
    #[tokio::test]
    async fn a_batch_without_a_write_token_caches_the_listed_states_locally() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let branched = branched_from_cached_segment().await;
                assert!(branched.client.try_write_token().is_none());
                let line = &branched.remote_line;
                let before = stored_locally(&branched.client, line).await;

                let history = batch_load_history(branched.client.clone(), branched.local)
                    .await
                    .expect("the page lists the line with its states");

                assert_eq!(
                    history,
                    vec![
                        (4, branched.local),
                        (3, line[2]),
                        (2, line[1]),
                        (1, line[0])
                    ]
                );
                assert_eq!(before, [false; 6]);
                assert_eq!(stored_locally(&branched.client, line).await, [true; 6]);
            })
            .await;
    }

    /// A page item's number is the server's claim, and the state it carries is the revision's own,
    /// so a batch numbers a listed revision from its state: here 3, where the item claims 9.
    #[tokio::test]
    async fn a_batch_numbers_a_listed_revision_from_its_state() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let below = remote_revision(3, Hash::default(), Hash::from([0xA0; 32])).await;
                let stores = client_stores().await;
                let local = write_line(&stores.writer, below.signature, 4, 1).await[0];
                let remote = PagedRemote::new(BranchId::from([7; 16]), local)
                    .holding([&below])
                    .page(
                        Anchor::Signature(below.signature),
                        vec![(9, below.signature)],
                        Hash::default(),
                    );
                let client = client_on(&stores, Arc::new(remote));

                let history = batch_load_history(client, local)
                    .await
                    .expect("the page lists the line with its states");

                assert_eq!(history, vec![(4, local), (3, below.signature)]);
            })
            .await;
    }

    /// A batch caches the metadata of the revisions the remote's page lists on the line in the
    /// local immutable store, in a context that cannot write the mutable store.
    #[tokio::test]
    async fn a_batch_without_a_write_token_caches_the_listed_metadata_locally() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let (source, stored, _) =
                    remote_copy(b"the metadata of a revision only the remote holds", 0, 0).await;
                let metadata = Address::zero_context_hash(stored.hash);
                let below = remote_revision(3, Hash::default(), metadata.hash).await;
                let stores = client_stores().await;
                let local = write_line(&stores.writer, below.signature, 4, 1).await[0];
                let remote = PagedRemote::new(BranchId::from([7; 16]), local)
                    .holding([&below])
                    .listing_metadata(below.signature, metadata.hash)
                    .page(
                        Anchor::Signature(below.signature),
                        vec![(3, below.signature)],
                        Hash::default(),
                    );
                let storage = serving_remote(&source, Serving::default());
                let client = client_on_storage(&stores, Arc::new(remote), vec![storage]);
                assert!(client.try_write_token().is_none());
                let before = is_stored_local(client.clone(), metadata).await;

                batch_load_history(client.clone(), local)
                    .await
                    .expect("the page lists the line with its states");

                assert!(!before);
                assert!(is_stored_local(client, metadata).await);
            })
            .await;
    }

    /// A page that lists a revision on the line without its own state fails the batch, whether
    /// the state is missing, cut short, or another revision's, and whether the revision is the one
    /// asked for or one below it.
    #[tokio::test]
    async fn a_page_listing_a_revision_without_its_own_state_fails_the_batch() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                for case in above_unverified(Hash::default()).await {
                    let history = batch_load_history(case.client, case.local).await;

                    assert!(
                        history.as_ref().is_err_and(|error| error.is_internal()),
                        "{} state for {}: {history:?}",
                        case.kind,
                        case.position
                    );
                }
            })
            .await;
    }
}
