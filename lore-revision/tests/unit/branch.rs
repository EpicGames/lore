// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use bytes::Bytes;
use lore_base::types::Address;
use lore_base::types::BranchPoint;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_revision::change;
use lore_revision::change::FileAction;
use lore_revision::change::NodeChange;
use lore_revision::lore::*;
use lore_revision::node::NodeFlags;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision;
use lore_revision::revision::DiffItem;
use lore_revision::state;
use lore_revision::state::State;
use tokio::sync::mpsc;

mod push;

use lore_revision::branch::*;
use lore_revision::util::path::RelativePathBuf;

fn branch_id(byte: u8) -> BranchId {
    BranchId::from([byte; 16])
}

fn revision(byte: u8) -> Hash {
    Hash::from([byte; 32])
}

fn branch_point(branch: u8, revision_byte: u8) -> BranchPoint {
    BranchPoint {
        branch: branch_id(branch),
        revision: revision(revision_byte),
    }
}

/// Names bytes that are not metadata, stored in `repository`'s local store, as the metadata of
/// `branch`, so a read of the local copy fails.
pub(crate) async fn store_unreadable_metadata(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
) {
    let bytes = Bytes::from_static(b"not branch metadata");
    let hash = lore_storage::hash::hash_slice(&bytes);
    repository
        .immutable_store()
        .put(
            repository.id,
            Address::zero_context_hash(hash),
            Fragment {
                flags: 0,
                size_payload: bytes.len() as u32,
                size_content: bytes.len() as u64,
            },
            Some(bytes),
            false,
        )
        .await
        .expect("storing bytes that are not metadata");
    mutable_store_metadata(repository.clone(), branch, hash)
        .await
        .expect("storing the metadata hash");
}

/// A branch merging the branch it was created from: the shared branch is the
/// target itself, so the target side contributes its tip and the source side
/// the revision it branched at.
#[test]
fn shared_branch_point_when_target_is_a_parent_of_source() {
    let shared = find_shared_branch_point(
        branch_id(1),
        revision(10),
        &[branch_point(2, 20), branch_point(3, 30)],
        branch_id(2),
        revision(21),
        &[branch_point(3, 30)],
    )
    .expect("The target branch is named by the source stack");

    assert_eq!(
        shared.branch,
        branch_id(2),
        "The target branch is the shared one"
    );
    assert_eq!(shared.source_point, revision(20));
    assert_eq!(shared.target_point, revision(21));
}

/// The same relation seen from the other side.
#[test]
fn shared_branch_point_when_source_is_a_parent_of_target() {
    let shared = find_shared_branch_point(
        branch_id(2),
        revision(21),
        &[branch_point(3, 30)],
        branch_id(1),
        revision(10),
        &[branch_point(2, 20), branch_point(3, 30)],
    )
    .expect("The source branch is named by the target stack");

    assert_eq!(
        shared.branch,
        branch_id(2),
        "The source branch is the shared one"
    );
    assert_eq!(shared.source_point, revision(21));
    assert_eq!(shared.target_point, revision(20));
}

/// Siblings created from the default branch, which is where both stacks end.
#[test]
fn sibling_branches_of_the_root_branch_meet_at_their_branch_points() {
    let shared = find_shared_branch_point(
        branch_id(1),
        revision(10),
        &[branch_point(9, 20)],
        branch_id(2),
        revision(11),
        &[branch_point(9, 21)],
    )
    .expect("Both stacks name the same branch");

    assert_eq!(
        shared.branch,
        branch_id(9),
        "A third branch both descend from"
    );
    assert_eq!(shared.source_point, revision(20));
    assert_eq!(shared.target_point, revision(21));
}

/// Branch points are matched on branch id, so stacks recording different
/// revisions below the shared branch make no difference to which branch is
/// shared or which points are returned.
#[test]
fn stacks_are_matched_on_branch_id_alone() {
    let shared = find_shared_branch_point(
        branch_id(1),
        revision(10),
        &[branch_point(5, 20), branch_point(9, 30)],
        branch_id(2),
        revision(11),
        &[branch_point(5, 21), branch_point(9, 31)],
    )
    .expect("Both stacks name branch 5");

    assert_eq!(shared.source_point, revision(20));
    assert_eq!(shared.target_point, revision(21));
}

/// The nearest shared branch wins, not the deepest.
#[test]
fn nearest_shared_branch_is_matched_first() {
    let shared = find_shared_branch_point(
        branch_id(1),
        revision(10),
        &[branch_point(5, 20), branch_point(9, 30)],
        branch_id(2),
        revision(11),
        &[branch_point(5, 21), branch_point(9, 30)],
    )
    .expect("Both stacks name branch 5 and branch 9");

    assert_eq!(
        shared.source_point,
        revision(20),
        "Branch 5 is nearer than branch 9 and has to be the shared branch"
    );
}

#[test]
fn stacks_sharing_no_branch_are_unresolvable() {
    assert!(
        find_shared_branch_point(
            branch_id(1),
            revision(10),
            &[branch_point(5, 20)],
            branch_id(2),
            revision(11),
            &[branch_point(6, 21)],
        )
        .is_none(),
        "Stacks naming unrelated branches cannot be resolved"
    );
}

