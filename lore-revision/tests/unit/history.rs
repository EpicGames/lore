// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

mod find_branch_point {
    use std::sync::Arc;
    use std::time::Duration;

    use lore_base::error::Disconnected;
    use lore_base::error::NotAuthorized;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::BranchId;
    use lore_base::types::Hash;
    use lore_error_set::FfiError;
    use lore_revision::history::HistoryError;
    use lore_revision::history::find_branch_point;
    use lore_revision::repository::RepositoryContext;
    use lore_transport::ProtocolError;

    use crate::branch::null_repository;
    use crate::branch::write_branch_revision;
    use crate::branch::write_line;
    use crate::branch::write_not_a_state;
    use crate::helper::setup_test_execution;
    use crate::paged_remote::Anchor;
    use crate::paged_remote::BranchedFromRemoteLine;
    use crate::paged_remote::PagedRemote;
    use crate::paged_remote::Reads;
    use crate::paged_remote::RemoteRevision;
    use crate::paged_remote::branched_from_cached_segment;
    use crate::paged_remote::branched_from_remote_line;
    use crate::paged_remote::client_and_writer;
    use crate::paged_remote::client_on;
    use crate::paged_remote::client_stores;
    use crate::paged_remote::counted_client_stores;
    use crate::paged_remote::numbered;
    use crate::paged_remote::remote_revision;
    use crate::paged_remote::stored_locally;
    use crate::paged_remote::without_its_own_state;

    /// A revision only the remote holds, numbered `number` above `parent`. Its metadata sets it
    /// apart from a revision the tests write locally with the same parent and number.
    async fn remote_only(number: u64, parent: Hash) -> RemoteRevision {
        remote_revision(number, parent, Hash::from([0xA0; 32])).await
    }

    /// The walk from `left` and `right`, bounded by a second so that a walk that loops fails the
    /// test.
    async fn walk(
        repository: Arc<RepositoryContext>,
        left: Hash,
        right: Hash,
    ) -> Result<(Hash, Vec<Hash>, Vec<Hash>), HistoryError> {
        let walk = find_branch_point(repository, left, right);
        tokio::time::timeout(
            Duration::from_secs(1),
            LORE_CONTEXT.scope(setup_test_execution(), walk),
        )
        .await
        .expect("the walk ends within a second")
    }

    async fn branch_point_of(branched: &BranchedFromRemoteLine) -> (Hash, Vec<Hash>, Vec<Hash>) {
        walk(
            branched.client.clone(),
            branched.remote_line[5],
            branched.local,
        )
        .await
        .expect("both lines are readable")
    }

    /// A page the remote serves from a cached segment also holds revisions newer than the one
    /// it was asked for. Those are not on the line below it, so the branch point is still where
    /// the local line left the remote one. Each history holds a revision once, including the
    /// revision a side's local walk stopped at, which also starts the page asked for at it.
    #[tokio::test]
    async fn a_remote_page_starting_above_its_anchor_leaves_the_branch_point_where_the_lines_split()
    {
        let branched = branched_from_cached_segment().await;

        let (branch_point, remote_history, local_history) = branch_point_of(&branched).await;

        let remote_line = &branched.remote_line;
        assert_eq!(branch_point, remote_line[2]);
        assert_eq!(
            remote_history,
            vec![remote_line[5], remote_line[4], remote_line[3]]
        );
        assert_eq!(local_history, vec![branched.local]);
    }

    /// A page that does not hold the revision it was asked for says nothing about the line
    /// below that revision, so its cursor to an older page is not followed either.
    #[tokio::test]
    async fn a_remote_page_without_its_anchor_ends_that_side_of_the_search() {
        let stray = Hash::from([0xEE; 32]);
        let branched = branched_from_remote_line(|remote, remote_line| {
            remote
                .page(
                    Anchor::Signature(remote_line[2]),
                    numbered(remote_line, &[6]),
                    stray,
                )
                .page(
                    Anchor::Signature(stray),
                    vec![(9, stray), (5, remote_line[4]), (4, remote_line[3])],
                    Hash::default(),
                )
        })
        .await;

        let (branch_point, remote_history, local_history) = branch_point_of(&branched).await;

        let remote_line = &branched.remote_line;
        assert_eq!(branch_point, remote_line[2]);
        assert_eq!(
            remote_history,
            vec![remote_line[5], remote_line[4], remote_line[3]]
        );
        assert_eq!(local_history, vec![branched.local]);
    }

