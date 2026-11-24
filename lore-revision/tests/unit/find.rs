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
    use crate::paged_remote::off_the_line;
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

    /// A page that does not continue the line from the revision asked for fails the batch: one
    /// listing a revision whose state is not the first parent the revision above it names, one
    /// whose cursor is not its last revision's first parent, and one without the revision asked
    /// for. Each revision it lists carries its own state, numbered in order.
    #[tokio::test]
    async fn a_page_that_does_not_continue_the_line_fails_the_batch() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let deeper = remote_revision(2, Hash::default(), Hash::from([0xA0; 32])).await;
                let below = remote_revision(3, deeper.signature, Hash::from([0xA0; 32])).await;
                let stray = remote_revision(2, Hash::default(), Hash::from([0xB0; 32])).await;
                for (kind, items, older) in
                    off_the_line((3, below.signature), deeper.signature, stray.signature)
                {
                    let stores = client_stores().await;
                    let local = write_line(&stores.writer, below.signature, 4, 1).await[0];
                    let remote = PagedRemote::new(BranchId::from([7; 16]), local)
                        .holding([&below, &deeper, &stray])
                        .page(Anchor::Signature(below.signature), items, older);
                    let client = client_on(&stores, Arc::new(remote));

                    let history = batch_load_history(client, local).await;

                    assert!(
                        history.as_ref().is_err_and(|error| error.is_internal()),
                        "{kind}: {history:?}"
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

    use crate::branch::write_branch_revision;
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

                    assert_eq!(
                        walk,
                        NumberWalk::Found {
                            revision,
                            metadata: Hash::default()
                        },
                        "number {number}"
                    );
                    assert_eq!(counting.take_reads(), reads(3 - index), "number {number}");
                }
            })
            .await;
    }

    /// A walk reports the metadata hash the found revision's state records, read in the step that
    /// finds it, whether that is the start or a revision the walk reaches below it.
    #[tokio::test]
    async fn number_walk_reports_the_found_revisions_metadata() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let (stores, counting) = counted_client_stores().await;
                let first = write_branch_revision(&stores.writer, Hash::default(), 1, 6).await;
                let second = write_branch_revision(&stores.writer, first, 2, 7).await;
                counting.take_reads();

                for (number, revision, metadata, steps) in [
                    (2, second, Hash::from([7; 32]), 1),
                    (1, first, Hash::from([6; 32]), 2),
                ] {
                    let walk = revision_by_number_local(&stores.writer, second, number)
                        .await
                        .expect("the store holds every state on the line");

                    assert_eq!(
                        walk,
                        NumberWalk::Found { revision, metadata },
                        "number {number}"
                    );
                    assert_eq!(counting.take_reads(), reads(steps), "number {number}");
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

mod verified_state {
    use bytes::Bytes;
    use lore_base::types::Hash;
    use lore_revision::find::verified_state;
    use lore_revision::state::StateData;
    use lore_transport::RevisionItem;

    /// A page item whose bytes hash to its signature but lack the state magic carries no state of
    /// its own, as a read of the same blob as a state finds none.
    #[test]
    fn an_item_whose_state_lacks_the_magic_has_no_verified_state() {
        let state = Bytes::from(vec![0u8; size_of::<StateData>()]);
        let item = RevisionItem {
            number: 0,
            signature: Hash::hash_buffer(&state),
            metadata: Hash::default(),
            state,
        };

        assert!(verified_state(&item).is_none());
    }
}

mod revision_state {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Hash;
    use lore_revision::find::StateMiss;
    use lore_revision::find::revision_state;
    use lore_revision::immutable::read_options_from_repository;

    use crate::branch::write_line;
    use crate::branch::write_not_a_state;
    use crate::helper::setup_test_execution;
    use crate::paged_remote::Reads;
    use crate::paged_remote::counted_client_stores;

    /// A read of a revision's state tells its three answers apart: the state, a hash the store
    /// lacks, and a blob that is not a revision state. A zero revision is missing, and reads
    /// nothing.
    #[tokio::test]
    async fn a_state_read_tells_a_missing_state_from_a_blob_that_is_not_one() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let (stores, counting) = counted_client_stores().await;
                let revision = write_line(&stores.writer, Hash::default(), 3, 1).await[0];
                let not_a_state = write_not_a_state(&stores.writer).await;
                let options = read_options_from_repository(&stores.writer);

                let state = revision_state(&stores.writer, revision, options)
                    .await
                    .expect("the read succeeds")
                    .expect("the store holds the state");
                assert_eq!(state.revision_number, 3);
                for (named, hash, miss) in [
                    (
                        "a hash the store lacks",
                        Hash::from([0xA0; 32]),
                        StateMiss::Missing,
                    ),
                    (
                        "a blob that is not a state",
                        not_a_state,
                        StateMiss::NotAState,
                    ),
                ] {
                    let read = revision_state(&stores.writer, hash, options)
                        .await
                        .expect("a miss is not a failed read");
                    assert_eq!(read.err(), Some(miss), "{named}");
                }
                counting.take_reads();

                let zero = revision_state(&stores.writer, Hash::default(), options)
                    .await
                    .expect("the read succeeds");

                assert_eq!(zero.err(), Some(StateMiss::Missing));
                assert_eq!(
                    counting.take_reads(),
                    Reads {
                        payloads: 0,
                        queries: 0
                    }
                );
            })
            .await;
    }
}

mod line_pager {
    use std::sync::Arc;
    use std::time::Duration;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::BranchId;
    use lore_base::types::Hash;
    use lore_revision::find::LINES_AHEAD;
    use lore_revision::find::LinePager;
    use lore_revision::find::PAGES_AHEAD;
    use lore_revision::find::lines_ahead;
    use lore_revision::metadata::Metadata;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::state::State;
    use tokio::sync::Semaphore;

    use crate::branch::write_line;
    use crate::helper::setup_test_execution;
    use crate::paged_remote::Anchor;
    use crate::paged_remote::PagedRemote;
    use crate::paged_remote::RemoteRevision;
    use crate::paged_remote::anchors;
    use crate::paged_remote::branch_metadata;
    use crate::paged_remote::client_on;
    use crate::paged_remote::client_stores;
    use crate::paged_remote::counted_client_stores;
    use crate::paged_remote::paging_remote;
    use crate::paged_remote::remote_line;
    use crate::paged_remote::remote_revision;
    use crate::paged_remote::signature_of;
    use crate::paged_remote::span;
    use crate::paged_remote::stored_locally;
    use crate::paged_remote::without_its_own_state;

    /// The branch of the remote's line.
    fn branch() -> BranchId {
        BranchId::from([7; 16])
    }

    /// Walks the line from `revision` back through first parents as a walk of a search does,
    /// asking `pager` for each revision before reading it, down to the first revision numbered
    /// `floor` or lower. Answers the numbers of the revisions read, newest first, up to the first
    /// that could not be read.
    async fn walk(
        pager: &mut LinePager,
        client: &Arc<RepositoryContext>,
        revision: Hash,
        floor: u64,
    ) -> Vec<u64> {
        walk_until(pager, client, revision, floor, floor).await
    }

    /// [`walk`], ending at the first revision numbered `until` or lower.
    async fn walk_until(
        pager: &mut LinePager,
        client: &Arc<RepositoryContext>,
        mut revision: Hash,
        floor: u64,
        until: u64,
    ) -> Vec<u64> {
        let mut numbers = Vec::new();
        while !revision.is_zero() {
            if pager.paging() {
                let below = numbers.last().copied().unwrap_or(u64::MAX);
                pager.page_in(revision, below, floor).await;
            }
            let Ok(state) = State::deserialize(client.clone(), revision).await else {
                break;
            };
            numbers.push(state.revision_number());
            if state.revision_number() <= until {
                break;
            }
            revision = state.parent_self();
        }
        numbers
    }

