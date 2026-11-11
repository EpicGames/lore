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
    use crate::branch::write_not_a_state;
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

    /// A batch stops its walk of the local store at a blob that is not a revision state, as at a
    /// state the store lacks, whether the start or a parent names it, rather than listing it with
    /// the number its bytes give. With no remote to list the line from there, the batch holds the
    /// revisions the walk read above it.
    #[tokio::test]
    async fn a_batch_stops_at_a_blob_that_is_not_a_state() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let not_a_state = write_not_a_state(&stores.writer).await;
                let start = write_line(&stores.writer, not_a_state, 2, 1).await[0];

                for (named_by, revision, read) in [
                    ("the start", not_a_state, vec![]),
                    ("a parent", start, vec![(2, start)]),
                ] {
                    let history = batch_load_history(stores.writer.clone(), revision)
                        .await
                        .expect("a batch without a remote to list the line from there succeeds");

                    assert_eq!(history, read, "named by {named_by}");
                }
            })
            .await;
    }

    /// A batch whose walk of the local store stops at a revision the store lacks, with no remote to
    /// list the line from there, holds the revisions the walk read, whether the context has no
    /// remote or one whose list of the line is not found.
    #[tokio::test]
    async fn a_batch_without_a_remote_to_list_the_line_holds_the_local_revisions() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let line = write_line(&stores.writer, Hash::from([9; 32]), 2, 3).await;
                let unlisted = Arc::new(PagedRemote::new(BranchId::from([7; 16]), line[2]));

                for (remote, client, lists) in [
                    ("none", stores.writer.clone(), 0),
                    ("not listing", client_on(&stores, unlisted.clone()), 1),
                ] {
                    let history = batch_load_history(client, line[2])
                        .await
                        .expect("a batch without a remote to list the line from there succeeds");

                    assert_eq!(
                        (history, unlisted.lists()),
                        (vec![(4, line[2]), (3, line[1]), (2, line[0])], lists),
                        "remote: {remote}"
                    );
                }
            })
            .await;
    }
}

mod revision_by_number_local {
    use bytes::Bytes;
    use lore_base::error::Disconnected;
    use lore_base::error::Oversized;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_revision::find::NumberWalk;
    use lore_revision::find::revision_by_number_local;
    use lore_revision::lore::RepositoryId;
    use lore_storage::WriteOptions;

    use crate::branch::write_line;
    use crate::branch::write_not_a_state;
    use crate::helper::setup_test_execution;
    use crate::paged_remote::Reads;
    use crate::paged_remote::counted_client_stores;
    use crate::repository::test_helpers::built_context;
    use crate::repository::test_helpers::caused_by;
    use crate::repository::test_helpers::counted_endpoint;

    /// The reads of a walk of `steps` states: one payload read each, and no query.
    fn reads(steps: usize) -> Reads {
        Reads {
            payloads: steps,
            queries: 0,
        }
    }

    /// A walk finds each number on the line, the start's and the first revision's included,
    /// reading one state per revision it walks.
    #[tokio::test]
    async fn number_walk_finds_the_numbered_revision() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let (stores, counting) = counted_client_stores().await;
                let line = write_line(&stores.writer, Hash::default(), 1, 3).await;
                counting.take_reads();

