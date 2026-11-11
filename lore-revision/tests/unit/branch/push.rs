// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use bytes::Bytes;
use lore_base::error::AddressNotFound;
use lore_base::error::Disconnected;
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_error_set::ForwardStrict;
use lore_revision::branch::push::*;
use lore_revision::event::EventError;
use lore_revision::interface::LoreError;
use lore_revision::repository::RepositoryContext;
use lore_transport::ProtocolError;
use lore_transport::quic::storage_service::QueryStatus;

fn address(seed: u8) -> Address {
    Address {
        hash: Hash::from([seed; 32]),
        context: lore_base::types::Context::from([seed; 16]),
    }
}

/// Statistics level zero reports nothing, so it keeps nothing.
#[test]
fn a_count_is_kept_only_where_something_reports_it() {
    for (statistics, expected) in [(false, 0), (true, 1)] {
        let stats = PushStats::new(statistics);
        stats.deduplicated(3);
        stats.copied();
        stats.put();

        let counts = stats.snapshot();
        assert_eq!(counts.deduplicated, 3 * expected, "statistics {statistics}");
        assert_eq!(counts.copied, expected, "statistics {statistics}");
        assert_eq!(counts.put, expected, "statistics {statistics}");
    }
}

/// Revisions pushed at once share the push-wide counts, and each progress event reports only what
/// its own revision registered and uploaded.
#[test]
fn a_progress_counts_its_own_revision_alone() {
    let stats = Arc::new(PushStats::new(true));
    let first = PushProgress::new(stats.clone());
    let second = PushProgress::new(stats.clone());

    first.copied();
    second.put(64);
    second.put(16);

    let repository = lore_base::types::RepositoryId::from([3; 16]);
    let branch = lore_base::types::BranchId::from([4; 16]);
    let first_event = first.event(repository, branch);
    assert_eq!(first_event.complete, 1);
    assert_eq!(first_event.bytes_transferred, 0);
    let second_event = second.event(repository, branch);
    assert_eq!(second_event.complete, 2);
    assert_eq!(second_event.bytes_transferred, 80);

    let counts = stats.snapshot();
    assert_eq!(counts.copied, 1);
    assert_eq!(counts.put, 2);
}

/// A fragment the peer is missing reaches the caller as the address it is, rather than as a
/// generic failure.
#[test]
fn a_fragment_the_peer_is_missing_keeps_its_address_on_the_way_out() {
    let result: Result<(), ProtocolError> =
        Err(ProtocolError::from(AddressNotFound { address: [7u8; 48] }));

    let error = result
        .forward::<PushError>("pushing branch to remote, missing fragment")
        .expect_err("an error was forwarded");

    assert!(error.is_address_not_found(), "{error:?}");
    assert!(error.translated() == LoreError::AddressNotFound);
}

/// An offer the peer refuses onto a branch holding revisions the local history does not is a
/// divergence. Any other failure keeps its own variant.
#[test]
fn a_refused_offer_is_divergent_only_onto_a_diverged_branch() {
    let refused = || PushError::internal("remote rejected the request");

    let error = refused_offer(refused(), true);
    assert!(error.is_divergent(), "{error:?}");

    let error = refused_offer(refused(), false);
    assert!(error.is_internal(), "{error:?}");

    let error = refused_offer(PushError::from(Disconnected), true);
    assert!(error.is_disconnected(), "{error:?}");
}

/// What the push does with a fragment is decided entirely by the status byte the peer answered
/// with, so this is where the copy path is chosen or missed.
mod classify {
    use super::*;

    fn classify(statuses: &[u8]) -> PushQueryResult {
        let batch: Vec<Address> = (0..statuses.len() as u8).map(address).collect();
        let mut queried = PushQueryResult::default();
        classify_query_batch(&batch, &Bytes::copy_from_slice(statuses), &mut queried);
        queried
    }

    /// A full match transfers nothing, but the answer still has to be kept: it is what tells
    /// the local store the payload is safe elsewhere, and an entry that never learns that is
    /// pinned against eviction and invisible to both store caps for the rest of its life.
    #[test]
    fn an_association_the_peer_holds_transfers_nothing_but_is_recorded() {
        let queried = classify(&[QueryStatus::ExistFullMatch as u8]);
        assert_eq!(queried.len(), 0, "nothing to transfer");
        assert_eq!(queried.present, vec![address(0)]);
    }

    /// The change this path exists for: the partition holds the hash, so the peer is asked to
    /// duplicate the association rather than sent the payload it already has.
    #[test]
    fn a_partition_match_is_copied_rather_than_uploaded() {
        let queried = classify(&[QueryStatus::ExistPartitionMatch as u8]);
        assert_eq!(queried.copyable, vec![address(0)]);
        assert!(queried.absent.is_empty());
    }

    #[test]
    fn a_miss_is_uploaded() {
        let queried = classify(&[QueryStatus::NotFound as u8]);
        assert_eq!(queried.absent, vec![address(0)]);
        assert!(queried.copyable.is_empty());
    }

