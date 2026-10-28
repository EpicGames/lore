// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::error::SlowDown;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::branch::CachedRevisionItem;
use lore_revision::branch::CachedRevisionListHeader;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::state::StateData;
use lore_server::cache::revision::*;
use lore_server::grpc::get_write_token;
use lore_server::grpc::server::RevisionListAcceleration;
use lore_storage::StoreError;
use rand::random;

use crate::store::test_support::FailingLoadStore;
use crate::store::test_support::test_store_create;

const STEP_ONE_HUNDRED: u64 = 100;

fn item(number: u64) -> CachedRevisionItem {
    CachedRevisionItem {
        number,
        signature: Hash::default(),
        metadata: Hash::default(),
        state: StateData::default(),
    }
}

fn test_repository(
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Arc<RepositoryContext> {
    Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        random::<RepositoryId>(),
    ))
}

/// Serialize a linear `parent_self` chain numbered `1..=count`.
/// Signatures are oldest-first, so index `n - 1` holds revision `n`.
async fn serialize_linear_chain(repository: &Arc<RepositoryContext>, count: u64) -> Vec<Hash> {
    let write_token = get_write_token();
    let mut parent = Hash::default();
    let mut signatures = Vec::with_capacity(count as usize);
    for number in 1..=count {
        let state = State::new();
        state.set_parent_self(parent);
        state.set_revision_number(number);
        parent = state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("serialize state");
        signatures.push(parent);
    }
    signatures
}

/// Serialize a revision whose number jumps past its parent's, as a merge
/// against a higher-numbered branch produces.
async fn serialize_jump_revision(
    repository: &Arc<RepositoryContext>,
    parent: Hash,
    revision_number: u64,
) -> Arc<State> {
    let write_token = get_write_token();
    let state = State::new();
    state.set_parent_self(parent);
    state.set_revision_number(revision_number);
    state
        .serialize(repository.clone(), &write_token)
        .await
        .expect("serialize jump state");
    state
}

async fn load_state(repository: &Arc<RepositoryContext>, revision: Hash) -> Arc<State> {
    State::deserialize(repository.clone(), revision)
        .await
        .expect("deserialize state")
}

/// Read the revision sealed at `boundary`, or `None` when unsealed.
async fn load_step_key(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    boundary: u64,
) -> Option<Hash> {
    let (key, key_type) = branch::revision_step_key(
        repository::SALT_LORE,
        repository.id,
        branch,
        boundary,
        STEP_ONE_HUNDRED,
    );
    repository
        .clone()
        .read_mutable_store()
        .load(repository.id, key, key_type)
        .await
        .ok()
        .filter(|revision| !revision.is_zero())
}

mod load_cached_list {
    use super::*;

    #[tokio::test]
    async fn absent_entry_is_a_miss() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = test_repository(immutable_store, mutable_store);
            let branch = BranchId::from(uuid::Uuid::now_v7());

            assert!(
                load_cached_list(&repository, branch, 150, STEP_ONE_HUNDRED)
                    .await
                    .expect("absent entry is not an error")
                    .is_none()
            );
        }))
        .await;
    }

    #[tokio::test]
    async fn slow_down_is_reported_rather_than_read_as_a_miss() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = test_repository(
                immutable_store,
                FailingLoadStore::all(mutable_store, StoreError::from(SlowDown)),
            );
            let branch = BranchId::from(uuid::Uuid::now_v7());

            let Err(err) = load_cached_list(&repository, branch, 150, STEP_ONE_HUNDRED).await
            else {
                panic!("backpressure must not be read as a miss");
            };
            assert!(err.is_slow_down());
        }))
        .await;
    }

    #[tokio::test]
    async fn store_failure_is_reported() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = test_repository(
                immutable_store,
                FailingLoadStore::all(mutable_store, StoreError::internal("store unusable")),
            );
            let branch = BranchId::from(uuid::Uuid::now_v7());

            let Err(err) = load_cached_list(&repository, branch, 150, STEP_ONE_HUNDRED).await
            else {
                panic!("a failing store must not be read as a miss");
            };
            assert!(!err.is_slow_down() && !err.is_address_not_found());
        }))
        .await;
    }
}