#[test]
fn empty_stacks_are_unresolvable() {
    assert!(
        find_shared_branch_point(
            branch_id(1),
            revision(10),
            &[],
            branch_id(2),
            revision(11),
            &[],
        )
        .is_none(),
        "Without stacks there is nothing to match"
    );
}

/// Run a body with an execution context installed, which the revision store
/// reads through `execution_context()`.
async fn with_execution<F: Future>(body: F) -> F::Output {
    let execution = Arc::new(lore_revision::interface::ExecutionContext::new_client(
        lore_revision::interface::LoreGlobalArgs::default(),
        lore_revision::relay::EventDispatcher::no_dispatch(),
    ));
    lore_base::runtime::LORE_CONTEXT
        .scope(execution, body)
        .await
}

/// A repository backed by in-memory stores and no remote, so every read has to
/// come from the revisions the test wrote.
async fn null_repository() -> Arc<RepositoryContext> {
    let immutable_store = lore_storage::local::immutable_store::create(
        None::<&str>,
        lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
        false,
        lore_storage::ImmutableStoreSettings::default(),
    )
    .await
    .expect("in-memory immutable store");
    let mutable_store = lore_storage::local::mutable_store::create(
        None::<&str>,
        lore_storage::MutableStoreSettings::default(),
        immutable_store.clone(),
    )
    .await
    .expect("in-memory mutable store");

    Arc::new(RepositoryContext::new_null_context(
        immutable_store,
        mutable_store,
    ))
}

/// Write a revision carrying nothing but its parent and number, which is all
/// the history search reads.
async fn write_revision(
    repository: &Arc<RepositoryContext>,
    parent: Hash,
    revision_number: u64,
) -> Hash {
    let token = repository
        .try_write_token()
        .expect("a null context carries a write token");
    let state = State::new();
    state.set_parent_self(parent);
    state.set_revision_number(revision_number);
    state
        .serialize(repository.clone(), token)
        .await
        .expect("serializing the revision state")
}

/// Write a revision on a branch of its own. Without a distinguishing metadata
/// hash it would be addressed as, and so be, the revision the parent's own line
/// holds at that number.
async fn write_branch_revision(
    repository: &Arc<RepositoryContext>,
    parent: Hash,
    revision_number: u64,
    distinguisher: u8,
) -> Hash {
    let token = repository
        .try_write_token()
        .expect("a null context carries a write token");
    let state = State::new();
    state.set_parent_self(parent);
    state.set_revision_number(revision_number);
    state.set_metadata_hash(revision(distinguisher));
    state
        .serialize(repository.clone(), token)
        .await
        .expect("serializing the revision state")
}

/// Write a merge revision, carrying the revision merged in as its other parent.
async fn write_merge_revision(
    repository: &Arc<RepositoryContext>,
    parent_self: Hash,
    parent_other: Hash,
    revision_number: u64,
) -> Hash {
    let token = repository
        .try_write_token()
        .expect("a null context carries a write token");
    let state = State::new();
    state.set_parent_self(parent_self);
    state.set_parent_other(parent_other);
    state.set_revision_number(revision_number);
    state
        .serialize(repository.clone(), token)
        .await
        .expect("serializing the revision state")
}

/// Write a line of `count` revisions numbered from `first_revision_number`,
/// returning them oldest first.
async fn write_line(
    repository: &Arc<RepositoryContext>,
    parent: Hash,
    first_revision_number: u64,
    count: u64,
) -> Vec<Hash> {
    let mut line = Vec::new();
    let mut parent = parent;
    for offset in 0..count {
        parent = write_revision(repository, parent, first_revision_number + offset).await;
        line.push(parent);
    }
    line
}

/// The branch point of a branch that has not moved since it was created is
/// still on the line of the branch it was created from.
#[tokio::test]
async fn history_line_reaches_the_older_revision() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        let line = write_line(&repository, Hash::default(), 1, 3).await;

        let found = find_revision_in_history_line(repository, line[2], 3, line[0], 1)
            .await
            .expect("the search must not fail on readable lines");

        assert_eq!(found, HistoryLineSearch::Reached);
    }))
    .await;
}

/// Two branch points at the same revision need no search at all.
#[tokio::test]
async fn history_line_reaches_the_same_revision_on_both_sides() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        let line = write_line(&repository, Hash::default(), 1, 2).await;

        let found = find_revision_in_history_line(repository, line[1], 2, line[1], 2)
            .await
            .expect("the search must not fail");

        assert_eq!(found, HistoryLineSearch::Reached);
    }))
    .await;
}

/// The rewritten-branch shape: two revisions on sibling lines under a common
/// parent. The line passes the sibling's number without reaching it, which is
/// what tells the caller to go looking for where the lines meet.
#[tokio::test]
async fn history_line_diverges_from_a_sibling_line() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        let shared = write_line(&repository, Hash::default(), 1, 2).await;
        let newer = write_revision(&repository, shared[1], 13).await;
        let older = write_revision(&repository, shared[1], 3).await;

        let found = find_revision_in_history_line(repository, newer, 13, older, 3)
            .await
            .expect("passing the older revision's number is not a failure");

        assert_eq!(
            found,
            HistoryLineSearch::Diverged,
            "The sibling is not on the line, and claiming it is would put an unproven base in the diff"
        );
    }))
    .await;
}

