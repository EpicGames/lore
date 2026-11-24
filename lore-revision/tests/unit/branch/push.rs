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
use lore_revision::state::allow_all_repositories;
use lore_transport::BranchPushError;
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
    let refused = || PushError::from(lore_base::error::BranchAdvanced);

    let error = refused_offer(refused(), true);
    assert!(error.is_divergent(), "{error:?}");

    let error = refused_offer(refused(), false);
    assert!(error.is_branch_advanced(), "{error:?}");

    let error = refused_offer(PushError::internal("a failed upload"), true);
    assert!(error.is_internal(), "{error:?}");

    let error = refused_offer(PushError::from(Disconnected), true);
    assert!(error.is_disconnected(), "{error:?}");
}

/// An answer to an offer with no revision number is the peer's refusal, `BranchAdvanced`, with a
/// fast-forward merge and without one. An answer with a revision number is no refusal.
#[test]
fn an_answer_with_no_revision_number_is_branch_advanced() {
    let answer = |revision_number| lore_transport::BranchPushResponse {
        fast_forward_merged: false,
        revision: Hash::from([2; 32]),
        revision_number,
        message: None,
    };

    assert!(refused_answer(&answer(0), false).is_err_and(|err| err.is_branch_advanced()));
    assert!(refused_answer(&answer(0), true).is_err_and(|err| err.is_branch_advanced()));
    assert!(refused_answer(&answer(3), false).is_ok());
}

/// An offer the peer accepts is what the push reports as published, and the remote latest from then
/// on. The merge flag is the accepting answer's own.
#[test]
fn the_report_records_what_an_offer_published() {
    let mut report = PushReport::default();
    let answer =
        |seed: u8, revision_number, fast_forward_merged| lore_transport::BranchPushResponse {
            fast_forward_merged,
            revision: Hash::from([seed; 32]),
            revision_number,
            message: None,
        };

    record_offer(&mut report, &answer(1, 4, true));
    let merged = report;
    record_offer(&mut report, &answer(2, 5, false));

    for (report, seed, revision_number, fast_forward_merged) in
        [(merged, 1, 4, true), (report, 2, 5, false)]
    {
        let published = Hash::from([seed; 32]);
        assert_eq!(
            (
                report.published,
                report.revision_number,
                report.fast_forward_merged,
                report.remote_latest,
            ),
            (published, revision_number, fast_forward_merged, published)
        );
    }
}