mod try_backfill_segment {
    use super::*;

    #[tokio::test]
    async fn absent_skip_pointer_is_a_miss() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = test_repository(immutable_store, mutable_store);
            let branch = BranchId::from(uuid::Uuid::now_v7());

            assert!(
                try_backfill_segment(&repository, branch, 150, STEP_ONE_HUNDRED)
                    .await
                    .expect("an unsealed segment is not an error")
                    .is_none()
            );
        }))
        .await;
    }

    #[tokio::test]
    async fn slow_down_is_reported_rather_than_read_as_a_miss() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = test_repository(
                immutable_store,
                FailingLoadStore::all(mutable_store, StoreError::from(SlowDown)),
            );
            let branch = BranchId::from(uuid::Uuid::now_v7());

            let Err(err) = try_backfill_segment(&repository, branch, 150, STEP_ONE_HUNDRED).await
            else {
                panic!("backpressure must not be read as an unsealed segment");
            };
            assert!(err.is_slow_down());
        }))
        .await;
    }

    #[tokio::test]
    async fn store_failure_is_reported() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = test_repository(
                immutable_store,
                FailingLoadStore::all(mutable_store, StoreError::internal("store unusable")),
            );
            let branch = BranchId::from(uuid::Uuid::now_v7());

            let Err(err) = try_backfill_segment(&repository, branch, 150, STEP_ONE_HUNDRED).await
            else {
                panic!("a failing store must not be read as an unsealed segment");
            };
            assert!(!err.is_slow_down() && !err.is_address_not_found());
        }))
        .await;
    }
}

mod partition_into_segments {
    use super::*;

    #[test]
    fn empty_input_returns_empty() {
        assert!(partition_into_segments(&[], 100).is_empty());
    }

    #[test]
    fn single_segment() {
        // Items 200..101 all live in segment 200 (div_ceil(N, 100) * 100).
        let items: Vec<_> = (101..=200).rev().map(item).collect();
        let segments = partition_into_segments(&items, 100);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].0, 200);
        assert_eq!(segments[0].1.len(), 100);
        assert_eq!(segments[0].1[0].number, 200);
        assert_eq!(segments[0].1[99].number, 101);
    }

    #[test]
    fn splits_at_segment_boundary() {
        // 101 lives in segment 200, 100 lives in segment 100, 1 in segment 100.
        let items: Vec<_> = [101, 100, 1].iter().map(|&n| item(n)).collect();
        let segments = partition_into_segments(&items, 100);
        assert_eq!(segments.len(), 2);
        // Walk order: highest segment first.
        assert_eq!(segments[0].0, 200);
        assert_eq!(segments[0].1.len(), 1);
        assert_eq!(segments[0].1[0].number, 101);
        assert_eq!(segments[1].0, 100);
        assert_eq!(segments[1].1.len(), 2);
        assert_eq!(segments[1].1[0].number, 100);
        assert_eq!(segments[1].1[1].number, 1);
    }

    #[test]
    fn single_item_at_segment_top() {
        // Revision 100 sits at the top of segment 100, not segment 200.
        let segments = partition_into_segments(&[item(100)], 100);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].0, 100);
    }

    #[test]
    fn handles_run_of_segments() {
        // Span four segments worth of items in walk order.
        let items: Vec<_> = (1..=350).rev().map(item).collect();
        let segments = partition_into_segments(&items, 100);
        // Segments: 400 (350..301), 300 (300..201), 200 (200..101), 100 (100..1).
        assert_eq!(segments.len(), 4);
        assert_eq!(segments[0].0, 400);
        assert_eq!(segments[0].1.len(), 50);
        assert_eq!(segments[1].0, 300);
        assert_eq!(segments[1].1.len(), 100);
        assert_eq!(segments[2].0, 200);
        assert_eq!(segments[2].1.len(), 100);
        assert_eq!(segments[3].0, 100);
        assert_eq!(segments[3].1.len(), 100);
    }
}

mod sealed_boundaries {
    use super::*;