    /// A status the client does not know must not be read as "the peer has it" — that would
    /// drop the fragment from the push and leave the revision unreadable on the peer.
    #[test]
    fn an_unknown_status_is_uploaded() {
        let queried = classify(&[2, 7, 255]);
        assert_eq!(queried.absent.len(), 3);
        assert!(queried.copyable.is_empty());
    }

    #[test]
    fn a_batch_is_split_by_status_in_order() {
        let queried = classify(&[
            QueryStatus::NotFound as u8,
            QueryStatus::ExistFullMatch as u8,
            QueryStatus::ExistPartitionMatch as u8,
            QueryStatus::NotFound as u8,
            QueryStatus::ExistPartitionMatch as u8,
        ]);
        assert_eq!(queried.absent, vec![address(0), address(3)]);
        assert_eq!(queried.copyable, vec![address(2), address(4)]);
        assert_eq!(queried.present, vec![address(1)]);
        assert_eq!(queried.len(), 4, "len counts only what is transferred");
    }

    /// Each status answers the address at its own position, so a batch where only some entries
    /// are copyable must not shift the rest.
    #[test]
    fn statuses_line_up_with_the_addresses_they_answer() {
        let queried = classify(&[
            QueryStatus::ExistPartitionMatch as u8,
            QueryStatus::ExistFullMatch as u8,
            QueryStatus::NotFound as u8,
        ]);
        assert_eq!(queried.copyable, vec![address(0)]);
        assert_eq!(queried.present, vec![address(1)]);
        assert_eq!(queried.absent, vec![address(2)]);
    }
}

const DURABLE: u32 = lore_base::types::FragmentFlags::PayloadStoredDurable.bits();

/// A repository over in-memory stores, holding only what the test puts in it.
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

/// Store a payload under `address` carrying no durability, as a local commit leaves it.
async fn store_local(repository: &Arc<RepositoryContext>, address: Address) {
    let payload = Bytes::from_static(b"payload");
    let fragment = Fragment {
        flags: 0,
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };
    repository
        .immutable_store()
        .put(repository.id, address, fragment, Some(payload), false)
        .await
        .expect("storing a local payload");
}

/// The flags the store holds for `address`.
async fn stored_flags(repository: &Arc<RepositoryContext>, address: Address) -> u32 {
    repository
        .immutable_store()
        .get_metadata(repository.id, address)
        .await
        .expect("the store holds the address")
        .fragment
        .flags
}

/// Recording a full match is what takes the fragment off the local store's protected list, so
/// this is where a push stops re-offering content the peer already holds.
mod mark {
    use super::*;

    /// The fact a full match establishes: the payload is safe on the peer, so the local entry
    /// is no longer the only copy.
    #[tokio::test]
    async fn a_present_address_becomes_durable() {
        let repository = null_repository().await;
        let present = address(1);
        store_local(&repository, present).await;
        assert_eq!(
            stored_flags(&repository, present).await & DURABLE,
            0,
            "a locally stored payload starts out non-durable"
        );

        mark_present_durable(&repository, vec![present]).await;

        assert_eq!(stored_flags(&repository, present).await & DURABLE, DURABLE);
    }

    /// An address the store cannot describe answers nothing to write back, and must not cost
    /// the addresses it can describe their record.
    #[tokio::test]
    async fn a_batch_marks_what_it_can_and_skips_the_rest() {
        let repository = null_repository().await;
        let present = address(2);
        store_local(&repository, present).await;

        mark_present_durable(&repository, vec![address(0), address(3), present]).await;

        assert_eq!(
            stored_flags(&repository, present).await & DURABLE,
            DURABLE,
            "a zero hash and an address the store never held are skipped, not fatal"
        );
    }
}

/// What an offer of a revision as a branch's latest does with each answer of the peer.
mod offer {
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use async_trait::async_trait;
    use lore_base::error::NotFound;
    use lore_base::types::BranchId;
    use lore_base::types::BranchMetadata;
    use lore_base::types::BranchPoint;
    use lore_base::types::RepositoryId;
    use lore_transport::BranchListResponse;
    use lore_transport::BranchPushResponse;
    use lore_transport::BranchQueryResponse;
    use lore_transport::BranchRecord;
    use lore_transport::MetadataSetResult;
    use lore_transport::Revision;
    use lore_transport::RevisionListResponse;
    use lore_transport::RevisionListStart;
    use parking_lot::Mutex;

    use super::*;
    use crate::branch::with_execution;

    /// A peer that answers each branch push with the next of its answers, and counts the
    /// branches it creates.
    struct Peer {
        answers: Mutex<VecDeque<Result<BranchPushResponse, ProtocolError>>>,
        created: AtomicUsize,
    }

    #[async_trait]
    impl Revision for Peer {
        async fn branch_create(
            &self,
            _: BranchId,
            _: &str,
            _: &str,
            _: &str,
            _: &[BranchPoint],
        ) -> Result<BranchRecord, ProtocolError> {
            self.created.fetch_add(1, Ordering::Relaxed);
            Ok(BranchRecord::default())
        }