    /// A line that reaches the newest revision the lines share a round before the other line,
    /// while both reach older shared revisions in the later round, meets the other line at the
    /// newest one.
    #[tokio::test]
    async fn lines_reaching_their_newest_shared_revision_a_round_apart_meet_there() {
        let branched = branched_from_remote_line(|remote, remote_line| {
            remote
                .page(
                    Anchor::Signature(remote_line[2]),
                    numbered(remote_line, &[3]),
                    remote_line[1],
                )
                .page(
                    Anchor::Signature(remote_line[1]),
                    numbered(remote_line, &[2, 1]),
                    Hash::default(),
                )
        })
        .await;

        let (branch_point, remote_history, local_history) = branch_point_of(&branched).await;

        let remote_line = &branched.remote_line;
        assert_eq!(branch_point, remote_line[2]);
        assert_eq!(
            remote_history,
            vec![remote_line[5], remote_line[4], remote_line[3]]
        );
        assert_eq!(local_history, vec![branched.local]);
    }

    /// As [`lines_reaching_their_newest_shared_revision_a_round_apart_meet_there`], with both
    /// lines in the local store.
    #[tokio::test]
    async fn local_lines_reaching_their_newest_shared_revision_a_round_apart_meet_there() {
        let repository = null_repository().await;
        let shared = write_line(&repository, Hash::from([0xA1; 32]), 1, 20).await;
        let mut left = write_line(&repository, shared[19], 100, 12).await;
        let mut right = write_line(&repository, shared[19], 200, 8).await;

        let walked = walk(repository, left[11], right[7])
            .await
            .expect("both lines are local");

        left.reverse();
        right.reverse();
        assert_eq!(walked, (shared[19], left, right));
    }

    /// A side whose line the remote has ended is not asked for again while the other side walks
    /// the local store.
    #[tokio::test]
    async fn a_side_the_remote_ended_is_not_asked_for_again() {
        let root = remote_only(1, Hash::default()).await;
        let remote_root = root.signature;
        let remote = Arc::new(
            PagedRemote::new(BranchId::from([7; 16]), remote_root)
                .holding([&root])
                .page(
                    Anchor::Signature(remote_root),
                    vec![(1, remote_root)],
                    Hash::default(),
                ),
        );
        let (client, writer) = client_and_writer(remote.clone()).await;
        let base = Hash::from([0xB2; 32]);
        let mut line = write_line(&writer, base, 1, 25).await;

        let walked = walk(client, remote_root, line[24])
            .await
            .expect("both lines end");

        line.reverse();
        line.push(base);
        assert_eq!(walked, (Hash::default(), vec![remote_root], line));
        assert_eq!(remote.lists(), 2, "one list for each side's line");
    }

    /// The remote is asked once for a line whose list fails, however far the other line walks the
    /// store, and the walk fails with the remote's error, which keeps its code, rather than
    /// answering that the lines share nothing.
    #[tokio::test]
    async fn a_line_whose_list_fails_is_not_asked_for_again() {
        for failure in [
            ProtocolError::from(Disconnected),
            ProtocolError::from(NotAuthorized),
        ] {
            let top = Hash::from([0xC1; 32]);
            let remote = Arc::new(
                PagedRemote::new(BranchId::from([7; 16]), top)
                    .failing(Anchor::Signature(top), failure.clone()),
            );
            let (client, writer) = client_and_writer(remote.clone()).await;
            let line = write_line(&writer, Hash::default(), 1, 25).await;

            let error = walk(client, top, line[24])
                .await
                .expect_err("the remote does not list the line below top");

            assert_eq!(
                error.ffi_code(),
                failure.ffi_code(),
                "{failure:?}: {error:?}"
            );
            assert_eq!(remote.lists(), 1, "{failure:?}: one list for the line");
        }
    }