/// A linked revision the peer holds nothing for, or holds only under another context, is one it
/// lacks. One it holds exactly is there.
#[test]
fn the_link_check_names_a_linked_revision_the_remote_lacks() {
    let revision = |seed: u8| Address::zero_context_hash(Hash::from([seed; 32]));
    let answered = |absent: Vec<Address>, copyable: Vec<Address>| PushQueryResult {
        absent,
        copyable,
        present: vec![revision(3)],
    };

    assert_eq!(
        (
            absent_linked_revision(&answered(vec![revision(1)], vec![])),
            absent_linked_revision(&answered(vec![], vec![revision(2)])),
            absent_linked_revision(&answered(vec![], vec![])),
        ),
        (Some(Hash::from([1; 32])), Some(Hash::from([2; 32])), None)
    );
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
        allow_all_repositories(),
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
        answers: Mutex<VecDeque<Result<BranchPushResponse, BranchPushError>>>,
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
        ) -> Result<BranchPushResponse, BranchPushError> {
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

    fn accepted() -> Result<BranchPushResponse, BranchPushError> {
        Ok(BranchPushResponse {
            fast_forward_merged: false,
            revision: Hash::from([1; 32]),
            revision_number: 1,
            message: None,
        })
    }

    fn deleted() -> Result<BranchPushResponse, BranchPushError> {
        Err(BranchPushError::from(NotFound))
    }

    fn missing() -> Result<BranchPushResponse, BranchPushError> {
        Err(BranchPushError::from(AddressNotFound { address: [7; 48] }))
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

    /// The remote's refusal of a push onto a latest that has moved on is `Internal` once
    /// [`refusal_as_internal`] maps it, and every other failure keeps its variant.
    #[test]
    fn a_refusal_onto_a_moved_latest_is_internal_and_the_rest_keep_their_variants() {
        let refused = refusal_as_internal(BranchPushError::from(lore_base::error::BranchAdvanced));
        let lost = refusal_as_internal(BranchPushError::from(lore_base::error::Disconnected));

        assert!(
            refused.is_internal() && lost.is_disconnected(),
            "{refused:?}; {lost:?}"
        );
    }
}

mod query_batches {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use std::time::Instant;

    use lore_base::error::SlowDown;
    use lore_base::retry::Retry;
    use lore_base::retry::retry_with_jitter;
    use tokio::sync::Semaphore;

    use super::*;

    const BACK_OFF: Duration = Duration::from_millis(50);

    /// Back-off of exactly [`BACK_OFF`] every time, exhausted after `limit` waits.
    fn back_off(limit: usize) -> Retry {
        retry_with_jitter(
            BACK_OFF.as_millis() as u64,
            BACK_OFF.as_millis() as u64,
            limit,
            0.0,
        )
    }

    /// A peer that throttles a batch if any address in it has been refused fewer times than that
    /// address's refusal count. Otherwise, it answers `NotFound` for every address.
    struct Peer {
        refusals: HashMap<Address, usize>,
        /// When each address with a refusal count was asked, throttled or answered.
        asks: Mutex<HashMap<Address, Vec<Instant>>>,
        /// Every batch asked, in the order asked.
        batches: Mutex<Vec<Vec<Address>>>,
    }

    impl Peer {
        fn new(refusals: &[(Address, usize)]) -> Arc<Self> {
            Arc::new(Self {
                refusals: refusals.iter().copied().collect(),
                asks: Mutex::new(HashMap::new()),
                batches: Mutex::new(Vec::new()),
            })
        }

        fn answer(&self, batch: &[Address]) -> Result<Bytes, ProtocolError> {
            self.batches.lock().expect("batches").push(batch.to_vec());
            let mut asks = self.asks.lock().expect("asks");
            let mut throttle = false;
            for address in batch {
                if let Some(&refusals) = self.refusals.get(address) {
                    let asked = asks.entry(*address).or_default();
                    asked.push(Instant::now());
                    throttle |= asked.len() <= refusals;
                }
            }
            if throttle {
                return Err(ProtocolError::from(SlowDown));
            }
            Ok(Bytes::from(vec![QueryStatus::NotFound as u8; batch.len()]))
        }

        fn asks(&self, address: Address) -> Vec<Instant> {
            self.asks
                .lock()
                .expect("asks")
                .get(&address)
                .cloned()
                .unwrap_or_default()
        }

        fn batch_count(&self) -> usize {
            self.batches.lock().expect("batches").len()
        }
    }

    fn slots() -> Arc<Semaphore> {
        Arc::new(Semaphore::new(Semaphore::MAX_PERMITS))
    }

    async fn run(
        peer: &Arc<Peer>,
        addresses: Vec<Address>,
        batch_size: usize,
        retry: Retry,
    ) -> Result<PushQueryResult, PushError> {
        let peer = peer.clone();
        query_batches(addresses, Some(batch_size), slots(), retry, move |batch| {
            let peer = peer.clone();
            async move {
                let result = peer.answer(&batch);
                (batch, result)
            }
        })
        .await
    }

    #[tokio::test]
    async fn every_address_is_answered_once_across_batches() {
        let addresses: Vec<Address> = (0..25).map(address).collect();
        let peer = Peer::new(&[]);

        let queried = run(&peer, addresses.clone(), 10, back_off(0))
            .await
            .expect("an unthrottled query needs no back-off");

        assert_eq!(queried.absent, addresses);
        assert_eq!(peer.batch_count(), 3);
    }

    /// A throttled batch is asked again only after a full back-off, and the batches the peer
    /// answered are not asked again with it.
    #[tokio::test]
    async fn a_throttled_batch_waits_for_the_back_off_before_it_is_asked_again() {
        let addresses: Vec<Address> = (0..30).map(address).collect();
        let peer = Peer::new(&[(address(5), 1)]);

        let queried = run(&peer, addresses.clone(), 10, back_off(1))
            .await
            .expect("one back-off is enough for a batch throttled once");

        assert_eq!(queried.absent, addresses);
        let asks = peer.asks(address(5));
        assert_eq!(asks.len(), 2, "the throttled batch is asked twice");
        assert!(
            asks[1] - asks[0] >= BACK_OFF,
            "asked again after {:?}",
            asks[1] - asks[0]
        );
        assert_eq!(
            peer.batch_count(),
            4,
            "only the throttled batch is asked again"
        );
    }

    #[tokio::test]
    async fn a_peer_that_keeps_throttling_fails_the_query_with_slow_down() {
        let peer = Peer::new(&[(address(0), usize::MAX)]);

        let error = run(&peer, vec![address(0)], 10, back_off(3))
            .await
            .expect_err("the back-off is spent");

        assert!(error.is_slow_down(), "{error:?}");
        assert!(error.translated() == LoreError::SlowDown);
        assert_eq!(
            peer.asks(address(0)).len(),
            4,
            "the first ask and one per back-off"
        );
    }

    /// The back-off limit counts consecutive rounds with no answers. Waiting continues as long as
    /// the peer answers some of the held batches in each round.
    #[tokio::test]
    async fn a_round_that_answers_any_batch_restarts_the_back_off() {
        let addresses: Vec<Address> = (0..4).map(address).collect();
        let refusals: Vec<(Address, usize)> =
            (1..4).map(|seed| (address(seed), seed as usize)).collect();
        let peer = Peer::new(&refusals);

        let queried = run(&peer, addresses.clone(), 1, back_off(1))
            .await
            .expect("every round answers a batch, so one back-off per round is enough");

        assert_eq!(queried.absent, addresses);
        assert_eq!(
            peer.asks(address(3)).len(),
            4,
            "the last address held for three rounds"
        );
    }

    /// Rounds that each answer something still end the query once there have been more of them
    /// than the cap allows.
    #[tokio::test]
    async fn throttled_rounds_are_capped_even_while_each_answers_something() {
        let last = MAX_THROTTLED_ROUNDS as u8 + 1;
        let addresses: Vec<Address> = (0..=last).map(address).collect();
        let refusals: Vec<(Address, usize)> = (1..=last)
            .map(|seed| (address(seed), seed as usize))
            .collect();
        let peer = Peer::new(&refusals);
        let brief_back_off = retry_with_jitter(1, 1, 1, 0.0);

        let error = run(&peer, addresses, 1, brief_back_off)
            .await
            .expect_err("the last address outlasts the cap");

        assert!(error.is_slow_down(), "{error:?}");
        assert_eq!(
            peer.asks(address(last)).len(),
            MAX_THROTTLED_ROUNDS + 1,
            "the first round and one per throttled round the cap allows"
        );
    }

    /// Any failure other than throttling ends the query immediately, without waiting for the
    /// back-off.
    #[tokio::test]
    async fn a_failure_other_than_throttling_is_not_retried() {
        let asks = Arc::new(AtomicUsize::new(0));
        let counted = asks.clone();

        let error = query_batches(vec![address(0)], None, slots(), back_off(3), move |batch| {
            counted.fetch_add(1, Ordering::Relaxed);
            async move {
                let result = Err(ProtocolError::from(AddressNotFound { address: [7u8; 48] }));
                (batch, result)
            }
        })
        .await
        .expect_err("the peer failed the query");

        assert!(error.is_address_not_found(), "{error:?}");
        assert_eq!(asks.load(Ordering::Relaxed), 1, "the peer is asked once");
    }
}

/// Asking the peer to duplicate an association it already holds, as a push does before it uploads.
/// The peer answers copy requests with `SlowDown`, a refusal, or success.
mod duplicate_association {
    use lore_base::error::NotFound;
    use lore_transport::test_util::StubPeer;

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
        let peer = Arc::new(StubPeer::default().with_copy_answers(slow_downs, refusal));
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
    use lore_revision::branch::BranchLatestStatus;
    use lore_revision::metadata::Metadata;
    use lore_revision::state::LinkReference;
    use lore_revision::state::State;

    use super::*;
    use crate::branch::with_execution;
    use crate::branch::write_branch_revision;
    use crate::paged_remote::Anchor;
    use crate::paged_remote::ClientStores;
    use crate::paged_remote::PagedRemote;
    use crate::paged_remote::RemoteRevision;
    use crate::paged_remote::client_on;
    use crate::paged_remote::client_stores;
    use crate::paged_remote::remote_revision;
    use crate::paged_remote::writing_client_on;

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
            record: None,
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
            held_links: HashSet::new(),
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
        write_on(
            repository,
            branch(),
            parent_self,
            parent_other,
            revision_number,
            name,
        )
        .await
    }

    /// Write a revision of `on`, named `name` so that revisions on the same parent differ.
    async fn write_on(
        repository: &Arc<RepositoryContext>,
        on: BranchId,
        parent_self: Hash,
        parent_other: Hash,
        revision_number: u64,
        name: &str,
    ) -> Hash {
        let mut metadata = Metadata::new();
        metadata.set_branch(on).expect("recording the branch");
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

            place_pending(&mut target, false)
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

            place_pending(&mut target, false)
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

            place_pending(&mut target, false)
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

    /// A forced push plans a local latest the remote's latest has moved past, so its offer moves the
    /// remote back to it, and one that isn't forced plans nothing, whatever the globals say.
    #[tokio::test]
    async fn a_forced_push_plans_a_local_latest_the_remote_moved_past() {
        Box::pin(with_execution(async {
            let stores = client_stores().await;
            let writer = &stores.writer;
            store_repository_metadata(writer).await;
            let base = write_on_branch(writer, Hash::default(), Hash::default(), 1, "base").await;
            let top = write_on_branch(writer, base, Hash::default(), 2, "top").await;
            let client = client_on(
                &stores,
                Arc::new(PagedRemote::new(branch(), top).metadata(Hash::from([1; 32]))),
            );
            let planned = |force| {
                let client = client.clone();
                async move {
                    let plan = plan_target(&client, branch(), base, force)
                        .await
                        .expect("planning the push");
                    plan.revisions
                        .iter()
                        .map(|pushed| (pushed.revision, pushed.local))
                        .collect::<Vec<_>>()
                }
            };

            assert_eq!(
                (planned(true).await, planned(false).await),
                (vec![(base, true)], vec![])
            );
        }))
        .await;
    }

    /// A push of the branch alone plans no layer and no linked repository: one target, whose
    /// revisions keep the links they hold, for the link check.
    #[tokio::test]
    async fn a_push_of_the_branch_alone_reaches_no_linked_repository() {
        Box::pin(with_execution(async {
            let stores = client_stores().await;
            let writer = &stores.writer;
            store_repository_metadata(writer).await;
            let base = write_on_branch(writer, Hash::default(), Hash::default(), 1, "base").await;
            let linked_repository = lore_base::types::RepositoryId::from([0x5A; 16]);
            let linked_revision = Hash::from([0x5B; 32]);
            let state = State::deserialize(writer.clone(), base)
                .await
                .expect("reading the base");
            let linking = State::new();
            linking.set_parent_self(base);
            linking.set_revision_number(2);
            linking.set_metadata_hash(state.metadata_hash());
            linking
                .link_add(
                    writer.clone(),
                    linked_repository,
                    branch(),
                    linked_revision,
                    2,
                    lore_revision::link::LinkFlags::NoFlags,
                )
                .await
                .expect("linking another repository's revision");
            let token = writer
                .try_write_token()
                .expect("a client context carries a write token");
            let latest = linking
                .serialize(writer.clone(), token)
                .await
                .expect("writing the linking revision");
            let client = client_on(
                &stores,
                Arc::new(PagedRemote::new(branch(), base).metadata(Hash::from([1; 32]))),
            );
            let run = PushRun {
                mounts: MountPolicy::RequireOnRemote,
                ..PushRun::default()
            };

            let (targets, _orders) = collect_targets(client, branch(), latest, &run, None)
                .await
                .expect("planning the push");

            let links: Vec<Vec<(lore_base::types::RepositoryId, Hash)>> = targets
                .iter()
                .map(|target| {
                    target
                        .plan
                        .revisions
                        .iter()
                        .flat_map(|pushed| &pushed.links)
                        .map(|link| (link.repository, link.signature))
                        .collect()
                })
                .collect();
            assert_eq!(links, vec![vec![(linked_repository, linked_revision)]]);
        }))
        .await;
    }

    /// Write a revision of `branch()` on `parent` that holds a link alone: to `pinned` in the
    /// repository `linked`.
    async fn write_linking(
        repository: &Arc<RepositoryContext>,
        parent: Hash,
        revision_number: u64,
        linked: lore_base::types::RepositoryId,
        pinned: Hash,
    ) -> Hash {
        let metadata = State::deserialize(repository.clone(), parent)
            .await
            .expect("reading the parent")
            .metadata_hash();
        let linking = State::new();
        linking.set_parent_self(parent);
        linking.set_revision_number(revision_number);
        linking.set_metadata_hash(metadata);
        linking
            .link_add(
                repository.clone(),
                linked,
                branch(),
                pinned,
                2,
                lore_revision::link::LinkFlags::NoFlags,
            )
            .await
            .expect("linking another repository's revision");
        let token = repository
            .try_write_token()
            .expect("a client context carries a write token");
        linking
            .serialize(repository.clone(), token)
            .await
            .expect("writing the linking revision")
    }

    fn linked() -> lore_base::types::RepositoryId {
        lore_base::types::RepositoryId::from([0x5A; 16])
    }

    /// The revision of `linked()` the remote's latest revision links.
    fn held() -> Hash {
        Hash::from([0x5B; 32])
    }

    /// A revision of `linked()` other than `held()`.
    fn changed() -> Hash {
        Hash::from([0x5C; 32])
    }

    /// The target of a push of a revision linking `pinned`, planned against a remote whose latest
    /// revision links `held()`.
    async fn linking_target(stores: &ClientStores, pinned: Hash) -> PushTarget {
        let writer = &stores.writer;
        store_repository_metadata(writer).await;
        let base = write_on_branch(writer, Hash::default(), Hash::default(), 1, "base").await;
        let remote_latest = write_linking(writer, base, 2, linked(), held()).await;
        let latest = write_linking(writer, remote_latest, 3, linked(), pinned).await;
        let client = client_on(
            stores,
            Arc::new(PagedRemote::new(branch(), remote_latest).metadata(Hash::from([1; 32]))),
        );
        let plan = plan_target(&client, branch(), latest, false)
            .await
            .expect("planning the push");
        let mut target = target(client, latest, &[], vec![]).await;
        target.plan = plan;
        target
    }

    /// The revisions of linked repositories that scanning `target` requests targets at.
    async fn requested_after_scan(target: PushTarget) -> Vec<Hash> {
        let mut collection = TargetCollection {
            targets: vec![target],
            requests: vec![],
            index: Default::default(),
        };
        scan_target(&mut collection, 0).await;
        collection
            .requests
            .iter()
            .map(|request| request.latest)
            .collect()
    }

    /// A revision whose link pins what the remote latest's link pins reaches no linked repository.
    #[tokio::test]
    async fn a_link_pinned_as_the_remote_latest_pins_it_requests_no_target() {
        Box::pin(with_execution(async {
            let target = linking_target(&client_stores().await, held()).await;

            assert_eq!(requested_after_scan(target).await, Vec::<Hash>::new());
        }))
        .await;
    }

    /// A revision whose link pins another revision than the remote latest's link requests the
    /// linked repository's branch at it.
    #[tokio::test]
    async fn a_link_pinned_elsewhere_than_the_remote_latest_requests_a_target_at_its_pin() {
        Box::pin(with_execution(async {
            let target = linking_target(&client_stores().await, changed()).await;

            assert_eq!(requested_after_scan(target).await, vec![changed()]);
        }))
        .await;
    }

    /// A forced push onto a remote that moved past the revision the local history starts from
    /// reaches no linked repository for a link pinned as that revision pins it.
    #[tokio::test]
    async fn a_link_pinned_as_where_a_diverged_remote_was_left_requests_no_target() {
        Box::pin(with_execution(async {
            let stores = client_stores().await;
            let writer = &stores.writer;
            store_repository_metadata(writer).await;
            let base = write_on_branch(writer, Hash::default(), Hash::default(), 1, "base").await;
            let left = write_linking(writer, base, 2, linked(), held()).await;
            let remote_latest = write_linking(writer, left, 3, linked(), changed()).await;
            let latest = write_linking(writer, left, 3, linked(), held()).await;
            let client = client_on(
                &stores,
                Arc::new(PagedRemote::new(branch(), remote_latest).metadata(Hash::from([1; 32]))),
            );
            let plan = plan_target(&client, branch(), latest, true)
                .await
                .expect("planning the push");
            let mut target = target(client, latest, &[], vec![]).await;
            target.plan = plan;

            assert_eq!(requested_after_scan(target).await, Vec::<Hash>::new());
        }))
        .await;
    }

    /// The push creating a branch reaches no linked repository for a link pinned as the branch
    /// point pins it.
    #[tokio::test]
    async fn a_link_pinned_as_the_branch_point_pins_it_requests_no_target() {
        Box::pin(with_execution(async {
            let stores = client_stores().await;
            let writer = &stores.writer;
            store_repository_metadata(writer).await;
            let root = write_on_branch(writer, Hash::default(), Hash::default(), 1, "root").await;
            let base = write_linking(writer, root, 2, linked(), held()).await;
            let child = BranchId::from([8; 16]);
            let latest = write_linking(writer, base, 3, linked(), held()).await;
            lore_revision::branch::store_latest(
                writer.clone(),
                branch(),
                Hash::default(),
                base,
                BranchLatestStatus::Convergent,
            )
            .await
            .expect("storing the parent's local latest");
            lore_revision::branch::create_record(
                writer.clone(),
                child,
                "child",
                lore_revision::branch::default_category(),
                "",
                0,
                vec![lore_base::types::BranchPoint {
                    branch: branch(),
                    revision: base,
                }],
                latest,
                BranchLatestStatus::Divergent,
                lore_base::types::Context::default(),
            )
            .await
            .expect("creating the child branch");
            let client = client_on(&stores, Arc::new(PagedRemote::new(child, Hash::default())));
            let plan = plan_target(&client, child, latest, false)
                .await
                .expect("planning the push");
            let mut target = target(client, latest, &[], vec![]).await;
            target.branch = child;
            target.plan = plan;

            assert_eq!(requested_after_scan(target).await, Vec::<Hash>::new());
        }))
        .await;
    }

    /// The pins of the fork and of the peer's latest revision of a merged branch are held, and
    /// those of a revision the plan pushes are not.
    #[tokio::test]
    async fn the_revisions_the_peer_holds_hold_their_pins_unless_the_plan_pushes_them() {
        Box::pin(with_execution(async {
            let stores = client_stores().await;
            let writer = &stores.writer;
            let base = write_on_branch(writer, Hash::default(), Hash::default(), 1, "base").await;
            let linking = write_linking(writer, base, 2, linked(), held()).await;
            let client = client_on(
                &stores,
                Arc::new(PagedRemote::new(branch(), Hash::default())),
            );
            let merged = BranchId::from([9; 16]);
            let mut held_links = vec![];
            for (pushed, peer_latest, fork) in [
                (vec![], Hash::default(), linking),
                (vec![], linking, Hash::default()),
                (vec![linking], linking, linking),
            ] {
                let mut target = target(client.clone(), linking, &pushed, vec![]).await;
                target.plan.peer_latest.push((merged, peer_latest));
                hold_remote_links(&client, &mut target.plan, 0, fork).await;
                held_links.push(target.plan.held_links);
            }

            let pinned = HashSet::from([(linked(), held())]);
            assert_eq!(held_links, vec![pinned.clone(), pinned, HashSet::new()]);
        }))
        .await;
    }

    /// A target of `branch()` pushing a revision on `base`, with a remote whose latest revision of
    /// another branch, `merged`, links `held()`, and that revision.
    async fn target_beside_a_merged_line(stores: &ClientStores) -> (PushTarget, Hash) {
        let writer = &stores.writer;
        let merged = BranchId::from([9; 16]);
        let base = write_on_branch(writer, Hash::default(), Hash::default(), 1, "base").await;
        let latest = write_on_branch(writer, base, Hash::default(), 2, "latest").await;
        let merged_root = write_on(writer, merged, base, Hash::default(), 2, "merged").await;
        let merged_latest = write_linking(writer, merged_root, 3, linked(), held()).await;
        let client = client_on(stores, Arc::new(PagedRemote::new(merged, merged_latest)));
        (
            target(client, latest, &[latest], vec![]).await,
            merged_latest,
        )
    }

    /// A plan extended to a merge of a line whose remote latest links `held()` holds that pin, so
    /// a revision on the extension linking it reaches no linked repository, and one linking
    /// another revision requests the linked repository's branch at it.
    #[tokio::test]
    async fn a_plan_extended_through_a_merged_line_holds_the_pin_of_its_remote_latest() {
        Box::pin(with_execution(async {
            for (pinned, requested) in [(held(), vec![]), (changed(), vec![changed()])] {
                let stores = client_stores().await;
                let (mut target, merged_latest) = target_beside_a_merged_line(&stores).await;
                let writer = &stores.writer;
                let merge = write_on_branch(writer, target.latest, merged_latest, 3, "merge").await;
                let newer = write_linking(writer, merge, 4, linked(), pinned).await;
                target.pending = vec![newer];

                place_pending(&mut target, false)
                    .await
                    .expect("placing the linked revision");

                assert_eq!(target.latest, newer);
                assert_eq!(requested_after_scan(target).await, requested);
            }
        }))
        .await;
    }

    /// A line of another branch uploaded back to its remote latest, which links `held()`, holds
    /// that pin, so a revision on the line linking it reaches no linked repository, and one
    /// linking another revision requests the linked repository's branch at it.
    #[tokio::test]
    async fn an_uploaded_line_holds_the_pin_of_its_branchs_remote_latest() {
        Box::pin(with_execution(async {
            for (pinned, requested) in [(held(), vec![]), (changed(), vec![changed()])] {
                let stores = client_stores().await;
                let (mut target, merged_latest) = target_beside_a_merged_line(&stores).await;
                let other = write_linking(&stores.writer, merged_latest, 4, linked(), pinned).await;
                target.pending = vec![other];

                place_pending(&mut target, false)
                    .await
                    .expect("placing the linked revision");

                assert_eq!(pushed(&target), vec![(target.latest, true), (other, false)]);
                assert_eq!(requested_after_scan(target).await, requested);
            }
        }))
        .await;
    }

    /// A push of a branch the remote holds at its latest revision reaches no linked repository
    /// whose branch is locally at the pin that revision holds.
    #[tokio::test]
    async fn a_pushed_branch_with_its_linked_branch_at_the_pin_it_holds_is_not_pushed() {
        Box::pin(with_execution(async {
            let stores = client_stores().await;
            let writer = &stores.writer;
            store_repository_metadata(writer).await;
            let base = write_on_branch(writer, Hash::default(), Hash::default(), 1, "base").await;
            let latest = write_linking(writer, base, 2, linked(), held()).await;
            lore_revision::branch::store_latest(
                writer.to_link_context(linked()).await,
                branch(),
                Hash::default(),
                held(),
                BranchLatestStatus::Convergent,
            )
            .await
            .expect("storing the linked branch's local latest");
            let client = client_on(
                &stores,
                Arc::new(PagedRemote::new(branch(), latest).metadata(Hash::from([1; 32]))),
            );

            let (targets, _orders) =
                collect_targets(client, branch(), latest, &PushRun::default(), None)
                    .await
                    .expect("planning the push");

            assert_eq!(targets.len(), 1);
        }))
        .await;
    }

    /// A push whose linked branch is locally at the revision the remote latest's link pins
    /// reaches no linked repository.
    #[tokio::test]
    async fn a_linked_branch_at_a_pin_the_remote_latest_holds_is_not_pushed() {
        Box::pin(with_execution(async {
            let stores = client_stores().await;
            let root = linking_target(&stores, held()).await;
            lore_revision::branch::store_latest(
                stores.writer.to_link_context(linked()).await,
                branch(),
                Hash::default(),
                held(),
                BranchLatestStatus::Convergent,
            )
            .await
            .expect("storing the linked branch's local latest");

            let (targets, _orders) = collect_targets(
                root.repository,
                branch(),
                root.latest,
                &PushRun::default(),
                None,
            )
            .await
            .expect("planning the push");

            assert_eq!(targets.len(), 1);
        }))
        .await;
    }

    /// The check that the remote holds each revision a push links asks nothing of a linked
    /// repository whose pin the remote latest holds.
    #[tokio::test]
    async fn a_link_pinned_as_the_remote_latest_pins_it_is_not_checked_on_the_remote() {
        Box::pin(with_execution(async {
            let target = linking_target(&client_stores().await, held()).await;

            check_links_on_remote(&target)
                .await
                .expect("checking the links on the remote");
        }))
        .await;
    }

    /// The walk against `remote` of the history of a branch the remote doesn't hold, created from
    /// `branch()` at the second of its two revisions, with a revision of its own on top, which
    /// reaches the parent branch at the branch point. `branch()` is divergent in the local store, so
    /// the walk goes on into it.
    async fn walk_from_a_child(remote: PagedRemote) -> Result<UnpushedHistory, PushError> {
        let stores = client_stores().await;
        let writer = &stores.writer;
        store_repository_metadata(writer).await;
        let root = write_on_branch(writer, Hash::default(), Hash::default(), 1, "root").await;
        let base = write_on_branch(writer, root, Hash::default(), 2, "base").await;
        let child = BranchId::from([8; 16]);
        let latest = write_on_branch(writer, base, Hash::default(), 3, "child").await;
        lore_revision::branch::store_latest(
            writer.clone(),
            branch(),
            Hash::default(),
            base,
            BranchLatestStatus::Divergent,
        )
        .await
        .expect("storing the parent's local latest");
        lore_revision::branch::create_record(
            writer.clone(),
            child,
            "child",
            lore_revision::branch::default_category(),
            "",
            0,
            vec![lore_base::types::BranchPoint {
                branch: branch(),
                revision: base,
            }],
            latest,
            BranchLatestStatus::Divergent,
            lore_base::types::Context::default(),
        )
        .await
        .expect("creating the child branch");
        let client = client_on(&stores, Arc::new(remote));
        let remote = client.remote().await.expect("the client is connected");
        let state = State::deserialize(client.clone(), latest)
            .await
            .expect("reading the child's revision");
        collect_unpushed_history(
            &client,
            &remote,
            &state,
            child,
            latest,
            Hash::default(),
            branch(),
        )
        .await
    }

    /// A read of the parent branch at the remote that fails fails the walk with the read's error,
    /// rather than taking the parent branch as absent.
    #[tokio::test]
    async fn a_failed_read_of_a_parent_branch_fails_the_walk() {
        Box::pin(with_execution(async {
            let refusing = PagedRemote::new(branch(), Hash::default())
                .refusing(ProtocolError::from(lore_base::error::NotAuthorized));
            let walked = walk_from_a_child(refusing).await;
            assert!(
                walked.as_ref().is_err_and(|err| err.is_not_authorized()),
                "{:?}",
                walked.err()
            );
        }))
        .await;
    }

    /// A push whose read of the remote branch fails, other than as the remote not holding the
    /// branch, fails with the read's error, rather than planning the branch as absent.
    #[tokio::test]
    async fn a_failed_branch_query_fails_the_plan() {
        Box::pin(with_execution(async {
            let stores = client_stores().await;
            let writer = &stores.writer;
            store_repository_metadata(writer).await;
            let latest = write_on_branch(writer, Hash::default(), Hash::default(), 1, "base").await;
            let planned = |refusal| {
                let client = client_on(
                    &stores,
                    Arc::new(PagedRemote::new(branch(), Hash::default()).refusing(refusal)),
                );
                async move { plan_target(&client, branch(), latest, false).await }
            };

            let refused = planned(ProtocolError::from(lore_base::error::NotAuthorized)).await;
            let disconnected = planned(ProtocolError::from(Disconnected)).await;

            assert!(
                refused.as_ref().is_err_and(|err| err.is_not_authorized())
                    && disconnected
                        .as_ref()
                        .is_err_and(|err| err.is_disconnected()),
                "{:?}; {:?}",
                refused.err(),
                disconnected.err()
            );
        }))
        .await;
    }

    /// A push refused while planning, as diverged, fails with `BranchAdvanced` and reports what it
    /// read of the remote.
    #[tokio::test]
    async fn a_push_refused_as_diverged_fails_branch_advanced_and_reports_the_remote() {
        Box::pin(with_execution(async {
            let stores = client_stores().await;
            let writer = &stores.writer;
            store_repository_metadata(writer).await;
            let base = write_on_branch(writer, Hash::default(), Hash::default(), 1, "base").await;
            let local = write_on_branch(writer, base, Hash::default(), 2, "local").await;
            let remote = write_on_branch(writer, base, Hash::default(), 2, "remote").await;
            let client = client_on(
                &stores,
                Arc::new(PagedRemote::new(branch(), remote).metadata(Hash::from([1; 32]))),
            );
            let mut report = PushReport::default();

            let collected = collect_targets(
                client,
                branch(),
                local,
                &PushRun::default(),
                Some(&mut report),
            )
            .await;

            assert!(
                collected
                    .as_ref()
                    .is_err_and(|err| err.is_branch_advanced()),
                "a diverged push is refused"
            );
            assert_eq!(
                report,
                PushReport {
                    remote_read: true,
                    remote_latest: remote,
                    ..PushReport::default()
                }
            );
        }))
        .await;
    }

    /// The local latest a target's push leaves as it finishes, with whether it is divergent, its
    /// last synchronized revision, and the local latest the report takes. The local latest is
    /// `stored` when the push finishes, the peer accepted `published` last, and `merged` says
    /// whether it published by a fast-forward merge.
    async fn finished(
        stores: &ClientStores,
        stored: &[Hash],
        published: Hash,
        merged: bool,
        run: PushRun,
    ) -> ((Hash, bool, Hash), Hash) {
        let mut previous = Hash::default();
        for &latest in stored {
            lore_revision::branch::store_latest(
                stores.writer.clone(),
                branch(),
                previous,
                latest,
                BranchLatestStatus::Divergent,
            )
            .await
            .expect("storing the local latest");
            previous = latest;
        }
        let remote = Arc::new(PagedRemote::new(branch(), published));
        let client = writing_client_on(stores, Some(remote.clone()));
        let target = target(client, stored[0], &[stored[0]], vec![]).await;
        let started = TargetRun {
            revision_protocol: remote,
            storage: Arc::new(lore_transport::StorageSession::pending(
                std::future::pending,
            )),
            remote_latest: published,
            current_latest: published,
            fast_forward_merged: merged,
        };
        let mut report = PushReport::default();

        finish_target(&target, &started, &run, Some(&mut report))
            .await
            .expect("finishing the push");

        (records(&stores.writer).await, report.local_latest)
    }

    /// A push run from the local latest it read writes the published revision over it as it
    /// finishes, convergent, a fast-forward merge's included, and reports it.
    #[tokio::test]
    async fn a_run_from_its_local_latest_writes_the_published_revision_over_it() {
        Box::pin(with_execution(async {
            let stores = client_stores().await;
            let writer = &stores.writer;
            let local = write_on_branch(writer, Hash::default(), Hash::default(), 1, "local").await;
            let merge = write_on_branch(writer, local, Hash::default(), 2, "merge").await;
            let run = PushRun {
                expected_local_latest: Some(local),
                ..PushRun::default()
            };

            assert_eq!(
                finished(&stores, &[local], merge, true, run).await,
                ((merge, false, merge), merge)
            );
        }))
        .await;
    }

    /// A local latest another writer moved while the push ran stays in place, and is what the
    /// push reports it left.
    #[tokio::test]
    async fn a_local_latest_another_writer_moved_stays() {
        Box::pin(with_execution(async {
            let stores = client_stores().await;
            let writer = &stores.writer;
            let local = write_on_branch(writer, Hash::default(), Hash::default(), 1, "local").await;
            let committed = write_on_branch(writer, local, Hash::default(), 2, "commit").await;
            let published = write_on_branch(writer, local, Hash::default(), 2, "published").await;
            let run = PushRun {
                expected_local_latest: Some(local),
                ..PushRun::default()
            };

            let (records, left) =
                finished(&stores, &[local, committed], published, false, run).await;

            assert_eq!((records.0, records.1, left), (committed, true, committed));
        }))
        .await;
    }

    /// A file-system push records the revision the peer merged as the local latest, divergent, and
    /// as the last synchronized revision once every revision is pushed.
    #[tokio::test]
    async fn a_file_system_push_records_a_merge_divergent() {
        Box::pin(with_execution(async {
            let stores = client_stores().await;
            let writer = &stores.writer;
            let local = write_on_branch(writer, Hash::default(), Hash::default(), 1, "local").await;
            let merge = write_on_branch(writer, local, Hash::default(), 2, "merge").await;

            assert_eq!(
                finished(&stores, &[local], merge, true, PushRun::default()).await,
                ((merge, true, merge), merge)
            );
        }))
        .await;
    }

    /// A run from its local latest records the revision the peer merged as the last synchronized
    /// revision where another writer moved the local latest, which stays.
    #[tokio::test]
    async fn a_merge_is_the_last_sync_where_another_writer_moved_the_local_latest() {
        Box::pin(with_execution(async {
            let stores = client_stores().await;
            let writer = &stores.writer;
            let local = write_on_branch(writer, Hash::default(), Hash::default(), 1, "local").await;
            let committed = write_on_branch(writer, local, Hash::default(), 2, "commit").await;
            let merge = write_on_branch(writer, local, Hash::default(), 2, "merge").await;
            let run = PushRun {
                expected_local_latest: Some(local),
                ..PushRun::default()
            };

            assert_eq!(
                finished(&stores, &[local, committed], merge, true, run).await,
                ((committed, true, merge), committed)
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

    /// A layer already at the peer's latest revision records it as its last synchronized
    /// revision when its push starts, and pushes nothing.
    #[tokio::test]
    async fn a_layer_at_the_remote_latest_records_its_last_sync() {
        Box::pin(with_execution(async {
            let stores = client_stores().await;
            let writer = &stores.writer;
            let latest = write_on_branch(writer, Hash::default(), Hash::default(), 1, "base").await;
            let client = writing_client_on(
                &stores,
                Some(Arc::new(PagedRemote::new(branch(), Hash::default()))),
            );
            let mut layer = target(client, latest, &[], vec![]).await;
            layer.plan.remote_latest = latest;
            layer.plan.remote_metadata = Hash::from([1; 32]);
            layer.repository = layer.repository.to_layer_context(layer.repository.id).await;
            layer.mount = Some(Mount::Layer("lay".into()));

            let run = start_target(&layer, false, None)
                .await
                .expect("starting the push");

            let last_sync = lore_revision::branch::load_last_sync(writer.clone(), branch())
                .await
                .unwrap_or_default();
            assert_eq!((run.is_none(), last_sync), (true, latest));
        }))
        .await;
    }

    /// A diverged target at `offered`, the local latest of `branch()` marked divergent, whose plan
    /// pushes `local` and whose peer is at `remote_latest` on the branch, past it.
    async fn diverged_target(
        stores: &ClientStores,
        offered: Hash,
        local: &[Hash],
        remote_latest: Hash,
    ) -> PushTarget {
        lore_revision::branch::store_latest(
            stores.writer.clone(),
            branch(),
            Hash::default(),
            offered,
            BranchLatestStatus::Divergent,
        )
        .await
        .expect("storing the local latest");
        let client = writing_client_on(
            stores,
            Some(Arc::new(PagedRemote::new(branch(), remote_latest))),
        );
        let mut target = target(client, offered, local, vec![]).await;
        target.plan.diverged = true;
        target.plan.remote_latest = remote_latest;
        target.plan.remote_metadata = Hash::from([1; 32]);
        target
    }

    /// Refuse or recover `targets` as a push plans them, then start the push of each recovered one
    /// with a record to write, which writes it.
    async fn settle(targets: &mut [PushTarget]) -> Result<(), PushError> {
        refuse_diverged_targets(targets, &PushRun::default()).await?;
        for target in targets.iter() {
            if target.plan.record.is_some() {
                start_target(target, false, None).await?;
            }
        }
        Ok(())
    }

    /// The local latest of `branch()`, whether it is marked divergent, and its last synchronized
    /// revision, zero when there is none.
    async fn records(writer: &Arc<RepositoryContext>) -> (Hash, bool, Hash) {
        let latest = lore_revision::branch::load_latest(writer.clone(), branch())
            .await
            .expect("loading the local latest");
        let divergent = lore_revision::branch::load_latest_divergent(writer.clone(), branch())
            .await
            .expect("loading the local latest status");
        let last_sync = lore_revision::branch::load_last_sync(writer.clone(), branch())
            .await
            .unwrap_or_default();
        (latest, divergent, last_sync)
    }

    /// How a local latest at the revision pushed is left.
    #[derive(Clone, Copy)]
    enum LeftBy {
        /// A push the peer accepted, killed before it recorded the answer: divergent, as the
        /// commit stored it, with the revision as the last synchronized one.
        KilledPush,
        /// A reset: divergent, with no last synchronized revision.
        Reset,
        /// A sync back to it: convergent, with the revision as the last synchronized one.
        SyncBack,
    }

    /// A diverged target with nothing to send, at a local latest left as `left` says at the revision
    /// pushed, which the remote moved past: whether the push fails, the records after it, and the
    /// revision pushed.
    async fn pushed_and_moved_past(left: LeftBy) -> (bool, (Hash, bool, Hash), Hash) {
        let stores = client_stores().await;
        let writer = &stores.writer;
        let base = write_on_branch(writer, Hash::default(), Hash::default(), 1, "base").await;
        let pushed = write_on_branch(writer, base, Hash::default(), 2, "pushed").await;
        let on_top = write_on_branch(writer, pushed, Hash::default(), 3, "on top").await;
        let mut target = diverged_target(&stores, pushed, &[], on_top).await;
        target.plan.local_history = 0;
        match left {
            LeftBy::KilledPush => {
                lore_revision::branch::store_last_sync(writer.clone(), branch(), pushed).await;
            }
            LeftBy::Reset => {
                lore_revision::branch::store_last_sync(writer.clone(), branch(), Hash::default())
                    .await;
            }
            LeftBy::SyncBack => {
                lore_revision::branch::store_latest(
                    writer.clone(),
                    branch(),
                    pushed,
                    pushed,
                    BranchLatestStatus::Convergent,
                )
                .await
                .expect("storing the local latest convergent");
                lore_revision::branch::store_last_sync(writer.clone(), branch(), pushed).await;
            }
        }
        let mut targets = [target];

        let refused = settle(&mut targets).await;

        (refused.is_err(), records(writer).await, pushed)
    }

    /// A local latest a push the peer accepted left unrecorded, killed before another instance
    /// pushed on top, is recorded convergent.
    #[tokio::test]
    async fn a_revision_a_killed_push_left_that_the_remote_moved_past_is_recorded() {
        Box::pin(with_execution(async {
            let (refused, records, pushed) = pushed_and_moved_past(LeftBy::KilledPush).await;
            assert_eq!((refused, records), (false, (pushed, false, pushed)));
        }))
        .await;
    }

    /// The same local latest left by a reset, or by a sync back to it, still fails as diverged.
    #[tokio::test]
    async fn a_revision_reset_or_synced_back_to_that_the_remote_moved_past_diverges() {
        Box::pin(with_execution(async {
            let (reset, reset_records, pushed) = pushed_and_moved_past(LeftBy::Reset).await;
            let (synced, synced_records, _) = pushed_and_moved_past(LeftBy::SyncBack).await;
            assert_eq!(
                (reset, reset_records.1, synced, synced_records.1),
                (true, true, true, false),
                "{pushed}"
            );
        }))
        .await;
    }

    /// The revisions a remote changed after planning is built from: `base`, the parent of the
    /// revision pushed; `planned`, the remote latest planning found, past the revision pushed; and
    /// two revisions only the remote holds: `further`, past `planned`, and `beside`, on a line off
    /// `base` without the revision pushed.
    struct Lines {
        base: Hash,
        planned: Hash,
        further: RemoteRevision,
        beside: RemoteRevision,
    }

    impl Lines {
        /// The remote at `latest` on the branch, with metadata.
        fn at(latest: Hash) -> PagedRemote {
            PagedRemote::new(branch(), latest).metadata(Hash::from([1; 32]))
        }

        /// The remote at `further`, which it lists above `planned`.
        fn moved_further(&self) -> PagedRemote {
            let further = self.further.signature;
            Self::at(further).holding([&self.further]).page(
                Anchor::Signature(further),
                vec![(4, further)],
                self.planned,
            )
        }

        /// The remote force-pushed to `beside`, which it lists above `base`.
        fn force_pushed(&self) -> PagedRemote {
            let beside = self.beside.signature;
            Self::at(beside).holding([&self.beside]).page(
                Anchor::Signature(beside),
                vec![(3, beside)],
                self.base,
            )
        }
    }

    /// How a target recovered from a killed push at a revision the remote moved past starts, on the
    /// remote `changed` builds from [`Lines`] after planning.
    struct Started {
        /// Whether the remote holds the revision pushed, `None` where asking fails.
        held: Option<bool>,
        refused: bool,
        records: (Hash, bool, Hash),
        pushed: Hash,
    }

    /// Recover a target, put it on the remote `changed` builds, and start its push ([`Started`]).
    async fn recovered_then_changed(changed: impl FnOnce(&Lines) -> PagedRemote) -> Started {
        let stores = client_stores().await;
        let writer = &stores.writer;
        let base = write_on_branch(writer, Hash::default(), Hash::default(), 1, "base").await;
        let pushed = write_on_branch(writer, base, Hash::default(), 2, "pushed").await;
        let planned = write_on_branch(writer, pushed, Hash::default(), 3, "on top").await;
        let lines = Lines {
            base,
            planned,
            further: remote_revision(4, planned, Hash::from([0xA0; 32])).await,
            beside: remote_revision(3, base, Hash::from([0xB0; 32])).await,
        };
        let mut target = diverged_target(&stores, pushed, &[], planned).await;
        target.plan.local_history = 0;
        lore_revision::branch::store_last_sync(writer.clone(), branch(), pushed).await;
        let mut targets = [target];
        refuse_diverged_targets(&mut targets, &PushRun::default())
            .await
            .expect("a killed push left the revision unrecorded");
        let [mut target] = targets;
        target.repository = writing_client_on(&stores, Some(Arc::new(changed(&lines))));
        target.plan.remote = target
            .repository
            .remote()
            .await
            .expect("the client is connected");

        let held = remote_holds_recorded(&target, pushed).await.ok();
        let started = start_target(&target, false, None).await;

        Started {
            held,
            refused: started.is_err_and(|err| err.is_branch_advanced()),
            records: records(writer).await,
            pushed,
        }
    }

    /// A recovered target whose remote is still at the planned latest, or moved further on to a
    /// revision only the remote holds, as its push starts is recorded convergent.
    #[tokio::test]
    async fn a_recovered_target_the_remote_still_holds_is_recorded() {
        Box::pin(with_execution(async {
            let unchanged = recovered_then_changed(|lines| Lines::at(lines.planned)).await;
            let further = recovered_then_changed(Lines::moved_further).await;
            for started in [unchanged, further] {
                assert_eq!(
                    (started.held, started.refused, started.records),
                    (Some(true), false, (started.pushed, false, started.pushed))
                );
            }
        }))
        .await;
    }

    /// A recovered target whose remote, between planning and the start of its push, is
    /// force-pushed to a line without its revision, archived, gone, emptied, moved to a revision it
    /// does not list, or reset behind the revision fails as diverged and keeps its local latest
    /// divergent.
    #[tokio::test]
    async fn a_recovered_target_the_remote_dropped_since_planning_diverges() {
        Box::pin(with_execution(async {
            let force_pushed = recovered_then_changed(Lines::force_pushed).await;
            let archived = recovered_then_changed(|lines| Lines::at(lines.planned).deleted()).await;
            let gone = recovered_then_changed(|_lines| {
                Lines::at(Hash::default()).refusing(ProtocolError::from(lore_base::error::NotFound))
            })
            .await;
            let emptied = recovered_then_changed(|_lines| Lines::at(Hash::default())).await;
            let unlisted = recovered_then_changed(|lines| Lines::at(lines.further.signature)).await;
            let behind = recovered_then_changed(|lines| Lines::at(lines.base)).await;
            for started in [force_pushed, archived, gone, emptied, unlisted, behind] {
                assert_eq!(
                    (started.held, started.refused, started.records.1),
                    (Some(false), true, true)
                );
            }
        }))
        .await;
    }

    /// Planning a push that then fails writes no record: a target recovered from a killed push,
    /// beside a layer that has diverged, keeps its local latest as it was.
    #[tokio::test]
    async fn a_push_that_fails_while_it_plans_records_nothing() {
        Box::pin(with_execution(async {
            let stores = client_stores().await;
            let writer = &stores.writer;
            let base = write_on_branch(writer, Hash::default(), Hash::default(), 1, "base").await;
            let pushed = write_on_branch(writer, base, Hash::default(), 2, "pushed").await;
            let on_top = write_on_branch(writer, pushed, Hash::default(), 3, "on top").await;
            let mut root = diverged_target(&stores, pushed, &[], on_top).await;
            root.plan.local_history = 0;
            lore_revision::branch::store_last_sync(writer.clone(), branch(), pushed).await;

            let layered = client_stores().await;
            let lone = write_on_branch(&layered.writer, base, Hash::default(), 2, "lone").await;
            let other = write_on_branch(&layered.writer, base, Hash::default(), 2, "other").await;
            let mut layer = diverged_target(&layered, lone, &[lone], other).await;
            layer.repository = layer.repository.to_layer_context(layer.repository.id).await;
            layer.mount = Some(Mount::Layer("lay".into()));
            let mut targets = [root, layer];

            let refused = refuse_diverged_targets(&mut targets, &PushRun::default()).await;

            assert_eq!(
                (refused.is_err(), records(writer).await),
                (true, (pushed, true, pushed))
            );
        }))
        .await;
    }

    /// A target recorded at a revision the remote moved past, then extended to a revision a link
    /// names on top of it, fails as diverged before anything is pushed.
    #[tokio::test]
    async fn a_recorded_target_the_remote_moved_past_is_not_extended() {
        Box::pin(with_execution(async {
            let stores = client_stores().await;
            let writer = &stores.writer;
            let base = write_on_branch(writer, Hash::default(), Hash::default(), 1, "base").await;
            let pushed_revision = write_on_branch(writer, base, Hash::default(), 2, "pushed").await;
            let on_top =
                write_on_branch(writer, pushed_revision, Hash::default(), 3, "on top").await;
            let newer = write_on_branch(writer, pushed_revision, Hash::default(), 3, "newer").await;
            let mut target = diverged_target(&stores, pushed_revision, &[], on_top).await;
            target.plan.local_history = 0;
            lore_revision::branch::store_last_sync(writer.clone(), branch(), pushed_revision).await;
            let mut targets = [target];
            settle(&mut targets)
                .await
                .expect("a killed push left the revision unrecorded");
            let [mut target] = targets;
            target.pending = vec![newer];

            let extended = place_pending(&mut target, false).await;

            assert!(extended.is_err_and(|err| err.is_branch_advanced()));
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
    /// which keeps its code: an internal error where the peer's page does not continue the line,
    /// its cursor naming the page's own revision, and the peer's `NotAuthorized` where its list
    /// fails.
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
                (stopped.is_internal(), refused.is_not_authorized()),
                (true, true),
                "{stopped:?}; {refused:?}"
            );
        }))
        .await;
    }
}