#[tokio::test]
async fn history_line_diverges_from_a_line_sharing_no_revision() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        let newer = write_line(&repository, Hash::default(), 11, 3).await;
        let older = write_line(&repository, Hash::default(), 1, 3).await;

        let found = find_revision_in_history_line(
            repository,
            *newer.last().unwrap(),
            13,
            *older.last().unwrap(),
            3,
        )
        .await
        .expect("running out of line is not a failure");

        assert_eq!(
            found,
            HistoryLineSearch::Diverged,
            "Reporting a base here would put a revision in the diff that is on neither line"
        );
    }))
    .await;
}

mod find_common_revision_in_history_lines {
    use super::*;

    /// Two lines that split under a shared parent meet again at that parent.
    #[tokio::test]
    async fn history_lines_meet_where_they_split() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let shared = write_line(&repository, Hash::default(), 1, 2).await;
            let one = write_revision(&repository, shared[1], 13).await;
            let other = write_revision(&repository, shared[1], 3).await;

            let met = Box::pin(find_common_revision_in_history_lines(
                repository, one, other,
            ))
            .await
            .expect("the search must not fail on readable lines");

            assert_eq!(
                met,
                Some(shared[1]),
                "The revision the two lines split at is the newest they share"
            );
        }))
        .await;
    }

    /// The newest shared revision wins, not the first one reached on either line.
    #[tokio::test]
    async fn history_lines_meet_at_the_newest_shared_revision() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let shared = write_line(&repository, Hash::default(), 1, 3).await;
            let one = write_revision(&repository, shared[2], 13).await;
            let other = write_revision(&repository, shared[2], 4).await;

            let met = Box::pin(find_common_revision_in_history_lines(
                repository, one, other,
            ))
            .await
            .expect("the search must not fail on readable lines");

            assert_eq!(met, Some(shared[2]));
        }))
        .await;
    }

    #[tokio::test]
    async fn history_lines_that_share_no_revision_meet_nowhere() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let one = write_line(&repository, Hash::default(), 1, 3).await;
            let other = write_line(&repository, Hash::default(), 11, 3).await;

            let met = Box::pin(find_common_revision_in_history_lines(
                repository,
                *one.last().unwrap(),
                *other.last().unwrap(),
            ))
            .await
            .expect("running out of line is not a failure");

            assert_eq!(met, None);
        }))
        .await;
    }

    #[tokio::test]
    async fn history_lines_that_cannot_be_read_meet_nowhere() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;

            let met = Box::pin(find_common_revision_in_history_lines(
                repository,
                revision(200),
                revision(201),
            ))
            .await
            .expect("an unreadable line is not a failure of the search");

            assert_eq!(met, None);
        }))
        .await;
    }

    /// A page the remote serves from a cached segment also holds revisions newer than the one it
    /// was asked for. Those are not on the line below it, so they cannot be where the lines meet.
    #[tokio::test]
    async fn history_lines_meet_where_they_split_when_a_remote_page_starts_above_its_anchor() {
        Box::pin(with_execution(async {
            let branched = crate::paged_remote::branched_from_cached_segment().await;

            let met = Box::pin(find_common_revision_in_history_lines(
                branched.client,
                branched.remote_line[5],
                branched.local,
            ))
            .await
            .expect("the search must not fail on readable lines");

            assert_eq!(
                met,
                Some(branched.remote_line[2]),
                "The local branch was created from revision 3"
            );
        }))
        .await;
    }
}

/// A branch that has not moved since it was created: its branch point is still
/// on the line of the branch it was created from, so that point is the answer.
#[tokio::test]
async fn common_ancestor_is_the_branch_point_still_on_the_line() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        let main_line = write_line(&repository, Hash::default(), 1, 4).await;

        let found = find_common_ancestor_from_branch_points(
            repository,
            branch_id(1),
            revision(10),
            &[BranchPoint {
                branch: branch_id(9),
                revision: main_line[1],
            }],
            branch_id(9),
            main_line[3],
            &[],
        )
        .await
        .expect("the search must not fail on readable lines");

        assert_eq!(
            found,
            Some(main_line[1]),
            "The branch point is reached by following the shared branch back"
        );
    }))
    .await;
}

/// A rewritten shared branch: the two points sit on lines that split below
/// them, and the answer is where those lines meet.
#[tokio::test]
async fn common_ancestor_is_where_rewritten_lines_meet() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        let shared = write_line(&repository, Hash::default(), 1, 2).await;
        let source_point = write_revision(&repository, shared[1], 13).await;
        let target_point = write_revision(&repository, shared[1], 3).await;

        let found = find_common_ancestor_from_branch_points(
            repository,
            branch_id(1),
            revision(10),
            &[BranchPoint {
                branch: branch_id(9),
                revision: source_point,
            }],
            branch_id(2),
            revision(11),
            &[BranchPoint {
                branch: branch_id(9),
                revision: target_point,
            }],
        )
        .await
        .expect("the search must not fail on readable lines");

        assert_eq!(found, Some(shared[1]));
    }))
    .await;
}