        async fn branch_delete(&self, _: BranchId) -> Result<(), ProtocolError> {
            unreachable!("an offer deletes no branch")
        }

        async fn branch_query(
            &self,
            _: Option<BranchId>,
            _: Option<&str>,
        ) -> Result<BranchQueryResponse, ProtocolError> {
            unreachable!("an offer queries no branch")
        }

        async fn branch_push(
            &self,
            _: BranchId,
            _: Hash,
            _: bool,
            _: bool,
        ) -> Result<BranchPushResponse, ProtocolError> {
            self.answers
                .lock()
                .pop_front()
                .expect("the peer was not offered more than it answers")
        }

        async fn branch_list(
            &self,
            _: Option<&str>,
            _: bool,
        ) -> Result<BranchListResponse, ProtocolError> {
            unreachable!("an offer lists no branches")
        }

        async fn revision_list(
            &self,
            _: RevisionListStart,
        ) -> Result<RevisionListResponse, ProtocolError> {
            unreachable!("an offer lists no revisions")
        }

        async fn branch_metadata_get(&self, _: BranchId) -> Result<Hash, ProtocolError> {
            unreachable!("an offer reads no branch metadata")
        }

        async fn branch_metadata_set(
            &self,
            _: BranchId,
            _: Hash,
            _: Hash,
        ) -> Result<MetadataSetResult, ProtocolError> {
            unreachable!("an offer writes no branch metadata")
        }
    }

    fn accepted() -> Result<BranchPushResponse, ProtocolError> {
        Ok(BranchPushResponse {
            fast_forward_merged: false,
            revision: Hash::from([1; 32]),
            revision_number: 1,
            message: None,
        })
    }

    fn deleted() -> Result<BranchPushResponse, ProtocolError> {
        Err(ProtocolError::from(NotFound))
    }

    fn missing() -> Result<BranchPushResponse, ProtocolError> {
        Err(ProtocolError::from(AddressNotFound { address: [7; 48] }))
    }

    /// The recovery runs once if the peer refuses the first offer or the offer after a deleted
    /// branch is created again for a missing fragment, and for no other answer. A refusal the
    /// recovery does not fix reaches the caller as the missing fragment it is.
    #[tokio::test]
    async fn recovery_runs_once_and_only_after_a_refusal_for_a_missing_fragment() {
        let cases = [
            ("accepted", vec![accepted()], 0, 0, None),
            ("deleted, accepted", vec![deleted(), accepted()], 1, 0, None),
            ("missing, accepted", vec![missing(), accepted()], 0, 1, None),
            (
                "deleted, missing, accepted",
                vec![deleted(), missing(), accepted()],
                1,
                1,
                None,
            ),
            (
                "missing, missing",
                vec![missing(), missing()],
                0,
                1,
                Some(LoreError::AddressNotFound),
            ),
            (
                "deleted, deleted",
                vec![deleted(), deleted()],
                1,
                0,
                Some(LoreError::NotFound),
            ),
        ];
        for (name, answers, created, recovered, error) in cases {
            let peer = Peer {
                answers: Mutex::new(answers.into()),
                created: AtomicUsize::new(0),
            };
            let recoveries = AtomicUsize::new(0);

            let result = with_execution(offer_revision(
                &peer,
                RepositoryId::default(),
                BranchId::default(),
                &BranchMetadata::default(),
                Hash::default(),
                Hash::from([1; 32]),
                false,
                false,
                async || {
                    recoveries.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                },
            ))
            .await;

            assert!(
                result.err().map(|error| error.translated()) == error,
                "{name}: the result"
            );
            assert_eq!(
                peer.created.load(Ordering::Relaxed),
                created,
                "{name}: branches created"
            );
            assert_eq!(
                recoveries.load(Ordering::Relaxed),
                recovered,
                "{name}: recoveries"
            );
            assert!(
                peer.answers.lock().is_empty(),
                "{name}: every answer was asked for"
            );
        }
    }
}

/// Asking the peer to duplicate an association it already holds, as a push does before it uploads.
/// The peer answers copy requests with `SlowDown`, a refusal, or success.
mod duplicate_association {
    use lore_base::error::NotFound;
    use lore_transport::test_util::CopyingPeer;

    use super::*;

    /// What the peer was asked for, how the copy ended, and the local entry's flags afterwards.
    struct Duplicated {
        result: Result<bool, PushError>,
        copies: usize,
        copied: u64,
        flags: u32,
    }

    /// Duplicate a locally stored address on a peer that answers as `slow_downs` and `refusal` say.
    async fn duplicate(seed: u8, slow_downs: usize, refusal: Option<ProtocolError>) -> Duplicated {
        let repository = null_repository().await;
        let wanted = address(seed);
        store_local(&repository, wanted).await;
        let peer = CopyingPeer::new(slow_downs, refusal);
        let session = peer.session(repository.id);
        let progress = PushProgress::new(Arc::new(PushStats::new(false)));

        let result = duplicate_association(&repository, &session, wanted, &progress).await;

        Duplicated {
            result,
            copies: peer.copies(),
            copied: progress.complete(),
            flags: stored_flags(&repository, wanted).await,
        }
    }