                for (index, &revision) in line.iter().enumerate() {
                    let number = index as u64 + 1;

                    let walk = revision_by_number_local(&stores.writer, line[2], number)
                        .await
                        .expect("the store holds every state on the line");

                    assert_eq!(walk, NumberWalk::Found(revision), "number {number}");
                    assert_eq!(counting.take_reads(), reads(3 - index), "number {number}");
                }
            })
            .await;
    }

    /// A walk that reaches a revision numbered below the target stops there, without reading its
    /// parent, here one the store lacks.
    #[tokio::test]
    async fn number_walk_stops_below_the_target() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let (stores, counting) = counted_client_stores().await;
                let below = write_line(&stores.writer, Hash::from([0xA0; 32]), 2, 1).await[0];
                let start = write_line(&stores.writer, below, 5, 1).await[0];
                counting.take_reads();

                let walk = revision_by_number_local(&stores.writer, start, 3)
                    .await
                    .expect("the walk reads only states the store holds");
                assert_eq!(walk, NumberWalk::Absent);
                assert_eq!(counting.take_reads(), reads(2));

                let walk = revision_by_number_local(&stores.writer, start, 6)
                    .await
                    .expect("the walk reads only the start");
                assert_eq!(walk, NumberWalk::Absent);
                assert_eq!(counting.take_reads(), reads(1));
            })
            .await;
    }

    /// A walk past the line's first revision, or from no revision, finds nothing.
    #[tokio::test]
    async fn number_walk_ends_at_the_first_revision() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let (stores, counting) = counted_client_stores().await;
                let line = write_line(&stores.writer, Hash::default(), 2, 2).await;
                counting.take_reads();

                let walk = revision_by_number_local(&stores.writer, line[1], 1)
                    .await
                    .expect("the store holds every state on the line");
                assert_eq!(walk, NumberWalk::Absent);
                assert_eq!(counting.take_reads(), reads(2));

                let walk = revision_by_number_local(&stores.writer, Hash::default(), 1)
                    .await
                    .expect("a walk from no revision reads nothing");
                assert_eq!(walk, NumberWalk::Absent);
                assert_eq!(counting.take_reads(), reads(0));
            })
            .await;
    }

    /// A walk that reaches a revision whose state the store lacks reports that revision, whether
    /// it is a parent or the start.
    #[tokio::test]
    async fn number_walk_reports_a_missing_parent() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let (stores, _counting) = counted_client_stores().await;
                let missing = Hash::from([0xA0; 32]);
                let start = write_line(&stores.writer, missing, 2, 1).await[0];

                let walk = revision_by_number_local(&stores.writer, start, 1)
                    .await
                    .expect("a state the store lacks is not a failed read");
                assert_eq!(walk, NumberWalk::Missing(missing));

                let walk = revision_by_number_local(&stores.writer, missing, 1)
                    .await
                    .expect("a state the store lacks is not a failed read");
                assert_eq!(walk, NumberWalk::Missing(missing));
            })
            .await;
    }

    /// A read that fails other than for a state the store lacks fails the walk: here the start
    /// names content too large to be a state.
    #[tokio::test]
    async fn number_walk_fails_on_a_read_that_is_not_a_miss() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let (stores, _counting) = counted_client_stores().await;
                let oversized = lore_revision::immutable::write(
                    stores.writer.clone(),
                    Context::default(),
                    Bytes::from(vec![7u8; 400]),
                    WriteOptions::default(),
                )
                .await
                .expect("writes the content");

                let walk = revision_by_number_local(&stores.writer, oversized.hash, 1).await;

                let error = walk.expect_err("content too large to be a state fails the walk");
                assert!(caused_by::<Oversized>(&error), "{error:?}");
            })
            .await;
    }

    /// A walk that reaches a blob that is not a revision state fails with an internal error,
    /// whether the start or a parent names it, rather than reading the blob as a number and a
    /// parent.
    #[tokio::test]
    async fn number_walk_fails_on_a_blob_that_is_not_a_state() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let (stores, _counting) = counted_client_stores().await;
                let not_a_state = write_not_a_state(&stores.writer).await;
                let start = write_line(&stores.writer, not_a_state, 2, 1).await[0];

                for (named_by, revision) in [("the start", not_a_state), ("a parent", start)] {
                    let walk = revision_by_number_local(&stores.writer, revision, 1).await;

                    assert!(
                        walk.as_ref().is_err_and(|error| error.is_internal()),
                        "named by {named_by}: {walk:?}"
                    );
                }
            })
            .await;
    }

    /// A failed store read fails the walk with the store's error on a strict-local context, and is
    /// a state the store lacks on any other.
    #[tokio::test]
    async fn number_walk_fails_on_a_failed_read_where_reads_are_strict() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let (stores, counting) = counted_client_stores().await;
                let start = write_line(&stores.writer, Hash::default(), 1, 1).await[0];
                let strict_reader = stores.strict_reader();
                counting.fail_reads();

                let strict = revision_by_number_local(&strict_reader, start, 1).await;
                let lenient = revision_by_number_local(&stores.writer, start, 1).await;

                let error = strict.expect_err("a strict-local context fails a failed read");
                assert!(caused_by::<Disconnected>(&error), "{error:?}");
                assert_eq!(
                    lenient.expect("a failed read is a miss where reads are not strict"),
                    NumberWalk::Missing(start)
                );
            })
            .await;
    }

    /// A walk on a context whose remote is in reach asks it for nothing: a state the local store
    /// lacks is `Missing`, and no session is started.
    #[tokio::test]
    async fn number_walk_leaves_the_remote_off() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let (endpoint, connection) = counted_endpoint(RepositoryId::default()).await;
                let context = built_context(|context| context.with_remote_endpoint(endpoint)).await;
                assert!(!context.is_offline());
                let missing = Hash::from([0xA0; 32]);

                let walk = revision_by_number_local(&context, missing, 1).await;

                assert_eq!(connection.storage_session_starts(), 0);
                assert_eq!(
                    walk.expect("a state the store lacks is not a failed read"),
                    NumberWalk::Missing(missing)
                );
            })
            .await;
    }
}