/// Branch points of equal revision number cannot reach one another, whatever
/// their hashes. The answer still comes from following both lines to where
/// they meet, rather than from taking one of the points.
#[tokio::test]
async fn common_ancestor_of_equal_numbered_points_is_where_lines_meet() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        // Same number, different parents, so the two are distinct revisions
        // that provably cannot reach one another.
        let shared = write_line(&repository, Hash::default(), 1, 2).await;
        let source_point = write_revision(&repository, shared[1], 3).await;
        let target_point = write_revision(&repository, shared[0], 3).await;

        let found = find_common_ancestor_from_branch_points(
            repository,
            branch_id(1),
            revision(10),
            &[BranchPoint {
                branch: branch_id(9),
                revision: source_point,
            }],
            branch_id(2),
            revision(11),
            &[BranchPoint {
                branch: branch_id(9),
                revision: target_point,
            }],
        )
        .await
        .expect("the search must not fail on readable lines");

        assert_eq!(
            found,
            Some(shared[0]),
            "The lines meet at the revision they both descend from"
        );
    }))
    .await;
}

/// Nothing found either way still answers, with the older of the two branch
/// points. It is a guess, and it beats refusing a diff the caller cannot
/// repair.
#[tokio::test]
async fn common_ancestor_falls_back_to_the_older_branch_point() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        let source_line = write_line(&repository, Hash::default(), 11, 2).await;
        let target_line = write_line(&repository, Hash::default(), 1, 2).await;

        let found = find_common_ancestor_from_branch_points(
            repository,
            branch_id(1),
            revision(10),
            &[BranchPoint {
                branch: branch_id(9),
                revision: source_line[1],
            }],
            branch_id(2),
            revision(11),
            &[BranchPoint {
                branch: branch_id(9),
                revision: target_line[1],
            }],
        )
        .await
        .expect("falling back is not a failure");

        assert_eq!(
            found,
            Some(target_line[1]),
            "The lower numbered branch point is the guess, and it is never zero"
        );
    }))
    .await;
}

/// A trunk 2000 revisions past the branch point, against a search depth of 500,
/// with the branch having merged the trunk once in between. The base is the
/// revision that merge carried across, which only the merge search can find.
#[tokio::test]
async fn common_ancestor_finds_an_earlier_merge_beyond_the_search_depth() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        // Numbers chosen to sit either side of the search depth the way the
        // reported case does: 2000 revisions of trunk since the branch point,
        // against a depth of 500.
        let trunk = write_line(&repository, Hash::default(), 1, 3000).await;
        let branch_point = trunk[999];
        let merged_in = trunk[1499];
        let trunk_tip = *trunk.last().expect("the trunk has revisions");

        let branch_first = write_branch_revision(&repository, branch_point, 1001, 200).await;
        let branch_merge = write_merge_revision(&repository, branch_first, merged_in, 1501).await;

        let target_stack = [BranchPoint {
            branch: branch_id(9),
            revision: branch_point,
        }];

        let from_points = Box::pin(find_common_ancestor_from_branch_points(
            repository.clone(),
            branch_id(9),
            trunk_tip,
            &[],
            branch_id(1),
            branch_merge,
            &target_stack,
        ))
        .await
        .expect("exhausting the depth is not a failure");

        assert_eq!(
            from_points,
            Some(branch_point),
            "Out of depth, the branch points can only offer the branch point"
        );

        let from_merges = find_common_ancestor_from_merges(
            repository,
            branch_id(9),
            trunk_tip,
            branch_id(1),
            branch_merge,
            branch_point,
        )
        .await
        .expect("the walk must not fail on readable history");

        assert_eq!(
            from_merges,
            Some(merged_in),
            "The revision the earlier merge carried across is the base, not the branch point"
        );
    }))
    .await;
}

/// The trunk merged the branch 1499 revisions below its tip, three times the
/// search depth. The base is the branch revision the trunk holds, reached through
/// the trunk's merge revision.
#[tokio::test]
async fn common_ancestor_is_what_the_trunk_already_merged_of_the_branch() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        let trunk_below = write_line(&repository, Hash::default(), 1, 1500).await;
        let branch_point = trunk_below[999];

        let branch_first = write_branch_revision(&repository, branch_point, 1001, 220).await;
        let branch_merged = write_branch_revision(&repository, branch_first, 1002, 221).await;
        // The branch carried on after the trunk took it, so its tip is not what
        // the trunk holds.
        let branch_tip = write_branch_revision(&repository, branch_merged, 1003, 222).await;

        let trunk_merge = write_merge_revision(
            &repository,
            *trunk_below.last().expect("the trunk has revisions"),
            branch_merged,
            1501,
        )
        .await;
        let trunk_above = write_line(&repository, trunk_merge, 1502, 1499).await;
        let trunk_tip = *trunk_above.last().expect("the trunk has revisions");

        let target_stack = [BranchPoint {
            branch: branch_id(9),
            revision: branch_point,
        }];

        let from_points = Box::pin(find_common_ancestor_from_branch_points(
            repository.clone(),
            branch_id(9),
            trunk_tip,
            &[],
            branch_id(1),
            branch_tip,
            &target_stack,
        ))
        .await
        .expect("exhausting the depth is not a failure");

        assert_eq!(
            from_points,
            Some(branch_point),
            "Out of depth, the branch points can only offer the branch point"
        );

        let from_merges = find_common_ancestor_from_merges(
            repository,
            branch_id(9),
            trunk_tip,
            branch_id(1),
            branch_tip,
            branch_point,
        )
        .await
        .expect("the walk must not fail on readable history");

        assert_eq!(
            from_merges,
            Some(branch_merged),
            "The base is the branch revision the trunk already merged, not the branch point"
        );
    }))
    .await;
}