    /// A page served from a cached segment can end at the revision it was asked for. It adds
    /// nothing to the line but moves its cursor, and the walk goes on from the cursor.
    #[tokio::test]
    async fn a_page_that_adds_nothing_but_moves_the_line_keeps_walking() {
        let stores = client_stores().await;
        let shared = write_line(&stores.writer, Hash::default(), 1, 2).await;
        let listed = remote_only(3, shared[1]).await;
        let top = listed.signature;
        let remote = PagedRemote::new(BranchId::from([7; 16]), top)
            .holding([&listed])
            .page(Anchor::Signature(top), vec![(3, top)], shared[1]);
        let client = client_on(&stores, Arc::new(remote));

        let walked = walk(client, top, shared[1])
            .await
            .expect("the line goes on in the store from the page's cursor");

        assert_eq!(walked, (shared[1], vec![top], vec![]));
    }

    /// A page that does not move its line stops the line, and the walk fails naming the revision
    /// the line stopped at.
    #[tokio::test]
    async fn a_page_that_does_not_move_the_line_fails_the_walk_naming_its_revision() {
        let listed = remote_only(3, Hash::default()).await;
        let top = listed.signature;
        let remote = Arc::new(
            PagedRemote::new(BranchId::from([7; 16]), top)
                .holding([&listed])
                .page(Anchor::Signature(top), vec![(3, top)], top),
        );
        let (client, writer) = client_and_writer(remote).await;
        let line = write_line(&writer, Hash::default(), 1, 2).await;

        let error = walk(client, top, line[1])
            .await
            .expect_err("the remote's line stops at top");

        assert_eq!(
            error
                .as_revision_not_found()
                .map(|error| error.revision.clone()),
            Some(top.to_string()),
            "{error:?}"
        );
    }

    /// A page listing a revision numbered no lower than the one above it does not follow self
    /// parents, so the walk fails.
    #[tokio::test]
    async fn a_page_whose_numbers_do_not_decrease_fails_the_walk() {
        let below = remote_only(4, Hash::default()).await;
        let top = remote_only(3, below.signature).await;
        let remote = Arc::new(
            PagedRemote::new(BranchId::from([7; 16]), top.signature)
                .holding([&top, &below])
                .page(
                    Anchor::Signature(top.signature),
                    vec![(3, top.signature), (4, below.signature)],
                    Hash::default(),
                ),
        );
        let top = top.signature;
        let (client, writer) = client_and_writer(remote).await;
        let line = write_line(&writer, Hash::default(), 1, 2).await;

        let error = walk(client, top, line[1])
            .await
            .expect_err("the page's numbers rise from top to below");

        assert!(error.is_internal(), "{error:?}");
    }

    /// A remote whose pages lead back to a revision they already listed would have the line repeat
    /// without end, so the walk fails.
    #[tokio::test]
    async fn a_line_the_remote_returns_to_fails_the_walk() {
        let (client, line, top) = remote_line_returning_to_its_top().await;

        let error = walk(client, top, line[1])
            .await
            .expect_err("the remote's line returns to middle");

        assert!(error.is_internal(), "{error:?}");
    }

    /// As [`a_line_the_remote_returns_to_fails_the_walk`], with the remote's line on the right.
    #[tokio::test]
    async fn a_right_line_the_remote_returns_to_fails_the_walk() {
        let (client, line, top) = remote_line_returning_to_its_top().await;

        let error = walk(client, line[1], top)
            .await
            .expect_err("the remote's line returns to middle");

        assert!(error.is_internal(), "{error:?}");
    }

