// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use futures::StreamExt;
use lore_base::error::AddressNotFound;
use lore_base::error::Disconnected;
use lore_base::error::NotAuthorized;
use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::BranchPoint;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_error_set::FfiError;
use lore_revision::change;
use lore_revision::change::FileAction;
use lore_revision::change::NodeChange;
use lore_revision::event::LoreEvent;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreEventCallback;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::lore::*;
use lore_revision::node::NodeFlags;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository::BranchSwitchOptions;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision;
use lore_revision::revision::DiffItem;
use lore_revision::state;
use lore_revision::state::State;
use lore_revision::state::StateData;
use lore_storage::KeyType;
use lore_storage::MutableStore;
use lore_storage::StoreError;
use lore_storage::WriteOptions;
use lore_transport::ProtocolError;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

mod push;
mod races;

use lore_revision::branch::*;
use lore_revision::util::path::RelativePathBuf;

use crate::helper::committed_repository;
use crate::helper::create_branch_on_instance;
use crate::helper::forced_execution;
use crate::helper::in_memory;
use crate::helper::setup_test_execution;
use crate::helper::stage_paths;
use crate::helper::test_execution_with;
use crate::helper::test_file_write;
use crate::hooked_store::Answered;
use crate::hooked_store::HookedStore;
use crate::hooked_store::StoreCall;
use crate::hooked_store::answered;
use crate::paged_remote::PagedRemote;
use crate::paged_remote::above_unverified;
use crate::paged_remote::client_stores;
use crate::paged_remote::hooked_client_stores;
use crate::paged_remote::writing_client_on;

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
pub(crate) async fn null_repository() -> Arc<RepositoryContext> {
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
pub(crate) async fn write_branch_revision(
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
pub(crate) async fn write_line(
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

/// Write a blob of a revision state's size that is not a revision state: zeroed, so its header
/// lacks the state magic. Read as a state, it is a revision numbered 0 without parents.
pub(crate) async fn write_not_a_state(repository: &Arc<RepositoryContext>) -> Hash {
    lore_revision::immutable::write(
        repository.clone(),
        Context::default(),
        Bytes::from(vec![0u8; size_of::<StateData>()]),
        WriteOptions::default(),
    )
    .await
    .expect("writes the blob")
    .hash
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

    /// A line that reaches the newest shared revision a batch before the other still meets the
    /// other there, not at an older shared revision both lines load in the later batch.
    #[tokio::test]
    async fn history_lines_meet_at_the_newest_shared_revision_a_batch_apart() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let shared = write_line(&repository, Hash::default(), 1, 120).await;
            let one = write_line(&repository, shared[119], 1000, 5).await;
            let other = write_line(&repository, shared[119], 2000, 100).await;

            let met = Box::pin(find_common_revision_in_history_lines(
                repository, one[4], other[99],
            ))
            .await
            .expect("the search must not fail on readable lines");

            assert_eq!(met, Some(shared[119]));
        }))
        .await;
    }

    /// Siblings at equal numbers are not shared, so both lines pass them and meet at
    /// the shared revision below.
    #[tokio::test]
    async fn history_lines_pass_siblings_at_equal_numbers_to_the_shared_revision_below() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let shared = write_line(&repository, Hash::default(), 1, 3).await;
            let one_sibling = write_branch_revision(&repository, shared[2], 4, 1).await;
            let other_sibling = write_branch_revision(&repository, shared[2], 4, 2).await;
            let one = write_line(&repository, one_sibling, 5, 2).await;
            let other = write_line(&repository, other_sibling, 5, 1).await;

            let met = Box::pin(find_common_revision_in_history_lines(
                repository, one[1], other[0],
            ))
            .await
            .expect("the search must not fail on readable lines");

            assert_eq!(met, Some(shared[2]));
        }))
        .await;
    }

    /// A line whose revision numbers do not decrease does not follow self parents,
    /// so the search fails rather than answering from it.
    #[tokio::test]
    async fn a_history_line_whose_numbers_do_not_decrease_fails_the_search() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let parent = write_revision(&repository, Hash::default(), 5).await;
            let child = write_revision(&repository, parent, 3).await;
            let other = write_line(&repository, Hash::default(), 1, 2).await;

            let error = Box::pin(find_common_revision_in_history_lines(
                repository, child, other[1],
            ))
            .await
            .expect_err("the line from child rises in number to its parent");

            assert!(error.is_internal(), "{error:?}");
        }))
        .await;
    }

    /// A batch is loaded only for the line whose oldest revision has the higher
    /// number, so the store is read for the line below only once the lines need it:
    /// three batches of `one`, and the first of `other`, which reaches the shared
    /// revision before `one` does.
    #[tokio::test]
    async fn history_lines_load_the_line_below_only_once_it_is_needed() {
        Box::pin(with_execution(async {
            let (stores, counting) = crate::paged_remote::counted_client_stores().await;
            let repository = stores.writer.clone();
            let shared = write_line(&repository, Hash::default(), 1, 150).await;
            let one = write_line(&repository, shared[149], 1000, 250).await;
            let other = write_line(&repository, shared[149], 151, 1).await;
            counting.take_reads();

            let met = Box::pin(find_common_revision_in_history_lines(
                repository, one[249], other[0],
            ))
            .await
            .expect("the search must not fail on readable lines");

            assert_eq!(met, Some(shared[149]));
            assert_eq!(
                counting.take_reads().payloads,
                400,
                "a state read for each revision of four batches of 100"
            );
        }))
        .await;
    }

    /// A page that lists a revision on a line without its own state fails the search, whether
    /// the state is missing, cut short, or another revision's, and whether the revision is the one
    /// asked for or one below it.
    #[tokio::test]
    async fn history_lines_fail_where_a_page_lists_a_revision_without_its_own_state() {
        Box::pin(with_execution(async {
            for case in above_unverified(Hash::default()).await {
                let unrelated = write_line(&case.writer, Hash::default(), 1, 2).await;

                let met = Box::pin(find_common_revision_in_history_lines(
                    case.client,
                    case.local,
                    unrelated[1],
                ))
                .await;

                assert!(
                    met.as_ref().is_err_and(|error| error.is_internal()),
                    "{} state for {}: {met:?}",
                    case.kind,
                    case.position
                );
            }
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

/// A page that lists a revision on the line without its own state fails the search for a
/// revision on that line, whether the state is missing, cut short, or another revision's, and
/// whether the revision is the one asked for or one below it.
#[tokio::test]
async fn history_line_search_fails_where_a_page_lists_a_revision_without_its_own_state() {
    Box::pin(with_execution(async {
        for case in above_unverified(Hash::default()).await {
            let found =
                find_revision_in_history_line(case.client, case.local, 4, revision(201), 1).await;

            assert!(
                found.as_ref().is_err_and(|error| error.is_internal()),
                "{} state for {}: {found:?}",
                case.kind,
                case.position
            );
        }
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
                .expect("an unknown revision is not a failure")
                .is_empty(),
            "A revision that is not stored yields no line to search"
        );
    }))
    .await;
}

/// A line whose first batch holds the revisions the local store holds ends where
/// it leaves the store, where the remote's list of the line from there is not
/// found, though the remote serves the state there.
#[tokio::test]
async fn history_line_the_remote_cannot_list_ends_where_it_leaves_the_store() {
    Box::pin(with_execution(async {
        let source = null_repository().await;
        let gap = write_line(&source, Hash::default(), 1, 1).await[0];
        let stores = client_stores().await;
        let line = write_line(&stores.writer, gap, 2, 2).await;
        let remote = PagedRemote::new(branch_id(7), line[1]);
        let storage = crate::immutable::serving::serving_remote(
            &source,
            crate::immutable::serving::Serving::default(),
        );
        let client =
            crate::paged_remote::client_on_storage(&stores, Arc::new(remote), vec![storage]);

        let history = load_history_line(client.clone(), line[1])
            .await
            .expect("a line the remote cannot list is not a failure");

        assert_eq!(history, vec![(3, line[1]), (2, line[0])]);
        State::deserialize(client, gap)
            .await
            .expect("the remote serves the state where the line leaves the store");
    }))
    .await;
}

/// Whether a line reached its floor is read from the number its oldest revision
/// carries. An unreadable oldest revision numbered above the floor is not at it;
/// the next batch, which cannot read it either, ends the line.
#[tokio::test]
async fn reaching_the_floor_is_read_from_the_oldest_revision_number() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        let line = write_line(&repository, Hash::default(), 1, 3).await;

        assert!(
            history_reached_floor(&[], 0),
            "An empty line has nothing left to load"
        );
        assert!(
            history_reached_floor(&[(1, line[0])], 1),
            "Revision number 1 is at a floor of 1"
        );
        assert!(
            !history_reached_floor(&[(3, line[2])], 1),
            "Revision number 3 is above a floor of 1, so the line can still be followed"
        );

        let mut unreadable = vec![(3, revision(200))];
        assert!(
            !history_reached_floor(&unreadable, 1),
            "An unreadable revision numbered above the floor is not at it"
        );
        assert!(
            load_additional_history(repository, &mut unreadable)
                .await
                .expect("an unreadable revision is not a failure"),
            "The next batch cannot read the revision either, so the line has nothing left to load"
        );
        assert_eq!(unreadable, vec![(3, revision(200))]);
    }))
    .await;
}

/// A line whose first batch passes the older revision's number without reaching
/// it has diverged, which the search answers without loading another batch.
#[tokio::test]
async fn history_line_diverges_without_loading_past_the_older_revisions_number() {
    Box::pin(with_execution(async {
        let (stores, counting) = crate::paged_remote::counted_client_stores().await;
        let repository = stores.writer.clone();
        let shared = write_line(&repository, Hash::default(), 1, 2).await;
        let newer = write_revision(&repository, shared[1], 13).await;
        let older = write_revision(&repository, shared[1], 3).await;
        counting.take_reads();

        let found = find_revision_in_history_line(repository, newer, 13, older, 3)
            .await
            .expect("passing the older revision's number is not a failure");

        assert_eq!(found, HistoryLineSearch::Diverged);
        assert_eq!(
            counting.take_reads().payloads,
            3,
            "one state read for each revision of the newer line's first batch"
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
        let mut history = vec![(2, line[1]), (1, line[0])];

        assert!(
            load_additional_history(repository.clone(), &mut history)
                .await
                .expect("an exhausted line is not a failure"),
            "A line followed to its root revision has nothing further to load"
        );
        assert_eq!(
            history,
            vec![(2, line[1]), (1, line[0])],
            "An exhausted line must not grow"
        );

        let mut empty = Vec::new();
        assert!(
            load_additional_history(repository, &mut empty)
                .await
                .expect("an empty line is not a failure"),
            "There is nothing to extend an empty line from"
        );
        assert!(empty.is_empty());
    }))
    .await;
}

/// Extending a line that leaves the local store, with no remote to list it from
/// there, adds the revisions the store holds below the line's oldest revision, and
/// the next extension ends the line where it leaves the store.
#[tokio::test]
async fn extending_a_line_that_leaves_the_store_adds_the_revisions_it_holds() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        let line = write_line(&repository, revision(200), 1, 3).await;
        let mut history = vec![(3, line[2])];

        let ended = load_additional_history(repository.clone(), &mut history)
            .await
            .expect("a line that leaves the store is not a failure");
        let ended_again = load_additional_history(repository, &mut history)
            .await
            .expect("a line that leaves the store is not a failure");

        assert_eq!((ended, ended_again), (false, true));
        assert_eq!(history, vec![(3, line[2]), (2, line[1]), (1, line[0])]);
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

/// The name the name-claim tests claim, release and look up.
const CLAIMED_NAME: &str = "claimed";

/// The id the mapping of `name` holds in `repository`'s local store, `None` where it holds none.
async fn mapped(repository: &Arc<RepositoryContext>, name: &str) -> Option<BranchId> {
    match load_name_to_id_local(repository.clone(), name).await {
        Ok(id) => Some(id),
        Err(err) if err.is_branch_not_found() => None,
        Err(err) => panic!("reading the mapping of {name}: {err:?}"),
    }
}

/// Stores `latest` as the latest revision of the id derived from `name`, the shape of a branch
/// that takes its id from its name.
async fn store_derived_latest(repository: &Arc<RepositoryContext>, name: &str, latest: Hash) {
    mutable_try_store(
        repository.clone(),
        LATEST,
        fallback_id(name),
        Hash::default(),
        latest,
    )
    .await
    .expect("storing the derived id's latest");
}

/// The value of every name mapping in `repository`'s local store.
async fn name_entries(repository: &Arc<RepositoryContext>) -> Vec<BranchId> {
    let mut entries: Vec<BranchId> = lore_revision::branch::list(repository.clone())
        .await
        .expect("listing the name mappings")
        .collect()
        .await;
    entries.sort();
    entries
}

/// A repository on in-memory stores whose mutable store runs `hook` before each load, store,
/// compare-and-swap and list it forwards.
pub(crate) async fn hooked_repository(
    hook: impl Fn(StoreCall, Hash, KeyType) -> Result<(), StoreError> + Send + Sync + 'static,
) -> Arc<RepositoryContext> {
    answering_repository(hook, None).await
}

/// [`hooked_repository`] whose mutable store also runs `answered`, if any, after each load, store
/// and compare-and-swap it forwards that the store answers without an error.
async fn answering_repository(
    hook: impl Fn(StoreCall, Hash, KeyType) -> Result<(), StoreError> + Send + Sync + 'static,
    answered: Option<Answered>,
) -> Arc<RepositoryContext> {
    let stores = null_repository().await;
    Arc::new(RepositoryContext::new_null_context(
        stores.immutable_store(),
        HookedStore::wrap(
            stores
                .try_mutable_store_arc()
                .expect("a null context holds a write token"),
            hook,
            answered,
        ),
    ))
}

/// A client that can write, on in-memory stores, with `remote` as its remote or none, whose mutable
/// store runs `hook` before each load, store, compare-and-swap and list it forwards, and a context
/// over the same stores with no remote.
async fn hooked_client(
    remote: Option<Arc<PagedRemote>>,
    hook: impl Fn(StoreCall, Hash, KeyType) -> Result<(), StoreError> + Send + Sync + 'static,
) -> (Arc<RepositoryContext>, Arc<RepositoryContext>) {
    let stores = hooked_client_stores(hook).await;
    let client = if remote.is_some() {
        writing_client_on(&stores, remote)
    } else {
        stores.writer.clone()
    };
    (client, stores.writer)
}

/// A hook that answers a load of a key of type `missed` as not found while `misses` is above zero,
/// and counts it down.
fn missing_loads(
    missed: KeyType,
    misses: Arc<AtomicUsize>,
) -> impl Fn(StoreCall, Hash, KeyType) -> Result<(), StoreError> + Send + Sync + 'static {
    move |call: StoreCall, _key: Hash, key_type: KeyType| {
        if call == StoreCall::Load
            && key_type == missed
            && misses
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
        {
            return Err(StoreError::from(AddressNotFound { address: [0; 48] }));
        }
        Ok(())
    }
}