/// The source carries the earlier merge, and the revision it took sits 1500
/// revisions below the target tip. Reaching it means walking the target's line
/// three times past the search depth, which the merge search is not bound by.
#[tokio::test]
async fn common_ancestor_finds_a_merge_the_source_took_far_below_the_target_tip() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        let trunk = write_line(&repository, Hash::default(), 1, 3000).await;
        let branch_point = trunk[999];
        let merged_in = trunk[1499];
        let trunk_tip = *trunk.last().expect("the trunk has revisions");

        let source_first = write_branch_revision(&repository, branch_point, 1001, 210).await;
        let source_merge =
            write_merge_revision(&repository, source_first, merged_in, 1501).await;

        let source_stack = [BranchPoint {
            branch: branch_id(9),
            revision: branch_point,
        }];

        let from_points = Box::pin(find_common_ancestor_from_branch_points(
            repository.clone(),
            branch_id(1),
            source_merge,
            &source_stack,
            branch_id(9),
            trunk_tip,
            &[],
        ))
        .await
        .expect("exhausting the depth is not a failure");

        assert_eq!(
            from_points,
            Some(branch_point),
            "Out of depth, the branch points can only offer the branch point"
        );

        let from_merges = find_common_ancestor_from_merges(
            repository,
            branch_id(1),
            source_merge,
            branch_id(9),
            trunk_tip,
            branch_point,
        )
        .await
        .expect("the walk must not fail on readable history");

        assert_eq!(
            from_merges,
            Some(merged_in),
            "The walk has to follow the target's line 1500 revisions down to the revision the source already took"
        );
    }))
    .await;
}

/// A branch of two commits that never merged the trunk, with the trunk 1417
/// revisions further on. The branch point is the answer, and the merge search
/// confirms it rather than leaving it a guess.
#[tokio::test]
async fn common_ancestor_of_a_branch_that_never_merged_is_its_branch_point() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        let trunk = write_line(&repository, Hash::default(), 1, 2624).await;
        let branch_point = trunk[1206];
        let trunk_tip = *trunk.last().expect("the trunk has revisions");

        let branch_first = write_branch_revision(&repository, branch_point, 1208, 201).await;
        let branch_tip = write_branch_revision(&repository, branch_first, 1209, 202).await;

        let target_stack = [BranchPoint {
            branch: branch_id(9),
            revision: branch_point,
        }];

        let from_points = Box::pin(find_common_ancestor_from_branch_points(
            repository.clone(),
            branch_id(9),
            trunk_tip,
            &[],
            branch_id(1),
            branch_tip,
            &target_stack,
        ))
        .await
        .expect("exhausting the depth is not a failure");

        assert_eq!(from_points, Some(branch_point));

        let from_merges = find_common_ancestor_from_merges(
            repository,
            branch_id(9),
            trunk_tip,
            branch_id(1),
            branch_tip,
            branch_point,
        )
        .await
        .expect("the walk must not fail on readable history");

        assert_eq!(
            from_merges,
            Some(branch_point),
            "The walk reaches the branch point from both sides, which is what makes it the answer rather than a guess"
        );
    }))
    .await;
}

/// Stacks naming no branch in common are the one case with no answer, which
/// the caller reports as an invalid branch configuration.
#[tokio::test]
async fn common_ancestor_is_absent_only_for_unshared_stacks() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;

        let found = find_common_ancestor_from_branch_points(
            repository,
            branch_id(1),
            revision(10),
            &[BranchPoint {
                branch: branch_id(5),
                revision: revision(20),
            }],
            branch_id(2),
            revision(11),
            &[BranchPoint {
                branch: branch_id(6),
                revision: revision(21),
            }],
        )
        .await
        .expect("unshared stacks are not a failure of the search");

        assert_eq!(found, None);
    }))
    .await;
}

/// An unreadable line cannot be followed, and the search says so rather than
/// answering with the revision it was asked to look for.
#[tokio::test]
async fn history_line_that_cannot_be_read_diverges() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;

        let found = find_revision_in_history_line(repository, revision(200), 2, revision(201), 1)
            .await
            .expect("an unreadable line is not a failure of the search");

        assert_eq!(found, HistoryLineSearch::Diverged);
    }))
    .await;
}

#[tokio::test]
async fn history_line_of_an_unknown_revision_is_empty() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;

        assert!(
            load_history_line(repository, revision(200))
                .await
                .is_empty(),
            "A revision that is not stored yields no line to search"
        );
    }))
    .await;
}

#[tokio::test]
async fn reaching_the_floor_needs_a_readable_line_below_it() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        let line = write_line(&repository, Hash::default(), 1, 3).await;

        assert!(
            history_reached_floor(repository.clone(), &[], 0).await,
            "An empty line has nothing left to load"
        );
        assert!(
            history_reached_floor(repository.clone(), &[revision(200)], 0).await,
            "A line that cannot be read any further has nothing left to load"
        );
        assert!(
            history_reached_floor(repository.clone(), &line[..1], 1).await,
            "Revision number 1 is at a floor of 1"
        );
        assert!(
            !history_reached_floor(repository, &line[2..], 1).await,
            "Revision number 3 is above a floor of 1, so the line can still be followed"
        );
    }))
    .await;
}