    fn permits() -> Arc<Semaphore> {
        Arc::new(Semaphore::new(1))
    }

    /// A walk down a long line reads its first two pages by hash signature, and every page below
    /// them by revision number on the branch of the line, requested ahead of it.
    #[tokio::test]
    async fn a_long_line_is_read_ahead_by_number() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let line = remote_line(1000, branch_metadata(&stores, branch()).await).await;
                let remote = Arc::new(paging_remote(branch(), &line));
                let client = client_on(&stores, remote.clone());
                let mut pager = LinePager::new(client.clone(), permits());

                let walked = walk(&mut pager, &client, line[999].signature, 0).await;
                pager.finish().await;

                assert_eq!(walked, (1..=1000).rev().collect::<Vec<u64>>());
                assert_eq!(
                    anchors(&remote),
                    (
                        vec![signature_of(&line, 1000), signature_of(&line, 900)],
                        vec![800, 700, 600, 500, 400, 300, 200, 100],
                    )
                );
            })
            .await;
    }

    /// A remote holding `line` of a thousand revisions whose first two pages, listed by hash
    /// signature, end off its page boundaries, at revision number 812. By revision number, it
    /// lists the whole page of a hundred revisions that holds revision number 811, and each page
    /// below from its top.
    fn unaligned_paging_remote(line: &[RemoteRevision]) -> PagedRemote {
        let mut remote = PagedRemote::new(branch(), line[999].signature)
            .holding(line)
            .page(
                signature_of(line, 1000),
                span(line, 1000, 912),
                Hash::default(),
            )
            .page(
                signature_of(line, 911),
                span(line, 911, 812),
                Hash::default(),
            )
            .page(Anchor::Number(811), span(line, 900, 801), Hash::default());
        for top in (100..=800).rev().step_by(100) {
            remote = remote.page(
                Anchor::Number(top),
                span(line, top, top - 99),
                Hash::default(),
            );
        }
        remote
    }

    /// Where the walk's own pages end off a page boundary of the remote, the first page ahead,
    /// which the remote lists from the top of its page, is cached from the number below them, and
    /// those after it are requested at the tops of the pages below, so the pages leave no gap.
    #[tokio::test]
    async fn pages_ahead_follow_the_remote_page_boundaries() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let line = remote_line(1000, branch_metadata(&stores, branch()).await).await;
                let remote = Arc::new(unaligned_paging_remote(&line));
                let client = client_on(&stores, remote.clone());
                let mut pager = LinePager::new(client.clone(), permits());

                let walked = walk(&mut pager, &client, line[999].signature, 0).await;
                pager.finish().await;

                assert_eq!(walked, (1..=1000).rev().collect::<Vec<u64>>());
                assert_eq!(
                    anchors(&remote),
                    (
                        vec![signature_of(&line, 1000), signature_of(&line, 911)],
                        vec![811, 800, 700, 600, 500, 400, 300, 200, 100],
                    )
                );
            })
            .await;
    }

    /// The first page ahead, requested off a page boundary of the remote, is retired once the walk
    /// reads below the lowest revision number of the remote's page that holds it, and the next
    /// page is requested then. The walk reads on once the pages have landed, as a walk that waited
    /// for the first would take it from the pages itself.
    #[tokio::test]
    async fn a_page_ahead_off_a_page_boundary_is_retired_below_its_page() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let line = remote_line(1000, branch_metadata(&stores, branch()).await).await;
                let remote = Arc::new(unaligned_paging_remote(&line));
                let client = client_on(&stores, remote.clone());
                let mut pager = LinePager::new(client.clone(), permits());

                walk_until(&mut pager, &client, line[999].signature, 0, 812).await;
                let bottoms = [800, 700, 600, 500].map(|index| line[index].signature);
                let landed = tokio::time::timeout(Duration::from_secs(5), async {
                    while stored_locally(&client, &bottoms).await != [true; 4] {
                        tokio::task::yield_now().await;
                    }
                });
                assert!(landed.await.is_ok(), "the pages requested ahead land");
                let walked = walk_until(&mut pager, &client, line[810].signature, 0, 790).await;
                for _ in 0..16 {
                    tokio::task::yield_now().await;
                }

                assert_eq!(walked.last(), Some(&790));
                assert_eq!(anchors(&remote).1, [811, 800, 700, 600, 500]);
                pager.finish().await;
            })
            .await;
    }

    /// The numbers of the revisions of `line` that `signatures` anchor, newest first.
    fn numbers_of(line: &[RemoteRevision], signatures: &[Anchor]) -> Vec<u64> {
        let mut numbers: Vec<u64> = signatures
            .iter()
            .map(|anchor| {
                line.iter()
                    .position(|revision| Anchor::Signature(revision.signature) == *anchor)
                    .expect("an anchor of the line") as u64
                    + 1
            })
            .collect();
        numbers.sort_unstable_by(|a, b| b.cmp(a));
        numbers
    }

    /// A remote holding `line` of a thousand revisions that lists its first two pages whole by hash
    /// signature, and from each revision number below them a page that holds the revision so
    /// numbered among newer and older ones, and ends half way to the next page. The page below it
    /// is listed by its cursor where `cursors`, and by the revision number below its oldest
    /// revision where not, and holds that revision among newer ones.
    fn short_paging_remote(line: &[RemoteRevision], cursors: bool) -> PagedRemote {
        let mut remote = PagedRemote::new(branch(), line[999].signature)
            .holding(line)
            .page(
                signature_of(line, 1000),
                span(line, 1000, 901),
                Hash::default(),
            )
            .page(
                signature_of(line, 900),
                span(line, 900, 801),
                Hash::default(),
            );
        for top in (100..=800).rev().step_by(100) {
            let below = top - 50;
            let (cursor, anchor) = if cursors {
                (
                    line[below as usize - 1].signature,
                    signature_of(line, below),
                )
            } else {
                (Hash::default(), Anchor::Number(below))
            };
            remote = remote
                .page(Anchor::Number(top), span(line, top + 30, below + 1), cursor)
                .page(anchor, span(line, below + 30, top - 99), Hash::default());
        }
        remote
    }

    /// A page ahead that ends above the next page requested is followed by the page below it, by
    /// its cursor, so the pages leave no gap however the remote sizes and aligns its pages.
    #[tokio::test]
    async fn a_short_page_ahead_is_followed_by_its_cursor() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let line = remote_line(1000, branch_metadata(&stores, branch()).await).await;
                let remote = Arc::new(short_paging_remote(&line, true));
                let client = client_on(&stores, remote.clone());
                let mut pager = LinePager::new(client.clone(), permits());

                let walked = walk(&mut pager, &client, line[999].signature, 0).await;
                pager.finish().await;

                assert_eq!(walked, (1..=1000).rev().collect::<Vec<u64>>());
                let (signatures, numbers) = anchors(&remote);
                assert_eq!(
                    numbers_of(&line, &signatures),
                    [1000, 900, 750, 650, 550, 450, 350, 250, 150, 50]
                );
                assert_eq!(numbers, [800, 700, 600, 500, 400, 300, 200, 100]);
            })
            .await;
    }

    /// A short page ahead without a cursor to the page below it is followed by the page from the
    /// number below its oldest revision.
    #[tokio::test]
    async fn a_short_page_ahead_without_a_cursor_is_followed_by_number() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let line = remote_line(1000, branch_metadata(&stores, branch()).await).await;
                let remote = Arc::new(short_paging_remote(&line, false));
                let client = client_on(&stores, remote.clone());
                let mut pager = LinePager::new(client.clone(), permits());

                let walked = walk(&mut pager, &client, line[999].signature, 0).await;
                pager.finish().await;

                assert_eq!(walked, (1..=1000).rev().collect::<Vec<u64>>());
                assert_eq!(
                    anchors(&remote),
                    (
                        vec![signature_of(&line, 1000), signature_of(&line, 900)],
                        (1..=16).rev().map(|step| step * 50).collect::<Vec<u64>>(),
                    )
                );
            })
            .await;
    }

    /// A page ahead ends the line at a revision the remote lists out of order, and follows no
    /// cursor past it.
    #[tokio::test]
    async fn a_page_ahead_ends_at_a_revision_listed_out_of_order() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let line = remote_line(1000, branch_metadata(&stores, branch()).await).await;
                let mut out_of_order = span(&line, 750, 750);
                out_of_order.extend(span(&line, 760, 760));
                let mut remote = PagedRemote::new(branch(), line[999].signature)
                    .holding(&line)
                    .page(
                        signature_of(&line, 1000),
                        span(&line, 1000, 901),
                        Hash::default(),
                    )
                    .page(
                        signature_of(&line, 900),
                        span(&line, 900, 801),
                        Hash::default(),
                    )
                    .page(
                        Anchor::Number(800),
                        span(&line, 800, 751),
                        line[749].signature,
                    )
                    .page(signature_of(&line, 750), out_of_order, line[758].signature)
                    .page(
                        signature_of(&line, 759),
                        span(&line, 759, 701),
                        Hash::default(),
                    );
                for top in (100..=700).rev().step_by(100) {
                    remote = remote.page(
                        Anchor::Number(top),
                        span(&line, top, top - 99),
                        Hash::default(),
                    );
                }
                let remote = Arc::new(remote);
                let client = client_on(&stores, remote.clone());
                let mut pager = LinePager::new(client.clone(), permits());

                let walked = walk(&mut pager, &client, line[999].signature, 0).await;
                pager.finish().await;

                assert_eq!(
                    walked.last(),
                    Some(&750),
                    "the walk reads the line the page listed"
                );
                assert!(
                    !anchors(&remote).0.contains(&signature_of(&line, 759)),
                    "no cursor past the revision listed out of order is followed"
                );
            })
            .await;
    }

    /// A remote holding `line` of a thousand revisions and `stray`, a revision numbered 775 off
    /// it, which lists the line's pages of a hundred revisions: the first two by hash signature,
    /// and those below them by revision number, but the page at 800 as `page_800` gives its items
    /// and cursor. By hash signature, it also lists the page of `stray`, the page of revision 775
    /// as `page_775` gives its items, and each page below from its top.
    fn paging_remote_with_page_800(
        line: &[RemoteRevision],
        stray: &RemoteRevision,
        page_800: (Vec<(u64, Hash)>, Hash),
        page_775: Vec<(u64, Hash)>,
    ) -> PagedRemote {
        let mut remote = PagedRemote::new(branch(), line[999].signature)
            .holding(line)
            .holding([stray])
            .page(
                signature_of(line, 1000),
                span(line, 1000, 901),
                Hash::default(),
            )
            .page(
                signature_of(line, 900),
                span(line, 900, 801),
                Hash::default(),
            )
            .page(Anchor::Number(800), page_800.0, page_800.1)
            .page(
                Anchor::Signature(stray.signature),
                vec![(775, stray.signature)],
                Hash::default(),
            )
            .page(signature_of(line, 775), page_775, Hash::default());
        for top in (100..=700).rev().step_by(100) {
            remote = remote
                .page(
                    Anchor::Number(top),
                    span(line, top, top - 99),
                    Hash::default(),
                )
                .page(
                    signature_of(line, top),
                    span(line, top, top - 99),
                    Hash::default(),
                );
        }
        remote
    }

    /// A page ahead lists the line down to the first revision whose state is not the first parent
    /// the revision above it names, and caches no state from there, whether the page lists that
    /// revision or names it as its cursor. The walk reads the line below by hash signature, from
    /// the revision below the last the page listed.
    #[tokio::test]
    async fn a_page_ahead_ends_at_a_revision_off_the_line() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                for cursor in [false, true] {
                    let stores = client_stores().await;
                    let metadata = branch_metadata(&stores, branch()).await;
                    let line = remote_line(1000, metadata).await;
                    let stray = remote_revision(775, Hash::default(), metadata).await;
                    let page_800 = if cursor {
                        (span(&line, 800, 776), stray.signature)
                    } else {
                        let mut items = span(&line, 800, 776);
                        items.push((775, stray.signature));
                        items.extend(span(&line, 774, 701));
                        (items, Hash::default())
                    };
                    let remote = Arc::new(paging_remote_with_page_800(
                        &line,
                        &stray,
                        page_800,
                        span(&line, 775, 701),
                    ));
                    let client = client_on(&stores, remote.clone());
                    let mut pager = LinePager::new(client.clone(), permits());

                    let walked = walk(&mut pager, &client, line[999].signature, 0).await;
                    pager.finish().await;

                    assert_eq!(
                        walked,
                        (1..=1000).rev().collect::<Vec<u64>>(),
                        "cursor: {cursor}"
                    );
                    assert_eq!(
                        stored_locally(&client, &[stray.signature]).await,
                        [false],
                        "cursor: {cursor}"
                    );
                    let (signatures, _) = anchors(&remote);
                    assert!(
                        signatures.contains(&signature_of(&line, 775))
                            && !signatures.contains(&Anchor::Signature(stray.signature)),
                        "cursor: {cursor}: {signatures:?}"
                    );
                }
            })
            .await;
    }

    /// A page ahead read from a cursor lists nothing where it doesn't hold the revision the cursor
    /// names, and caches none of its states. Neither does the walk's own page of that revision, so
    /// the walk ends above it.
    #[tokio::test]
    async fn a_page_ahead_from_a_cursor_without_its_revision_lists_nothing() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let metadata = branch_metadata(&stores, branch()).await;
                let line = remote_line(1000, metadata).await;
                let stray = remote_revision(775, Hash::default(), metadata).await;
                let remote = Arc::new(paging_remote_with_page_800(
                    &line,
                    &stray,
                    (span(&line, 800, 776), line[774].signature),
                    span(&line, 774, 701),
                ));
                let client = client_on(&stores, remote.clone());
                let mut pager = LinePager::new(client.clone(), permits());

                let walked = walk(&mut pager, &client, line[999].signature, 0).await;
                pager.finish().await;

                assert_eq!(walked, (776..=1000).rev().collect::<Vec<u64>>());
                assert_eq!(
                    stored_locally(&client, &[line[773].signature, line[700].signature]).await,
                    [false, false]
                );
            })
            .await;
    }

    /// A walk's own page, read by hash signature, caches the line down to the first revision whose
    /// state is not the first parent the revision above it names, and no state from there. The
    /// walk reads the line below by hash signature.
    #[tokio::test]
    async fn a_page_of_the_walks_own_ends_at_a_revision_off_the_line() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let metadata = branch_metadata(&stores, branch()).await;
                let line = remote_line(5, metadata).await;
                let stray = remote_revision(3, Hash::default(), metadata).await;
                let mut items = span(&line, 5, 4);
                items.push((3, stray.signature));
                let remote = Arc::new(
                    PagedRemote::new(branch(), line[4].signature)
                        .holding(&line)
                        .holding([&stray])
                        .page(signature_of(&line, 5), items, Hash::default())
                        .page(signature_of(&line, 3), span(&line, 3, 1), Hash::default()),
                );
                let client = client_on(&stores, remote.clone());
                let mut pager = LinePager::new(client.clone(), permits());

                let walked = walk(&mut pager, &client, line[4].signature, 0).await;
                pager.finish().await;

                assert_eq!(walked, vec![5, 4, 3, 2, 1]);
                assert_eq!(stored_locally(&client, &[stray.signature]).await, [false]);
                assert_eq!(
                    anchors(&remote).0,
                    [signature_of(&line, 5), signature_of(&line, 3)]
                );
            })
            .await;
    }

    /// A page of the walk's own that lists a revision without its own state below the one asked
    /// for stores none of the page and stops the pager paging.
    #[tokio::test]
    async fn a_page_of_the_walks_own_listing_a_state_not_its_own_stores_nothing() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let line = remote_line(5, Hash::default()).await;
                let other = remote_revision(4, Hash::default(), Hash::from([0xB0; 32])).await;
                for (kind, listed) in without_its_own_state(&line[3], &other) {
                    let stores = client_stores().await;
                    let held: Vec<RemoteRevision> = line
                        .iter()
                        .filter(|revision| revision.signature != line[3].signature)
                        .cloned()
                        .chain(listed)
                        .collect();
                    let remote = PagedRemote::new(branch(), line[4].signature)
                        .holding(&held)
                        .page(signature_of(&line, 5), span(&line, 5, 3), Hash::default());
                    let client = client_on(&stores, Arc::new(remote));
                    let mut pager = LinePager::new(client.clone(), permits());

                    pager.page_in(line[4].signature, u64::MAX, 0).await;

                    assert!(!pager.paging(), "{kind}: the pager stops paging");
                    assert_eq!(
                        stored_locally(&client, &[line[4].signature]).await,
                        [false],
                        "{kind}: the page stores nothing"
                    );
                }
            })
            .await;
    }

    /// A remote that answers a revision number the line skips with the page holding the line's
    /// nearest revision below it, as Lore Server does, lists the line ahead from there: the walk
    /// reads no page by hash signature below its own two.
    #[tokio::test]
    async fn pages_ahead_list_the_line_from_a_page_answered_for_a_number_the_line_skips() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let line = skipping_line(branch_metadata(&stores, branch()).await, 640).await;
                let mut remote = PagedRemote::new(branch(), line[999].signature)
                    .holding(&line)
                    .page(
                        signature_of(&line, 1000),
                        span(&line, 1000, 901),
                        Hash::default(),
                    )
                    .page(
                        signature_of(&line, 900),
                        span(&line, 900, 805),
                        line[639].signature,
                    );
                for skipped in [804, 800, 700] {
                    remote = remote.page(
                        Anchor::Number(skipped),
                        span(&line, 640, 601),
                        line[599].signature,
                    );
                }
                for top in (100..=600).rev().step_by(100) {
                    remote = remote.page(
                        Anchor::Number(top),
                        span(&line, top, top - 99),
                        Hash::default(),
                    );
                }
                let remote = Arc::new(remote);
                let client = client_on(&stores, remote.clone());
                let mut pager = LinePager::new(client.clone(), permits());

                let walked = walk(&mut pager, &client, line[999].signature, 0).await;
                pager.finish().await;

                let line_numbers: Vec<u64> = (805..=1000).rev().chain((1..=640).rev()).collect();
                assert_eq!(walked, line_numbers);
                assert_eq!(
                    anchors(&remote).0,
                    [signature_of(&line, 1000), signature_of(&line, 900)]
                );
            })
            .await;
    }

    /// A page ahead that lists the line only part way down to the next page, as where the remote
    /// fails the request for the rest, leaves the walk to wait for every page requested and read
    /// its own page below the part listed. The pages go on below the last page requested: none is
    /// requested twice.
    #[tokio::test]
    async fn pages_ahead_go_on_below_a_page_that_lists_the_line_part_way() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let line = remote_line(1000, branch_metadata(&stores, branch()).await).await;
                let mut remote = PagedRemote::new(branch(), line[999].signature)
                    .holding(&line)
                    .page(
                        signature_of(&line, 1000),
                        span(&line, 1000, 901),
                        Hash::default(),
                    )
                    .page(
                        signature_of(&line, 900),
                        span(&line, 900, 801),
                        Hash::default(),
                    )
                    .page(Anchor::Number(700), span(&line, 700, 651), Hash::default())
                    .page(
                        signature_of(&line, 650),
                        span(&line, 650, 601),
                        Hash::default(),
                    );
                for top in [800, 600, 500, 400, 300, 200, 100] {
                    remote = remote.page(
                        Anchor::Number(top),
                        span(&line, top, top - 99),
                        Hash::default(),
                    );
                }
                let remote = Arc::new(remote);
                let client = client_on(&stores, remote.clone());
                let mut pager = LinePager::new(client.clone(), permits());

                let walked = walk(&mut pager, &client, line[999].signature, 0).await;
                pager.finish().await;

                assert_eq!(walked, (1..=1000).rev().collect::<Vec<u64>>());
                assert_eq!(
                    anchors(&remote),
                    (
                        vec![
                            signature_of(&line, 1000),
                            signature_of(&line, 900),
                            signature_of(&line, 650)
                        ],
                        vec![800, 700, 650, 600, 500, 400, 300, 200, 100],
                    )
                );
            })
            .await;
    }

    /// Where the last page requested ahead lists nothing, and the walk's own page read in its place
    /// ends above the bottom of that page's numbers, the pages go on below the walk's page: the
    /// rest of the numbers of the page that listed nothing is requested ahead, not read by hash
    /// signature.
    #[tokio::test]
    async fn the_rest_of_a_page_ahead_that_listed_nothing_is_requested_ahead() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let line = remote_line(1000, branch_metadata(&stores, branch()).await).await;
                let remote = Arc::new(
                    PagedRemote::new(branch(), line[999].signature)
                        .holding(&line)
                        .page(
                            signature_of(&line, 1000),
                            span(&line, 1000, 901),
                            Hash::default(),
                        )
                        .page(
                            signature_of(&line, 900),
                            span(&line, 900, 801),
                            Hash::default(),
                        )
                        .page(Anchor::Number(800), span(&line, 800, 701), Hash::default())
                        .page(Anchor::Number(700), span(&line, 700, 601), Hash::default())
                        .page(
                            signature_of(&line, 600),
                            span(&line, 600, 551),
                            Hash::default(),
                        )
                        .page(Anchor::Number(550), span(&line, 550, 501), Hash::default()),
                );
                let client = client_on(&stores, remote.clone());
                let mut pager = LinePager::new(client.clone(), permits());

                let walked = walk(&mut pager, &client, line[999].signature, 501).await;
                pager.finish().await;

                assert_eq!(walked.last(), Some(&501));
                assert_eq!(
                    anchors(&remote),
                    (
                        vec![
                            signature_of(&line, 1000),
                            signature_of(&line, 900),
                            signature_of(&line, 600)
                        ],
                        vec![800, 700, 600, 550],
                    )
                );
            })
            .await;
    }

    /// Where two pages of one window list the line only part way down to the next page, the walk
    /// reads its own page below each part listed, and each time the pages go on below the last
    /// page requested: none is requested twice, and the walk reads no other page of its own.
    #[tokio::test]
    async fn pages_ahead_go_on_below_two_pages_of_a_window_that_list_the_line_part_way() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let line = remote_line(1500, branch_metadata(&stores, branch()).await).await;
                let mut remote = PagedRemote::new(branch(), line[1499].signature)
                    .holding(&line)
                    .page(
                        signature_of(&line, 1500),
                        span(&line, 1500, 1401),
                        Hash::default(),
                    )
                    .page(
                        signature_of(&line, 1400),
                        span(&line, 1400, 1301),
                        Hash::default(),
                    )
                    .page(
                        Anchor::Number(1200),
                        span(&line, 1200, 1151),
                        Hash::default(),
                    )
                    .page(
                        Anchor::Number(1000),
                        span(&line, 1000, 951),
                        Hash::default(),
                    )
                    .page(
                        signature_of(&line, 1150),
                        span(&line, 1150, 1101),
                        Hash::default(),
                    )
                    .page(
                        signature_of(&line, 950),
                        span(&line, 950, 901),
                        Hash::default(),
                    );
                for top in [1300, 1100, 900, 800, 700, 600, 500, 400, 300, 200, 100] {
                    remote = remote.page(
                        Anchor::Number(top),
                        span(&line, top, top - 99),
                        Hash::default(),
                    );
                }
                let remote = Arc::new(remote);
                let client = client_on(&stores, remote.clone());
                let mut pager = LinePager::new(client.clone(), permits());

                let walked = walk(&mut pager, &client, line[1499].signature, 0).await;
                pager.finish().await;

                assert_eq!(walked, (1..=1500).rev().collect::<Vec<u64>>());
                let (signatures, numbers) = anchors(&remote);
                assert_eq!(numbers_of(&line, &signatures), [1500, 1400, 1150, 950]);
                assert_eq!(
                    numbers,
                    [
                        1300, 1200, 1150, 1100, 1000, 950, 900, 800, 700, 600, 500, 400, 300, 200,
                        100
                    ]
                );
            })
            .await;
    }

    /// The pages requested run ahead of where the walk reads, not of where the pages land: a walk
    /// that reads into the second page requested ahead has one more page requested than the first
    /// window, though every page lands at once.
    #[tokio::test]
    async fn pages_ahead_follow_the_walk() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let line = remote_line(1000, branch_metadata(&stores, branch()).await).await;
                let remote = Arc::new(paging_remote(branch(), &line));
                let client = client_on(&stores, remote.clone());
                let mut pager = LinePager::new(client.clone(), permits());

                let walked = walk_until(&mut pager, &client, line[999].signature, 0, 700).await;
                for _ in 0..16 {
                    tokio::task::yield_now().await;
                }

                assert_eq!(walked.last(), Some(&700));
                assert_eq!(anchors(&remote).1, [800, 700, 600, 500, 400]);
                pager.finish().await;
            })
            .await;
    }

    /// Pages are requested ahead down to the first that lists the floor, and each stores the line
    /// down to it, and none below.
    #[tokio::test]
    async fn pages_ahead_stop_at_the_floor() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let line = remote_line(1000, branch_metadata(&stores, branch()).await).await;
                let remote = Arc::new(paging_remote(branch(), &line));
                let client = client_on(&stores, remote.clone());
                let mut pager = LinePager::new(client.clone(), permits());

                let walked = walk(&mut pager, &client, line[999].signature, 450).await;
                pager.finish().await;

                assert_eq!(walked.last(), Some(&450));
                assert_eq!(anchors(&remote).1, [800, 700, 600, 500]);
                assert_eq!(
                    stored_locally(&client, &[line[449].signature, line[448].signature]).await,
                    [true, false]
                );
            })
            .await;
    }

    /// A walk that ends within its first two pages requests no page ahead, and takes no permit to.
    #[tokio::test]
    async fn a_short_walk_requests_no_pages_ahead() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                for floor in [950, 850, 801] {
                    let stores = client_stores().await;
                    let line = remote_line(1000, branch_metadata(&stores, branch()).await).await;
                    let remote = Arc::new(paging_remote(branch(), &line));
                    let client = client_on(&stores, remote.clone());
                    let lines_ahead = permits();
                    let mut pager = LinePager::new(client.clone(), lines_ahead.clone());

                    walk(&mut pager, &client, line[999].signature, floor).await;

                    assert_eq!(anchors(&remote).1, [] as [u64; 0], "floor {floor}");
                    assert_eq!(
                        lines_ahead.available_permits(),
                        1,
                        "floor {floor}: the pager took no permit"
                    );
                    pager.finish().await;
                }
            })
            .await;
    }

    /// A pager keeps at most [`PAGES_AHEAD`] pages requested ahead of its walk, and dropping it
    /// ends their requests.
    #[tokio::test]
    async fn at_most_the_window_of_pages_is_requested_ahead() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let line = remote_line(1000, branch_metadata(&stores, branch()).await).await;
                let mut remote = paging_remote(branch(), &line);
                for number in (100..=800).step_by(100) {
                    remote = remote.stalling(Anchor::Number(number));
                }
                let remote = Arc::new(remote);
                let client = client_on(&stores, remote.clone());
                let mut pager = LinePager::new(client.clone(), permits());

                pager.page_in(line[999].signature, u64::MAX, 0).await;
                pager.page_in(line[899].signature, 901, 0).await;
                remote.unanswered(PAGES_AHEAD).await;
                tokio::task::yield_now().await;

                assert_eq!(anchors(&remote).1, [800, 700, 600, 500]);
                drop(pager);
                let ended = tokio::time::timeout(Duration::from_secs(5), remote.all_abandoned());
                assert!(ended.await.is_ok(), "dropping the pager ends its requests");
            })
            .await;
    }

    /// Finishing a pager ends the requests of its pages before it returns.
    #[tokio::test]
    async fn finishing_a_pager_ends_its_pages() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let line = remote_line(1000, branch_metadata(&stores, branch()).await).await;
                let mut remote = paging_remote(branch(), &line);
                for number in (100..=800).step_by(100) {
                    remote = remote.stalling(Anchor::Number(number));
                }
                let remote = Arc::new(remote);
                let client = client_on(&stores, remote.clone());
                let lines_ahead = permits();
                let mut pager = LinePager::new(client.clone(), lines_ahead.clone());
                pager.page_in(line[999].signature, u64::MAX, 0).await;
                pager.page_in(line[899].signature, 901, 0).await;
                remote.unanswered(PAGES_AHEAD).await;

                pager.finish().await;

                assert_eq!(remote.lists_unanswered(), 0);
                assert_eq!(
                    lines_ahead.available_permits(),
                    1,
                    "the pager returns its permit"
                );
            })
            .await;
    }

    /// A page ahead that the remote fails stops the walk's wait at it. The walk reads the page it
    /// lacks by hash signature, and the pages after the failed one go on, since those above held
    /// what the walk asked for, so no page is requested twice.
    #[tokio::test]
    async fn a_page_ahead_that_lists_nothing_is_read_by_signature() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let line = remote_line(1000, branch_metadata(&stores, branch()).await).await;
                let remote = paging_remote(branch(), &line)
                    .failing(
                        Anchor::Number(700),
                        lore_transport::ProtocolError::from(lore_base::error::NotFound),
                    )
                    .page(
                        signature_of(&line, 700),
                        span(&line, 700, 601),
                        Hash::default(),
                    );
                let remote = Arc::new(remote);
                let client = client_on(&stores, remote.clone());
                let mut pager = LinePager::new(client.clone(), permits());

                let walked = walk(&mut pager, &client, line[999].signature, 0).await;
                pager.finish().await;

                assert_eq!(walked, (1..=1000).rev().collect::<Vec<u64>>());
                let (signatures, numbers) = anchors(&remote);
                assert_eq!(
                    signatures,
                    [
                        signature_of(&line, 1000),
                        signature_of(&line, 900),
                        signature_of(&line, 700)
                    ]
                );
                assert_eq!(numbers, [800, 700, 600, 500, 400, 300, 200, 100]);
            })
            .await;
    }

    /// Pages ahead that hold none of the revisions the walk asks for, as those of a line other
    /// than the walk's do, stop the pager requesting more, and the walk reads its line by hash
    /// signature.
    #[tokio::test]
    async fn pages_ahead_that_hold_nothing_asked_for_stop() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let metadata = branch_metadata(&stores, branch()).await;
                let line = remote_line(1000, metadata).await;
                let mut other = Vec::new();
                for number in 1..=1000 {
                    other.push(
                        remote_revision(number, Hash::default(), Hash::from([0xB0; 32])).await,
                    );
                }
                let mut remote = PagedRemote::new(branch(), line[999].signature)
                    .holding(&line)
                    .holding(&other);
                for top in (100..=1000).rev().step_by(100) {
                    remote = remote
                        .page(
                            signature_of(&line, top),
                            span(&line, top, top - 99),
                            Hash::default(),
                        )
                        .page(
                            Anchor::Number(top),
                            span(&other, top, top - 99),
                            Hash::default(),
                        );
                }
                let remote = Arc::new(remote);
                let client = client_on(&stores, remote.clone());
                let lines_ahead = permits();
                let mut pager = LinePager::new(client.clone(), lines_ahead.clone());

                let walked = walk(&mut pager, &client, line[999].signature, 0).await;
                pager.finish().await;

                assert_eq!(walked, (1..=1000).rev().collect::<Vec<u64>>());
                let (signatures, numbers) = anchors(&remote);
                assert_eq!(numbers, [800, 700, 600, 500]);
                assert_eq!(signatures.len(), 10, "every page is read by signature");
                assert_eq!(
                    lines_ahead.available_permits(),
                    1,
                    "the pager returns its permit"
                );
            })
            .await;
    }

    /// A page ahead stores the line only down to the first state that isn't its revision's own,
    /// whether that is the revision the page starts at or one below it. A page ahead that so lists
    /// none of the line stops the walk's wait at it, and the walk waits for no page after it.
    #[tokio::test]
    async fn a_page_ahead_stores_no_state_not_its_own() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let metadata = branch_metadata(&stores, branch()).await;
                let line = remote_line(1000, metadata).await;
                for unowned in [750, 800] {
                    let index = unowned as usize - 1;
                    let other =
                        remote_revision(unowned, Hash::default(), Hash::from([0xB0; 32])).await;
                    for (kind, listed) in without_its_own_state(&line[index], &other) {
                        let stores = client_stores().await;
                        branch_metadata(&stores, branch()).await;
                        let held: Vec<RemoteRevision> = line
                            .iter()
                            .filter(|revision| revision.signature != line[index].signature)
                            .cloned()
                            .chain(listed)
                            .collect();
                        let mut remote =
                            PagedRemote::new(branch(), line[999].signature).holding(&held);
                        remote = remote
                            .page(
                                signature_of(&line, 1000),
                                span(&line, 1000, 901),
                                Hash::default(),
                            )
                            .page(
                                signature_of(&line, 900),
                                span(&line, 900, 801),
                                Hash::default(),
                            );
                        for top in (100..=800).rev().step_by(100) {
                            remote = remote.page(
                                Anchor::Number(top),
                                span(&line, top, top - 99),
                                Hash::default(),
                            );
                        }
                        if unowned == 800 {
                            for top in [700, 600, 500] {
                                remote = remote.stalling(Anchor::Number(top));
                            }
                        }
                        let client = client_on(&stores, Arc::new(remote));
                        let mut pager = LinePager::new(client.clone(), permits());

                        let walked = tokio::time::timeout(
                            Duration::from_secs(5),
                            walk(&mut pager, &client, line[999].signature, 0),
                        )
                        .await
                        .expect("the walk waits for no page after one that lists none of the line");
                        pager.finish().await;

                        assert_eq!(
                            walked.last(),
                            Some(&(unowned + 1)),
                            "{kind} at {unowned}: the walk reads down to it"
                        );
                        assert_eq!(
                            stored_locally(
                                &client,
                                &[
                                    line[index + 1].signature,
                                    line[index].signature,
                                    line[index - 1].signature
                                ]
                            )
                            .await,
                            [true, false, false],
                            "{kind} at {unowned}"
                        );
                    }
                }
            })
            .await;
    }

    /// A pager that finds no permit free requests no page ahead, and reads its walk's pages by hash
    /// signature. A later pager takes the permit once the pager holding it finishes.
    #[tokio::test]
    async fn a_pager_without_a_permit_reads_its_own_pages() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let lines_ahead = permits();
                let mut holding = None;
                for expected in [8, 0, 8] {
                    let stores = client_stores().await;
                    let line = remote_line(1000, branch_metadata(&stores, branch()).await).await;
                    let remote = Arc::new(paging_remote(branch(), &line));
                    let client = client_on(&stores, remote.clone());
                    let mut pager = LinePager::new(client.clone(), lines_ahead.clone());

                    walk(&mut pager, &client, line[999].signature, 0).await;

                    assert_eq!(anchors(&remote).1.len(), expected);
                    match holding.take() {
                        None => holding = Some(pager),
                        Some(mut first) => {
                            first.finish().await;
                            pager.finish().await;
                        }
                    }
                }
            })
            .await;
    }

    /// Revisions numbered 1 to 1000 that only the remote holds, as [`remote_line`] gives them, but
    /// revision 805 has the revision numbered `below` as its first parent, as a merged revision
    /// numbered above its other parent does. The line skips the revision numbers between them,
    /// whose revisions are off it.
    async fn skipping_line(metadata: Hash, below: u64) -> Vec<RemoteRevision> {
        let mut line = remote_line(1000, metadata).await;
        for number in 805..=1000 {
            let parent = if number == 805 { below } else { number - 1 };
            line[number as usize - 1] =
                remote_revision(number, line[parent as usize - 1].signature, metadata).await;
        }
        line
    }

    /// Pages requested ahead from revision numbers the line skips list nothing. The walk reads the
    /// page below the skipped numbers by hash signature, which retires the pages it covers, and the
    /// pages below it go on, each requested once: after a page from the first window, where the
    /// skipped numbers end within it, or from the next page down, where they run past it.
    #[tokio::test]
    async fn pages_ahead_go_on_below_numbers_the_line_skips() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                for below in [640, 300] {
                    let stores = client_stores().await;
                    let line =
                        skipping_line(branch_metadata(&stores, branch()).await, below).await;
                    let bottom = (below - 1) / 100 * 100 + 1;
                    let mut remote = PagedRemote::new(branch(), line[999].signature)
                        .holding(&line)
                        .page(
                            signature_of(&line, 1000),
                            span(&line, 1000, 901),
                            Hash::default(),
                        )
                        .page(
                            signature_of(&line, 900),
                            span(&line, 900, 805),
                            line[below as usize - 1].signature,
                        )
                        .page(
                            signature_of(&line, below),
                            span(&line, below, bottom),
                            Hash::default(),
                        );
                    let tops: Vec<u64> = (100..bottom).rev().step_by(100).collect();
                    for &top in &tops {
                        remote = remote.page(
                            Anchor::Number(top),
                            span(&line, top, top - 99),
                            Hash::default(),
                        );
                    }
                    let remote = Arc::new(remote);
                    let client = client_on(&stores, remote.clone());
                    let mut pager = LinePager::new(client.clone(), permits());

                    let walked = walk(&mut pager, &client, line[999].signature, 0).await;
                    pager.finish().await;

                    let line_numbers: Vec<u64> =
                        (805..=1000).rev().chain((1..=below).rev()).collect();
                    assert_eq!(walked, line_numbers, "below {below}");
                    let (signatures, numbers) = anchors(&remote);
                    assert_eq!(
                        signatures,
                        [
                            signature_of(&line, 1000),
                            signature_of(&line, 900),
                            signature_of(&line, below)
                        ],
                        "below {below}"
                    );
                    assert_eq!(numbers.first(), Some(&804), "below {below}");
                    let (on_line, skipped): (Vec<u64>, Vec<u64>) =
                        numbers.iter().partition(|&&number| number <= below);
                    assert_eq!(
                        on_line, tops,
                        "below {below}: pages are requested ahead below the skipped numbers"
                    );
                    assert!(
                        skipped
                            .iter()
                            .all(|number| [804, 800, 700, 600].contains(number)),
                        "below {below}: no page is requested from a skipped number past the first window: {numbers:?}"
                    );
                    let mut once = numbers.clone();
                    once.dedup();
                    assert_eq!(once, numbers, "below {below}: no page is requested twice");
                }
            })
            .await;
    }

    /// Pages ahead that list nothing from revision numbers the walk's line holds stop the pager
    /// requesting more, as from a remote that lists no pages by number, and the walk reads its line
    /// by hash signature.
    #[tokio::test]
    async fn pages_ahead_that_list_nothing_the_line_holds_stop() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let line = remote_line(1000, branch_metadata(&stores, branch()).await).await;
                let mut remote = PagedRemote::new(branch(), line[999].signature).holding(&line);
                for top in (100..=1000).rev().step_by(100) {
                    remote = remote.page(
                        signature_of(&line, top),
                        span(&line, top, top - 99),
                        Hash::default(),
                    );
                }
                let remote = Arc::new(remote);
                let client = client_on(&stores, remote.clone());
                let lines_ahead = permits();
                let mut pager = LinePager::new(client.clone(), lines_ahead.clone());

                let walked = walk(&mut pager, &client, line[999].signature, 0).await;
                pager.finish().await;

                assert_eq!(walked, (1..=1000).rev().collect::<Vec<u64>>());
                let (signatures, numbers) = anchors(&remote);
                assert_eq!(signatures.len(), 10, "every page is read by signature");
                assert!(
                    numbers.iter().all(|&number| number >= 500),
                    "no page is requested past the first window: {numbers:?}"
                );
                assert_eq!(
                    lines_ahead.available_permits(),
                    1,
                    "the pager returns its permit"
                );
            })
            .await;
    }

    /// Pages ahead go on past a page the walk reads by hash signature where a page listed some of
    /// the line since the walk's last page of its own, and end at its next page where none has
    /// since.
    #[tokio::test]
    async fn pages_ahead_end_where_none_listed_the_line_since_the_walks_last_page() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let line = remote_line(1000, branch_metadata(&stores, branch()).await).await;
                let mut remote = paging_remote(branch(), &line);
                for top in [600, 500] {
                    remote = remote.failing(
                        Anchor::Number(top),
                        lore_transport::ProtocolError::from(lore_base::error::NotFound),
                    );
                }
                for top in (100..=600).rev().step_by(100) {
                    remote = remote.page(
                        signature_of(&line, top),
                        span(&line, top, top - 99),
                        Hash::default(),
                    );
                }
                let remote = Arc::new(remote);
                let client = client_on(&stores, remote.clone());
                let mut pager = LinePager::new(client.clone(), permits());

                let walked = walk(&mut pager, &client, line[999].signature, 0).await;
                pager.finish().await;

                assert_eq!(walked, (1..=1000).rev().collect::<Vec<u64>>());
                let (signatures, numbers) = anchors(&remote);
                assert_eq!(
                    signatures[..4],
                    [
                        signature_of(&line, 1000),
                        signature_of(&line, 900),
                        signature_of(&line, 600),
                        signature_of(&line, 500)
                    ]
                );
                assert_eq!(signatures.last(), Some(&signature_of(&line, 100)));
                assert!(
                    !numbers.contains(&100),
                    "the pages ahead end at the page from revision number 500: {numbers:?}"
                );
            })
            .await;
    }

    /// A walk whose line's metadata can't be read, or names no branch, requests no page ahead, and
    /// reads its line by hash signature.
    #[tokio::test]
    async fn a_line_without_a_known_branch_is_read_by_signature() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                for unreadable in [true, false] {
                    let stores = client_stores().await;
                    let metadata = if unreadable {
                        Hash::from([0xC0; 32])
                    } else {
                        Metadata::new()
                            .serialize_local(stores.writer.clone())
                            .await
                            .expect("writing the metadata")
                    };
                    let line = remote_line(1000, metadata).await;
                    let mut remote = PagedRemote::new(branch(), line[999].signature).holding(&line);
                    for top in (100..=1000).rev().step_by(100) {
                        remote = remote
                            .page(
                                signature_of(&line, top),
                                span(&line, top, top - 99),
                                Hash::default(),
                            )
                            .page(
                                Anchor::Number(top),
                                span(&line, top, top - 99),
                                Hash::default(),
                            );
                    }
                    let remote = Arc::new(remote);
                    let client = client_on(&stores, remote.clone());
                    let lines_ahead = permits();
                    let mut pager = LinePager::new(client.clone(), lines_ahead.clone());

                    let walked = walk(&mut pager, &client, line[999].signature, 0).await;

                    assert_eq!(walked.len(), 1000, "unreadable {unreadable}");
                    let (signatures, numbers) = anchors(&remote);
                    assert_eq!(signatures.len(), 10, "unreadable {unreadable}");
                    assert_eq!(numbers, [] as [u64; 0], "unreadable {unreadable}");
                    assert_eq!(
                        lines_ahead.available_permits(),
                        1,
                        "unreadable {unreadable}: the pager holds no permit"
                    );
                    pager.finish().await;
                }
            })
            .await;
    }

    /// A pager refused a permit takes one at a later page of its walk's own, once one is free, and
    /// requests the pages below that page ahead.
    #[tokio::test]
    async fn a_pager_takes_a_permit_freed_during_its_walk() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let line = remote_line(1000, branch_metadata(&stores, branch()).await).await;
                let remote = Arc::new(paging_remote(branch(), &line).page(
                    signature_of(&line, 800),
                    span(&line, 800, 701),
                    Hash::default(),
                ));
                let client = client_on(&stores, remote.clone());
                let lines_ahead = permits();
                let taken = lines_ahead
                    .clone()
                    .try_acquire_owned()
                    .expect("the only permit");
                let mut pager = LinePager::new(client.clone(), lines_ahead.clone());

                let above = walk_until(&mut pager, &client, line[999].signature, 0, 801).await;
                drop(taken);
                let below = walk(&mut pager, &client, line[799].signature, 0).await;
                pager.finish().await;

                assert_eq!((above.last(), below.last()), (Some(&801), Some(&1)));
                assert_eq!(
                    anchors(&remote),
                    (
                        vec![
                            signature_of(&line, 1000),
                            signature_of(&line, 900),
                            signature_of(&line, 800)
                        ],
                        vec![700, 600, 500, 400, 300, 200, 100],
                    )
                );
            })
            .await;
    }

    /// A page the remote lists neither by number nor by hash signature, while pages are requested
    /// ahead, stops the pager paging, and the pages requested have ended once it returns.
    #[tokio::test]
    async fn a_page_listed_neither_way_ends_the_pages_ahead() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let line = remote_line(1000, branch_metadata(&stores, branch()).await).await;
                let mut remote = paging_remote(branch(), &line).failing(
                    Anchor::Number(700),
                    lore_transport::ProtocolError::from(lore_base::error::NotFound),
                );
                for top in [600, 500, 400] {
                    remote = remote.stalling(Anchor::Number(top));
                }
                let remote = Arc::new(remote);
                let client = client_on(&stores, remote.clone());
                let lines_ahead = permits();
                let mut pager = LinePager::new(client.clone(), lines_ahead.clone());

                walk_until(&mut pager, &client, line[999].signature, 0, 701).await;
                remote.unanswered(2).await;
                pager.page_in(line[699].signature, 701, 0).await;

                assert!(!pager.paging());
                assert_eq!(remote.lists_unanswered(), 0, "the pages ahead have ended");
                assert_eq!(
                    lines_ahead.available_permits(),
                    1,
                    "the pager returns its permit"
                );
            })
            .await;
    }

    /// Of the pagers sharing [`lines_ahead`], only [`LINES_AHEAD`] request pages ahead at once,
    /// [`PAGES_AHEAD`] each, however many walks read long lines.
    #[tokio::test]
    async fn pagers_sharing_their_permits_request_pages_ahead_for_at_most_their_lines() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let lines_ahead = lines_ahead();
                let mut walks = Vec::new();
                for _ in 0..=LINES_AHEAD {
                    let stores = client_stores().await;
                    let line = remote_line(1000, branch_metadata(&stores, branch()).await).await;
                    let mut remote = paging_remote(branch(), &line);
                    for top in (100..=800).step_by(100) {
                        remote = remote.stalling(Anchor::Number(top));
                    }
                    let remote = Arc::new(remote);
                    let client = client_on(&stores, remote.clone());
                    let mut pager = LinePager::new(client.clone(), lines_ahead.clone());
                    pager.page_in(line[999].signature, u64::MAX, 0).await;
                    pager.page_in(line[899].signature, 901, 0).await;
                    walks.push((pager, remote));
                }
                for (_, remote) in &walks[..LINES_AHEAD] {
                    remote.unanswered(PAGES_AHEAD).await;
                }
                tokio::task::yield_now().await;

                let requested: Vec<usize> = walks
                    .iter()
                    .map(|(_, remote)| anchors(remote).1.len())
                    .collect();
                let mut expected = vec![PAGES_AHEAD; LINES_AHEAD];
                expected.push(0);
                assert_eq!(requested, expected);
                for (mut pager, _) in walks {
                    pager.finish().await;
                }
            })
            .await;
    }

    /// Five revisions numbered 1 to 5 that only the remote holds, and a remote holding them, which
    /// lists them whole from the newest, or refuses to.
    async fn short_line(refusing: bool) -> (Vec<RemoteRevision>, Arc<PagedRemote>) {
        let line = remote_line(5, Hash::from([0xA0; 32])).await;
        let newest = line[4].signature;
        let remote = PagedRemote::new(branch(), newest).holding(&line);
        let remote = if refusing {
            remote.failing(
                Anchor::Signature(newest),
                lore_transport::ProtocolError::from(lore_base::error::NotAuthorized),
            )
        } else {
            remote.page(
                Anchor::Signature(newest),
                span(&line, 5, 1),
                Hash::default(),
            )
        };
        (line, Arc::new(remote))
    }

    fn signatures(line: &[RemoteRevision]) -> Vec<Hash> {
        line.iter().map(|revision| revision.signature).collect()
    }

    /// The page from a revision the store lacks is read in one request and stored down to its
    /// first revision numbered at or below the floor, and none below that.
    #[tokio::test]
    async fn a_page_is_stored_down_to_the_floor() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let (line, remote) = short_line(false).await;
                let client = client_on(&stores, remote.clone());
                let mut pager = LinePager::new(client.clone(), permits());

                pager.page_in(line[4].signature, u64::MAX, 3).await;

                assert!(pager.paging());
                assert_eq!(remote.lists(), 1);
                assert_eq!(
                    stored_locally(&client, &signatures(&line)).await,
                    [false, false, true, true, true]
                );
            })
            .await;
    }

    /// A revision the store holds asks the remote for nothing.
    #[tokio::test]
    async fn a_revision_the_store_holds_asks_the_remote_nothing() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let (line, remote) = short_line(false).await;
                let local = write_line(&stores.writer, line[4].signature, 6, 1).await[0];
                let client = client_on(&stores, remote.clone());
                let mut pager = LinePager::new(client, permits());

                pager.page_in(local, u64::MAX, 3).await;

                assert!(pager.paging());
                assert_eq!(remote.lists(), 0);
            })
            .await;
    }

    /// A page the remote refuses stops the pager paging, so the walk stops asking.
    #[tokio::test]
    async fn a_refused_page_stops_the_pager_paging() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let stores = client_stores().await;
                let (line, remote) = short_line(true).await;
                let client = client_on(&stores, remote);
                let mut pager = LinePager::new(client.clone(), permits());

                pager.page_in(line[4].signature, u64::MAX, 3).await;

                assert!(!pager.paging());
                assert_eq!(
                    stored_locally(&client, &signatures(&line)).await,
                    [false; 5]
                );
            })
            .await;
    }

    /// An empty page stops the pager paging.
    #[tokio::test]
    async fn an_empty_page_stops_the_pager_paging() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let line = remote_line(5, Hash::from([0xA0; 32])).await;
                let newest = line[4].signature;
                let stores = client_stores().await;
                let remote = Arc::new(PagedRemote::new(branch(), newest).holding(&line).page(
                    Anchor::Signature(newest),
                    Vec::new(),
                    Hash::default(),
                ));
                let client = client_on(&stores, remote.clone());
                let mut pager = LinePager::new(client, permits());

                pager.page_in(newest, u64::MAX, 3).await;

                assert!(!pager.paging());
                assert_eq!(remote.lists(), 1);
            })
            .await;
    }

    /// A store that reports nothing held locally, as a server's does, without a remote, stops the
    /// pager paging without reading a payload, so a walk reads each revision from the store on its
    /// own.
    #[tokio::test]
    async fn a_store_without_a_remote_stops_the_pager_paging() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let (stores, counting) = counted_client_stores().await;
                let revision = write_line(&stores.writer, Hash::default(), 1, 1).await[0];
                counting.report_durable_only();
                counting.take_reads();
                let mut pager = LinePager::new(stores.writer.clone(), permits());

                pager.page_in(revision, u64::MAX, 0).await;

                assert!(!pager.paging());
                assert_eq!(counting.take_reads().payloads, 0);
            })
            .await;
    }
}