/// The calls a [`recording_hook`] saw, in order.
type RecordedCalls = Arc<Mutex<Vec<(StoreCall, KeyType)>>>;

/// A hook that records each call it sees and its key type, and the list it records them in.
fn recording_hook() -> (
    RecordedCalls,
    impl Fn(StoreCall, Hash, KeyType) -> Result<(), StoreError> + Send + Sync + 'static,
) {
    let calls = RecordedCalls::default();
    let recorded = calls.clone();
    let hook = move |call: StoreCall, _key: Hash, key_type: KeyType| -> Result<(), StoreError> {
        recorded
            .lock()
            .expect("the recorded calls")
            .push((call, key_type));
        Ok(())
    };
    (calls, hook)
}

/// Asserts that `calls` holds a write of branch metadata, a store or a compare-and-swap, and after
/// it a compare-and-swap of a name mapping.
#[track_caller]
fn assert_metadata_stored_before_the_claim(calls: &RecordedCalls) {
    let calls = calls.lock().expect("the recorded calls");
    let metadata = calls.iter().position(|&(call, key_type)| {
        matches!(call, StoreCall::Store | StoreCall::CompareAndSwap)
            && key_type == KeyType::BranchMetadata
    });
    let claim = calls
        .iter()
        .position(|&made| made == (StoreCall::CompareAndSwap, KeyType::BranchId));
    assert!(
        metadata.is_some() && claim.is_some() && metadata < claim,
        "got {calls:?}"
    );
}

/// A repository where `name` was claimed and then released, as an archive does, which removes
/// its mapping.
async fn archived_name_repository(name: &str) -> Arc<RepositoryContext> {
    let repository = null_repository().await;
    claim_name(repository.clone(), name, BranchId::default(), branch_id(1))
        .await
        .expect("an absent mapping is claimed");
    release_name(repository.clone(), name, branch_id(1))
        .await
        .expect("the holder releases its name");
    repository
}

/// A claim against an absent mapping takes the name, and a second claim that read it absent is
/// refused and leaves the first in place.
#[tokio::test]
async fn claim_name_against_absent_then_refuses_a_second_claim() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;

        claim_name(
            repository.clone(),
            CLAIMED_NAME,
            BranchId::default(),
            branch_id(1),
        )
        .await
        .expect("an absent mapping is claimed");
        let second = claim_name(
            repository.clone(),
            CLAIMED_NAME,
            BranchId::default(),
            branch_id(2),
        )
        .await;

        assert!(
            second
                .as_ref()
                .is_err_and(|err| err.is_branch_already_exists()),
            "got {second:?}"
        );
        assert_eq!(mapped(&repository, CLAIMED_NAME).await, Some(branch_id(1)));
    }))
    .await;
}

/// A release removes the mapping, and a claim that read the name as absent takes it.
#[tokio::test]
async fn claim_name_succeeds_over_an_archived_name() {
    Box::pin(with_execution(async {
        let repository = archived_name_repository(CLAIMED_NAME).await;
        assert_eq!(
            mapped(&repository, CLAIMED_NAME).await,
            None,
            "an archived name reads as no mapping"
        );

        claim_name(
            repository.clone(),
            CLAIMED_NAME,
            BranchId::default(),
            branch_id(2),
        )
        .await
        .expect("an archived name is claimed as an absent mapping");

        assert_eq!(mapped(&repository, CLAIMED_NAME).await, Some(branch_id(2)));
    }))
    .await;
}

/// A claim against the id it read replaces that mapping. One against an id the mapping no longer
/// holds, because it holds another id or nothing, archived or never written, is refused and writes
/// nothing.
#[tokio::test]
async fn claim_name_swaps_only_from_the_id_it_read() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        claim_name(
            repository.clone(),
            CLAIMED_NAME,
            BranchId::default(),
            branch_id(1),
        )
        .await
        .expect("an absent mapping is claimed");

        claim_name(repository.clone(), CLAIMED_NAME, branch_id(1), branch_id(2))
            .await
            .expect("the mapping read is replaced");
        assert_eq!(mapped(&repository, CLAIMED_NAME).await, Some(branch_id(2)));

        let archived = archived_name_repository(CLAIMED_NAME).await;
        let unwritten = null_repository().await;
        for (held, repository) in [
            ("another id", repository),
            ("an archived name", archived),
            ("nothing", unwritten),
        ] {
            let before = name_entries(&repository).await;
            let claimed =
                claim_name(repository.clone(), CLAIMED_NAME, branch_id(1), branch_id(3)).await;
            assert!(
                claimed
                    .as_ref()
                    .is_err_and(|err| err.is_branch_already_exists()),
                "a mapping holding {held}: got {claimed:?}"
            );
            assert_eq!(name_entries(&repository).await, before, "{held}");
        }
    }))
    .await;
}

/// A claim over a mapping that already names the claiming branch succeeds and leaves the mapping
/// as it is, whatever value the claim read.
#[tokio::test]
async fn claim_name_succeeds_when_the_mapping_names_the_claiming_branch() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        claim_name(
            repository.clone(),
            CLAIMED_NAME,
            BranchId::default(),
            branch_id(1),
        )
        .await
        .expect("an absent mapping is claimed");

        for read in [BranchId::default(), branch_id(2)] {
            claim_name(repository.clone(), CLAIMED_NAME, read, branch_id(1))
                .await
                .expect("the mapping already names the claiming branch");
            assert_eq!(
                name_entries(&repository).await,
                [branch_id(1)],
                "read {read:?}"
            );
        }
    }))
    .await;
}

/// A claim for branch zero fails `InvalidArguments` and leaves the mapping as it is, absent or
/// held.
#[tokio::test]
async fn claim_name_for_branch_zero_fails_invalid_arguments() {
    Box::pin(with_execution(async {
        let held = null_repository().await;
        claim_name(
            held.clone(),
            CLAIMED_NAME,
            BranchId::default(),
            branch_id(1),
        )
        .await
        .expect("an absent mapping is claimed");

        for (read, repository) in [
            (BranchId::default(), null_repository().await),
            (branch_id(1), held),
        ] {
            let before = name_entries(&repository).await;
            let claimed =
                claim_name(repository.clone(), CLAIMED_NAME, read, BranchId::default()).await;
            assert!(
                claimed
                    .as_ref()
                    .is_err_and(|err| err.is_invalid_arguments()),
                "read {read:?}: got {claimed:?}"
            );
            assert_eq!(name_entries(&repository).await, before, "read {read:?}");
        }
    }))
    .await;
}

/// A release of a name with no mapping, never written or archived, fails `BranchNotFound` and
/// writes nothing.
#[tokio::test]
async fn release_name_refuses_a_name_with_no_mapping() {
    Box::pin(with_execution(async {
        for (shape, repository) in [
            ("never written", null_repository().await),
            (
                "an archived name",
                archived_name_repository(CLAIMED_NAME).await,
            ),
        ] {
            let before = name_entries(&repository).await;
            let released = release_name(repository.clone(), CLAIMED_NAME, branch_id(1)).await;
            assert!(
                released
                    .as_ref()
                    .is_err_and(|err| err.is_branch_not_found()),
                "a mapping {shape}: got {released:?}"
            );
            assert_eq!(name_entries(&repository).await, before, "a mapping {shape}");
        }
    }))
    .await;
}

/// A release by branch zero fails `BranchNotFound` and writes nothing, over a name never written
/// or archived.
#[tokio::test]
async fn release_name_by_branch_zero_fails_and_writes_nothing() {
    Box::pin(with_execution(async {
        for (shape, repository) in [
            ("never written", null_repository().await),
            (
                "an archived name",
                archived_name_repository(CLAIMED_NAME).await,
            ),
        ] {
            let before = name_entries(&repository).await;
            let released =
                release_name(repository.clone(), CLAIMED_NAME, BranchId::default()).await;
            assert!(
                released
                    .as_ref()
                    .is_err_and(|err| err.is_branch_not_found()),
                "a mapping {shape}: got {released:?}"
            );
            assert_eq!(name_entries(&repository).await, before, "a mapping {shape}");
        }
    }))
    .await;
}

/// A claim and a release on a context without a write token fail with `WriteRequired`.
#[tokio::test]
async fn claim_name_and_release_name_need_a_write_token() {
    Box::pin(with_execution(async {
        let repository = Arc::new(null_repository().await.without_command_locks());

        let claimed = claim_name(
            repository.clone(),
            CLAIMED_NAME,
            BranchId::default(),
            branch_id(1),
        )
        .await;
        let released = release_name(repository, CLAIMED_NAME, branch_id(1)).await;

        assert!(
            claimed.as_ref().is_err_and(|err| err.is_write_required()),
            "claim: got {claimed:?}"
        );
        assert!(
            released.as_ref().is_err_and(|err| err.is_write_required()),
            "release: got {released:?}"
        );
    }))
    .await;
}

/// A claim and a release whose compare-and-swap of the name's mapping fails report the store's
/// error.
#[tokio::test]
async fn claim_name_and_release_name_report_a_failed_swap() {
    Box::pin(with_execution(async {
        let repository = hooked_repository(|call, _key, key_type| match (call, key_type) {
            (StoreCall::CompareAndSwap, KeyType::BranchId) => Err(StoreError::from(Disconnected)),
            _ => Ok(()),
        })
        .await;

        let claimed = claim_name(
            repository.clone(),
            CLAIMED_NAME,
            BranchId::default(),
            branch_id(1),
        )
        .await;
        let released = release_name(repository, CLAIMED_NAME, branch_id(1)).await;

        assert!(
            claimed.as_ref().is_err_and(|err| err.is_disconnected()),
            "claim: got {claimed:?}"
        );
        assert!(
            released.as_ref().is_err_and(|err| err.is_disconnected()),
            "release: got {released:?}"
        );
    }))
    .await;
}

/// A release by a branch the mapping no longer names is refused and leaves the mapping alone.
#[tokio::test]
async fn release_name_leaves_a_mapping_that_names_another_branch() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        claim_name(
            repository.clone(),
            CLAIMED_NAME,
            BranchId::default(),
            branch_id(2),
        )
        .await
        .expect("an absent mapping is claimed");

        let released = release_name(repository.clone(), CLAIMED_NAME, branch_id(1)).await;

        assert!(
            released
                .as_ref()
                .is_err_and(|err| err.is_branch_not_found()),
            "got {released:?}"
        );
        assert_eq!(mapped(&repository, CLAIMED_NAME).await, Some(branch_id(2)));
    }))
    .await;
}

/// A lookup that answers the id derived from the name leaves the name unmapped.
#[tokio::test]
async fn lookup_name_local_unwritten_writes_nothing() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        store_derived_latest(&repository, CLAIMED_NAME, revision(1)).await;

        let found = lookup_name_local_unwritten(repository.clone(), CLAIMED_NAME)
            .await
            .expect("the lookup reads the local store");

        assert_eq!(found, Some(NamedBranch::Derived(fallback_id(CLAIMED_NAME))));
        assert_eq!(
            name_entries(&repository).await,
            Vec::<BranchId>::new(),
            "the lookup leaves the name unmapped"
        );
    }))
    .await;
}

/// A name with no mapping, never written or archived, names the id derived from it when that id
/// has a latest revision, and a claim against the answer's mapping value takes the name.
#[tokio::test]
async fn lookup_name_local_unwritten_answers_the_derived_id_for_an_absent_key() {
    Box::pin(with_execution(async {
        for (shape, repository) in [
            ("never written", null_repository().await),
            (
                "an archived name",
                archived_name_repository(CLAIMED_NAME).await,
            ),
        ] {
            store_derived_latest(&repository, CLAIMED_NAME, revision(1)).await;

            let found = lookup_name_local_unwritten(repository.clone(), CLAIMED_NAME)
                .await
                .expect("the lookup reads the local store")
                .expect("the derived id names the branch");

            assert_eq!(
                found,
                NamedBranch::Derived(fallback_id(CLAIMED_NAME)),
                "a mapping {shape}"
            );
            claim_name(
                repository.clone(),
                CLAIMED_NAME,
                found.mapping(),
                branch_id(2),
            )
            .await
            .expect("a claim against the mapping value read takes the name");
            assert_eq!(
                mapped(&repository, CLAIMED_NAME).await,
                Some(branch_id(2)),
                "a mapping {shape}"
            );
        }
    }))
    .await;
}

/// A name with no mapping names no branch when the id derived from it has no latest revision.
#[tokio::test]
async fn lookup_name_local_unwritten_answers_none_without_a_derived_latest() {
    Box::pin(with_execution(async {
        let found = lookup_name_local_unwritten(null_repository().await, CLAIMED_NAME)
            .await
            .expect("the lookup reads the local store");

        assert_eq!(found, None);
    }))
    .await;
}