/// Extending a line that has reached its root revision adds nothing. Reporting
/// progress instead would keep the caller re-comparing the same line until its
/// depth limit stopped it.
#[tokio::test]
async fn extending_an_exhausted_line_reports_it_exhausted() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        let line = write_line(&repository, Hash::default(), 1, 2).await;
        let mut history = vec![line[1], line[0]];

        assert!(
            load_additional_history(repository.clone(), &mut history, 0).await,
            "A line followed to its root revision has nothing further to load"
        );
        assert_eq!(
            history,
            vec![line[1], line[0]],
            "An exhausted line must not grow"
        );

        let mut empty = Vec::new();
        assert!(
            load_additional_history(repository, &mut empty, 0).await,
            "There is nothing to extend an empty line from"
        );
        assert!(empty.is_empty());
    }))
    .await;
}

/// A change between two nodes of one kind, carrying the path a move or copy
/// came from.
fn node_change(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    action: FileAction,
    flags: NodeFlags,
    path: &str,
    from_path: Option<&str>,
) -> NodeChange {
    let side = |node, side_path: &str| change::NodeChangeState {
        mapping: state::NodeMapping {
            repository: repository.clone(),
            state: state.clone(),
            path: RelativePathBuf::new().push_and_freeze(side_path),
            node,
        },
        observed: None,
        flags,
        address: Address::default(),
        mode: 0,
    };
    NodeChange {
        action,
        flags: change::Flags::None,
        from: side(1, from_path.unwrap_or_default()),
        to: side(2, path),
    }
}

/// Without the source path a receiver reads a move as an add at the new path
/// and cannot tell where the content came from.
#[tokio::test]
async fn diff_change_carries_the_move_source_path() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        let state = State::new();
        let change = node_change(
            &repository,
            &state,
            FileAction::Move,
            NodeFlags::File,
            "new.txt",
            Some("old.txt"),
        );

        let data = LoreBranchDiffNodeData::new(&change);

        assert_eq!(data.path.as_str(), "new.txt");
        assert_eq!(data.from_path.as_str(), "old.txt");
    }))
    .await;
}

/// A change that moved nothing maps to the empty string the C API documents,
/// not to a dangling pointer a receiver would read past.
#[tokio::test]
async fn diff_change_without_a_move_reports_no_source_path() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        let state = State::new();
        let change = node_change(
            &repository,
            &state,
            FileAction::Add,
            NodeFlags::File,
            "new.txt",
            None,
        );

        let data = LoreBranchDiffNodeData::new(&change);

        assert!(data.from_path.is_empty());
        assert_eq!(data.from_path.as_str(), "");
    }))
    .await;
}

/// Both paths of a moved directory get the trailing separator that tells a
/// directory from a file, so the two can be compared as they are reported.
#[tokio::test]
async fn diff_change_marks_a_moved_directory_on_both_paths() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        let state = State::new();
        let change = node_change(
            &repository,
            &state,
            FileAction::Move,
            NodeFlags::NoFlags,
            "new",
            Some("old"),
        );

        let data = LoreBranchDiffNodeData::new(&change);

        assert_eq!(data.path.as_str(), "new/");
        assert_eq!(data.from_path.as_str(), "old/");
    }))
    .await;
}

/// Every three-way diff item passes through the auto-resolve step and few reach the text
/// merge, so the step does not hold the merge.
#[tokio::test]
async fn the_auto_resolve_step_does_not_hold_the_text_merge() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        let state = State::new();
        let change = node_change(
            &repository,
            &state,
            FileAction::Add,
            NodeFlags::File,
            "file.txt",
            None,
        );
        let (tx, _rx) = mpsc::channel(1);

        let merge = try_auto_resolve_conflict(&change, &change);
        let step = emit_diff_item_with_auto_resolve(DiffItem::Change(change.clone()), true, &tx);

        assert!(
            size_of_val(&step) < size_of_val(&merge),
            "the step holds {} bytes, the merge {}",
            size_of_val(&step),
            size_of_val(&merge)
        );
    }))
    .await;
}

/// A resolved conflict replaces the item in place, so the auto-resolve step holds its item
/// once, and the relay holds no item beside the step.
#[tokio::test]
async fn each_relayed_diff_item_is_held_once() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        let state = State::new();
        let change = node_change(
            &repository,
            &state,
            FileAction::Add,
            NodeFlags::File,
            "file.txt",
            None,
        );
        let (tx, _rx) = mpsc::channel(1);
        let (inner_tx, inner_rx) = mpsc::channel(1);
        let hash = Hash::default();

        let step = emit_diff_item_with_auto_resolve(DiffItem::Change(change), true, &tx);
        let driver = std::pin::pin!(revision::diff3_with_source_cap(
            repository.clone(),
            hash,
            hash,
            hash,
            None,
            false,
            None,
            None,
            None,
            inner_tx,
        ));
        let relay = relay_revision_diff3(driver, inner_rx, true, &tx);

        assert!(
            size_of_val(&step) < 2 * size_of::<DiffItem>(),
            "the step holds {} bytes for an item of {}",
            size_of_val(&step),
            size_of::<DiffItem>()
        );
        assert!(
            size_of_val(&relay) < size_of_val(&step) + size_of::<DiffItem>(),
            "the relay holds {} bytes, its step {}",
            size_of_val(&relay),
            size_of_val(&step)
        );
    }))
    .await;
}