    /// A client whose remote's line runs from `top` through `middle` and back to `top`, page by
    /// page, with a local line of two revisions that shares nothing with it. Returns the client,
    /// the local line, oldest first, and `top`.
    async fn remote_line_returning_to_its_top() -> (Arc<RepositoryContext>, Vec<Hash>, Hash) {
        let older = remote_only(1, Hash::default()).await;
        let middle = remote_only(2, older.signature).await;
        let top = remote_only(3, middle.signature).await;
        let held = [&older, &middle, &top];
        let [older, middle, top] = [older.signature, middle.signature, top.signature];
        let remote = PagedRemote::new(BranchId::from([7; 16]), top)
            .holding(held)
            .page(Anchor::Signature(top), vec![(3, top), (2, middle)], middle)
            .page(
                Anchor::Signature(middle),
                vec![(2, middle), (1, older)],
                top,
            );
        let (client, writer) = client_and_writer(Arc::new(remote)).await;
        let line = write_line(&writer, Hash::default(), 1, 2).await;
        (client, line, top)
    }

    /// The cursor a page moves a line to can name a revision the store holds, and the walk follows
    /// the line on in the store from there.
    #[tokio::test]
    async fn a_line_continues_in_the_store_from_a_page_cursor_it_holds() {
        let stores = client_stores().await;
        let shared = write_line(&stores.writer, Hash::default(), 1, 3).await;
        let bottom = remote_only(8, shared[2]).await;
        let top = remote_only(9, bottom.signature).await;
        let [gap_top, gap_bottom] = [top.signature, bottom.signature];
        let left = write_line(&stores.writer, gap_top, 10, 1).await;
        let right = write_line(&stores.writer, shared[2], 20, 2).await;
        let remote = PagedRemote::new(BranchId::from([7; 16]), left[0])
            .holding([&top, &bottom])
            .page(
                Anchor::Signature(gap_top),
                vec![(9, gap_top), (8, gap_bottom)],
                shared[2],
            );
        let client = client_on(&stores, Arc::new(remote));

        let walked = walk(client, left[0], right[1])
            .await
            .expect("the lines meet in the store");

        assert_eq!(
            walked,
            (
                shared[2],
                vec![left[0], gap_top, gap_bottom],
                vec![right[1], right[0]]
            )
        );
    }

    /// With no remote to list what lies below a revision the store lacks, a side stops there,
    /// and the walk fails naming that revision once no side has anything left to walk.
    #[tokio::test]
    async fn walk_without_a_remote_fails_naming_the_missing_revision() {
        let repository = null_repository().await;
        let left_base = Hash::from([0xA1; 32]);
        let right_base = Hash::from([0xB2; 32]);
        let left = write_line(&repository, left_base, 1, 3).await;
        let right = write_line(&repository, right_base, 1, 2).await;

        let error = walk(repository, left[2], right[1])
            .await
            .expect_err("the lines share no revision the store holds");

        assert_eq!(
            error
                .as_revision_not_found()
                .map(|error| error.revision.clone()),
            Some(left_base.to_string()),
            "{error:?}"
        );
    }

    /// A walk whose left line ends at the root while the right line stops at a revision the store
    /// lacks fails naming the right line's revision.
    #[tokio::test]
    async fn walk_without_a_remote_names_the_right_line_where_only_it_stops() {
        let repository = null_repository().await;
        let right_base = Hash::from([0xB2; 32]);
        let left = write_line(&repository, Hash::default(), 1, 2).await;
        let right = write_line(&repository, right_base, 10, 2).await;

        let error = walk(repository, left[1], right[1])
            .await
            .expect_err("the right line stops at a revision the store lacks");

        assert_eq!(
            error
                .as_revision_not_found()
                .map(|error| error.revision.clone()),
            Some(right_base.to_string()),
            "{error:?}"
        );
    }