/// A held mapping answers the id it holds, the derived id included, ahead of an id derived from
/// the name that has a latest revision, and a claim against the answer's mapping value takes the
/// name.
#[tokio::test]
async fn lookup_name_local_unwritten_answers_a_held_mapping() {
    Box::pin(with_execution(async {
        for held in [branch_id(1), fallback_id(CLAIMED_NAME)] {
            let repository = null_repository().await;
            claim_name(repository.clone(), CLAIMED_NAME, BranchId::default(), held)
                .await
                .expect("an absent mapping is claimed");
            store_derived_latest(&repository, CLAIMED_NAME, revision(1)).await;

            let found = lookup_name_local_unwritten(repository.clone(), CLAIMED_NAME)
                .await
                .expect("the lookup reads the local store")
                .expect("the mapping names the branch");

            assert_eq!(found, NamedBranch::Mapped(held));
            claim_name(
                repository.clone(),
                CLAIMED_NAME,
                found.mapping(),
                branch_id(2),
            )
            .await
            .expect("a claim against the mapping value read takes the name");
            assert_eq!(
                mapped(&repository, CLAIMED_NAME).await,
                Some(branch_id(2)),
                "held {held:?}"
            );
        }
    }))
    .await;
}

/// A failed read of the name's mapping, or of the latest revision of the id derived from the
/// name, fails the lookup.
#[tokio::test]
async fn lookup_name_local_unwritten_reports_a_failed_read() {
    Box::pin(with_execution(async {
        for failing in [KeyType::BranchId, KeyType::BranchLatestPointer] {
            let repository = hooked_repository(move |call, _key, key_type| match call {
                StoreCall::Load if key_type == failing => Err(StoreError::from(Disconnected)),
                _ => Ok(()),
            })
            .await;

            let found = lookup_name_local_unwritten(repository, CLAIMED_NAME).await;

            assert!(
                found.as_ref().is_err_and(|err| err.is_disconnected()),
                "a failed {failing:?} read: got {found:?}"
            );
        }
    }))
    .await;
}

/// Without a remote, a name with no mapping maps to the id derived from it when that id has a
/// latest revision, and the lookup caches the mapping.
#[tokio::test]
async fn load_name_to_id_answers_and_caches_the_derived_id() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        store_derived_latest(&repository, CLAIMED_NAME, revision(1)).await;

        let id = load_name_to_id(repository.clone(), CLAIMED_NAME)
            .await
            .expect("the derived id has a latest revision");

        assert_eq!(id, fallback_id(CLAIMED_NAME));
        assert_eq!(
            mapped(&repository, CLAIMED_NAME).await,
            Some(fallback_id(CLAIMED_NAME)),
            "the lookup caches the mapping"
        );
    }))
    .await;
}

/// Without a remote, a name with no mapping whose derived id has no latest revision is not found,
/// and the lookup writes no mapping.
#[tokio::test]
async fn load_name_to_id_without_a_derived_latest_finds_no_branch() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;

        let found = load_name_to_id(repository.clone(), CLAIMED_NAME).await;

        assert!(
            found.as_ref().is_err_and(|err| err.is_branch_not_found()),
            "got {found:?}"
        );
        assert_eq!(
            name_entries(&repository).await,
            Vec::<BranchId>::new(),
            "the lookup writes no mapping"
        );
    }))
    .await;
}

/// Creates the root branch `id` named `name` with [`create_on`].
async fn create_root(
    repository: &Arc<RepositoryContext>,
    id: BranchId,
    name: &str,
) -> Result<BranchId, BranchError> {
    create_on(repository, id, name, Vec::new()).await
}

/// Creates the branch `id` named `name`, with `stack` as its branch points, with [`create`], as a
/// file-system create does.
async fn create_on(
    repository: &Arc<RepositoryContext>,
    id: BranchId,
    name: &str,
    stack: Vec<BranchPoint>,
) -> Result<BranchId, BranchError> {
    create(
        repository.clone(),
        &in_memory::token(),
        id,
        name,
        default_category(),
        "creator",
        1,
        stack,
        false,
        false,
    )
    .await
}

/// Writes the record of the root branch `id` named [`CLAIMED_NAME`] at a latest revision, claiming
/// the name from `name_expected`.
async fn write_record(
    repository: &Arc<RepositoryContext>,
    id: BranchId,
    status: BranchLatestStatus,
    name_expected: BranchId,
) -> Result<(), BranchError> {
    create_record(
        repository.clone(),
        id,
        CLAIMED_NAME,
        default_category(),
        "creator",
        1,
        Vec::new(),
        revision(1),
        status,
        name_expected,
    )
    .await
}

/// Two creates of one name with different ids, run at once, leave one branch: one succeeds, the
/// other fails with `BranchAlreadyExists`, and the name maps to the one that succeeded. The creates
/// meet between their reads and their claims only by chance, so
/// `a_lost_create_rolls_back_its_metadata` is what checks a lost claim.
#[tokio::test]
async fn concurrent_creates_of_one_name_leave_one_winner() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        for round in 0..32u8 {
            let name = format!("concurrent-{round}");
            let mut creates = JoinSet::new();
            for id in [branch_id(2 * round + 1), branch_id(2 * round + 2)] {
                let repository = repository.clone();
                let name = name.clone();
                lore_spawn!(
                    creates,
                    async move { create_root(&repository, id, &name).await }
                );
            }
            let mut outcomes = Vec::new();
            while let Some(outcome) = creates.join_next().await {
                outcomes.push(outcome.expect("the create runs to its end"));
            }

            let won = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
            let lost = outcomes
                .iter()
                .filter(|outcome| {
                    outcome
                        .as_ref()
                        .is_err_and(|err| err.is_branch_already_exists())
                })
                .count();
            assert_eq!((won, lost), (1, 1), "{name}: got {outcomes:?}");
            let winner = outcomes.iter().find_map(|outcome| outcome.as_ref().ok());
            assert_eq!(mapped(&repository, &name).await.as_ref(), winner, "{name}");
        }
    }))
    .await;
}

/// A record write that loses its claim, because another branch claimed the name after the value
/// it claims from was read, fails with `BranchAlreadyExists` and leaves no record: its metadata
/// reads as absent, it has no latest revision, and the mapping names the other branch.
#[tokio::test]
async fn a_lost_create_rolls_back_its_metadata() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        claim_name(
            repository.clone(),
            CLAIMED_NAME,
            BranchId::default(),
            branch_id(1),
        )
        .await
        .expect("an absent mapping is claimed");

        let written = write_record(
            &repository,
            branch_id(2),
            BranchLatestStatus::Divergent,
            BranchId::default(),
        )
        .await;

        assert!(
            written
                .as_ref()
                .is_err_and(|err| err.is_branch_already_exists()),
            "got {written:?}"
        );
        assert_no_record(&repository, branch_id(2)).await;
        assert_eq!(mapped(&repository, CLAIMED_NAME).await, Some(branch_id(1)));
    }))
    .await;
}

/// A record write whose claim fails in the store fails with the store's error and leaves no
/// record.
#[tokio::test]
async fn a_record_write_whose_claim_fails_in_the_store_rolls_back_its_metadata() {
    Box::pin(with_execution(async {
        let repository = hooked_repository(|call, _key, key_type| match (call, key_type) {
            (StoreCall::CompareAndSwap, KeyType::BranchId) => Err(StoreError::from(Disconnected)),
            _ => Ok(()),
        })
        .await;

        let written = write_record(
            &repository,
            branch_id(2),
            BranchLatestStatus::Divergent,
            BranchId::default(),
        )
        .await;

        assert!(
            written.as_ref().is_err_and(|err| err.is_disconnected()),
            "got {written:?}"
        );
        assert_no_record(&repository, branch_id(2)).await;
    }))
    .await;
}

/// A record write whose claim fails leaves a metadata value another writer stored after the
/// record's metadata write.
#[tokio::test]
async fn a_failed_claim_leaves_metadata_stored_after_the_records() {
    Box::pin(with_execution(async {
        let armed = Arc::new(AtomicBool::new(true));
        let repository = answering_repository(
            |call, _key, key_type| match (call, key_type) {
                (StoreCall::CompareAndSwap, KeyType::BranchId) => {
                    Err(StoreError::from(Disconnected))
                }
                _ => Ok(()),
            },
            answered(
                move |inner: Arc<dyn MutableStore>, partition, call, key, key_type| {
                    let overwrite = call == StoreCall::CompareAndSwap
                        && key_type == KeyType::BranchMetadata
                        && armed.swap(false, Ordering::SeqCst);
                    async move {
                        if overwrite {
                            inner.store(partition, key, revision(9), key_type).await
                        } else {
                            Ok(())
                        }
                    }
                },
            ),
        )
        .await;

        let written = write_record(
            &repository,
            branch_id(2),
            BranchLatestStatus::Divergent,
            BranchId::default(),
        )
        .await;

        assert!(
            written.as_ref().is_err_and(|err| err.is_disconnected()),
            "got {written:?}"
        );
        assert_eq!(
            metadata_hash(repository.clone(), branch_id(2)).await.ok(),
            Some(revision(9))
        );
    }))
    .await;
}

/// A record write for branch zero fails `InvalidArguments` and writes nothing.
#[tokio::test]
async fn create_record_of_branch_zero_fails_and_writes_nothing() {
    Box::pin(with_execution(async {
        let (calls, hook) = recording_hook();
        let repository = hooked_repository(hook).await;

        let written = write_record(
            &repository,
            BranchId::default(),
            BranchLatestStatus::Divergent,
            BranchId::default(),
        )
        .await;

        assert!(
            written
                .as_ref()
                .is_err_and(|err| err.is_invalid_arguments()),
            "got {written:?}"
        );
        let calls = calls.lock().expect("the recorded calls");
        assert!(
            calls
                .iter()
                .all(|&(call, _)| !matches!(call, StoreCall::Store | StoreCall::CompareAndSwap)),
            "got {calls:?}"
        );
    }))
    .await;
}

/// A record write for an id whose metadata key holds a value, as another create of the id leaves
/// it, fails with `BranchAlreadyExists` and writes nothing: the value stays, the name has no
/// mapping, and the id has no latest revision.
#[tokio::test]
async fn a_record_write_for_an_id_with_metadata_writes_nothing() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        mutable_store_metadata(repository.clone(), branch_id(2), revision(9))
            .await
            .expect("storing the other create's metadata");

        let written = write_record(
            &repository,
            branch_id(2),
            BranchLatestStatus::Divergent,
            BranchId::default(),
        )
        .await;

        assert!(
            written
                .as_ref()
                .is_err_and(|err| err.is_branch_already_exists()),
            "got {written:?}"
        );
        assert_eq!(
            metadata_hash(repository.clone(), branch_id(2)).await.ok(),
            Some(revision(9))
        );
        assert_eq!(mapped(&repository, CLAIMED_NAME).await, None);
        let latest = load_latest_local(repository.clone(), branch_id(2)).await;
        assert!(
            latest.as_ref().is_err_and(|err| err.is_branch_not_found()),
            "latest: got {latest:?}"
        );
    }))
    .await;
}

/// Asserts that branch `id` has no record: its metadata reads as absent, and it has no latest
/// revision.
async fn assert_no_record(repository: &Arc<RepositoryContext>, id: BranchId) {
    let metadata = metadata_hash(repository.clone(), id).await;
    assert!(
        metadata
            .as_ref()
            .is_err_and(|err| err.is_branch_not_found()),
        "metadata: got {metadata:?}"
    );
    let latest = load_latest_local(repository.clone(), id).await;
    assert!(
        latest.as_ref().is_err_and(|err| err.is_branch_not_found()),
        "latest: got {latest:?}"
    );
}

/// A create of a name whose mapping names a branch with no metadata claims the name from that
/// mapping.
#[tokio::test]
async fn create_over_a_stale_mapping_claims_the_name() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        claim_name(
            repository.clone(),
            CLAIMED_NAME,
            BranchId::default(),
            branch_id(1),
        )
        .await
        .expect("an absent mapping is claimed");

        create_root(&repository, branch_id(2), CLAIMED_NAME)
            .await
            .expect("a mapping to a branch with no metadata leaves the name unclaimed");

        assert_eq!(mapped(&repository, CLAIMED_NAME).await, Some(branch_id(2)));
    }))
    .await;
}

/// A create given a name that isn't valid, or a parent that's zero, the branch itself or a
/// personal branch, fails with `InvalidArguments` and maps no name.
#[tokio::test]
async fn a_create_with_an_invalid_name_or_parent_fails_with_invalid_arguments() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        create(
            repository.clone(),
            &in_memory::token(),
            branch_id(1),
            "personal",
            personal_category(),
            "creator",
            1,
            Vec::new(),
            false,
            false,
        )
        .await
        .expect("the personal branch is created");
        let parent = |branch| {
            vec![BranchPoint {
                branch,
                revision: revision(1),
            }]
        };

        for (name, stack) in [
            ("", Vec::new()),
            (CLAIMED_NAME, parent(BranchId::default())),
            (CLAIMED_NAME, parent(branch_id(2))),
            (CLAIMED_NAME, parent(branch_id(1))),
        ] {
            let before = name_entries(&repository).await;
            let created = create_on(&repository, branch_id(2), name, stack.clone()).await;
            assert!(
                created
                    .as_ref()
                    .is_err_and(|err| err.is_invalid_arguments()),
                "{name:?} off {stack:?}: got {created:?}"
            );
            assert_eq!(
                name_entries(&repository).await,
                before,
                "{name:?} off {stack:?}"
            );
        }
    }))
    .await;
}