/// Resolving a branch through the remote, where the instance does not hold it.
mod resolve_through_the_remote {
    use std::fmt::Debug;
    use std::path::PathBuf;

    use lore_base::error::BranchNotFound;
    use lore_base::error::Disconnected;
    use lore_base::error::NotAuthenticated;
    use lore_base::error::NotAuthorized;
    use lore_base::test_util::TempDir;
    use lore_error_set::FfiError;
    use lore_revision::file::history::HistoryOptions;
    use lore_revision::file::reset::ResetMergeSide;
    use lore_revision::file::reset::ResetOptions;
    use lore_revision::instance::InstanceId;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreString;
    use lore_revision::metadata::Metadata;
    use lore_revision::repository::RemoteState;
    use lore_revision::repository::RepositoryPaths;
    use lore_revision::test_util::context_with_state;
    use lore_transport::Connection;
    use lore_transport::ProtocolError;

    use super::*;
    use crate::helper::in_memory;
    use crate::immutable::serving::Serving;
    use crate::immutable::serving::serving_remote;
    use crate::immutable::session_tests::under_globals;
    use crate::paged_remote::PagedRemote;
    use crate::paged_remote::client_and_writer;

    /// Globals for a lookup that consults only the remote where `remote_only`, and prefers what
    /// the instance holds otherwise.
    fn globals(remote_only: bool) -> LoreGlobalArgs {
        LoreGlobalArgs {
            remote: remote_only as u8,
            ..LoreGlobalArgs::default()
        }
    }

    /// A lookup that reaches the remote fails with the remote's refusal of the credentials,
    /// whether the remote refuses the connect or the query, by name or by id, with `--remote` or
    /// without.
    #[tokio::test]
    async fn a_lookup_the_remote_refuses_fails_with_the_refusal() {
        let id = branch_id(7).to_string();
        let mut unrefused = Vec::new();
        for remote_only in [false, true] {
            for branch in ["refused", id.as_str()] {
                let refused_connect =
                    context_with_state(RemoteState::Failed(ProtocolError::from(NotAuthorized)))
                        .await;
                let (refused_query, _) = client_and_writer(Arc::new(
                    PagedRemote::new(branch_id(7), Hash::default())
                        .refusing(ProtocolError::from(NotAuthorized)),
                ))
                .await;
                for (refused, repository) in
                    [("connect", refused_connect), ("query", refused_query)]
                {
                    let resolved =
                        under_globals(globals(remote_only), || resolve(repository, branch)).await;
                    if !resolved.as_ref().is_err_and(|err| err.is_not_authorized()) {
                        unrefused.push(format!(
                            "{branch} with its {refused} refused, remote only {remote_only}: \
                             got {resolved:?}"
                        ));
                    }
                }
            }
        }
        assert!(unrefused.is_empty(), "{unrefused:#?}");
    }

    /// A lookup whose connect fails for any reason but a refusal finds no branch: by name, or by
    /// id with `--remote`, it fails as `BranchNotFound`, and by id otherwise it answers the id
    /// alone.
    #[tokio::test]
    async fn a_lookup_without_a_reachable_remote_finds_no_branch() {
        let id = branch_id(7);
        let id_text = id.to_string();
        for (remote_only, branch, answered) in [
            (false, "unreachable", None),
            (true, "unreachable", None),
            (true, id_text.as_str(), None),
            (false, id_text.as_str(), Some(id)),
        ] {
            let repository =
                context_with_state(RemoteState::Failed(ProtocolError::from(Disconnected))).await;
            let resolved =
                under_globals(globals(remote_only), || resolve(repository, branch)).await;
            match answered {
                Some(id) => assert!(
                    resolved
                        .as_ref()
                        .is_ok_and(|status| status.id == id && status.latest.is_zero()),
                    "{branch}, remote only {remote_only}: got {resolved:?}"
                ),
                None => assert!(
                    resolved
                        .as_ref()
                        .is_err_and(|err| err.is_branch_not_found()),
                    "{branch}, remote only {remote_only}: got {resolved:?}"
                ),
            }
        }
    }