    /// A copy the peer throttles and then accepts registers the association and marks the local
    /// entry as durable.
    #[tokio::test(start_paused = true)]
    async fn a_throttled_copy_that_is_accepted_is_recorded_as_durable() {
        let duplicated = duplicate(0xD1, 2, None).await;

        assert!(duplicated.result.expect("the copy is accepted"));
        assert_eq!(duplicated.copies, 3);
        assert_eq!(duplicated.copied, 1);
        assert_eq!(duplicated.flags & DURABLE, DURABLE);
    }

    /// If the peer throttles a copy until retries run out, the push fails with `SlowDown` rather
    /// than uploading the payload.
    #[tokio::test(start_paused = true)]
    async fn a_copy_throttled_through_every_retry_fails_the_push() {
        let duplicated = duplicate(0xD3, usize::MAX, None).await;

        let error = duplicated
            .result
            .expect_err("a copy still throttled fails the push");
        assert!(error.is_slow_down(), "{error:?}");
        assert_eq!(duplicated.copies, lore_storage::store_retry().limit() + 1);
        assert_eq!(duplicated.copied, 0);
        assert_eq!(duplicated.flags & DURABLE, 0);
    }

    /// A copy refused for a reason other than throttling is not retried, and leaves the payload to
    /// be uploaded.
    #[tokio::test(start_paused = true)]
    async fn a_refused_copy_leaves_the_payload_to_upload() {
        let duplicated = duplicate(0xD5, 0, Some(ProtocolError::from(NotFound))).await;

        assert!(!duplicated.result.expect("a refusal is not an error"));
        assert_eq!(duplicated.copies, 1);
        assert_eq!(duplicated.copied, 0);
        assert_eq!(duplicated.flags & DURABLE, 0);
    }
}

/// The order a push pushes its targets in: each once the targets it comes after have pushed, one
/// target of a repository at a time, a bounded number at once.
mod schedule {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use super::*;

    fn target(after: Vec<usize>, group: usize) -> TargetOrder {
        TargetOrder { after, group }
    }

    /// What pushing some targets did: the targets pushed, in the order they started, and the
    /// failed ones.
    struct Pushed {
        started: Vec<usize>,
        failed: Vec<usize>,
    }

    /// Push the targets of `orders`, failing those `failing` names.
    async fn push(orders: &[TargetOrder], failing: &[usize]) -> Pushed {
        let started = Mutex::new(vec![]);
        let failures = schedule_targets(orders, |target| {
            started.lock().unwrap().push(target);
            let fails = failing.contains(&target);
            async move {
                tokio::task::yield_now().await;
                if fails {
                    Err(PushError::internal("the push failed"))
                } else {
                    Ok(())
                }
            }
        })
        .await;
        Pushed {
            started: started.into_inner().unwrap(),
            failed: failures.into_iter().map(|(target, _)| target).collect(),
        }
    }

    /// A target starts once each target it comes after has pushed.
    #[tokio::test]
    async fn a_target_starts_after_the_targets_it_comes_after() {
        let orders = [target(vec![1], 0), target(vec![], 1), target(vec![0], 2)];

        let pushed = push(&orders, &[]).await;

        assert_eq!(pushed.started, vec![1, 0, 2]);
        assert!(pushed.failed.is_empty(), "{:?}", pushed.failed);
    }

    /// A failed target holds back the targets after it, and the others push.
    #[tokio::test]
    async fn a_failed_target_holds_back_only_the_targets_after_it() {
        let orders = [
            target(vec![1], 0),
            target(vec![], 1),
            target(vec![], 2),
            target(vec![0], 3),
        ];

        let mut pushed = push(&orders, &[1]).await;

        pushed.started.sort_unstable();
        assert_eq!(pushed.started, vec![1, 2]);
        assert_eq!(pushed.failed, vec![1]);
    }

    /// Targets waiting on each other fail the first of them rather than waiting forever.
    #[tokio::test]
    async fn targets_waiting_on_each_other_fail() {
        let orders = [target(vec![1], 0), target(vec![0], 1)];

        let pushed = push(&orders, &[]).await;

        assert_eq!(pushed.failed, vec![0]);
        assert_eq!(pushed.started, Vec::<usize>::new());
    }