    /// A walk that reaches a blob that is not a revision state in the local store fails with an
    /// internal error, whether a latest or a parent names it, rather than reading the blob as a
    /// number and a parent.
    #[tokio::test]
    async fn walk_fails_on_a_blob_that_is_not_a_state() {
        let repository = null_repository().await;
        let not_a_state = LORE_CONTEXT
            .scope(setup_test_execution(), write_not_a_state(&repository))
            .await;
        let left = write_line(&repository, not_a_state, 5, 1).await[0];
        let right = write_line(&repository, Hash::default(), 1, 2).await;

        for (named_by, latest) in [("a latest", not_a_state), ("a parent", left)] {
            let walked = walk(repository.clone(), latest, right[1]).await;

            assert!(
                walked.as_ref().is_err_and(|error| error.is_internal()),
                "named by {named_by}: {walked:?}"
            );
        }
    }

    /// A side that stops early leaves the other walking, and the walk answers the revision where
    /// the other side reaches the stopped side's line.
    #[tokio::test]
    async fn walk_finds_the_shared_revision_past_a_side_that_stops_early() {
        let repository = null_repository().await;
        let mut line = write_line(&repository, Hash::from([0xA1; 32]), 1, 26).await;
        let stopped = line[0];

        let walked = walk(repository, line[25], stopped)
            .await
            .expect("the walk reaches the stopped side's line");

        line.reverse();
        assert_eq!(walked, (stopped, line[..25].to_vec(), vec![]));
    }

    /// A line whose request fails stops at the revision the store lacks, which it holds
    /// unnumbered. The other line's page lists that revision with its number, and the lines meet
    /// there.
    #[tokio::test]
    async fn a_stopped_line_meets_the_other_at_its_unnumbered_revision() {
        let older = remote_only(3, Hash::default()).await;
        let gap = remote_only(4, older.signature).await;
        let sibling = remote_only(5, gap.signature).await;
        let top = remote_only(6, sibling.signature).await;
        let held = [&older, &gap, &sibling, &top];
        let [older, gap, sibling, top] = [
            older.signature,
            gap.signature,
            sibling.signature,
            top.signature,
        ];
        let remote = PagedRemote::new(BranchId::from([7; 16]), top)
            .holding(held)
            .page(
                Anchor::Signature(top),
                vec![(6, top), (5, sibling), (4, gap), (3, older)],
                Hash::default(),
            )
            .failing(Anchor::Signature(gap), ProtocolError::from(Disconnected));
        let (client, writer) = client_and_writer(Arc::new(remote)).await;
        let left = write_line(&writer, gap, 5, 1).await;

        let walked = walk(client, left[0], top)
            .await
            .expect("the right line's page lists the revision the left line stopped at");

        assert_eq!(walked, (gap, vec![left[0]], vec![top, sibling]));
    }

    /// A page item's number is the server's claim, and the state it carries is the revision's own,
    /// so the walk numbers a listed revision from its state: here 4, above the shared revision,
    /// where the item claims 2.
    #[tokio::test]
    async fn the_walk_numbers_a_listed_revision_from_its_state() {
        let stores = client_stores().await;
        let shared = write_line(&stores.writer, Hash::default(), 1, 3).await;
        let right = write_line(&stores.writer, shared[2], 4, 1).await;
        let top = remote_only(4, shared[2]).await;
        let remote = PagedRemote::new(BranchId::from([7; 16]), top.signature)
            .holding([&top])
            .page(
                Anchor::Signature(top.signature),
                vec![(2, top.signature)],
                shared[2],
            );
        let client = client_on(&stores, Arc::new(remote));

        let walked = walk(client, top.signature, right[0])
            .await
            .expect("top's state numbers it above the shared revision");

        assert_eq!(walked, (shared[2], vec![top.signature], vec![right[0]]));
    }