/// A create on an instance given a name that isn't valid, or run on a personal branch, fails with
/// `InvalidArguments`.
#[tokio::test]
async fn an_instance_create_with_an_invalid_name_or_on_a_personal_branch_fails_with_invalid_arguments()
 {
    Box::pin(LORE_CONTEXT.scope(setup_test_execution(), async {
        let (repository, _tempdir) = committed_repository("", &[]).await;

        let invalid_name = create_branch_on_instance(&repository, "", None, "", false).await;
        create_branch_on_instance(&repository, "personal", None, personal_category(), false)
            .await
            .expect("the personal branch is created and becomes current");
        let on_personal =
            create_branch_on_instance(&repository, CLAIMED_NAME, None, "", false).await;

        let created = [
            ("an invalid name", invalid_name),
            ("on a personal branch", on_personal),
        ];
        assert!(
            created.iter().all(|(_, result)| {
                result.as_ref().is_err_and(|err| err.is_invalid_arguments())
            }),
            "{created:?}"
        );
    }))
    .await;
}

/// A create on an instance given a zero id fails with `InvalidArguments`: in a dry run too, and
/// over a branch an older version made with id zero, whose metadata it leaves as it is.
#[tokio::test]
async fn an_instance_create_with_a_zero_id_fails_with_invalid_arguments() {
    Box::pin(LORE_CONTEXT.scope(setup_test_execution(), async {
        let (repository, _tempdir) = committed_repository("", &[]).await;
        let zero = BranchId::default();

        let plain =
            create_branch_on_instance(&repository, CLAIMED_NAME, Some(zero), "", false).await;
        let dry_run = Box::pin(LORE_CONTEXT.scope(
            test_execution_with(LoreGlobalArgs {
                dry_run: 1,
                ..Default::default()
            }),
            create_branch_on_instance(&repository, CLAIMED_NAME, Some(zero), "", false),
        ))
        .await;
        let (restorable, _restorable_dir) = committed_repository("", &[]).await;
        let mut metadata = lore_revision::metadata::Metadata::new();
        metadata_populate(
            &mut metadata,
            zero,
            CLAIMED_NAME,
            default_category(),
            "creator",
            1,
            Vec::new(),
        )
        .expect("populating the metadata");
        let metadata = metadata
            .serialize(restorable.clone())
            .await
            .expect("storing the metadata");
        mutable_store_metadata(restorable.clone(), zero, metadata)
            .await
            .expect("storing the metadata hash");
        mutable_try_store(
            restorable.clone(),
            LATEST,
            zero,
            Hash::default(),
            revision(1),
        )
        .await
        .expect("storing the latest revision");
        let over_branch =
            create_branch_on_instance(&restorable, CLAIMED_NAME, Some(zero), "", false).await;

        let created = [
            ("a zero id", plain),
            ("a zero id in a dry run", dry_run),
            ("a zero id over a branch", over_branch),
        ];
        let kept = metadata_hash(restorable.clone(), zero).await.ok() == Some(metadata);
        assert!(
            kept && created.iter().all(|(_, result)| {
                result.as_ref().is_err_and(|err| err.is_invalid_arguments())
            }),
            "the branch with id zero keeps its metadata: {kept}, {created:?}"
        );
    }))
    .await;
}

/// A create of a name with no mapping, whose derived id has a latest revision and no metadata,
/// claims the name from the mapping its lookup caches for the derived id.
#[tokio::test]
async fn create_over_a_derived_id_with_a_latest_and_no_metadata_claims_the_name() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        store_derived_latest(&repository, CLAIMED_NAME, revision(1)).await;

        create_root(&repository, branch_id(2), CLAIMED_NAME)
            .await
            .expect("a derived id with no metadata leaves the name unclaimed");

        assert_eq!(mapped(&repository, CLAIMED_NAME).await, Some(branch_id(2)));
    }))
    .await;
}

/// A create stores the branch's metadata before it claims the name.
#[tokio::test]
async fn a_mapping_is_never_written_before_its_metadata() {
    Box::pin(with_execution(async {
        let (calls, hook) = recording_hook();
        let repository = hooked_repository(hook).await;

        create_root(&repository, branch_id(1), CLAIMED_NAME)
            .await
            .expect("an unclaimed name is created");

        assert_metadata_stored_before_the_claim(&calls);
    }))
    .await;
}

/// A create whose lookup finds no mapping, and whose claim then meets one that names a branch with
/// metadata, as another create can write between the two, fails with `BranchAlreadyExists` and
/// leaves that mapping. The lookup's miss is a read of the mapping that answers not found.
#[tokio::test]
async fn a_create_refuses_a_name_claimed_after_its_lookup() {
    Box::pin(with_execution(async {
        let misses = Arc::new(AtomicUsize::new(0));
        let repository = hooked_repository(missing_loads(KeyType::BranchId, misses.clone())).await;
        create_root(&repository, branch_id(1), CLAIMED_NAME)
            .await
            .expect("an unclaimed name is created");
        misses.store(1, Ordering::SeqCst);

        let created = create_root(&repository, branch_id(2), CLAIMED_NAME).await;

        assert!(
            created
                .as_ref()
                .is_err_and(|err| err.is_branch_already_exists()),
            "got {created:?}"
        );
        assert_eq!(
            misses.load(Ordering::SeqCst),
            0,
            "the lookup read the mapping"
        );
        assert_eq!(mapped(&repository, CLAIMED_NAME).await, Some(branch_id(1)));
    }))
    .await;
}

/// Two creates of one id under two names, the second of which reads the id's metadata as absent
/// before the first stores it, leave the first: the second fails with `BranchAlreadyExists`, the
/// id keeps the first's metadata, and the second's name has no mapping. The second's miss is a
/// read of the metadata that answers not found.
#[tokio::test]
async fn a_create_refuses_an_id_created_after_its_metadata_read() {
    Box::pin(with_execution(async {
        let misses = Arc::new(AtomicUsize::new(0));
        let repository =
            hooked_repository(missing_loads(KeyType::BranchMetadata, misses.clone())).await;
        create_root(&repository, branch_id(2), "first")
            .await
            .expect("an unclaimed name is created");
        let first = metadata_hash(repository.clone(), branch_id(2))
            .await
            .expect("the create stores the id's metadata");
        misses.store(1, Ordering::SeqCst);

        let created = create_root(&repository, branch_id(2), CLAIMED_NAME).await;

        let Err(BranchError::BranchAlreadyExists(exists)) = &created else {
            panic!("got {created:?}");
        };
        assert_eq!(
            exists.branch,
            branch_id(2).to_string(),
            "the error names the id that exists, not the name it was to have"
        );
        assert_eq!(
            misses.load(Ordering::SeqCst),
            0,
            "the create read the id's metadata"
        );
        assert_eq!(
            metadata_hash(repository.clone(), branch_id(2)).await.ok(),
            Some(first)
        );
        assert_eq!(mapped(&repository, CLAIMED_NAME).await, None);
        assert_eq!(mapped(&repository, "first").await, Some(branch_id(2)));
    }))
    .await;
}

/// Creates the branch `id` named `name` at a latest revision, then archives it.
async fn archive_branch(repository: &Arc<RepositoryContext>, id: BranchId, name: &str) {
    create_on(repository, id, name, vec![branch_point(9, 9)])
        .await
        .expect("an unclaimed name is created");
    lore_revision::branch::delete(repository.clone(), id)
        .await
        .expect("the branch is archived");
}

/// A create that renames a branch archived before archives recorded their time, whose name was
/// released and whose metadata holds neither time, restores it: the metadata records an archive
/// time first, then the restore time, and the new name maps to the branch.
#[tokio::test]
async fn a_restore_of_a_branch_archived_without_a_time_records_both_times() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        create_on(
            &repository,
            branch_id(2),
            "archived",
            vec![branch_point(9, 9)],
        )
        .await
        .expect("an unclaimed name is created");
        release_name(repository.clone(), "archived", branch_id(2))
            .await
            .expect("the branch releases its name");

        create_root(&repository, branch_id(2), CLAIMED_NAME)
            .await
            .expect("the archived branch is restored");

        let metadata = lore_revision::branch::metadata(repository.clone(), branch_id(2))
            .await
            .expect("the branch has metadata");
        assert!(metadata.get_u64(ARCHIVED).is_ok(), "an archive time");
        assert!(metadata.get_u64(RESTORED).is_ok(), "a restore time");
        assert_eq!(mapped(&repository, CLAIMED_NAME).await, Some(branch_id(2)));
    }))
    .await;
}

/// The time the metadata of branch `id` holds as its last restore.
async fn restored_at(repository: &Arc<RepositoryContext>, id: BranchId) -> u64 {
    lore_revision::branch::metadata(repository.clone(), id)
        .await
        .expect("the branch has metadata")
        .get_u64(RESTORED)
        .expect("the metadata holds a restore time")
}

/// A metadata set of any key an archive or a restore writes, or a clear of one by name, fails with
/// `InvalidArguments` before it reads anything: the keys decide what a later archive or restore of
/// the branch does.
#[tokio::test]
async fn a_metadata_write_refuses_the_archive_keys() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        for &key in lore_revision::metadata::branch::ARCHIVE_KEYS {
            let set = lore_revision::metadata::branch::set_boxed(
                repository.clone(),
                branch_id(1),
                &[key.as_bytes()],
                &[b"1"],
                &[lore_revision::metadata::MetadataType::String],
            )
            .await;
            assert!(
                matches!(
                    set,
                    Err(lore_revision::metadata::branch::BranchMetadataError::InvalidArguments(_))
                ),
                "{key}: got {set:?}"
            );
            let clear = lore_revision::metadata::branch::clear_boxed(
                repository.clone(),
                branch_id(1),
                &[key],
            )
            .await;
            assert!(
                matches!(
                    clear,
                    Err(lore_revision::metadata::branch::BranchMetadataError::InvalidArguments(_))
                ),
                "{key}: got {clear:?}"
            );
        }
    }))
    .await;
}

/// A create of an archived branch under its name records the time of the restore in the branch's
/// metadata, in milliseconds since the Unix epoch, and a restore after a later archive records a
/// time no earlier: the archive removes the first time, so two restores in one millisecond record
/// the same time.
#[tokio::test]
async fn a_restore_records_its_time_in_the_branch_metadata() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        archive_branch(&repository, branch_id(2), CLAIMED_NAME).await;
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is past the epoch")
            .as_millis() as u64;

        create_root(&repository, branch_id(2), CLAIMED_NAME)
            .await
            .expect("an archived branch is restored");
        let first = restored_at(&repository, branch_id(2)).await;
        lore_revision::branch::delete(repository.clone(), branch_id(2))
            .await
            .expect("the branch is archived again");
        create_root(&repository, branch_id(2), CLAIMED_NAME)
            .await
            .expect("an archived branch is restored again");

        assert!(first >= before, "restored at {first}, before {before}");
        let second = restored_at(&repository, branch_id(2)).await;
        assert!(
            second >= first,
            "restored again at {second}, first at {first}"
        );
    }))
    .await;
}

/// An archive records its time in the branch's metadata and removes the restore time, a restore
/// keeps the archive time, and a later archive records a later time.
#[tokio::test]
async fn an_archive_records_its_time_and_a_restore_keeps_it() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        create_on(
            &repository,
            branch_id(2),
            CLAIMED_NAME,
            vec![branch_point(9, 9)],
        )
        .await
        .expect("an unclaimed name is created");
        let read = async |repository: &Arc<RepositoryContext>| {
            let metadata = lore_revision::branch::metadata(repository.clone(), branch_id(2))
                .await
                .expect("the branch has metadata");
            (
                metadata.get_u64(ARCHIVED).ok(),
                metadata.get_u64(RESTORED).ok(),
            )
        };

        lore_revision::branch::delete(repository.clone(), branch_id(2))
            .await
            .expect("the branch is archived");
        let (Some(archived), None) = read(&repository).await else {
            panic!("got {:?}", read(&repository).await);
        };
        create_root(&repository, branch_id(2), CLAIMED_NAME)
            .await
            .expect("an archived branch is restored");
        let (kept, restored) = read(&repository).await;
        lore_revision::branch::delete(repository.clone(), branch_id(2))
            .await
            .expect("the branch is archived again");

        assert_eq!(kept, Some(archived));
        assert!(restored.is_some());
        let (rearchived, unrestored) = read(&repository).await;
        assert!(
            rearchived.is_some_and(|time| time > archived),
            "got {rearchived:?} after {archived}"
        );
        assert_eq!(unrestored, None);
    }))
    .await;
}

/// A create that renames an archived branch without a latest revision, as one made from an empty
/// default branch is, fails with `BranchAlreadyExists` naming the branch and writes nothing: the
/// branch keeps its metadata, and the name stays unclaimed. A create under the old name restores
/// it.
#[tokio::test]
async fn a_rename_of_an_archived_branch_without_a_latest_revision_fails() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        create_root(&repository, branch_id(2), "archived")
            .await
            .expect("an unclaimed name is created");
        release_name(repository.clone(), "archived", branch_id(2))
            .await
            .expect("the branch releases its name");
        let archived = metadata_hash(repository.clone(), branch_id(2))
            .await
            .expect("an archived branch keeps its metadata");

        let renamed = create_root(&repository, branch_id(2), CLAIMED_NAME).await;

        let Err(BranchError::BranchAlreadyExists(exists)) = &renamed else {
            panic!("got {renamed:?}");
        };
        assert_eq!(exists.branch, branch_id(2).to_string());
        assert_eq!(
            metadata_hash(repository.clone(), branch_id(2)).await.ok(),
            Some(archived)
        );
        assert_eq!(mapped(&repository, CLAIMED_NAME).await, None);
        create_root(&repository, branch_id(2), "archived")
            .await
            .expect("a create under the old name restores the branch");
        assert_eq!(mapped(&repository, "archived").await, Some(branch_id(2)));
    }))
    .await;
}

/// A create of an archived branch under its name, whose mapping names a branch with no metadata,
/// claims the name from that mapping.
#[tokio::test]
async fn a_restore_claims_the_name_from_a_stale_mapping() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        archive_branch(&repository, branch_id(2), CLAIMED_NAME).await;
        claim_name(
            repository.clone(),
            CLAIMED_NAME,
            BranchId::default(),
            branch_id(3),
        )
        .await
        .expect("an archived name is claimed");

        create_root(&repository, branch_id(2), CLAIMED_NAME)
            .await
            .expect("a mapping to a branch with no metadata leaves the name unclaimed");

        assert_eq!(mapped(&repository, CLAIMED_NAME).await, Some(branch_id(2)));
    }))
    .await;
}