    /// The boundaries a caller actually seals: multiples of the step
    /// size across the returned inclusive range.
    fn boundaries(older: u64, newer: u64, step_size: u64) -> Vec<u64> {
        match lore_server::cache::revision::sealed_boundaries(older, newer, step_size) {
            Some((lowest_b, highest_b)) => {
                (lowest_b..=highest_b).step_by(step_size as usize).collect()
            }
            None => Vec::new(),
        }
    }

    #[test]
    fn no_boundary_between_consecutive_revisions_inside_a_segment() {
        assert_eq!(sealed_boundaries(101, 102, STEP_ONE_HUNDRED), None);
    }

    #[test]
    fn landing_exactly_on_a_boundary_does_not_seal_it() {
        // The segment holding the branch head is always the open one, so
        // pushing revision 100 leaves boundary 100 unsealed until the
        // next push moves past it.
        assert_eq!(sealed_boundaries(99, 100, STEP_ONE_HUNDRED), None);
    }

    #[test]
    fn boundary_is_sealed_once_the_head_moves_past_it() {
        assert_eq!(
            sealed_boundaries(100, 101, STEP_ONE_HUNDRED),
            Some((100, 100))
        );
    }

    #[test]
    fn jump_over_a_boundary_seals_the_boundary_it_crossed() {
        // The boundary sealed is the one between the two revisions, not
        // the boundary of the segment the new revision lands in.
        assert_eq!(boundaries(99, 105, STEP_ONE_HUNDRED), vec![100]);
    }

    #[test]
    fn jump_seals_every_boundary_it_crossed() {
        assert_eq!(boundaries(150, 400, STEP_ONE_HUNDRED), vec![200, 300]);
    }

    #[test]
    fn large_jump_seals_one_boundary_per_step_not_per_revision() {
        // A jump closes one boundary per step, not one per revision
        // number it spans.
        let sealed = boundaries(150, 100_000, STEP_ONE_HUNDRED);
        assert_eq!(sealed.len(), 998);
        assert_eq!(sealed.first(), Some(&200));
        assert_eq!(sealed.last(), Some(&99_900));
    }

    #[test]
    fn first_revision_seals_nothing() {
        assert_eq!(sealed_boundaries(0, 1, STEP_ONE_HUNDRED), None);
    }

    #[test]
    fn unchanged_revision_number_seals_nothing() {
        assert_eq!(sealed_boundaries(200, 200, STEP_ONE_HUNDRED), None);
    }

    #[test]
    fn zero_target_seals_nothing() {
        assert_eq!(sealed_boundaries(0, 0, STEP_ONE_HUNDRED), None);
    }

    #[test]
    fn honours_a_non_default_step_size() {
        assert_eq!(boundaries(150, 400, 50), vec![150, 200, 250, 300, 350]);
        assert_eq!(boundaries(1, 4, 1), vec![1, 2, 3]);
    }
}

mod seal_boundary_revision_number {
    use super::*;

    #[tokio::test]
    async fn seals_boundary_with_the_parent_revision() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = test_repository(immutable_store, mutable_store);
            let branch = BranchId::from(uuid::Uuid::now_v7());

            let chain = serialize_linear_chain(&repository, 150).await;
            let older = load_state(&repository, chain[149]).await;
            let newer = serialize_jump_revision(&repository, chain[149], 400).await;

            seal_boundary_revision_number(
                repository.clone(),
                branch,
                STEP_ONE_HUNDRED,
                200,
                &older,
                &newer,
            )
            .await
            .expect("seal boundary");