    /// A page that lists a revision on a line without its own state fails the walk, whether the
    /// state is missing, cut short, or another revision's, and whether the revision is the one the
    /// line asked for or one below it. The lines meet below the page the left line asks for, at the
    /// revision the right line asks for, so the walk needs the page. Another revision's state
    /// numbers the revision in order on the line, so only the state check fails the walk.
    #[tokio::test]
    async fn a_page_listing_a_revision_without_its_own_state_fails_the_walk() {
        let deeper = remote_only(3, Hash::default()).await;
        let below = remote_only(4, deeper.signature).await;
        let other_at_4 = remote_only(4, Hash::default()).await;
        let other_at_2 = remote_only(2, Hash::default()).await;
        for (position, unverified, verified, other) in [
            ("the revision asked for", &below, &deeper, &other_at_4),
            ("a revision below it", &deeper, &below, &other_at_2),
        ] {
            for (kind, listed) in without_its_own_state(unverified, other) {
                let stores = client_stores().await;
                let left = write_line(&stores.writer, below.signature, 5, 1).await[0];
                let right = write_branch_revision(&stores.writer, deeper.signature, 4, 1).await;
                let remote = PagedRemote::new(BranchId::from([7; 16]), right)
                    .holding(listed.iter().chain([verified]))
                    .page(
                        Anchor::Signature(below.signature),
                        vec![(4, below.signature), (3, deeper.signature)],
                        Hash::default(),
                    )
                    .page(
                        Anchor::Signature(deeper.signature),
                        vec![(3, deeper.signature)],
                        Hash::default(),
                    );
                let client = client_on(&stores, Arc::new(remote));

                let walked = walk(client, left, right).await;

                assert!(
                    walked.as_ref().is_err_and(|error| error.is_internal()),
                    "{kind} state for {position}: {walked:?}"
                );
            }
        }
    }

    /// Lines that both reach the same revision the store lacks, in the same round, meet there
    /// without a remote request: no revision above it is on both lines.
    #[tokio::test]
    async fn lines_that_reach_the_same_missing_revision_meet_there_without_asking_the_remote() {
        let stores = client_stores().await;
        let below = remote_only(4, Hash::default()).await;
        let left = write_line(&stores.writer, below.signature, 5, 1).await[0];
        let right = write_branch_revision(&stores.writer, below.signature, 5, 1).await;
        let remote = Arc::new(
            PagedRemote::new(BranchId::from([7; 16]), right)
                .holding([&below])
                .page(
                    Anchor::Signature(below.signature),
                    vec![(4, below.signature)],
                    Hash::default(),
                ),
        );
        let client = client_on(&stores, remote.clone());

        let walked = walk(client, left, right)
            .await
            .expect("the lines meet at the revision both reach");

        assert_eq!(walked, (below.signature, vec![left], vec![right]));
        assert_eq!(remote.lists(), 0);
    }

    /// Lines that meet among the revisions the store holds answer the walk without a remote
    /// request, though both reach a revision the store lacks in the round they meet.
    #[tokio::test]
    async fn lines_that_meet_in_the_store_answer_without_asking_the_remote() {
        let stores = client_stores().await;
        let below = remote_only(4, Hash::default()).await;
        let left = write_line(&stores.writer, below.signature, 5, 1).await[0];
        let right = write_line(&stores.writer, left, 6, 1).await[0];
        let remote = Arc::new(
            PagedRemote::new(BranchId::from([7; 16]), right)
                .holding([&below])
                .page(
                    Anchor::Signature(below.signature),
                    vec![(4, below.signature)],
                    Hash::default(),
                ),
        );
        let client = client_on(&stores, remote.clone());

        let walked = walk(client, left, right)
            .await
            .expect("the lines meet at the left line's latest");

        assert_eq!(walked, (left, vec![], vec![right]));
        assert_eq!(remote.lists(), 0);
    }

    /// A line that reaches a revision the store lacks waits on the remote's page to number it, so
    /// the other line is not taken past the revisions below it before the page does: here the right
    /// line runs from revision 4 to its root in the store, all below the left line's latest, and
    /// meets the left line at 3 once the page numbers the left line's revision 4.
    #[tokio::test]
    async fn a_line_waiting_on_the_remote_meets_the_other_below_the_revision_it_waits_on() {
        let stores = client_stores().await;
        let shared = write_line(&stores.writer, Hash::default(), 1, 3).await;
        let gap = remote_only(4, shared[2]).await;
        let left = write_line(&stores.writer, gap.signature, 5, 1).await[0];
        let right = write_branch_revision(&stores.writer, shared[2], 4, 1).await;
        let remote = PagedRemote::new(BranchId::from([7; 16]), right)
            .holding([&gap])
            .page(
                Anchor::Signature(gap.signature),
                vec![(4, gap.signature)],
                shared[2],
            );
        let client = client_on(&stores, Arc::new(remote));

        let walked = walk(client, left, right)
            .await
            .expect("the lines meet at the shared revision");

        assert_eq!(walked, (shared[2], vec![left, gap.signature], vec![right]));
    }