    /// The most targets pushing at once in `orders`, and the most of one group.
    async fn most_at_once(orders: &[TargetOrder]) -> (usize, usize) {
        let pushing = AtomicUsize::new(0);
        let most = AtomicUsize::new(0);
        let group_pushing = AtomicUsize::new(0);
        let group_most = AtomicUsize::new(0);

        let failures = schedule_targets(orders, |target| {
            let grouped = orders[target].group == 0;
            let (pushing, most, group_pushing, group_most) =
                (&pushing, &most, &group_pushing, &group_most);
            async move {
                most.fetch_max(pushing.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                if grouped {
                    group_most.fetch_max(
                        group_pushing.fetch_add(1, Ordering::SeqCst) + 1,
                        Ordering::SeqCst,
                    );
                }
                for _ in 0..4 {
                    tokio::task::yield_now().await;
                }
                pushing.fetch_sub(1, Ordering::SeqCst);
                if grouped {
                    group_pushing.fetch_sub(1, Ordering::SeqCst);
                }
                Ok(())
            }
        })
        .await;

        assert!(failures.is_empty());
        (most.into_inner(), group_most.into_inner())
    }

    /// As many targets push at once as the bound allows, and targets of one repository one at a
    /// time.
    #[tokio::test]
    async fn targets_push_at_once_up_to_the_bound_and_one_per_repository() {
        let orders: Vec<TargetOrder> = (0..3 * MAX_CONCURRENT_TARGETS)
            .map(|position| target(vec![], if position % 3 == 0 { 0 } else { position }))
            .collect();

        assert_eq!(most_at_once(&orders).await, (MAX_CONCURRENT_TARGETS, 1));
    }
}

/// Placing the revisions other targets link against a target's plan, and the order targets push
/// in.
mod place {
    use std::collections::HashSet;

    use lore_base::types::BranchId;
    use lore_revision::metadata::Metadata;
    use lore_revision::state::LinkReference;
    use lore_revision::state::State;

    use super::*;
    use crate::branch::with_execution;
    use crate::branch::write_branch_revision;
    use crate::paged_remote::PagedRemote;
    use crate::paged_remote::client_on;
    use crate::paged_remote::client_stores;

    fn branch() -> BranchId {
        BranchId::from([7; 16])
    }

    /// A target of `branch()` at `latest` on `repository`, whose plan pushes `local` as revisions
    /// of its branch.
    pub(super) async fn target(
        repository: Arc<RepositoryContext>,
        latest: Hash,
        local: &[Hash],
        pending: Vec<Hash>,
    ) -> PushTarget {
        let remote = repository.remote().await.expect("the client is connected");
        let plan = TargetPlan {
            remote,
            default_branch: branch(),
            branch_metadata: lore_base::types::BranchMetadata::default(),
            branch_point: Hash::default(),
            remote_latest: Hash::default(),
            remote_metadata: Hash::default(),
            remote_deleted: false,
            remote_revision: None,
            remote_history: 0,
            local_history: local.len(),
            diverged: false,
            revisions: local
                .iter()
                .map(|&revision| TargetRevision {
                    revision,
                    local: true,
                    links: vec![],
                })
                .collect(),
            pushed: local.iter().copied().collect(),
            peer_latest: vec![(branch(), Hash::default())],
        };
        PushTarget {
            repository,
            branch: branch(),
            latest,
            mount: None,
            plan,
            linked: vec![],
            pending,
            scanned: 0,
        }
    }

    /// Write a revision of `branch()`, named `name` so that revisions on the same parent differ.
    async fn write_on_branch(
        repository: &Arc<RepositoryContext>,
        parent_self: Hash,
        parent_other: Hash,
        revision_number: u64,
        name: &str,
    ) -> Hash {
        let mut metadata = Metadata::new();
        metadata.set_branch(branch()).expect("recording the branch");
        metadata
            .set_string("name", name)
            .expect("naming the revision");
        let metadata = metadata
            .serialize(repository.clone())
            .await
            .expect("writing the revision metadata");
        let token = repository
            .try_write_token()
            .expect("a client context carries a write token");
        let state = State::new();
        state.set_parent_self(parent_self);
        state.set_parent_other(parent_other);
        state.set_revision_number(revision_number);
        state.set_metadata_hash(metadata);
        state
            .serialize(repository.clone(), token)
            .await
            .expect("writing the revision")
    }

    /// Store the repository's metadata, naming `branch()` its default branch, and `branch()`'s
    /// own, which planning a target reads.
    async fn store_repository_metadata(repository: &Arc<RepositoryContext>) {
        let mut metadata = Metadata::new();
        for (key, value) in [
            (lore_revision::repository::NAME, "pushed"),
            (lore_revision::repository::DESCRIPTION, ""),
            (lore_revision::repository::DEFAULT_BRANCH_NAME, "main"),
            (lore_revision::repository::CREATOR, ""),
        ] {
            metadata
                .set_string(key, value)
                .expect("describing the repository");
        }
        metadata
            .set_context(lore_revision::repository::DEFAULT_BRANCH, branch())
            .expect("naming the default branch");
        metadata
            .set_u64(lore_revision::repository::CREATED, 0)
            .expect("dating the repository");
        let metadata = metadata
            .serialize(repository.clone())
            .await
            .expect("writing the repository metadata");
        lore_revision::repository::metadata_store_hash(repository.clone(), metadata)
            .await
            .expect("storing the repository metadata");

        let mut branch_metadata = Metadata::new();
        branch_metadata
            .set_string("name", "main")
            .expect("naming the branch");
        let branch_metadata = branch_metadata
            .serialize(repository.clone())
            .await
            .expect("writing the branch metadata");
        lore_revision::branch::mutable_store_metadata(
            repository.clone(),
            branch(),
            branch_metadata,
        )
        .await
        .expect("storing the branch metadata");
    }