            // Boundary 200 holds the highest revision numbered <= 200,
            // which across the jump is the parent at 150.
            assert_eq!(
                load_step_key(&repository, branch, 200).await,
                Some(chain[149])
            );
            // Exactly one boundary is sealed per call: neighbours, and in
            // particular the ceil-space bucket of the new revision number,
            // must be left untouched.
            assert_eq!(load_step_key(&repository, branch, 100).await, None);
            assert_eq!(load_step_key(&repository, branch, 300).await, None);
            assert_eq!(load_step_key(&repository, branch, 400).await, None);
        }))
        .await;
    }

    #[tokio::test]
    async fn seals_boundary_at_or_above_the_new_revision_with_that_revision() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = test_repository(immutable_store, mutable_store);
            let branch = BranchId::from(uuid::Uuid::now_v7());

            let chain = serialize_linear_chain(&repository, 150).await;
            let older = load_state(&repository, chain[149]).await;
            let newer = serialize_jump_revision(&repository, chain[149], 400).await;

            seal_boundary_revision_number(
                repository.clone(),
                branch,
                STEP_ONE_HUNDRED,
                400,
                &older,
                &newer,
            )
            .await
            .expect("seal boundary");

            assert_eq!(
                load_step_key(&repository, branch, 400).await,
                Some(newer.revision())
            );
            assert_eq!(load_step_key(&repository, branch, 200).await, None);
            assert_eq!(load_step_key(&repository, branch, 300).await, None);
            assert_eq!(load_step_key(&repository, branch, 500).await, None);
        }))
        .await;
    }
}

mod store_history_step {
    use super::*;

    #[tokio::test]
    async fn seals_every_boundary_crossed_by_a_jump() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = test_repository(immutable_store, mutable_store);
            let branch = BranchId::from(uuid::Uuid::now_v7());

            let chain = serialize_linear_chain(&repository, 150).await;
            let older = load_state(&repository, chain[149]).await;
            let newer = serialize_jump_revision(&repository, chain[149], 400).await;

            store_history_step(
                repository.clone(),
                branch,
                STEP_ONE_HUNDRED,
                RevisionListAcceleration {
                    step_keys: true,
                    list_cache: false,
                },
                older,
                newer,
            )
            .await;

            // 150 -> 400 closes boundaries 200 and 300, both answered by
            // the highest revision numbered <= them: the parent at 150.
            assert_eq!(
                load_step_key(&repository, branch, 200).await,
                Some(chain[149])
            );
            assert_eq!(
                load_step_key(&repository, branch, 300).await,
                Some(chain[149])
            );

            // 100 was already closed before this push, and 400 holds the
            // new head so its segment is still open. Nothing outside the
            // crossed range may be written.
            assert_eq!(load_step_key(&repository, branch, 100).await, None);
            assert_eq!(load_step_key(&repository, branch, 400).await, None);
            assert_eq!(load_step_key(&repository, branch, 500).await, None);
        }))
        .await;
    }

    #[tokio::test]
    async fn seals_nothing_when_no_boundary_is_crossed() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = test_repository(immutable_store, mutable_store);
            let branch = BranchId::from(uuid::Uuid::now_v7());

            let chain = serialize_linear_chain(&repository, 105).await;
            let older = load_state(&repository, chain[103]).await;
            let newer = load_state(&repository, chain[104]).await;

            store_history_step(
                repository.clone(),
                branch,
                STEP_ONE_HUNDRED,
                RevisionListAcceleration::default(),
                older,
                newer,
            )
            .await;

            assert_eq!(load_step_key(&repository, branch, 100).await, None);
            assert_eq!(load_step_key(&repository, branch, 200).await, None);
        }))
        .await;
    }

    #[tokio::test]
    async fn caches_lists_only_for_segments_the_jump_closed() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = test_repository(immutable_store, mutable_store);
            let branch = BranchId::from(uuid::Uuid::now_v7());

            let chain = serialize_linear_chain(&repository, 150).await;
            let older = load_state(&repository, chain[149]).await;
            let newer = serialize_jump_revision(&repository, chain[149], 400).await;

            store_history_step(
                repository.clone(),
                branch,
                STEP_ONE_HUNDRED,
                RevisionListAcceleration::default(),
                older,
                newer,
            )
            .await;

            // Segment 200 spans (100, 200] and really holds 150..=101.
            let cached = load_cached_list(&repository, branch, 150, STEP_ONE_HUNDRED)
                .await
                .expect("load cached list")
                .expect("segment 200 cached");
            let numbers: Vec<u64> = cached.items().iter().map(|item| item.number).collect();
            assert_eq!(numbers.len(), 50);
            assert_eq!(numbers.first(), Some(&150));
            assert_eq!(numbers.last(), Some(&101));

            // Segment 300 is closed but genuinely empty — the jump skipped
            // every number in (200, 300] — and empty segments aren't written.
            assert!(
                load_cached_list(&repository, branch, 250, STEP_ONE_HUNDRED)
                    .await
                    .expect("load cached list")
                    .is_none()
            );

            // The open segment holding the new head is never cached.
            assert!(
                load_cached_list(&repository, branch, 400, STEP_ONE_HUNDRED)
                    .await
                    .expect("load cached list")
                    .is_none()
            );
        }))
        .await;
    }

    #[tokio::test]
    async fn writes_nothing_when_acceleration_is_disabled() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = test_repository(immutable_store, mutable_store);
            let branch = BranchId::from(uuid::Uuid::now_v7());

            let chain = serialize_linear_chain(&repository, 150).await;
            let older = load_state(&repository, chain[149]).await;
            let newer = serialize_jump_revision(&repository, chain[149], 400).await;

            store_history_step(
                repository.clone(),
                branch,
                STEP_ONE_HUNDRED,
                RevisionListAcceleration {
                    step_keys: false,
                    list_cache: false,
                },
                older,
                newer,
            )
            .await;

            assert_eq!(load_step_key(&repository, branch, 200).await, None);
            assert_eq!(load_step_key(&repository, branch, 300).await, None);
            assert!(
                load_cached_list(&repository, branch, 150, STEP_ONE_HUNDRED)
                    .await
                    .expect("load cached list")
                    .is_none()
            );
        }))
        .await;
    }
}