    /// A context that cannot write, whose local copy of a branch's metadata is unreadable, reads the
    /// remote's metadata and keeps it in its local store.
    #[tokio::test]
    async fn a_read_only_metadata_read_keeps_the_remote_metadata_locally() {
        let branch = branch_id(7);
        let source = context_with_state(RemoteState::Offline).await;
        let mut served = Metadata::new();
        served
            .set_branch(branch)
            .expect("the metadata holds the branch");
        let hash = under_globals(globals(false), || served.serialize_local(source.clone()))
            .await
            .expect("the remote's metadata is stored");
        let (immutable, mutable) = lore_revision::repository::create_client_memory_stores()
            .await
            .expect("in-memory stores should be creatable");
        let writer = Arc::new(
            RepositoryContext::new_with_state(
                None,
                immutable,
                mutable,
                source.id,
                InstanceId::default(),
                RemoteState::Connected(Connection::detached_revision(
                    vec![serving_remote(&source, Serving::default())],
                    Ok(()),
                    source.id,
                    Arc::new(PagedRemote::new(branch, Hash::default()).metadata(hash)),
                )),
                Arc::default(),
                None,
            )
            .with_write_token(in_memory::token()),
        );
        store_unreadable_metadata(&writer, branch).await;
        let repository = Arc::new(writer.without_command_locks());

        let read = under_globals(globals(false), || {
            lore_revision::branch::metadata(repository.clone(), branch)
        })
        .await
        .expect("the remote's metadata is read");

        assert_eq!(read.get_branch().ok(), Some(branch));
        assert!(
            lore_revision::immutable::is_stored_local(repository, Address::zero_context_hash(hash))
                .await,
            "the remote's metadata is kept locally"
        );
    }

    /// An instance on in-memory stores, rooted at the directory returned with it, whose current
    /// branch is set and whose connect to the remote fails with `failure`.
    async fn instance_whose_connect_fails(
        failure: ProtocolError,
    ) -> (TempDir, Arc<RepositoryContext>) {
        let root = TempDir::new("refused-lookup-");
        let (immutable, mutable) = lore_revision::repository::create_client_memory_stores()
            .await
            .expect("in-memory stores should be creatable");
        let repository = Arc::new(
            RepositoryContext::new_with_state(
                Some(RepositoryPaths::new(
                    root.path().to_path_buf(),
                    root.child(".lore"),
                )),
                immutable,
                mutable,
                lore_revision::lore::RepositoryId::default(),
                InstanceId::default(),
                RemoteState::Failed(failure),
                Arc::default(),
                None,
            )
            .with_write_token(in_memory::token()),
        );
        lore_revision::instance::store_current_anchor_branch(&repository, branch_id(1))
            .await
            .expect("the current branch should be storable");
        (root, repository)
    }

    /// The FFI code of `BranchNotFound`.
    fn branch_not_found() -> i32 {
        BranchNotFound {
            branch: String::new(),
        }
        .ffi_code()
    }

    /// The connect failures for which `command`, run on an instance whose connect to the remote
    /// fails that way, does not fail with the code expected of it: the refusal's own for a
    /// refusal, and `not_found` for a remote it cannot reach.
    async fn codes_that_differ<T, E, F>(
        not_found: i32,
        command: impl Fn(Arc<RepositoryContext>, PathBuf) -> F,
    ) -> Vec<String>
    where
        T: Debug,
        E: FfiError + Debug,
        F: Future<Output = Result<T, E>>,
    {
        let command = &command;
        let mut differing = Vec::new();
        for (failure, expected) in [
            (
                ProtocolError::from(NotAuthenticated),
                NotAuthenticated.ffi_code(),
            ),
            (ProtocolError::from(NotAuthorized), NotAuthorized.ffi_code()),
            (ProtocolError::from(Disconnected), not_found),
        ] {
            let described = format!("{failure:?}");
            let result = under_globals(globals(false), || async move {
                let (root, repository) = instance_whose_connect_fails(failure).await;
                command(repository, root.path().to_path_buf()).await
            })
            .await;
            if result.as_ref().err().map(FfiError::ffi_code) != Some(expected) {
                differing.push(format!("{described}: got {result:?}"));
            }
        }
        differing
    }

    /// `lore file history --branch` fails with the remote's refusal of the branch lookup, and
    /// with `BranchNotFound` where the remote cannot be reached.
    #[tokio::test]
    async fn file_history_fails_with_a_refused_branch_lookup() {
        let differing = codes_that_differ(branch_not_found(), |repository, root| {
            lore_revision::file::history::history(
                repository,
                root.join("file").to_string_lossy().into_owned(),
                HistoryOptions {
                    revision: None,
                    branch: Some("refused".to_string()),
                    length: 0,
                    depth: 0,
                },
            )
        })
        .await;
        assert!(differing.is_empty(), "{differing:#?}");
    }

    /// `lore branch reset` fails with the remote's refusal of the branch lookup, and with
    /// `BranchNotFound` where the remote cannot be reached.
    #[tokio::test]
    async fn branch_reset_fails_with_a_refused_branch_lookup() {
        let differing = codes_that_differ(branch_not_found(), |repository, _| async move {
            lore_revision::branch::reset::reset(
                repository,
                &in_memory::token(),
                "refused".to_string(),
                String::new(),
            )
            .await
        })
        .await;
        assert!(differing.is_empty(), "{differing:#?}");
    }

    /// `lore file reset` to the last merge of a branch fails with the remote's refusal of the
    /// branch lookup, and with `BranchNotFound` where the remote cannot be reached.
    #[tokio::test]
    async fn file_reset_to_last_merged_fails_with_a_refused_branch_lookup() {
        let differing = codes_that_differ(branch_not_found(), |repository, _| {
            lore_revision::file::reset::reset_to_last_merged(
                repository,
                LoreArray::default(),
                LoreString::from("refused"),
                ResetMergeSide::default(),
                ResetOptions::default(),
            )
        })
        .await;
        assert!(differing.is_empty(), "{differing:#?}");
    }
}