    /// The revisions `target`'s plan pushes, each with whether it is offered.
    fn pushed(target: &PushTarget) -> Vec<(Hash, bool)> {
        target
            .plan
            .revisions
            .iter()
            .map(|pushed| (pushed.revision, pushed.local))
            .collect()
    }

    /// A linked revision of another branch that descends from the target's latest revision is
    /// uploaded, not made the target's latest, so a newer revision of the target's own branch
    /// still extends the plan.
    #[tokio::test]
    async fn another_branchs_revision_is_uploaded_and_a_newer_one_extends_the_plan() {
        Box::pin(with_execution(async {
            let stores = client_stores().await;
            let writer = &stores.writer;
            let base = write_on_branch(writer, Hash::default(), Hash::default(), 1, "base").await;
            let latest = write_on_branch(writer, base, Hash::default(), 2, "latest").await;
            let other = write_branch_revision(writer, latest, 3, 0xB0).await;
            let between = write_on_branch(writer, latest, Hash::default(), 3, "between").await;
            let newer = write_on_branch(writer, between, Hash::default(), 4, "newer").await;
            let client = client_on(&stores, Arc::new(PagedRemote::new(branch(), base)));
            let mut target = target(client, latest, &[latest], vec![other, newer]).await;

            place_pending(&mut target)
                .await
                .expect("placing the linked revisions");

            assert_eq!(target.latest, newer);
            assert_eq!(
                pushed(&target),
                vec![
                    (latest, true),
                    (other, false),
                    (between, true),
                    (newer, true)
                ]
            );
        }))
        .await;
    }

    /// A linked revision older than the target's latest one leaves its plan as it was.
    #[tokio::test]
    async fn an_older_revision_leaves_the_plan_as_it_was() {
        Box::pin(with_execution(async {
            let stores = client_stores().await;
            let writer = &stores.writer;
            let base = write_on_branch(writer, Hash::default(), Hash::default(), 1, "base").await;
            let latest = write_on_branch(writer, base, Hash::default(), 2, "latest").await;
            let client = client_on(&stores, Arc::new(PagedRemote::new(branch(), base)));
            let mut target = target(client, latest, &[latest], vec![base]).await;

            place_pending(&mut target)
                .await
                .expect("placing the linked revision");

            assert_eq!(target.latest, latest);
            assert_eq!(pushed(&target), vec![(latest, true)]);
        }))
        .await;
    }

    /// A linked revision of the target's branch that reaches its latest revision only through a
    /// merge replaces the plan with its own, which uploads the merged line.
    #[tokio::test]
    async fn a_newer_revision_through_a_merge_replaces_the_plan() {
        Box::pin(with_execution(async {
            let stores = client_stores().await;
            let writer = &stores.writer;
            store_repository_metadata(writer).await;
            let base = write_on_branch(writer, Hash::default(), Hash::default(), 1, "base").await;
            let latest = write_on_branch(writer, base, Hash::default(), 2, "latest").await;
            let remote_line = write_on_branch(writer, base, Hash::default(), 2, "remote").await;
            let merge = write_on_branch(writer, remote_line, latest, 3, "merge").await;
            let client = client_on(&stores, Arc::new(PagedRemote::new(branch(), base)));
            let mut target = target(client, latest, &[latest], vec![merge]).await;

            place_pending(&mut target)
                .await
                .expect("placing the linked revision");

            assert_eq!(target.latest, merge);
            assert_eq!(
                pushed(&target).into_iter().collect::<HashSet<_>>(),
                HashSet::from([(latest, false), (remote_line, true), (merge, true)])
            );
        }))
        .await;
    }

    /// A target comes after the targets pushing the revisions it links, and a layer after the
    /// pushing repository. Targets of one repository share a group.
    #[tokio::test]
    async fn targets_come_after_what_they_link_and_layers_after_the_repository() {
        Box::pin(with_execution(async {
            let stores = client_stores().await;
            let client = client_on(
                &stores,
                Arc::new(PagedRemote::new(branch(), Hash::default())),
            );
            let pushed_revision = Hash::from([1; 32]);
            let held_revision = Hash::from([2; 32]);
            let mut root = target(client.clone(), Hash::default(), &[], vec![]).await;
            root.linked = vec![(1, pushed_revision), (2, held_revision)];
            let mut link =
                target(client.clone(), pushed_revision, &[pushed_revision], vec![]).await;
            link.mount = Some(Mount::Link {
                target: 0,
                revision: Hash::default(),
                link: LinkReference::default(),
            });
            let mut layer = target(client, Hash::default(), &[], vec![]).await;
            layer.mount = Some(Mount::Layer("lay".into()));

            let (_targets, orders) = order_targets(vec![root, link, layer]);

            let ordered: Vec<(Vec<usize>, usize)> = orders
                .into_iter()
                .map(|order| (order.after, order.group))
                .collect();
            assert_eq!(ordered, vec![(vec![1], 0), (vec![], 0), (vec![0], 0)]);
        }))
        .await;
    }
}

/// How a failed push names the link or layer it pushed, and what it fails with.
mod failure {
    use lore_base::error::SlowDown;
    use lore_base::types::BranchId;
    use lore_revision::state::LinkReference;
    use lore_revision::state::State;