mod resolve_revision_number {
    use super::*;

    const BOTH: RevisionListAcceleration = RevisionListAcceleration {
        step_keys: true,
        list_cache: true,
    };
    const STEP_KEYS_ONLY: RevisionListAcceleration = RevisionListAcceleration {
        step_keys: true,
        list_cache: false,
    };
    const NEITHER: RevisionListAcceleration = RevisionListAcceleration {
        step_keys: false,
        list_cache: false,
    };

    /// Create a branch and push `numbers` as a linear chain, then a merge
    /// revision whose `parent_other` is numbered `jump_other_number` so the
    /// branch's numbering gains a gap. Returns the branch and a map of
    /// revision number to signature.
    async fn push_history_with_a_gap(
        repository: &Arc<RepositoryContext>,
        linear_before: u64,
        jump_other_number: u64,
    ) -> (BranchId, std::collections::BTreeMap<u64, Hash>) {
        let write_token = get_write_token();
        let branch = BranchId::from(uuid::Uuid::now_v7());
        branch::create(
            repository.clone(),
            &write_token,
            branch,
            "test-branch",
            branch::default_category(),
            "creator",
            1,
            vec![],
            false,
            false,
        )
        .await
        .expect("create branch");

        let mut revisions = std::collections::BTreeMap::new();
        let mut parent = Hash::default();
        for number in 1..=linear_before {
            let state = State::new();
            state.set_parent_self(parent);
            state.set_revision_number(number);
            let mut metadata = lore_revision::metadata::Metadata::new();
            metadata.set_branch(branch).expect("set branch");
            state.set_metadata_hash(
                metadata
                    .serialize(repository.clone())
                    .await
                    .expect("serialize metadata"),
            );
            let serialized = state
                .serialize(repository.clone(), &write_token)
                .await
                .expect("serialize state");
            parent = lore_server::grpc::handlers::branch_push::push(
                repository.clone(),
                branch,
                serialized,
                crate::util::test_support::PUSH_ANY,
                true,
                false,
                STEP_ONE_HUNDRED,
                BOTH,
            )
            .await
            .expect("push revision")
            .revision;
            revisions.insert(number, parent);
        }

        let other = serialize_jump_revision(repository, Hash::default(), jump_other_number).await;
        let state = State::new();
        state.set_parent_self(parent);
        state.set_parent_other(other.revision());
        let mut metadata = lore_revision::metadata::Metadata::new();
        metadata.set_branch(branch).expect("set branch");
        state.set_metadata_hash(
            metadata
                .serialize(repository.clone())
                .await
                .expect("serialize metadata"),
        );
        let serialized = state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("serialize state");
        let result = lore_server::grpc::handlers::branch_push::push(
            repository.clone(),
            branch,
            serialized,
            crate::util::test_support::PUSH_ANY,
            true,
            false,
            STEP_ONE_HUNDRED,
            BOTH,
        )
        .await
        .expect("push jump revision");
        revisions.insert(result.revision_number, result.revision);

        (branch, revisions)
    }

