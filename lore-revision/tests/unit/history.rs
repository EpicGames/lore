// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

mod find_branch_point {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Hash;
    use lore_revision::history::find_branch_point;

    use crate::helper::setup_test_execution;
    use crate::paged_remote::Anchor;
    use crate::paged_remote::BranchedFromRemoteLine;
    use crate::paged_remote::branched_from_cached_segment;
    use crate::paged_remote::branched_from_remote_line;
    use crate::paged_remote::numbered;

    async fn branch_point_of(branched: &BranchedFromRemoteLine) -> (Hash, Vec<Hash>, Vec<Hash>) {
        LORE_CONTEXT
            .scope(
                setup_test_execution(),
                find_branch_point(
                    branched.client.clone(),
                    branched.remote_line[5],
                    branched.local,
                ),
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
}