    use super::*;
    use crate::branch::null_repository;
    use crate::branch::with_execution;

    fn link(local_node: u32) -> LinkReference {
        LinkReference {
            local_node,
            ..LinkReference::default()
        }
    }

    /// A layer is named by its path, and a link inside a linked repository by the path of each
    /// mount on the way to it.
    #[tokio::test]
    async fn a_mount_is_named_by_its_path_in_the_pushing_repository() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let branch = BranchId::from([7; 16]);

            let layer = mount_name(&[], &repository, branch, Some(&Mount::Layer("lay/".into())))
                .await
                .expect("a layer is named");
            assert_eq!(
                layer,
                MountName {
                    kind: "layer",
                    path: "lay".into(),
                    branch: branch.to_string(),
                }
            );
            assert_eq!(mount_name(&[], &repository, branch, None).await, None);
        }))
        .await;
    }

    /// A push that failed several times fails with the first failure.
    #[test]
    fn several_failures_fail_with_the_first() {
        let failed = report_failures(vec![
            (PushError::from(Disconnected), "first".into()),
            (PushError::from(SlowDown), "second".into()),
        ]);

        assert!(failed.is_disconnected(), "{failed:?}");
    }

    /// A revision whose link list can't be read fails the push with the read's error.
    #[tokio::test]
    async fn an_unreadable_link_list_fails() {
        Box::pin(with_execution(async {
            let repository = null_repository().await;
            let state = State::new();
            state.set_link_list_hash(Hash::from([9; 32]));

            let links = revision_links(&repository, &state).await;

            let Err(err) = links else {
                panic!("an unreadable link list fails");
            };
            assert!(err.is_address_not_found(), "{err:?}");
        }))
        .await;
    }

    /// A link inside a linked repository is named by the path of each mount on the way to it,
    /// each the node holding the link where the revision holding it can't be read.
    #[tokio::test]
    async fn a_nested_link_is_named_by_each_mount_on_the_way() {
        Box::pin(with_execution(async {
            let branch = BranchId::from([7; 16]);
            let stores = crate::paged_remote::client_stores().await;
            let client = crate::paged_remote::client_on(
                &stores,
                Arc::new(crate::paged_remote::PagedRemote::new(
                    branch,
                    Hash::default(),
                )),
            );
            let mut middle =
                super::place::target(client.clone(), Hash::default(), &[], vec![]).await;
            middle.mount = Some(Mount::Link {
                target: 0,
                revision: Hash::default(),
                link: link(3),
            });
            let targets = vec![
                super::place::target(client.clone(), Hash::default(), &[], vec![]).await,
                middle,
            ];

            let nested = Mount::Link {
                target: 1,
                revision: Hash::default(),
                link: link(5),
            };
            let name = mount_name(&targets, &client, branch, Some(&nested))
                .await
                .expect("a link is named");

            assert_eq!((name.kind, name.path.as_str()), ("link", "node 3/node 5"));
        }))
        .await;
    }
}

/// Uploading the payloads the peer is missing.
mod upload {
    use lore_base::error::NotAuthorized;
    use lore_base::types::BranchId;
    use lore_base::types::RepositoryId;
    use lore_transport::StorageSession;

    use super::*;
    use crate::branch::with_execution;
    use crate::immutable::session_tests::refusing_session;

    /// An upload the peer refuses fails the push with the refusal.
    #[tokio::test]
    async fn an_upload_the_peer_refuses_fails_the_push_with_the_refusal() {
        let pushed = with_execution(async {
            let repository = null_repository().await;
            let wanted = address(0xE1);
            store_local(&repository, wanted).await;
            push_fragments(
                repository,
                refusing_session(ProtocolError::from(NotAuthorized)),
                PushQueryResult {
                    absent: vec![wanted],
                    ..PushQueryResult::default()
                },
                Arc::new(PushProgress::new(Arc::new(PushStats::new(false)))),
            )
            .await
        })
        .await;

        assert!(
            pushed.as_ref().is_err_and(|err| err.is_not_authorized()),
            "got {pushed:?}"
        );
    }