/// A create of an archived branch under its name, whose lookup finds no mapping and whose claim
/// then meets one that names a live branch, as another create can write between the two, fails
/// with `BranchAlreadyExists` and leaves that mapping. The lookup's miss is a read of the mapping
/// that answers not found.
#[tokio::test]
async fn a_restore_fails_when_the_name_is_claimed_after_its_lookup() {
    Box::pin(with_execution(async {
        let misses = Arc::new(AtomicUsize::new(0));
        let repository = hooked_repository(missing_loads(KeyType::BranchId, misses.clone())).await;
        archive_branch(&repository, branch_id(2), CLAIMED_NAME).await;
        create_root(&repository, branch_id(1), CLAIMED_NAME)
            .await
            .expect("an archived name is claimed");
        misses.store(1, Ordering::SeqCst);

        let restored = create_root(&repository, branch_id(2), CLAIMED_NAME).await;

        assert!(
            restored
                .as_ref()
                .is_err_and(|err| err.is_branch_already_exists()),
            "got {restored:?}"
        );
        assert_eq!(
            misses.load(Ordering::SeqCst),
            0,
            "the lookup read the mapping"
        );
        assert_eq!(mapped(&repository, CLAIMED_NAME).await, Some(branch_id(1)));
    }))
    .await;
}

/// A create that restores an archived branch under another name, and loses the claim of that name
/// to a branch that claimed it after the create's lookup, sets the archived branch's metadata back
/// to the value it read.
#[tokio::test]
async fn a_restore_that_loses_its_claim_sets_its_metadata_back() {
    Box::pin(with_execution(async {
        let misses = Arc::new(AtomicUsize::new(0));
        let repository = hooked_repository(missing_loads(KeyType::BranchId, misses.clone())).await;
        archive_branch(&repository, branch_id(2), "archived").await;
        create_root(&repository, branch_id(1), CLAIMED_NAME)
            .await
            .expect("an unclaimed name is created");
        let archived = metadata_hash(repository.clone(), branch_id(2))
            .await
            .expect("an archived branch keeps its metadata");
        misses.store(1, Ordering::SeqCst);

        let restored = create_root(&repository, branch_id(2), CLAIMED_NAME).await;

        assert!(
            restored
                .as_ref()
                .is_err_and(|err| err.is_branch_already_exists()),
            "got {restored:?}"
        );
        assert_eq!(
            misses.load(Ordering::SeqCst),
            0,
            "the lookup read the mapping"
        );
        assert_eq!(
            metadata_hash(repository.clone(), branch_id(2)).await.ok(),
            Some(archived)
        );
        assert_eq!(mapped(&repository, CLAIMED_NAME).await, Some(branch_id(1)));
    }))
    .await;
}

/// A create that restores an archived branch under another name, and whose claim of that name
/// lands in the store before the claim fails, fails with the store's error and leaves the restore
/// recorded: the name maps to the branch, and its metadata names the old name, says it is archived
/// and records the restore. The next create of the branch settles it and restores the branch.
#[tokio::test]
async fn a_restore_whose_claim_lands_before_it_fails_is_settled_by_the_next_restore() {
    Box::pin(with_execution(async {
        let losing = Arc::new(AtomicBool::new(false));
        let lose = losing.clone();
        let repository = answering_repository(
            |_call, _key, _key_type| Ok(()),
            answered(move |_inner, _partition, call, _key, key_type| {
                let lost = lose.load(Ordering::SeqCst)
                    && call == StoreCall::CompareAndSwap
                    && key_type == KeyType::BranchId;
                std::future::ready(if lost {
                    Err(StoreError::from(Disconnected))
                } else {
                    Ok(())
                })
            }),
        )
        .await;
        archive_branch(&repository, branch_id(2), "archived").await;
        losing.store(true, Ordering::SeqCst);

        let restored = create_root(&repository, branch_id(2), CLAIMED_NAME).await;

        assert!(
            restored.as_ref().is_err_and(|err| err.is_disconnected()),
            "got {restored:?}"
        );
        assert_eq!(mapped(&repository, CLAIMED_NAME).await, Some(branch_id(2)));
        let metadata = lore_revision::branch::metadata(repository.clone(), branch_id(2))
            .await
            .expect("the archived branch keeps its metadata");
        assert_eq!(
            lore_revision::branch::name(&metadata).ok(),
            Some("archived")
        );
        assert!(
            metadata.get_u64(ARCHIVED).is_ok() && metadata.get_u64(RESTORED).is_err(),
            "the metadata says the branch is archived"
        );
        assert_eq!(metadata.get_string(RESTORING).ok(), Some(CLAIMED_NAME));

        losing.store(false, Ordering::SeqCst);
        create_root(&repository, branch_id(2), CLAIMED_NAME)
            .await
            .expect("the next restore settles the first and restores the branch");
        let metadata = lore_revision::branch::metadata(repository.clone(), branch_id(2))
            .await
            .expect("the restored branch has metadata");
        assert_eq!(
            lore_revision::branch::name(&metadata).ok(),
            Some(CLAIMED_NAME)
        );
        assert!(metadata.get_u64(RESTORED).is_ok(), "the branch is restored");
        assert!(
            metadata.get_string(RESTORING).is_err(),
            "no restore is recorded"
        );
        assert_eq!(mapped(&repository, CLAIMED_NAME).await, Some(branch_id(2)));
    }))
    .await;
}

/// A repository whose mutable store answers the `failing`th compare-and-swap of a branch metadata
/// key after `armed` is set with `Disconnected`, once the swap has landed.
async fn failing_metadata_swap(armed: Arc<AtomicBool>, failing: usize) -> Arc<RepositoryContext> {
    let seen = Arc::new(AtomicUsize::new(0));
    answering_repository(
        |_call, _key, _key_type| Ok(()),
        answered(move |_inner, _partition, call, _key, key_type| {
            let fails = armed.load(Ordering::SeqCst)
                && call == StoreCall::CompareAndSwap
                && key_type == KeyType::BranchMetadata
                && seen.fetch_add(1, Ordering::SeqCst) + 1 == failing;
            std::future::ready(if fails {
                Err(StoreError::from(Disconnected))
            } else {
                Ok(())
            })
        }),
    )
    .await
}

/// A restore whose first metadata swap, which records it, lands and then fails, fails with the
/// store's error and claims nothing. The next create of the branch settles it and restores the
/// branch. An archive of the branch settles it too, and fails with `BranchNotFound`, as the branch
/// stays archived.
#[tokio::test]
async fn a_restore_whose_record_lands_before_it_fails_is_settled_later() {
    Box::pin(with_execution(async {
        for settle_by_archive in [false, true] {
            let armed = Arc::new(AtomicBool::new(false));
            let repository = failing_metadata_swap(armed.clone(), 1).await;
            archive_branch(&repository, branch_id(2), "archived").await;
            armed.store(true, Ordering::SeqCst);

            let restored = create_root(&repository, branch_id(2), CLAIMED_NAME).await;

            assert!(
                restored.as_ref().is_err_and(|err| err.is_disconnected()),
                "got {restored:?}"
            );
            assert_eq!(mapped(&repository, CLAIMED_NAME).await, None);
            let settled = if settle_by_archive {
                lore_revision::branch::delete(repository.clone(), branch_id(2)).await
            } else {
                create_root(&repository, branch_id(2), CLAIMED_NAME)
                    .await
                    .map(|_| ())
            };
            if settle_by_archive {
                assert!(
                    settled.as_ref().is_err_and(|err| err.is_branch_not_found()),
                    "got {settled:?}"
                );
            } else {
                assert!(settled.is_ok(), "got {settled:?}");
            }
            let metadata = lore_revision::branch::metadata(repository.clone(), branch_id(2))
                .await
                .expect("the branch has metadata");
            assert!(
                metadata.get_string(RESTORING).is_err(),
                "no restore is recorded"
            );
            assert_eq!(
                mapped(&repository, CLAIMED_NAME).await,
                (!settle_by_archive).then(|| branch_id(2))
            );
        }
    }))
    .await;
}

/// A restore whose final swap lands and then fails reads the metadata again, finds the swap's value
/// there, and succeeds: the branch is restored under its name.
#[tokio::test]
async fn a_restore_whose_final_swap_lands_before_it_fails_succeeds() {
    Box::pin(with_execution(async {
        let armed = Arc::new(AtomicBool::new(false));
        let repository = failing_metadata_swap(armed.clone(), 2).await;
        archive_branch(&repository, branch_id(2), "archived").await;
        armed.store(true, Ordering::SeqCst);

        create_root(&repository, branch_id(2), CLAIMED_NAME)
            .await
            .expect("a final swap found in the store succeeds");

        assert_eq!(mapped(&repository, CLAIMED_NAME).await, Some(branch_id(2)));
        let metadata = lore_revision::branch::metadata(repository.clone(), branch_id(2))
            .await
            .expect("the restored branch has metadata");
        assert_eq!(
            lore_revision::branch::name(&metadata).ok(),
            Some(CLAIMED_NAME)
        );
        assert!(metadata.get_u64(RESTORED).is_ok(), "the branch is restored");
    }))
    .await;
}

/// A branch archived by this version whose name maps to it again, as an older version's restore
/// leaves it, reads as live: a create of it fails with `BranchAlreadyExists`, and an archive
/// archives it again and releases the name.
#[tokio::test]
async fn an_archived_branch_whose_name_maps_to_it_again_reads_as_live() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        archive_branch(&repository, branch_id(2), CLAIMED_NAME).await;
        claim_name(
            repository.clone(),
            CLAIMED_NAME,
            BranchId::default(),
            branch_id(2),
        )
        .await
        .expect("the name maps to the branch again");

        let restored = create_root(&repository, branch_id(2), CLAIMED_NAME).await;
        assert!(
            restored
                .as_ref()
                .is_err_and(|err| err.is_branch_already_exists()),
            "got {restored:?}"
        );
        lore_revision::branch::delete(repository.clone(), branch_id(2))
            .await
            .expect("the branch is archived again");

        assert_eq!(mapped(&repository, CLAIMED_NAME).await, None);
    }))
    .await;
}

/// An archive of a live branch whose metadata still holds a restore's record, as an older
/// version's restore under another name leaves it, settles the record, then archives the branch:
/// its name maps to nothing afterwards, and no record stays.
#[tokio::test]
async fn an_archive_of_a_live_branch_with_a_stale_restore_record_archives_it() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        archive_branch(&repository, branch_id(2), CLAIMED_NAME).await;
        create_root(&repository, branch_id(2), CLAIMED_NAME)
            .await
            .expect("the branch is restored");
        let mut metadata = lore_revision::branch::metadata(repository.clone(), branch_id(2))
            .await
            .expect("the restored branch has metadata");
        metadata
            .set_string(RESTORING, "elsewhere")
            .expect("recording a stale restore");
        let stale = metadata
            .serialize(repository.clone())
            .await
            .expect("storing the metadata");
        mutable_store_metadata(repository.clone(), branch_id(2), stale)
            .await
            .expect("storing the metadata hash");

        lore_revision::branch::delete(repository.clone(), branch_id(2))
            .await
            .expect("the branch is archived");

        assert_eq!(mapped(&repository, CLAIMED_NAME).await, None);
        let metadata = lore_revision::branch::metadata(repository.clone(), branch_id(2))
            .await
            .expect("the archived branch keeps its metadata");
        assert!(metadata.get_string(RESTORING).is_err(), "no record stays");
    }))
    .await;
}

/// A branch restored by this version whose name an older version's archive released, keeping the
/// metadata's restore time, is restored by a create: the metadata says it is live only while its
/// name maps to it.
#[tokio::test]
async fn a_restored_branch_whose_name_an_older_version_released_is_restored() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        archive_branch(&repository, branch_id(2), CLAIMED_NAME).await;
        create_root(&repository, branch_id(2), CLAIMED_NAME)
            .await
            .expect("the branch is restored");
        release_name(repository.clone(), CLAIMED_NAME, branch_id(2))
            .await
            .expect("an older version's archive releases the name");

        create_root(&repository, branch_id(2), "renamed")
            .await
            .expect("the branch is restored again");

        assert_eq!(mapped(&repository, "renamed").await, Some(branch_id(2)));
        let metadata = lore_revision::branch::metadata(repository.clone(), branch_id(2))
            .await
            .expect("the restored branch has metadata");
        assert_eq!(lore_revision::branch::name(&metadata).ok(), Some("renamed"));
    }))
    .await;
}

/// A create that renames a branch whose metadata reads as live, as an older version's archive
/// leaves it, asks the remote for the old name once: the lookup that finds the name unmapped also
/// serves as the rename's check of it.
#[tokio::test]
async fn a_rename_of_a_branch_reading_as_live_looks_its_old_name_up_once() {
    Box::pin(with_execution(async {
        let stores = client_stores().await;
        archive_branch(&stores.writer, branch_id(2), CLAIMED_NAME).await;
        create_root(&stores.writer, branch_id(2), CLAIMED_NAME)
            .await
            .expect("the branch is restored");
        release_name(stores.writer.clone(), CLAIMED_NAME, branch_id(2))
            .await
            .expect("an older version's archive releases the name");
        let remote = Arc::new(PagedRemote::new(BranchId::default(), Hash::default()));
        let client = writing_client_on(&stores, Some(remote.clone()));

        create_root(&client, branch_id(2), "renamed")
            .await
            .expect("the branch is restored again");

        let queried = remote.queried_names();
        let old_name_lookups = queried
            .iter()
            .filter(|queried| queried.eq_ignore_ascii_case(CLAIMED_NAME))
            .count();
        assert_eq!(old_name_lookups, 1, "the remote was asked for {queried:?}");
        assert_eq!(mapped(&stores.writer, "renamed").await, Some(branch_id(2)));
    }))
    .await;
}