    #[tokio::test]
    async fn every_acceleration_setting_resolves_the_same_revision() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = test_repository(immutable_store, mutable_store);
            // 1..=250 then a merge jumping to 400, so 251..=399 are absent.
            let (branch, revisions) = push_history_with_a_gap(&repository, 250, 399).await;
            assert!(revisions.contains_key(&400));

            for number in [1, 100, 101, 200, 250, 400] {
                let expected = revisions[&number];
                for acceleration in [BOTH, STEP_KEYS_ONLY, NEITHER] {
                    let resolved = resolve_revision_number(
                        &repository,
                        branch,
                        number,
                        STEP_ONE_HUNDRED,
                        acceleration,
                    )
                    .await
                    .unwrap_or_else(|err| panic!("revision {number} should resolve: {err}"));
                    assert_eq!(
                        resolved, expected,
                        "revision {number} with acceleration {acceleration:?}"
                    );
                }
            }
        }))
        .await;
    }

    #[tokio::test]
    async fn a_number_the_gap_skipped_does_not_resolve() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = test_repository(immutable_store, mutable_store);
            let (branch, revisions) = push_history_with_a_gap(&repository, 250, 399).await;
            assert!(!revisions.contains_key(&300));

            for acceleration in [BOTH, STEP_KEYS_ONLY, NEITHER] {
                let err = resolve_revision_number(
                    &repository,
                    branch,
                    300,
                    STEP_ONE_HUNDRED,
                    acceleration,
                )
                .await
                .expect_err("absent revision must not resolve");
                // Only these two are mapped to a not-found response by the
                // callers, so any other error reaches the client as an
                // internal fault.
                assert!(
                    err.is_not_found() || err.is_revision_not_found(),
                    "absent revision must report not found, got {err:?} \
                         with acceleration {acceleration:?}",
                );
            }
        }))
        .await;
    }

    /// A cached segment answers without consulting the step key, so a
    /// number inside a closed segment resolves even when every step key
    /// for the branch has been removed.
    #[tokio::test]
    async fn cached_segment_resolves_without_a_step_key() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = test_repository(immutable_store, mutable_store);
            let (branch, revisions) = push_history_with_a_gap(&repository, 250, 399).await;

            let write_token = get_write_token();
            for boundary in [100, 200, 300, 400] {
                let (key, key_type) = branch::revision_step_key(
                    repository::SALT_LORE,
                    repository.id,
                    branch,
                    boundary,
                    STEP_ONE_HUNDRED,
                );
                repository
                    .clone()
                    .write_mutable_store(&write_token)
                    .store(repository.id, key, Hash::default(), key_type)
                    .await
                    .expect("remove step key");
            }

            assert_eq!(
                resolve_revision_number(&repository, branch, 100, STEP_ONE_HUNDRED, BOTH)
                    .await
                    .expect("cached segment resolves revision 100"),
                revisions[&100],
            );
        }))
        .await;
    }
}

/// Sanity check: the on-disk struct sizes don't accidentally
/// change. Any field/layout change must also bump
/// `CACHED_REVISION_LIST_VERSION` and update these numbers.
#[test]
fn cached_revision_item_size_is_stable() {
    assert_eq!(std::mem::size_of::<CachedRevisionListHeader>(), 8);
    assert_eq!(std::mem::align_of::<CachedRevisionListHeader>(), 4);
    assert_eq!(std::mem::size_of::<CachedRevisionItem>(), 392);
    assert_eq!(std::mem::align_of::<CachedRevisionItem>(), 8);
}

/// Header offset is item-aligned (8): items at offset
/// `HEADER_SIZE = 8` end up properly aligned for the
/// `as_type_slice::<CachedRevisionItem>` view in `items()`.
#[test]
fn header_size_preserves_item_alignment() {
    assert_eq!(HEADER_SIZE % std::mem::align_of::<CachedRevisionItem>(), 0);
}