    /// Pushes sharing upload slots start no more uploads between them than there are slots.
    #[tokio::test(start_paused = true)]
    async fn pushes_sharing_upload_slots_start_no_more_uploads_than_there_are_slots() {
        with_execution(async {
            let repository = null_repository().await;
            let session = Arc::new(StorageSession::pending(std::future::pending));
            let slots = Arc::new(tokio::sync::Semaphore::new(2));
            let mut pushes = vec![];
            let mut progresses = vec![];
            for push in 0..2u8 {
                let absent: Vec<Address> =
                    (0..3).map(|seed| address(0xF0 + 3 * push + seed)).collect();
                for &wanted in &absent {
                    store_local(&repository, wanted).await;
                }
                let progress = Arc::new(PushProgress::new(Arc::new(PushStats::new(false))));
                progresses.push(progress.clone());
                pushes.push(push_fragments_within(
                    repository.clone(),
                    session.clone(),
                    PushQueryResult {
                        absent,
                        ..PushQueryResult::default()
                    },
                    progress,
                    slots.clone(),
                ));
            }

            let stalled = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                futures::future::join_all(pushes),
            )
            .await;

            assert!(
                stalled.is_err(),
                "the uploads wait on a session that never connects"
            );
            let queued: u64 = progresses
                .iter()
                .map(|progress| {
                    progress
                        .event(RepositoryId::default(), BranchId::default())
                        .bytes_total
                })
                .sum();
            assert_eq!(queued, 2 * b"payload".len() as u64);
        })
        .await;
    }
}

/// The history a push collects by walking back from the peer's latest revision of the branch to
/// where the local line leaves it.
mod unpushed_history {
    use std::time::Duration;

    use lore_base::error::NotAuthorized;
    use lore_base::types::BranchId;
    use lore_revision::branch::BranchLatestStatus;
    use lore_revision::metadata::Metadata;
    use lore_revision::state::State;

    use super::*;
    use crate::branch::with_execution;
    use crate::branch::write_line;
    use crate::paged_remote::Anchor;
    use crate::paged_remote::PagedRemote;
    use crate::paged_remote::client_on;
    use crate::paged_remote::client_stores;
    use crate::paged_remote::remote_revision;

    /// The error a push of a local line of two revisions meets collecting its history against
    /// `peer`, whose latest revision of the branch is `peer_latest`, a revision only the peer
    /// holds. The walk is bounded by a second, so that a walk that loops fails the test.
    async fn unpushed_history_error(peer: PagedRemote, peer_latest: Hash) -> PushError {
        let branch = BranchId::from([7; 16]);
        let stores = client_stores().await;
        let line = write_line(&stores.writer, Hash::default(), 1, 2).await;
        let mut metadata = Metadata::new();
        metadata
            .set_string("name", "pushed")
            .expect("naming the branch");
        let metadata = metadata
            .serialize(stores.writer.clone())
            .await
            .expect("writing the branch metadata");
        lore_revision::branch::mutable_store_metadata(stores.writer.clone(), branch, metadata)
            .await
            .expect("storing the branch metadata");
        lore_revision::branch::store_latest(
            stores.writer.clone(),
            branch,
            Hash::default(),
            line[1],
            BranchLatestStatus::Divergent,
        )
        .await
        .expect("storing the local latest");
        let client = client_on(&stores, Arc::new(peer));
        let remote = client.remote().await.expect("the client is connected");
        let state = State::deserialize(client.clone(), line[1])
            .await
            .expect("reading the local latest");

        let collected = collect_unpushed_history(
            &client,
            &remote,
            &state,
            branch,
            line[1],
            peer_latest,
            branch,
        );
        match tokio::time::timeout(Duration::from_secs(1), collected)
            .await
            .expect("the walk ends within a second")
        {
            Ok(_) => panic!("the walk stops at the peer's latest"),
            Err(error) => error,
        }
    }

    /// A push whose walk back from the peer's latest revision stops fails with the walk's error,
    /// which keeps its code: `RevisionNotFound` naming the peer's latest where the peer's page does
    /// not move the line, and the peer's `NotAuthorized` where its list fails.
    #[tokio::test]
    async fn a_push_whose_walk_stops_fails_with_the_walks_error() {
        Box::pin(with_execution(async {
            let branch = BranchId::from([7; 16]);
            let peer_latest = remote_revision(3, Hash::default(), Hash::from([0xA0; 32])).await;
            let top = peer_latest.signature;

            let stopped = unpushed_history_error(
                PagedRemote::new(branch, top).holding([&peer_latest]).page(
                    Anchor::Signature(top),
                    vec![(3, top)],
                    top,
                ),
                top,
            )
            .await;
            let refused = unpushed_history_error(
                PagedRemote::new(branch, top)
                    .failing(Anchor::Signature(top), ProtocolError::from(NotAuthorized)),
                top,
            )
            .await;

            assert_eq!(
                (
                    stopped
                        .as_revision_not_found()
                        .map(|error| error.revision.clone()),
                    refused.is_not_authorized(),
                ),
                (Some(top.to_string()), true),
                "{stopped:?}; {refused:?}"
            );
        }))
        .await;
    }
}