    /// The walk caches the states the remote's pages carry in the local immutable store, in a
    /// context that cannot write the mutable store: those of the revisions on the lines, and those
    /// a page lists above the revision asked for. The right line's page lists revision 4 above its
    /// anchor, so the left line goes on from 4 in the store, and the walk lists two pages.
    #[tokio::test]
    async fn a_walk_without_a_write_token_caches_the_listed_states_locally() {
        let branched = branched_from_cached_segment().await;
        assert!(branched.client.try_write_token().is_none());
        let stored = || stored_locally(&branched.client, &branched.remote_line);
        let before = LORE_CONTEXT.scope(setup_test_execution(), stored()).await;

        branch_point_of(&branched).await;

        let after = LORE_CONTEXT.scope(setup_test_execution(), stored()).await;
        assert_eq!(before, [false; 6]);
        assert_eq!(after, [true; 6]);
        assert_eq!(branched.remote.lists(), 2);
    }

    /// A round that asks the remote for both lines requests the right line's page in a task of its
    /// own, which dropping the walk aborts, so the request does not outlive the walk. Here the left
    /// line's page answers and the right line's is never answered.
    #[tokio::test]
    async fn dropping_the_walk_aborts_the_right_lines_request() {
        let stores = client_stores().await;
        let left_gap = remote_only(4, Hash::default()).await;
        let right_gap = remote_only(3, Hash::default()).await;
        let left = write_line(&stores.writer, left_gap.signature, 5, 1).await[0];
        let right = write_line(&stores.writer, right_gap.signature, 4, 1).await[0];
        let remote = Arc::new(
            PagedRemote::new(BranchId::from([7; 16]), left)
                .holding([&left_gap])
                .page(
                    Anchor::Signature(left_gap.signature),
                    vec![(4, left_gap.signature)],
                    Hash::default(),
                )
                .stalling(Anchor::Signature(right_gap.signature)),
        );
        let walk = find_branch_point(client_on(&stores, remote.clone()), left, right);

        let dropped = LORE_CONTEXT
            .scope(setup_test_execution(), async {
                tokio::select! {
                    walked = walk => panic!("the right line's page is never listed: {walked:?}"),
                    () = remote.stalled() => {}
                }
                tokio::time::timeout(Duration::from_secs(1), remote.abandoned()).await
            })
            .await;

        assert!(
            dropped.is_ok(),
            "the right line's request is dropped with the walk"
        );
    }

    /// A round walks only the line whose oldest revision has the higher number, so the line
    /// below it is not asked for a page the lines do not need: the left line meets the right
    /// one's first page before the right line's next page would be needed.
    #[tokio::test]
    async fn a_line_below_the_other_is_not_asked_for_pages_it_does_not_need() {
        let stores = client_stores().await;
        let below = remote_only(3, Hash::default()).await;
        let shared = write_line(&stores.writer, below.signature, 4, 1).await[0];
        let listed_shared = remote_revision(4, below.signature, Hash::default()).await;
        assert_eq!(listed_shared.signature, shared);
        let mut left = write_line(&stores.writer, shared, 5, 19).await;
        let top = remote_only(5, shared).await;
        let remote = Arc::new(
            PagedRemote::new(BranchId::from([7; 16]), top.signature)
                .holding([&top, &listed_shared, &below])
                .page(
                    Anchor::Signature(top.signature),
                    vec![(5, top.signature), (4, shared)],
                    below.signature,
                )
                .page(
                    Anchor::Signature(below.signature),
                    vec![(3, below.signature)],
                    Hash::default(),
                ),
        );
        let client = client_on(&stores, remote.clone());

        let walked = walk(client, left[18], top.signature)
            .await
            .expect("the lines meet at the shared revision");

        left.reverse();
        assert_eq!(walked, (shared, left, vec![top.signature]));
        assert_eq!(
            remote.lists(),
            1,
            "one list, for the right line's first page"
        );
    }

