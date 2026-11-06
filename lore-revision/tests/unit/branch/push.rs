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

/// Statistics level zero reports nothing, so it keeps nothing beyond what a
/// progress event reads: the fragments registered, which `copied` and `put`
/// sum to, and the bytes uploaded.
#[test]
fn a_count_is_kept_only_where_something_reports_it() {
    for (statistics, deduplicated) in [(false, 0), (true, 3)] {
        let stats = PushStats::new(statistics);
        stats.deduplicated(3);
        stats.copied();
        stats.put(64);

        let counts = stats.snapshot();
        assert_eq!(counts.deduplicated, deduplicated, "statistics {statistics}");
        assert_eq!(counts.copied, 1, "statistics {statistics}");
        assert_eq!(counts.put, 1, "statistics {statistics}");
        assert_eq!(stats.registered(), 2, "statistics {statistics}");
        assert_eq!(stats.put_bytes(), 64, "statistics {statistics}");
    }
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
        ) -> Result<Hash, ProtocolError> {
            self.created.fetch_add(1, Ordering::Relaxed);
            Ok(Hash::default())
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

        async fn branch_list(&self) -> Result<BranchListResponse, ProtocolError> {
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
        let stats = PushStats::new(false);

        let result = duplicate_association(&repository, &session, wanted, &stats).await;

        Duplicated {
            result,
            copies: peer.copies(),
            copied: stats.snapshot().copied,
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

/// Uploading the payloads the peer is missing.
mod upload {
    use lore_base::error::NotAuthorized;

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
}