/// A create of an archived branch whose metadata read fails, other than as not found, fails with
/// that error and writes nothing: the branch keeps its metadata, and the name stays unclaimed.
#[tokio::test]
async fn a_restore_whose_metadata_read_fails_writes_nothing() {
    Box::pin(with_execution(async {
        let failing = Arc::new(AtomicBool::new(false));
        let fail = failing.clone();
        let repository = hooked_repository(move |call, _key, key_type| {
            if fail.load(Ordering::SeqCst)
                && call == StoreCall::Load
                && key_type == KeyType::BranchMetadata
            {
                return Err(StoreError::from(Disconnected));
            }
            Ok(())
        })
        .await;
        archive_branch(&repository, branch_id(2), "archived").await;
        let archived = metadata_hash(repository.clone(), branch_id(2))
            .await
            .expect("an archived branch keeps its metadata");
        failing.store(true, Ordering::SeqCst);

        let restored = create_root(&repository, branch_id(2), CLAIMED_NAME).await;

        failing.store(false, Ordering::SeqCst);
        assert!(
            restored.as_ref().is_err_and(|err| err.is_disconnected()),
            "got {restored:?}"
        );
        assert_eq!(
            metadata_hash(repository.clone(), branch_id(2)).await.ok(),
            Some(archived)
        );
        assert_eq!(mapped(&repository, CLAIMED_NAME).await, None);
    }))
    .await;
}

/// A create of a branch whose metadata hash names bytes that are not metadata fails and writes
/// nothing: the hash stays, and the name stays unclaimed.
#[tokio::test]
async fn a_restore_over_unreadable_metadata_writes_nothing() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        store_unreadable_metadata(&repository, branch_id(2)).await;
        let unreadable = metadata_hash(repository.clone(), branch_id(2))
            .await
            .expect("the metadata hash is stored");

        let restored = create_root(&repository, branch_id(2), CLAIMED_NAME).await;

        assert!(
            restored.as_ref().is_err_and(|err| err.is_internal()),
            "got {restored:?}"
        );
        assert_eq!(
            metadata_hash(repository.clone(), branch_id(2)).await.ok(),
            Some(unreadable)
        );
        assert_eq!(mapped(&repository, CLAIMED_NAME).await, None);
    }))
    .await;
}

/// A dry-run create of an archived branch under another name answers the branch and writes
/// nothing: the branch keeps its metadata, and the name stays unclaimed.
#[tokio::test]
async fn a_dry_run_restore_writes_nothing() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        archive_branch(&repository, branch_id(2), "archived").await;
        let archived = metadata_hash(repository.clone(), branch_id(2))
            .await
            .expect("an archived branch keeps its metadata");

        let restored = create(
            repository.clone(),
            &in_memory::token(),
            branch_id(2),
            CLAIMED_NAME,
            default_category(),
            "creator",
            1,
            Vec::new(),
            true,
            false,
        )
        .await;

        assert_eq!(
            restored.as_ref().ok(),
            Some(&branch_id(2)),
            "got {restored:?}"
        );
        assert_eq!(
            metadata_hash(repository.clone(), branch_id(2)).await.ok(),
            Some(archived)
        );
        assert_eq!(mapped(&repository, CLAIMED_NAME).await, None);
    }))
    .await;
}

/// A create that restores an archived branch under another name, whose old name maps to another
/// branch since, restores it: the new name maps to it, and the old name keeps its mapping.
#[tokio::test]
async fn a_restore_whose_old_name_maps_to_another_branch_succeeds() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        archive_branch(&repository, branch_id(2), "archived").await;
        claim_name(
            repository.clone(),
            "archived",
            BranchId::default(),
            branch_id(3),
        )
        .await
        .expect("an archived name is claimed");

        let restored = create_root(&repository, branch_id(2), CLAIMED_NAME).await;

        assert_eq!(
            restored.as_ref().ok(),
            Some(&branch_id(2)),
            "got {restored:?}"
        );
        assert_eq!(mapped(&repository, CLAIMED_NAME).await, Some(branch_id(2)));
        assert_eq!(mapped(&repository, "archived").await, Some(branch_id(3)));
    }))
    .await;
}

/// A create that restores an archived branch under another name, whose lookup of the old name the
/// remote refuses, fails with the refusal and writes nothing.
#[tokio::test]
async fn a_restore_reports_a_refused_lookup_of_its_old_name() {
    Box::pin(with_execution(async {
        let stores = client_stores().await;
        archive_branch(&stores.writer, branch_id(2), "archived").await;
        let archived = metadata_hash(stores.writer.clone(), branch_id(2))
            .await
            .expect("an archived branch keeps its metadata");
        let remote = PagedRemote::new(branch_id(7), Hash::default())
            .refusing(ProtocolError::from(NotAuthorized));
        let client = writing_client_on(&stores, Some(Arc::new(remote)));

        let restored = create_root(&client, branch_id(2), CLAIMED_NAME).await;

        assert!(
            restored.as_ref().is_err_and(|err| err.is_not_authorized()),
            "got {restored:?}"
        );
        assert_eq!(
            metadata_hash(stores.writer.clone(), branch_id(2))
                .await
                .ok(),
            Some(archived)
        );
        assert_eq!(mapped(&stores.writer, CLAIMED_NAME).await, None);
    }))
    .await;
}

/// A create that restores an archived branch under its own name, whose lookup of the name the
/// remote refuses, fails with the refusal and writes nothing.
#[tokio::test]
async fn a_restore_reports_a_refused_lookup_of_its_name() {
    Box::pin(with_execution(async {
        let stores = client_stores().await;
        archive_branch(&stores.writer, branch_id(2), "archived").await;
        let archived = metadata_hash(stores.writer.clone(), branch_id(2))
            .await
            .expect("an archived branch keeps its metadata");
        let remote = PagedRemote::new(branch_id(7), Hash::default())
            .refusing(ProtocolError::from(NotAuthorized));
        let client = writing_client_on(&stores, Some(Arc::new(remote)));

        let restored = create_root(&client, branch_id(2), "archived").await;

        assert!(
            restored.as_ref().is_err_and(|err| err.is_not_authorized()),
            "got {restored:?}"
        );
        assert_eq!(
            metadata_hash(stores.writer.clone(), branch_id(2))
                .await
                .ok(),
            Some(archived)
        );
        assert_eq!(mapped(&stores.writer, "archived").await, None);
    }))
    .await;
}

/// A create whose read of the metadata of the branch the name maps to fails, other than as not
/// found, fails with that error and leaves the mapping, whether the first read of the mapping found
/// it or the read again after the derived id answered the lookup.
#[tokio::test]
async fn a_create_fails_when_the_mapped_branchs_metadata_read_fails() {
    Box::pin(with_execution(async {
        for derived in [false, true] {
            let misses = Arc::new(AtomicUsize::new(0));
            let failing = Arc::new(Mutex::new(Hash::default()));
            let miss = missing_loads(KeyType::BranchId, misses.clone());
            let fail = failing.clone();
            let repository = hooked_repository(move |call, key, key_type| {
                if call == StoreCall::Load && key == *fail.lock().expect("the failing key") {
                    return Err(StoreError::from(Disconnected));
                }
                miss(call, key, key_type)
            })
            .await;
            create_root(&repository, branch_id(1), CLAIMED_NAME)
                .await
                .expect("an unclaimed name is created");
            if derived {
                store_derived_latest(&repository, CLAIMED_NAME, revision(1)).await;
                misses.store(1, Ordering::SeqCst);
            }
            *failing.lock().expect("the failing key") =
                mutable_key(repository.salt(), METADATA, repository.id, branch_id(1)).0;

            let created = create_root(&repository, branch_id(2), CLAIMED_NAME).await;

            assert!(
                created.as_ref().is_err_and(|err| err.is_disconnected()),
                "derived {derived}: got {created:?}"
            );
            assert_eq!(
                misses.load(Ordering::SeqCst),
                0,
                "derived {derived}: the lookup read the mapping"
            );
            assert_eq!(
                mapped(&repository, CLAIMED_NAME).await,
                Some(branch_id(1)),
                "derived {derived}"
            );
        }
    }))
    .await;
}

/// A create reads the name's local mapping once where the name maps to a branch without metadata
/// or to nothing anywhere, and again only after the remote or the derived id answers the lookup,
/// whose cache may have changed the mapping.
#[tokio::test]
async fn a_create_reads_the_name_mapping_again_only_after_a_lookup_answers() {
    Box::pin(with_execution(async {
        let mut reads = Vec::new();
        for shape in ["absent", "stale", "derived", "remote"] {
            let remote = (shape == "remote")
                .then(|| Arc::new(PagedRemote::new(branch_id(7), Hash::default())));
            let (calls, hook) = recording_hook();
            let (client, local) = hooked_client(remote, hook).await;
            match shape {
                "stale" => claim_name(
                    local.clone(),
                    CLAIMED_NAME,
                    BranchId::default(),
                    branch_id(1),
                )
                .await
                .expect("an absent mapping is claimed"),
                "derived" => store_derived_latest(&local, CLAIMED_NAME, revision(1)).await,
                _ => {}
            }
            calls.lock().expect("the recorded calls").clear();

            create_root(&client, branch_id(7), CLAIMED_NAME)
                .await
                .expect("a name without a live branch is created");

            let loads = calls
                .lock()
                .expect("the recorded calls")
                .iter()
                .filter(|&&made| made == (StoreCall::Load, KeyType::BranchId))
                .count();
            reads.push((shape, loads));
        }
        assert_eq!(
            reads,
            [("absent", 1), ("stale", 1), ("derived", 2), ("remote", 2)]
        );
    }))
    .await;
}

/// A create whose read of the name's local mapping fails, other than as not found, fails with that
/// error and claims nothing, whether the first read fails or the read again after the remote
/// answered the lookup.
#[tokio::test]
async fn a_create_fails_when_a_read_of_the_name_mapping_fails() {
    Box::pin(with_execution(async {
        let remote = Arc::new(PagedRemote::new(branch_id(7), Hash::default()));
        for (failing, remote, cached) in [(1, None, None), (2, Some(remote), Some(branch_id(7)))] {
            let loads = Arc::new(AtomicUsize::new(0));
            let (client, _) = hooked_client(remote, move |call, _key, key_type| {
                if call == StoreCall::Load
                    && key_type == KeyType::BranchId
                    && loads.fetch_add(1, Ordering::SeqCst) + 1 == failing
                {
                    return Err(StoreError::from(Disconnected));
                }
                Ok(())
            })
            .await;

            let created = create_root(&client, branch_id(7), CLAIMED_NAME).await;

            assert!(
                created.as_ref().is_err_and(|err| err.is_disconnected()),
                "read {failing}: got {created:?}"
            );
            assert_eq!(
                mapped(&client, CLAIMED_NAME).await,
                cached,
                "read {failing}: the mapping the lookup cached"
            );
            assert_no_record(&client, branch_id(7)).await;
        }
    }))
    .await;
}

/// A lookup whose read misses the name's mapping, which another branch claimed after the read,
/// answers the remote's mapping or the derived id and leaves the claim in place.
#[tokio::test]
async fn a_lookup_cache_leaves_a_claim_made_after_its_read() {
    Box::pin(with_execution(async {
        let remote = PagedRemote::new(branch_id(7), Hash::default()).metadata(revision(7));
        for (shape, remote, answer) in [
            ("derived", None, fallback_id(CLAIMED_NAME)),
            ("remote", Some(Arc::new(remote)), branch_id(7)),
        ] {
            let misses = Arc::new(AtomicUsize::new(0));
            let (client, local) =
                hooked_client(remote, missing_loads(KeyType::BranchId, misses.clone())).await;
            create_root(&local, branch_id(1), CLAIMED_NAME)
                .await
                .expect("an unclaimed name is created");
            store_derived_latest(&local, CLAIMED_NAME, revision(1)).await;
            misses.store(1, Ordering::SeqCst);

            let found = load_name_to_id(client.clone(), CLAIMED_NAME).await;

            assert_eq!(found.as_ref().ok(), Some(&answer), "{shape}: got {found:?}");
            assert_eq!(misses.load(Ordering::SeqCst), 0, "{shape}: the lookup read");
            assert_eq!(
                mapped(&client, CLAIMED_NAME).await,
                Some(branch_id(1)),
                "{shape}"
            );
        }
    }))
    .await;
}

/// A lookup that finds the name's mapping at the remote stores the remote's metadata of its branch
/// before it claims the name.
#[tokio::test]
async fn a_lookup_stores_the_remote_metadata_before_the_mapping() {
    Box::pin(with_execution(async {
        let (calls, hook) = recording_hook();
        let remote = PagedRemote::new(branch_id(7), Hash::default()).metadata(revision(7));
        let (client, _) = hooked_client(Some(Arc::new(remote)), hook).await;

        let found = load_name_to_id(client.clone(), CLAIMED_NAME).await;

        assert_eq!(found.as_ref().ok(), Some(&branch_id(7)), "got {found:?}");
        assert_metadata_stored_before_the_claim(&calls);
        assert_eq!(mapped(&client, CLAIMED_NAME).await, Some(branch_id(7)));
    }))
    .await;
}