    /// A round walks only the line whose oldest revision has the higher number, so the store is
    /// read for the line below only once the lines need it: 30 revisions of the left line and 10
    /// of the right line, which the first round walks with the left.
    #[tokio::test]
    async fn a_line_below_the_other_is_not_read_before_the_lines_need_it() {
        let (stores, counting) = counted_client_stores().await;
        let shared = write_line(&stores.writer, Hash::default(), 1, 30).await;
        let mut left = write_line(&stores.writer, shared[29], 100, 25).await;
        let right = write_line(&stores.writer, shared[29], 31, 1).await;
        counting.take_reads();

        let walked = walk(stores.writer.clone(), left[24], right[0])
            .await
            .expect("both lines are local");

        left.reverse();
        assert_eq!(walked, (shared[29], left, right));
        assert_eq!(
            counting.take_reads(),
            Reads {
                payloads: 40,
                queries: 0
            },
            "a state read for each of the 40 revisions walked"
        );
    }

    /// Siblings at equal numbers are not shared, so both lines pass them and meet at the shared
    /// revision below.
    #[tokio::test]
    async fn lines_pass_siblings_at_equal_numbers_to_the_shared_revision_below() {
        let repository = null_repository().await;
        let shared = write_line(&repository, Hash::default(), 1, 3).await;
        let left_sibling = write_branch_revision(&repository, shared[2], 4, 1).await;
        let right_sibling = write_branch_revision(&repository, shared[2], 4, 2).await;
        let left = write_line(&repository, left_sibling, 5, 2).await;
        let right = write_line(&repository, right_sibling, 5, 1).await;

        let walked = walk(repository, left[1], right[0])
            .await
            .expect("both lines are local");

        assert_eq!(
            walked,
            (
                shared[2],
                vec![left[1], left[0], left_sibling],
                vec![right[0], right_sibling]
            )
        );
    }

    /// A walk from a zero left latest, as push runs for a branch the remote has no latest for,
    /// walks the right line to its root, past the ten revisions of the first round.
    #[tokio::test]
    async fn a_walk_from_a_zero_latest_walks_the_other_line_to_its_root() {
        let repository = null_repository().await;
        let mut right = write_line(&repository, Hash::default(), 1, 25).await;

        let walked = walk(repository, Hash::default(), right[24])
            .await
            .expect("the right line is local");

        right.reverse();
        assert_eq!(walked, (Hash::default(), vec![], right));
    }

    /// A line that ends above an unrelated, longer line leaves nothing to meet, and the walk
    /// still walks the other line to its root for its history.
    #[tokio::test]
    async fn a_line_that_ends_above_the_other_leaves_it_walked_to_its_root() {
        let repository = null_repository().await;
        let mut left = write_line(&repository, Hash::default(), 100, 2).await;
        let mut right = write_line(&repository, Hash::default(), 1, 25).await;

        let walked = walk(repository, left[1], right[24])
            .await
            .expect("both lines are local");

        left.reverse();
        right.reverse();
        assert_eq!(walked, (Hash::default(), left, right));
    }

    /// Two lines leaving one local line meet at its newest revision.
    #[tokio::test]
    async fn walk_finds_the_shared_revision_when_both_sides_are_local() {
        let repository = null_repository().await;
        let shared = write_line(&repository, Hash::from([0xA1; 32]), 1, 3).await;
        let mut left = write_line(&repository, shared[2], 10, 2).await;
        let mut right = write_line(&repository, shared[2], 20, 4).await;

        let walked = walk(repository, left[1], right[3])
            .await
            .expect("both lines are local");

        left.reverse();
        right.reverse();
        assert_eq!(walked, (shared[2], left, right));
    }
}