/// A lookup that finds the name's mapping at the remote, and fails to swap the remote's metadata of
/// its branch into the local store, answers the branch and claims no mapping.
#[tokio::test]
async fn a_lookup_whose_metadata_store_fails_claims_no_mapping() {
    Box::pin(with_execution(async {
        let (calls, record) = recording_hook();
        let remote = PagedRemote::new(branch_id(7), Hash::default()).metadata(revision(7));
        let (client, _) = hooked_client(Some(Arc::new(remote)), move |call, key, key_type| {
            record(call, key, key_type)?;
            if call == StoreCall::CompareAndSwap && key_type == KeyType::BranchMetadata {
                return Err(StoreError::from(Disconnected));
            }
            Ok(())
        })
        .await;

        let found = load_name_to_id(client.clone(), CLAIMED_NAME).await;

        assert_eq!(found.as_ref().ok(), Some(&branch_id(7)), "got {found:?}");
        let claims = calls
            .lock()
            .expect("the recorded calls")
            .iter()
            .filter(|&&made| made == (StoreCall::CompareAndSwap, KeyType::BranchId))
            .count();
        assert_eq!(claims, 0, "the lookup claimed the name");
        assert_eq!(mapped(&client, CLAIMED_NAME).await, None);
    }))
    .await;
}

/// A record write stores the latest status it is given.
#[tokio::test]
async fn create_record_records_the_given_latest_status() {
    Box::pin(with_execution(async {
        for (label, status, divergent) in [
            ("divergent", BranchLatestStatus::Divergent, true),
            ("convergent", BranchLatestStatus::Convergent, false),
        ] {
            let repository = null_repository().await;

            write_record(&repository, branch_id(1), status, BranchId::default())
                .await
                .expect("an unclaimed name is written");

            let stored = load_latest_divergent(repository, branch_id(1))
                .await
                .expect("the latest status reads");
            assert_eq!(stored, divergent, "{label}");
        }
    }))
    .await;
}

/// The kind of each event a [`recording_execution`] sent, in order.
type SentEvents = Arc<Mutex<Vec<std::mem::Discriminant<LoreEvent>>>>;

/// A client execution context on `globals` that records the kind of each event it sends, other
/// than `End` and `Log`, and the list it records them in. The list is complete once the context's
/// dispatcher is drained.
fn recording_execution(globals: LoreGlobalArgs) -> (SentEvents, Arc<ExecutionContext>) {
    let events = SentEvents::default();
    let sink = events.clone();
    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
        if !matches!(event, LoreEvent::End(_) | LoreEvent::Log(_)) {
            sink.lock()
                .expect("the sent events")
                .push(std::mem::discriminant(event));
        }
    }));
    let execution = Arc::new(ExecutionContext::new_client(
        globals,
        EventDispatcher::new(callback),
    ));
    (events, execution)
}

/// How many of `events` are `BranchArchive` events.
fn archive_events(events: &SentEvents) -> usize {
    let archive = std::mem::discriminant(&LoreEvent::BranchArchive(LoreBranchArchiveEventData {
        name: String::new().into(),
    }));
    events
        .lock()
        .expect("the sent events")
        .iter()
        .filter(|&&sent| sent == archive)
        .count()
}

/// A record write sends no event.
#[tokio::test]
async fn create_record_sends_no_event() {
    let (events, execution) = recording_execution(LoreGlobalArgs::default());

    Box::pin(LORE_CONTEXT.scope(execution.clone(), async {
        let repository = null_repository().await;
        write_record(
            &repository,
            branch_id(1),
            BranchLatestStatus::Divergent,
            BranchId::default(),
        )
        .await
        .expect("an unclaimed name is written");
    }))
    .await;
    execution.dispatcher.drain().await;

    let events = events.lock().expect("the sent events");
    assert!(events.is_empty(), "got {events:?}");
}

/// A delete whose read finds the name mapped to the branch, and whose release then meets a mapping
/// another writer changed, fails `BranchNotFound` and leaves that mapping: one moved to another
/// branch, or one a concurrent delete of the branch released.
#[tokio::test]
async fn a_delete_fails_when_the_mapping_changed_after_its_read() {
    Box::pin(with_execution(async {
        for (changed, held) in [
            ("released", None),
            ("moved to another branch", Some(branch_id(3))),
        ] {
            let armed = Arc::new(AtomicBool::new(false));
            let arm = armed.clone();
            let repository = answering_repository(
                |_call, _key, _key_type| Ok(()),
                answered(
                    move |inner: Arc<dyn MutableStore>, partition, call, key, key_type| {
                        let change = call == StoreCall::Load
                            && key_type == KeyType::BranchId
                            && armed.swap(false, Ordering::SeqCst);
                        async move {
                            if change {
                                let value = held.map(Hash::from_context).unwrap_or_default();
                                inner.store(partition, key, value, key_type).await
                            } else {
                                Ok(())
                            }
                        }
                    },
                ),
            )
            .await;
            create_on(
                &repository,
                branch_id(2),
                CLAIMED_NAME,
                vec![branch_point(1, 1)],
            )
            .await
            .expect("an unclaimed name is created");
            arm.store(true, Ordering::SeqCst);

            let deleted = delete(repository.clone(), branch_id(2)).await;

            assert!(
                deleted.as_ref().is_err_and(|err| err.is_branch_not_found()),
                "{changed}: got {deleted:?}"
            );
            assert!(
                !arm.load(Ordering::SeqCst),
                "{changed}: the delete read the mapping"
            );
            assert_eq!(mapped(&repository, CLAIMED_NAME).await, held, "{changed}");
        }
    }))
    .await;
}

/// A delete of a branch archived already fails `BranchNotFound` and sends no `BranchArchive` event.
#[tokio::test]
async fn deleting_an_archived_branch_again_fails_branch_not_found() {
    let (events, execution) = recording_execution(LoreGlobalArgs::default());

    let again = Box::pin(LORE_CONTEXT.scope(execution.clone(), async {
        let repository = null_repository().await;
        create_on(
            &repository,
            branch_id(2),
            CLAIMED_NAME,
            vec![branch_point(1, 1)],
        )
        .await
        .expect("an unclaimed name is created");
        delete(repository.clone(), branch_id(2))
            .await
            .expect("a live branch is archived");
        delete(repository, branch_id(2)).await
    }))
    .await;
    execution.dispatcher.drain().await;

    assert!(
        again.as_ref().is_err_and(|err| err.is_branch_not_found()),
        "got {again:?}"
    );
    assert_eq!(
        archive_events(&events),
        1,
        "only the archive sends the event"
    );
}

/// An archive releases the name, keeps the branch's latest revision, and keeps its metadata with
/// an archive time added. One whose expected metadata hash isn't the one the key holds fails with
/// `BranchAlreadyExists` and writes nothing.
#[tokio::test]
async fn archive_keeps_metadata_and_latest() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        write_record(
            &repository,
            branch_id(1),
            BranchLatestStatus::Divergent,
            BranchId::default(),
        )
        .await
        .expect("an unclaimed name is written");
        let hash = metadata_hash(repository.clone(), branch_id(1))
            .await
            .expect("the record holds metadata");
        let metadata = lore_revision::branch::metadata(repository.clone(), branch_id(1))
            .await
            .expect("the record holds metadata");

        let stale = archive_record(
            repository.clone(),
            branch_id(1),
            CLAIMED_NAME,
            Hash::default(),
            metadata.clone(),
        )
        .await;
        assert!(
            stale
                .as_ref()
                .is_err_and(|err| err.is_branch_already_exists()),
            "got {stale:?}"
        );
        assert_eq!(mapped(&repository, CLAIMED_NAME).await, Some(branch_id(1)));

        archive_record(
            repository.clone(),
            branch_id(1),
            CLAIMED_NAME,
            hash,
            metadata,
        )
        .await
        .expect("the branch holds its name");

        assert_eq!(mapped(&repository, CLAIMED_NAME).await, None);
        let archived = lore_revision::branch::metadata(repository.clone(), branch_id(1))
            .await
            .expect("the archived branch keeps its metadata");
        assert_eq!(
            lore_revision::branch::name(&archived).ok(),
            Some(CLAIMED_NAME)
        );
        assert!(archived.get_u64(ARCHIVED).is_ok(), "an archive time");
        assert_eq!(
            load_latest_local(repository, branch_id(1)).await.ok(),
            Some(revision(1))
        );
    }))
    .await;
}

/// Stores in `writer`'s stores a branch named [`CLAIMED_NAME`] that [`delete`] archives, and
/// whose name the local store doesn't map. With `derived`, the branch's id is the one derived from
/// its name, and the local store holds its metadata and a latest revision. Otherwise only the
/// remote it answers maps the name and holds the branch, and the local store holds only the blob of
/// the branch's metadata. Answers the branch's id and the remote, if any.
async fn branch_named_elsewhere(
    writer: &Arc<RepositoryContext>,
    derived: bool,
) -> (BranchId, Option<Arc<PagedRemote>>) {
    let id = if derived {
        fallback_id(CLAIMED_NAME)
    } else {
        branch_id(7)
    };
    let mut metadata = lore_revision::metadata::Metadata::new();
    metadata_populate(
        &mut metadata,
        id,
        CLAIMED_NAME,
        default_category(),
        "creator",
        1,
        vec![branch_point(1, 1)],
    )
    .expect("populating the branch's metadata");
    let metadata = metadata
        .serialize(writer.clone())
        .await
        .expect("storing the branch's metadata");
    // Without the repository's metadata stored locally, the default-branch check asks the remote's
    // repository service, which a detached connection never answers.
    let repository_metadata = lore_revision::repository::metadata_store(
        writer.clone(),
        lore_revision::repository::RepositoryMetadata::default(),
    )
    .await
    .expect("storing the repository's metadata");
    lore_revision::repository::metadata_store_hash(writer.clone(), repository_metadata)
        .await
        .expect("storing the repository's metadata hash");
    if !derived {
        let remote = PagedRemote::new(id, revision(1)).metadata(metadata);
        return (id, Some(Arc::new(remote)));
    }
    mutable_store_metadata(writer.clone(), id, metadata)
        .await
        .expect("storing the branch's metadata hash");
    store_derived_latest(writer, CLAIMED_NAME, revision(1)).await;
    (id, None)
}

/// A delete of a branch whose name the local store doesn't map, and only the remote's mapping or
/// the id derived from the name names, succeeds and sends the event. A delete releases the mapping
/// its lookup caches, which leaves no entry for the name, and a dry run writes none.
#[tokio::test]
async fn a_delete_of_a_name_the_local_store_does_not_map_succeeds() {
    Box::pin(with_execution(async {
        for derived in [true, false] {
            for dry_run in [1, 0] {
                let case = format!("derived {derived}, dry run {dry_run}");
                let stores = client_stores().await;
                let (id, remote) = branch_named_elsewhere(&stores.writer, derived).await;

                let (events, execution) = recording_execution(LoreGlobalArgs {
                    dry_run,
                    ..Default::default()
                });
                let deleted = Box::pin(LORE_CONTEXT.scope(execution.clone(), async {
                    delete(writing_client_on(&stores, remote), id).await
                }))
                .await;
                execution.dispatcher.drain().await;

                assert!(deleted.is_ok(), "{case}: got {deleted:?}");
                assert_eq!(archive_events(&events), 1, "{case}");
                assert_eq!(
                    name_entries(&stores.writer).await,
                    Vec::<BranchId>::new(),
                    "{case}: no mapping of the name is left, cached or released"
                );
            }
        }
    }))
    .await;
}

/// A delete of a branch archived in the local store and live at the remote, as `lore branch archive
/// --local` leaves it, archives it locally again: the lookup caches the remote's mapping and
/// metadata, the archive reads the metadata again, and it ends archived with no mapping left.
#[tokio::test]
async fn a_delete_of_a_branch_archived_only_locally_archives_it_again() {
    Box::pin(with_execution(async {
        let stores = client_stores().await;
        let (id, remote) = branch_named_elsewhere(&stores.writer, false).await;
        let mut archived = lore_revision::metadata::Metadata::new();
        metadata_populate(
            &mut archived,
            id,
            CLAIMED_NAME,
            default_category(),
            "creator",
            1,
            vec![branch_point(1, 1)],
        )
        .expect("populating the branch's metadata");
        archived
            .set_u64(ARCHIVED, 1)
            .expect("recording the local archive");
        let archived = archived
            .serialize(stores.writer.clone())
            .await
            .expect("storing the branch's metadata");
        mutable_store_metadata(stores.writer.clone(), id, archived)
            .await
            .expect("storing the branch's local metadata hash");

        delete(writing_client_on(&stores, remote), id)
            .await
            .expect("the branch is archived again");

        assert_eq!(name_entries(&stores.writer).await, Vec::<BranchId>::new());
        let metadata = lore_revision::branch::metadata(stores.writer.clone(), id)
            .await
            .expect("the branch keeps its metadata");
        assert!(
            metadata.get_u64(ARCHIVED).is_ok_and(|time| time > 1),
            "the archive records its time"
        );
    }))
    .await;
}

/// A lookup that caches the remote's metadata of a branch whose local metadata records a stopped
/// restore settles that restore first: the name the restore claimed maps to nothing afterwards,
/// and the branch's own name maps to it as the remote says.
#[tokio::test]
async fn a_lookup_that_caches_remote_metadata_settles_a_stopped_local_restore() {
    Box::pin(with_execution(async {
        let stores = client_stores().await;
        let (id, remote) = branch_named_elsewhere(&stores.writer, false).await;
        let mut stopped = lore_revision::metadata::Metadata::new();
        metadata_populate(
            &mut stopped,
            id,
            CLAIMED_NAME,
            default_category(),
            "creator",
            1,
            vec![branch_point(1, 1)],
        )
        .expect("populating the branch's metadata");
        stopped.set_u64(ARCHIVED, 1).expect("recording the archive");
        stopped
            .set_string(RESTORING, "bee")
            .expect("recording the stopped restore");
        let stopped = stopped
            .serialize(stores.writer.clone())
            .await
            .expect("storing the branch's metadata");
        mutable_store_metadata(stores.writer.clone(), id, stopped)
            .await
            .expect("storing the branch's local metadata hash");
        claim_name(stores.writer.clone(), "bee", BranchId::default(), id)
            .await
            .expect("the stopped restore claimed its name");

        let found = load_name_to_id(writing_client_on(&stores, remote), CLAIMED_NAME)
            .await
            .expect("the remote maps the name");

        assert_eq!(found, id);
        assert_eq!(mapped(&stores.writer, "bee").await, None);
        assert_eq!(mapped(&stores.writer, CLAIMED_NAME).await, Some(id));
    }))
    .await;
}

/// A lookup that caches the remote's metadata of a branch swaps it into the local metadata key
/// from the value it read there, so a local write another writer makes after that read, as a
/// settle's is, stays, and the lookup still answers the branch.
#[tokio::test]
async fn a_lookup_cache_keeps_local_metadata_written_after_its_read() {
    Box::pin(with_execution(async {
        let armed = ArmedWrite::default();
        let stores = crate::paged_remote::answering_client_stores(
            |_call, _key, _key_type| Ok(()),
            writing_after_its_load(armed.clone()),
        )
        .await;
        let (id, remote) = branch_named_elsewhere(&stores.writer, false).await;
        let mut local = lore_revision::metadata::Metadata::new();
        metadata_populate(
            &mut local,
            id,
            CLAIMED_NAME,
            default_category(),
            "creator",
            1,
            vec![branch_point(1, 1)],
        )
        .expect("populating the branch's metadata");
        local.set_u64(ARCHIVED, 1).expect("recording an archive");
        let read = local
            .serialize(stores.writer.clone())
            .await
            .expect("storing the branch's local metadata");
        mutable_store_metadata(stores.writer.clone(), id, read)
            .await
            .expect("storing the branch's local metadata hash");
        local
            .set_string(RELEASED, "bee")
            .expect("marking a released name");
        let written = local
            .serialize(stores.writer.clone())
            .await
            .expect("storing the later local metadata");
        let client = writing_client_on(&stores, remote);
        let (key, _key_type) = mutable_key(client.salt(), METADATA, client.id, id);
        *armed.lock().expect("the arming lock") = Some((key, written));

        let found = load_name_to_id(client, CLAIMED_NAME)
            .await
            .expect("the remote maps the name");

        assert_eq!(found, id);
        assert_eq!(
            metadata_hash(stores.writer.clone(), id).await.ok(),
            Some(written)
        );
    }))
    .await;
}

/// A key and the value a [`writing_after_its_load`] hook stores under it after the next load of it.
type ArmedWrite = Arc<Mutex<Option<(Hash, Hash)>>>;

/// An `answered` hook that stores the value `armed` holds under its key once a load of that key is
/// answered, as another writer does right after the read, and then disarms.
fn writing_after_its_load(armed: ArmedWrite) -> Option<Answered> {
    answered(move |inner, partition, call, key, key_type| {
        let write = match (call, *armed.lock().expect("the arming lock")) {
            (StoreCall::Load, Some((armed_key, value))) if armed_key == key => Some(value),
            _ => None,
        };
        if write.is_some() {
            *armed.lock().expect("the arming lock") = None;
        }
        async move {
            if let Some(value) = write {
                inner.store(partition, key, value, key_type).await?;
            }
            Ok(())
        }
    })
}

/// A metadata read whose local metadata can't be read caches the remote's metadata hash with a
/// compare-and-swap from the hash it read, so metadata another writer stores locally after that
/// read stays.
#[tokio::test]
async fn a_metadata_read_keeps_local_metadata_written_after_its_read() {
    Box::pin(with_execution(async {
        let id = branch_id(7);
        let armed = ArmedWrite::default();
        let stores = crate::paged_remote::answering_client_stores(
            |_call, _key, _key_type| Ok(()),
            writing_after_its_load(armed.clone()),
        )
        .await;
        store_unreadable_metadata(&stores.writer, id).await;
        let remote = Arc::new(PagedRemote::new(id, Hash::default()).metadata(revision(9)));
        let client = writing_client_on(&stores, Some(remote));
        let (key, _key_type) = mutable_key(client.salt(), METADATA, client.id, id);
        let written = revision(8);
        *armed.lock().expect("the arming lock") = Some((key, written));

        let _ = lore_revision::branch::metadata(client, id).await;

        assert_eq!(
            metadata_hash(stores.writer.clone(), id).await.ok(),
            Some(written)
        );
    }))
    .await;
}

/// The name of the branch the reset and switch tests restore.
const TARGET_NAME: &str = "target";

/// [`lore_revision::repository::branch_switch`] of `repository` onto `branch`, with the local
/// latest revision, answering the FFI code of a failure.
async fn switch_to(repository: &Arc<RepositoryContext>, branch: BranchId) -> Result<(), i32> {
    let token = repository
        .try_write_token()
        .expect("The repository was opened for writing");
    Box::pin(lore_revision::repository::branch_switch(
        repository.clone(),
        token,
        branch.to_string(),
        BranchSwitchOptions {
            local: true,
            ..Default::default()
        },
    ))
    .await
    .map(|_| ())
    .map_err(|err| err.ffi_code())
}

/// A [`committed_repository`] on its default branch, holding the branch `target`, named
/// [`TARGET_NAME`], whose latest revision changes the file `seed.txt` and whose name maps to
/// another live branch. Answers the repository, its directory, the default branch, its latest
/// revision, and the target's latest revision.
async fn instance_whose_target_name_another_branch_holds(
    target: BranchId,
) -> (
    Arc<RepositoryContext>,
    lore_base::test_util::TempDir,
    BranchId,
    Hash,
    Hash,
) {
    let (repository, tempdir) = committed_repository("", &[]).await;
    let (latest, main) = lore_revision::instance::load_current_anchor_boxed(&repository)
        .await
        .expect("the committed revision is current");
    create_on(
        &repository,
        target,
        TARGET_NAME,
        vec![BranchPoint {
            branch: main,
            revision: latest,
        }],
    )
    .await
    .expect("an unclaimed name is created");
    switch_to(&repository, target)
        .await
        .expect("the instance switches onto the target");
    test_file_write(&tempdir.path().join("seed.txt"), b"target");
    stage_paths(&repository, tempdir.path(), &["."], true, false).await;
    lore_revision::commit::commit_boxed(
        repository.clone(),
        repository
            .try_write_token()
            .expect("The repository was opened for writing"),
        lore_revision::commit::CommitOptions::new("Target".to_string()),
    )
    .await
    .expect("the target's change commits");
    let (target_latest, _) = lore_revision::instance::load_current_anchor_boxed(&repository)
        .await
        .expect("the target's revision is current");
    switch_to(&repository, main)
        .await
        .expect("the instance switches back onto the default branch");
    create_root(&repository, branch_id(9), "elsewhere")
        .await
        .expect("an unclaimed name is created");
    claim_name(repository.clone(), TARGET_NAME, target, branch_id(9))
        .await
        .expect("the target branch holds its name");
    (repository, tempdir, main, latest, target_latest)
}

/// A branch reset or switch onto another branch, whose name maps to a third, live branch, succeeds
/// and leaves the mapping with the third branch, forced or not: neither maps a branch's name to it.
/// The switch moves the instance onto the branch's latest revision, and the reset leaves the
/// instance where it was: its anchors and its working tree.
#[tokio::test]
async fn reset_and_switch_onto_a_branch_whose_name_another_branch_holds_leave_the_name() {
    let target = branch_id(8);
    for command in ["reset", "switch"] {
        for force in [false, true] {
            let case = format!("{command}, force {force}");
            Box::pin(LORE_CONTEXT.scope(setup_test_execution(), async {
                let (repository, tempdir, main, latest, target_latest) =
                    instance_whose_target_name_another_branch_holds(target).await;
                let staged = lore_revision::instance::load_staged_revision(&repository)
                    .await
                    .expect("the staged anchor reads");
                let execution = if force {
                    forced_execution()
                } else {
                    setup_test_execution()
                };

                let outcome = LORE_CONTEXT
                    .scope(execution, async {
                        if command == "reset" {
                            reset::reset(
                                repository.clone(),
                                repository
                                    .try_write_token()
                                    .expect("The repository was opened for writing"),
                                target.to_string(),
                                target_latest.to_string(),
                            )
                            .await
                            .map_err(|err| err.ffi_code())
                        } else {
                            switch_to(&repository, target).await
                        }
                    })
                    .await;

                assert_eq!(outcome, Ok(()), "{case}");
                assert_eq!(
                    mapped(&repository, TARGET_NAME).await,
                    Some(branch_id(9)),
                    "{case}"
                );
                let switched = command == "switch";
                let (anchor, seed) = if switched {
                    ((target_latest, target), b"target".as_slice())
                } else {
                    ((latest, main), b"seed".as_slice())
                };
                assert_eq!(
                    lore_revision::instance::load_current_anchor_boxed(&repository)
                        .await
                        .ok(),
                    Some(anchor),
                    "{case}"
                );
                assert_eq!(
                    std::fs::read(tempdir.path().join("seed.txt"))
                        .ok()
                        .as_deref(),
                    Some(seed),
                    "{case}"
                );
                if !switched {
                    assert_eq!(
                        lore_revision::instance::load_staged_revision(&repository)
                            .await
                            .ok(),
                        Some(staged),
                        "{case}"
                    );
                }
            }))
            .await;
        }
    }
}

/// A claim unless another branch holds the name, whose read of the mapping fails other than as not
/// found, fails with that error and claims nothing.
#[tokio::test]
async fn claim_name_unless_held_fails_when_its_read_of_the_mapping_fails() {
    Box::pin(with_execution(async {
        let failing = Arc::new(AtomicBool::new(true));
        let fail = failing.clone();
        let repository = hooked_repository(move |call, _key, key_type| {
            if fail.load(Ordering::SeqCst)
                && call == StoreCall::Load
                && key_type == KeyType::BranchId
            {
                return Err(StoreError::from(Disconnected));
            }
            Ok(())
        })
        .await;

        let claimed = claim_name_unless_held(repository.clone(), CLAIMED_NAME, branch_id(2)).await;

        failing.store(false, Ordering::SeqCst);
        assert!(
            claimed.as_ref().is_err_and(|err| err.is_disconnected()),
            "got {claimed:?}"
        );
        assert_eq!(mapped(&repository, CLAIMED_NAME).await, None);
    }))
    .await;
}

/// A claim unless held of a name another branch claimed, before its read or between its read and
/// its claim, answers `false` and leaves the mapping naming that branch. A claim between the two
/// is a read of the mapping that answers not found.
#[tokio::test]
async fn claim_name_unless_held_skips_a_name_another_writer_claimed() {
    Box::pin(with_execution(async {
        for missed in [0, 1] {
            let misses = Arc::new(AtomicUsize::new(0));
            let repository =
                hooked_repository(missing_loads(KeyType::BranchId, misses.clone())).await;
            create_root(&repository, branch_id(1), CLAIMED_NAME)
                .await
                .expect("an unclaimed name is created");
            misses.store(missed, Ordering::SeqCst);

            let claimed =
                claim_name_unless_held(repository.clone(), CLAIMED_NAME, branch_id(2)).await;

            assert_eq!(
                claimed.as_ref().ok(),
                Some(&false),
                "missed {missed}: got {claimed:?}"
            );
            assert_eq!(misses.load(Ordering::SeqCst), 0, "missed {missed}");
            assert_eq!(
                mapped(&repository, CLAIMED_NAME).await,
                Some(branch_id(1)),
                "missed {missed}"
            );
        }
    }))
    .await;
}

/// A claim unless held of a name whose mapping names an id without metadata, or the claiming branch
/// itself, which has metadata, answers `true` and leaves the name mapped to the claiming branch.
#[tokio::test]
async fn claim_name_unless_held_replaces_a_stale_mapping() {
    Box::pin(with_execution(async {
        for (held, label) in [
            (branch_id(1), "an id without metadata"),
            (branch_id(2), "the claiming branch"),
        ] {
            let repository = null_repository().await;
            mutable_store_metadata(repository.clone(), branch_id(2), revision(2))
                .await
                .expect("storing the claiming branch's metadata");
            claim_name(repository.clone(), CLAIMED_NAME, BranchId::default(), held)
                .await
                .expect("an absent mapping is claimed");

            let claimed =
                claim_name_unless_held(repository.clone(), CLAIMED_NAME, branch_id(2)).await;

            assert_eq!(
                claimed.as_ref().ok(),
                Some(&true),
                "{label}: got {claimed:?}"
            );
            assert_eq!(
                mapped(&repository, CLAIMED_NAME).await,
                Some(branch_id(2)),
                "{label}"
            );
        }
    }))
    .await;
}

/// A dry-run claim unless held succeeds and writes nothing: over an archived name, and over a
/// mapping to an id without metadata. The dry run's store discards the claim and answers it with
/// the value the store holds.
#[tokio::test]
async fn a_dry_run_claim_unless_held_succeeds_and_writes_nothing() {
    for (case, archived) in [
        ("an archived name", true),
        ("a mapping to an id without metadata", false),
    ] {
        let (stores, before) = Box::pin(with_execution(async {
            let stores = client_stores().await;
            claim_name(
                stores.writer.clone(),
                CLAIMED_NAME,
                BranchId::default(),
                branch_id(1),
            )
            .await
            .expect("an absent mapping is claimed");
            if archived {
                release_name(stores.writer.clone(), CLAIMED_NAME, branch_id(1))
                    .await
                    .expect("the holder releases its name");
            }
            let before = name_entries(&stores.writer).await;
            (stores, before)
        }))
        .await;

        let execution = Arc::new(ExecutionContext::new_client(
            LoreGlobalArgs {
                dry_run: 1,
                ..Default::default()
            },
            EventDispatcher::no_dispatch(),
        ));
        let claimed = Box::pin(LORE_CONTEXT.scope(execution, async {
            let client = writing_client_on(&stores, None);
            claim_name_unless_held(client, CLAIMED_NAME, branch_id(2)).await
        }))
        .await;

        assert_eq!(
            claimed.as_ref().ok(),
            Some(&true),
            "{case}: got {claimed:?}"
        );
        let after = Box::pin(with_execution(name_entries(&stores.writer))).await;
        assert_eq!(after, before, "{case}");
    }
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
