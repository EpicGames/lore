// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod metadata_migrator;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use lore_aws::store::immutable_store::*;
use lore_aws::store::object_metadata::PAYLOAD_FLAGS;
use lore_aws::store::object_metadata::from_object_metadata;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::FragmentReference;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_storage::ImmutableStore as ImmutableStoreTrait;
use lore_storage::StoreError;
use lore_storage::StoreMatch;
use lore_storage::StoreMatchResult;
use lore_storage::StoreObliterateStats;
use lore_storage::immutable_store::CopyBehavior;
use lore_storage::immutable_store::query_one;
use rand::random;
use tokio::task::JoinSet;
use zerocopy::IntoBytes;

use crate::store::fake::*;

#[tokio::test]
async fn put_stores_the_fragment_on_the_object() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    store(&fake)
        .await
        .put(partition, address, fragment, Some(payload.clone()), false)
        .await
        .expect("put should succeed");

    assert_eq!(fake.stored_fragment(address.hash), Some(fragment));
    assert_eq!(fake.object(address.hash).unwrap().0, payload.as_ref());
    assert_eq!(fake.state_of(address.hash), Some(FragmentState::Stored));
    assert_eq!(fake.association_count(address.hash), 1);
}

#[tokio::test]
async fn put_of_an_already_associated_fragment_writes_nothing() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, address, fragment, Some(payload.clone()), false)
        .await
        .expect("first put should succeed");

    let (other, other_payload) = representation(FragmentFlags::PayloadCompressedLZ4, 96, 256);
    store
        .put(partition, address, other, Some(other_payload), false)
        .await
        .expect("second put should succeed");

    assert_eq!(
        fake.stored_fragment(address.hash),
        Some(fragment),
        "an already associated put must not re-upload"
    );
    assert_eq!(fake.object(address.hash).unwrap().0, payload.as_ref());
}

#[tokio::test]
async fn put_deduplicates_across_partitions_without_uploading() {
    let fake = Fake::default();
    let hash: Hash = random();
    let first = Address {
        hash,
        context: random(),
    };
    let second = Address {
        hash,
        context: random(),
    };
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(
            random::<Context>().into(),
            first,
            fragment,
            Some(payload.clone()),
            false,
        )
        .await
        .expect("first put should succeed");

    let (other, other_payload) = representation(FragmentFlags::PayloadCompressedLZ4, 96, 256);
    store
        .put(
            random::<Context>().into(),
            second,
            other,
            Some(other_payload),
            false,
        )
        .await
        .expect("cross-partition put should succeed");

    assert_eq!(
        fake.stored_fragment(hash),
        Some(fragment),
        "deduplication must leave the stored representation alone"
    );
    assert_eq!(fake.association_count(hash), 2);
}

#[tokio::test]
async fn put_without_a_payload_may_not_claim_stored_content() {
    let fake = Fake::default();
    let hash: Hash = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(
            random::<Context>().into(),
            Address {
                hash,
                context: random(),
            },
            fragment,
            Some(payload),
            false,
        )
        .await
        .expect("first put should succeed");

    let claimed = Address {
        hash,
        context: random(),
    };
    store
        .put(random::<Context>().into(), claimed, fragment, None, false)
        .await
        .expect_err("a hash alone is not evidence the caller holds the content");

    assert_eq!(fake.association_count(hash), 1);
}

#[tokio::test]
async fn get_reads_the_fragment_from_the_object_it_returns() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, address, fragment, Some(payload.clone()), false)
        .await
        .expect("put should succeed");

    let (loaded, bytes) = store
        .get(partition, address)
        .await
        .and_then(lore_storage::StoreGetData::into_payload)
        .expect("get should succeed");

    assert_eq!(bytes, payload);
    assert_eq!(loaded.size_payload, fragment.size_payload);
    assert_eq!(loaded.size_content, fragment.size_content);
    assert_eq!(
        loaded.flags & FragmentFlags::PayloadStoredDurable,
        FragmentFlags::PayloadStoredDurable,
        "durability is derived from the object existing, not read from a record"
    );
}

/// Query answers from `DynamoDB` alone. It is on the ingress write path, once per fragment
/// stored, so it must not reach S3 — which means it reports whether the payload is there and
/// durable, not what representation is stored.
#[tokio::test]
async fn query_reports_a_match_without_reading_s3() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("put should succeed");

    let result = query_one(&(store as Arc<dyn ImmutableStoreTrait>), partition, address)
        .await
        .expect("resolve should succeed");

    assert_eq!(result.match_made, StoreMatch::MatchFull);
    assert!(result.stored_durable);
    assert_eq!(
        fake.object_reads(),
        0,
        "resolve must not touch S3; it is called once per fragment on the ingress write path"
    );
}

/// The representation a put stored must come back from a later metadata read — the whole
/// reason this path exists separately from `query`, which cannot report it.
#[tokio::test]
async fn get_metadata_returns_the_representation_that_was_stored() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("put should succeed");

    let result = store
        .get_metadata(partition, address)
        .await
        .expect("get_metadata should succeed");

    assert_eq!(result.match_made, StoreMatch::MatchFull);
    assert_eq!(result.fragment.size_payload, fragment.size_payload);
    assert_eq!(result.fragment.size_content, fragment.size_content);
    assert_eq!(
        result.fragment.flags & PAYLOAD_FLAGS,
        fragment.flags & PAYLOAD_FLAGS,
        "the stored compression must survive the round trip"
    );
    assert_eq!(
        result.fragment.flags & FragmentFlags::PayloadStoredDurable,
        FragmentFlags::PayloadStoredDurable
    );
    assert_eq!(
        fake.object_reads(),
        1,
        "exactly one HeadObject, and only on this path"
    );
}

#[tokio::test]
async fn get_metadata_reads_a_preexisting_object_from_the_fragment_metadata_table() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, _) = preexisting_object(&fake, partition, address);

    let result = migrated_store(&fake)
        .await
        .get_metadata(partition, address)
        .await
        .expect("get_metadata should succeed");

    assert_eq!(result.match_made, StoreMatch::MatchFull);
    assert_eq!(result.fragment.size_payload, fragment.size_payload);
    assert_eq!(result.fragment.size_content, fragment.size_content);
}

/// The resolution alone decides a miss, and it is not held back for the object.
///
/// Whether the overlapped `HeadObject` reaches S3 first is not part of the contract: it is
/// started alongside the resolution rather than after it, so a resolution that answers without
/// ever yielding cancels it unsent, while one that yields lets it go out. Both are correct and
/// its result is discarded either way, so this bounds the reads instead of fixing them — an
/// equality here asserts the scheduler, not the store.
#[tokio::test]
async fn get_metadata_reports_a_miss_for_an_unknown_address() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };

    let result = store(&fake)
        .await
        .get_metadata(random::<Context>().into(), address)
        .await
        .expect("get_metadata should succeed");

    assert_eq!(result.match_made, StoreMatch::MatchNone);
    assert!(
        fake.object_reads() <= 1,
        "a miss must cost at most the one overlapped HeadObject, saw {}",
        fake.object_reads()
    );
}

/// The ordering the single-read existence path rests on: the association is written last, so
/// nothing can observe one whose content is not already stored and published.
#[tokio::test]
async fn a_put_associates_only_after_publishing_the_state() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("put should succeed");

    assert_eq!(fake.state_of(address.hash), Some(FragmentState::Stored));
    assert!(fake.has_association(partition, address));
}

/// Obliteration drops the reference before it marks the hash, because the reference is the
/// obligation and the mark is only bookkeeping for the reclamation that follows.
#[tokio::test]
async fn obliterate_deletes_the_association_before_marking_the_hash() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("put should succeed");

    store
        .clone()
        .obliterate(
            partition,
            address,
            Arc::new(StoreObliterateStats::default()),
        )
        .await
        .expect("obliterate should succeed");

    assert!(!fake.has_association(partition, address));

    let result = query_one(&(store as Arc<dyn ImmutableStoreTrait>), partition, address)
        .await
        .expect("query should succeed");
    assert_eq!(result.match_made, StoreMatch::MatchNone);
}

#[tokio::test]
async fn force_write_replaces_the_stored_representation() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    store(&fake)
        .await
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("put should succeed");

    let (replacement, replacement_payload) =
        representation(FragmentFlags::PayloadCompressedLZ4, 96, 256);
    store_with(&fake, true, false)
        .await
        .put(
            partition,
            address,
            replacement,
            Some(replacement_payload.clone()),
            false,
        )
        .await
        .expect("forced put should succeed");

    assert_eq!(fake.stored_fragment(address.hash), Some(replacement));
    assert_eq!(
        fake.object(address.hash).unwrap().0,
        replacement_payload.as_ref()
    );
}

#[tokio::test]
async fn put_backs_off_while_an_obliteration_holds_the_hash() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    fake.set_state(address.hash, FragmentState::Obliterating);

    let error = store(&fake)
        .await
        .put(
            random::<Context>().into(),
            address,
            fragment,
            Some(payload),
            false,
        )
        .await
        .expect_err("a put racing an obliteration must back off");

    assert!(error.is_slow_down(), "expected a retryable back-off");
    assert_eq!(fake.association_count(address.hash), 0);
}

/// Sets up an object as it was stored before the fragment moved onto it: bare bytes in S3, the
/// fragment in a table row.
fn preexisting_object(fake: &Fake, partition: Partition, address: Address) -> (Fragment, Bytes) {
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    fake.put_object_without_metadata(address.hash, payload.as_ref());
    fake.set_fragment_metadata_row(address.hash, fragment);
    fake.add_association(partition, address);

    (fragment, payload)
}

#[tokio::test]
async fn get_falls_back_to_the_legacy_row_for_an_object_with_no_metadata() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = preexisting_object(&fake, partition, address);

    let (loaded, bytes) = migrated_store(&fake)
        .await
        .get(partition, address)
        .await
        .and_then(lore_storage::StoreGetData::into_payload)
        .expect("an object written before the cut-over must still be readable");

    assert_eq!(bytes, payload);
    assert_eq!(loaded.size_payload, fragment.size_payload);
    assert_eq!(loaded.size_content, fragment.size_content);
    assert_eq!(
        loaded.flags & FragmentFlags::PayloadStoredDurable,
        FragmentFlags::PayloadStoredDurable
    );
}

/// A pre-cut-over object is answered from its state row like any other, with no fallback read:
/// query never needs the representation, so it never needs the fragment metadata table either.
#[tokio::test]
async fn query_matches_a_preexisting_object_without_reading_s3() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    preexisting_object(&fake, partition, address);

    let result = query_one(
        &(migrated_store(&fake).await as Arc<dyn ImmutableStoreTrait>),
        partition,
        address,
    )
    .await
    .expect("resolve should succeed");

    assert_eq!(result.match_made, StoreMatch::MatchFull);
    assert_eq!(fake.object_reads(), 0, "resolve must not touch S3");
}

/// A deployment that never wrote an object without metadata should not go looking for a row
/// describing one. An object in that shape is damaged, not old.
#[tokio::test]
async fn get_refuses_an_object_with_no_metadata_when_no_legacy_table_is_configured() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    preexisting_object(&fake, partition, address);

    store(&fake)
        .await
        .get(partition, address)
        .await
        .expect_err("without a legacy table configured there is nothing to fall back to");
}

/// Metadata that is present but unreadable means a damaged object. Describing it from a
/// separate record is exactly the mismatch this design removes, so it must not fall back even
/// where a legacy row exists.
#[tokio::test]
async fn get_does_not_fall_back_for_an_object_with_damaged_metadata() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    fake.put_object_with_damaged_metadata(address.hash, payload.as_ref());
    fake.set_fragment_metadata_row(address.hash, fragment);
    fake.add_association(partition, address);

    migrated_store(&fake)
        .await
        .get(partition, address)
        .await
        .expect_err("a damaged object must not be described by a legacy row");
}

mod separate_metadata_table {
    use super::*;

    /// When the metadata table IS configured but holds no row for a hash that has an
    /// association but no state row, `query` must still return a miss. The metadata-table
    /// check in `do_query` must not turn a genuine miss into a phantom match.
    #[tokio::test]
    async fn query_misses_when_no_state_or_legacy_row() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let partition: Partition = random();
        fake.add_association(partition, address);
        // No state row, no legacy metadata row.

        let result = query_one(
            &(store_with_separate_metadata_table(&fake).await as Arc<dyn ImmutableStoreTrait>),
            partition,
            address,
        )
        .await
        .expect("query should succeed even when no row is found");

        assert_eq!(result.match_made, StoreMatch::MatchNone);
    }

    /// A legacy fragment whose flags carry obliteration bits must not be returned as a match.
    /// The state table has no row (pre-state-table era), but the metadata row records that the
    /// fragment was obliterated — `do_query` must treat it the same as a state-row obliteration.
    #[tokio::test]
    async fn query_misses_for_an_obliterated_legacy_fragment() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let partition: Partition = random();
        let obliterated = Fragment {
            flags: FragmentFlags::PayloadObliterated.bits(),
            size_payload: 64,
            size_content: 256,
        };

        fake.add_association(partition, address);
        fake.set_legacy_metadata_row(address.hash, obliterated);
        // No state row — the obliteration bit lives only in the legacy metadata flags.

        let result = query_one(
            &(store_with_separate_metadata_table(&fake).await as Arc<dyn ImmutableStoreTrait>),
            partition,
            address,
        )
        .await
        .expect("query should succeed");

        assert_eq!(result.match_made, StoreMatch::MatchNone);
    }

    /// What a push actually calls: one batch holding a legacy fragment, a current one and an
    /// address the store does not have. Each must resolve on its own terms, and the legacy rows
    /// must be read together rather than one round trip at a time.
    #[tokio::test]
    async fn query_resolves_a_batch_mixing_legacy_and_current_fragments() {
        let fake = Fake::default();
        let partition: Partition = random();
        let (fragment, _) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        let legacy = Address {
            hash: random(),
            context: random(),
        };
        fake.add_association(partition, legacy);
        fake.set_legacy_metadata_row(legacy.hash, fragment);

        let current = Address {
            hash: random(),
            context: random(),
        };
        fake.add_association(partition, current);
        fake.set_state(current.hash, FragmentState::Stored);

        let obliterated = Address {
            hash: random(),
            context: random(),
        };
        fake.add_association(partition, obliterated);
        fake.set_state(obliterated.hash, FragmentState::Obliterated);

        let absent = Address {
            hash: random(),
            context: random(),
        };

        let addresses = [legacy, current, obliterated, absent];
        let mut results = [StoreMatchResult::default(); 4];
        store_with_separate_metadata_table(&fake)
            .await
            .query(partition, &addresses, &mut results)
            .await
            .expect("query should succeed");

        assert_eq!(
            results.map(|result| result.match_made),
            [
                StoreMatch::MatchFull,
                StoreMatch::MatchFull,
                StoreMatch::MatchNone,
                StoreMatch::MatchNone
            ],
            "a legacy fragment and a current one both resolve, an obliterated one does not, \
             and an address the store never had does not"
        );
    }

    /// A fragment stored before the state table existed: an association exists, no state row,
    /// but the metadata table holds the legacy fragment description. `query` must report a
    /// match — the legacy fallback exists precisely for this scenario.
    #[tokio::test]
    async fn query_matches_a_legacy_fragment_with_no_state_row() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let partition: Partition = random();
        let (fragment, _) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        fake.add_association(partition, address);
        fake.set_legacy_metadata_row(address.hash, fragment);
        // No state row — `load_state` returns `Ok(None)`.

        let result = query_one(
            &(store_with_separate_metadata_table(&fake).await as Arc<dyn ImmutableStoreTrait>),
            partition,
            address,
        )
        .await
        .expect("a legacy fragment with no state row must be queryable");

        assert_eq!(result.match_made, StoreMatch::MatchFull);
    }

    /// When `do_query` returns `QueryResultSource::LegacyMetadata`, `get_metadata` must report
    /// the fragment from the metadata table, not whatever the object says.
    ///
    /// It may well have read the object: the `HeadObject` is issued alongside the resolution,
    /// so on this path it is spent and discarded. That is deliberate — the legacy row is the
    /// fallback, and holding the common path back to spare it would be the wrong trade. What
    /// matters is which fragment wins, which is what this asserts.
    ///
    /// The returned fragment must carry `PayloadStoredDurable` (set by `do_query` when it
    /// takes the `LegacyMetadata` branch) and must preserve the original flags from the
    /// metadata row.
    #[tokio::test]
    async fn get_metadata_prefers_legacy_metadata_over_the_object() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let partition: Partition = random();
        let (mut fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);
        fragment.flags |= FragmentFlags::PayloadDoNotReplicate;

        fake.add_association(partition, address);
        fake.set_legacy_metadata_row(address.hash, fragment);
        // Unusable metadata on the object, so the concurrent head cannot describe it: if its
        // result were the one reported, this call would fail instead of returning the row.
        fake.put_object_with_damaged_metadata(address.hash, payload.as_ref());
        // No state row — `do_query` returns `QueryResultSource::LegacyMetadata`.

        let result = store_with_separate_metadata_table(&fake)
            .await
            .get_metadata(partition, address)
            .await
            .expect("get_metadata must succeed for a legacy fragment");

        assert_eq!(result.match_made, StoreMatch::MatchFull);
        assert_eq!(result.fragment.size_payload, fragment.size_payload);
        assert_eq!(result.fragment.size_content, fragment.size_content);
        assert_eq!(
            result.fragment.flags & FragmentFlags::PayloadStoredDurable,
            FragmentFlags::PayloadStoredDurable,
            "do_query must mark legacy metadata fragments as durably stored"
        );
        assert_eq!(
            result.fragment.flags & FragmentFlags::PayloadDoNotReplicate,
            FragmentFlags::PayloadDoNotReplicate,
            "original flags from the metadata row must be preserved"
        );
    }

    /// When `head_fragment` falls back to `fragment_from_metadata_table` and finds no row
    /// (`Ok(None)`), `get_metadata` must return an error. This covers the caller-side
    /// `ok_or_else` added in `head_fragment`.
    #[tokio::test]
    async fn get_metadata_fails_when_object_has_no_s3_metadata_and_no_legacy_row() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let partition: Partition = random();
        let (_, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        fake.add_association(partition, address);
        fake.set_state(address.hash, FragmentState::Stored);
        fake.put_object_without_metadata(address.hash, payload.as_ref());
        // No metadata row — `fragment_from_metadata_table` returns `Ok(None)`.

        store_with_separate_metadata_table(&fake)
            .await
            .get_metadata(partition, address)
            .await
            .expect_err("an object with no S3 metadata and no legacy row must not be returned");
    }

    /// The same `ok_or_else` guard on the `get_s3_object_contents` path: when `get` reads an
    /// object with no S3 metadata and the metadata table holds no row either, it must fail.
    #[tokio::test]
    async fn get_fails_when_object_has_no_s3_metadata_and_no_legacy_row() {
        let fake = Fake::default();
        let address = Address {
            hash: random(),
            context: random(),
        };
        let partition: Partition = random();
        let (_, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

        fake.add_association(partition, address);
        fake.set_state(address.hash, FragmentState::Stored);
        fake.put_object_without_metadata(address.hash, payload.as_ref());
        // No legacy row — `fragment_from_metadata_table` returns `Ok(None)`, which
        // `get_s3_object_contents` maps to an error.

        store_with_separate_metadata_table(&fake)
            .await
            .get(partition, address)
            .await
            .expect_err("an object with no S3 metadata and no legacy row must not be returned");
    }
}

/// A lost payload must not stay lost. Clearing the state row is what lets the next put stop
/// short-circuiting on "already durable" and upload the content again.
#[tokio::test]
async fn a_read_of_a_lost_payload_clears_its_state_so_a_put_can_restore_it() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, address, fragment, Some(payload.clone()), false)
        .await
        .expect("put should succeed");

    fake.lose_object(address.hash);

    let error = store
        .clone()
        .get(partition, address)
        .await
        .expect_err("a lost payload reads as not found");
    assert!(error.is_address_not_found());

    assert_eq!(
        fake.state_of(address.hash),
        None,
        "the state row must be cleared so the hash stops looking durable"
    );

    store
        .clone()
        .put(partition, address, fragment, Some(payload.clone()), false)
        .await
        .expect("re-put should succeed");

    assert_eq!(fake.object(address.hash).unwrap().0, payload.as_ref());
    assert_eq!(fake.state_of(address.hash), Some(FragmentState::Stored));

    let (_, restored) = store
        .get(partition, address)
        .await
        .and_then(lore_storage::StoreGetData::into_payload)
        .expect("the payload should be readable again");
    assert_eq!(restored, payload);
}

/// A lost payload must be reported however it is found. `get_metadata` is the cheaper call, so
/// a client that only ever reads metadata would otherwise never raise the alarm.
#[tokio::test]
async fn get_metadata_of_a_lost_payload_reports_and_repairs_it() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("put should succeed");

    fake.lose_object(address.hash);

    let result = store
        .get_metadata(partition, address)
        .await
        .expect("a lost payload reports a miss");

    assert_eq!(result.match_made, StoreMatch::MatchNone);
    assert_eq!(
        fake.state_of(address.hash),
        None,
        "get_metadata must repair the loss it found, exactly as get does"
    );
}

/// An obliteration in flight owns the hash. Its mark must survive a read that races the payload
/// deletion, or a put could republish underneath it.
#[tokio::test]
async fn a_read_of_a_lost_payload_leaves_an_obliteration_mark_alone() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("put should succeed");

    fake.lose_object(address.hash);
    fake.set_state(address.hash, FragmentState::Obliterating);

    store
        .get(partition, address)
        .await
        .expect_err("a lost payload reads as not found");

    assert_eq!(
        fake.state_of(address.hash),
        Some(FragmentState::Obliterating),
        "a read must not clear a mark an obliteration is holding"
    );
}

#[tokio::test]
async fn put_revives_a_tombstoned_hash() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    fake.set_state(address.hash, FragmentState::Obliterated);

    store(&fake)
        .await
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("re-upload over a tombstone is allowed");

    assert_eq!(fake.state_of(address.hash), Some(FragmentState::Stored));
    assert_eq!(fake.stored_fragment(address.hash), Some(fragment));
    assert_eq!(fake.association_count(address.hash), 1);
}

/// Store a fragmented parent whose payload is a list of references to `leaves`, all under one
/// partition and context so obliteration walks from the parent into each leaf.
async fn store_fragmented(
    store: &Arc<AwsImmutableStore>,
    partition: Partition,
    context: Context,
    leaves: &[Hash],
) -> Address {
    const LEAF_CONTENT: u64 = 256;

    for (index, hash) in leaves.iter().enumerate() {
        let (fragment, payload) = representation(
            FragmentFlags::PayloadCompressedZstd,
            64 + index,
            LEAF_CONTENT,
        );
        store
            .clone()
            .put(
                partition,
                Address {
                    hash: *hash,
                    context,
                },
                fragment,
                Some(payload),
                false,
            )
            .await
            .expect("leaf put should succeed");
    }

    let references: Vec<FragmentReference> = leaves
        .iter()
        .enumerate()
        .map(|(index, hash)| FragmentReference {
            hash: *hash,
            offset_content: index as u64 * LEAF_CONTENT,
        })
        .collect();

    let payload = Bytes::from(references.as_bytes().to_vec());
    let parent = Address {
        hash: random(),
        context,
    };
    let fragment = Fragment {
        flags: FragmentFlags::PayloadFragmented.bits(),
        size_payload: u32::try_from(payload.len()).unwrap(),
        size_content: LEAF_CONTENT * leaves.len() as u64,
    };

    store
        .clone()
        .put(partition, parent, fragment, Some(payload), false)
        .await
        .expect("parent put should succeed");

    parent
}

#[tokio::test]
async fn obliterate_recurses_into_sub_fragments() {
    let fake = Fake::default();
    let partition: Partition = random();
    let context: Context = random();
    let leaves = [random::<Hash>(), random::<Hash>()];

    let store = store(&fake).await;
    let parent = store_fragmented(&store, partition, context, &leaves).await;

    let stats = Arc::new(StoreObliterateStats::default());
    store
        .obliterate(partition, parent, stats.clone())
        .await
        .expect("obliterate should succeed");

    assert!(fake.object(parent.hash).is_none(), "parent payload remains");
    assert_eq!(fake.association_count(parent.hash), 0);

    for leaf in leaves {
        assert!(
            fake.object(leaf).is_none(),
            "a sub-fragment payload was not obliterated"
        );
        assert_eq!(fake.association_count(leaf), 0);
        assert_eq!(fake.state_of(leaf), Some(FragmentState::Obliterated));
    }

    assert_eq!(
        stats.num_fragments.load(Ordering::Relaxed),
        3,
        "the parent and both sub-fragments should each be counted"
    );
    assert_eq!(stats.num_payloads.load(Ordering::Relaxed), 3);
}

/// Obliterating a list passes over its holes: a hole names no fragment, so its state is never
/// read, and the list's leaves are obliterated as those of a list without holes are.
#[tokio::test]
async fn obliterate_passes_over_the_holes_of_a_list() {
    let fake = Fake::default();
    let partition: Partition = random();
    let context: Context = random();
    let store = store(&fake).await;
    let mut state_reads = Vec::new();
    for with_hole in [false, true] {
        let leaves: [Hash; 2] = [random(), random()];
        for hash in leaves {
            let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);
            store
                .clone()
                .put(
                    partition,
                    Address { hash, context },
                    fragment,
                    Some(payload),
                    false,
                )
                .await
                .expect("put a leaf");
        }
        let mut references = vec![FragmentReference {
            hash: leaves[0],
            offset_content: 0,
        }];
        if with_hole {
            references.push(FragmentReference {
                hash: Hash::default(),
                offset_content: 256,
            });
        }
        references.push(FragmentReference {
            hash: leaves[1],
            offset_content: 4096,
        });
        let list = Bytes::copy_from_slice(references.as_bytes());
        let fragment = Fragment {
            flags: FragmentFlags::PayloadFragmented.bits(),
            size_payload: u32::try_from(list.len()).unwrap(),
            size_content: 4096 + 256,
        };
        let list_address = Address {
            hash: random(),
            context,
        };
        store
            .clone()
            .put(partition, list_address, fragment, Some(list), false)
            .await
            .expect("put the list");

        let before = fake.state_reads();
        store
            .clone()
            .obliterate(partition, list_address, Arc::default())
            .await
            .expect("obliterate the list");
        state_reads.push(fake.state_reads() - before);
        for hash in leaves {
            assert_eq!(
                fake.state_of(hash),
                Some(FragmentState::Obliterated),
                "with a hole: {with_hole}"
            );
        }
    }
    assert_eq!(
        state_reads[0], state_reads[1],
        "the state of a hole is read"
    );
}

/// Recursion must respect each sub-fragment's own reference count. A leaf another partition
/// still holds survives, and its mark is released so the hash stays writable.
#[tokio::test]
async fn obliterate_keeps_a_sub_fragment_another_partition_references() {
    let fake = Fake::default();
    let partition: Partition = random();
    let context: Context = random();
    let shared = random::<Hash>();
    let leaves = [shared, random::<Hash>()];

    let store = store(&fake).await;
    let parent = store_fragmented(&store, partition, context, &leaves).await;

    let other = Address {
        hash: shared,
        context: random(),
    };
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);
    store
        .clone()
        .put(
            random::<Context>().into(),
            other,
            fragment,
            Some(payload),
            false,
        )
        .await
        .expect("second partition put should succeed");

    store
        .obliterate(partition, parent, Arc::new(StoreObliterateStats::default()))
        .await
        .expect("obliterate should succeed");

    assert!(
        fake.object(shared).is_some(),
        "a sub-fragment referenced elsewhere must survive"
    );
    assert_eq!(fake.association_count(shared), 1);
    assert_eq!(
        fake.state_of(shared),
        Some(FragmentState::Stored),
        "the surviving sub-fragment must not be left marked"
    );

    assert!(fake.object(leaves[1]).is_none(), "unshared leaf remains");
    assert!(fake.object(parent.hash).is_none(), "parent remains");
}

/// The parent's payload must still be readable when recursion runs, since that is where the
/// reference list comes from — it is deleted only after the sub-fragments are handled.
#[tokio::test]
async fn obliterate_reads_the_reference_list_before_deleting_the_parent() {
    let fake = Fake::default();
    let partition: Partition = random();
    let context: Context = random();
    let leaves = [random::<Hash>()];

    let store = store(&fake).await;
    let parent = store_fragmented(&store, partition, context, &leaves).await;

    store
        .obliterate(partition, parent, Arc::new(StoreObliterateStats::default()))
        .await
        .expect("obliterate should succeed");

    assert!(
        fake.object(leaves[0]).is_none(),
        "the reference list was not read, so the sub-fragment survived"
    );
}

#[tokio::test]
async fn obliterate_removes_the_reference_and_the_payload() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("put should succeed");

    let stats = Arc::new(StoreObliterateStats::default());
    store
        .obliterate(partition, address, stats.clone())
        .await
        .expect("obliterate should succeed");

    assert_eq!(fake.association_count(address.hash), 0);
    assert!(fake.object(address.hash).is_none());
    assert_eq!(
        fake.state_of(address.hash),
        Some(FragmentState::Obliterated)
    );
    assert_eq!(stats.num_fragments.load(Ordering::Relaxed), 1);
    assert_eq!(stats.num_payloads.load(Ordering::Relaxed), 1);
}

/// A take of the obliteration mark still shed when the store ladder runs out fails as internal, not
/// as a back-off: a retry after a take that applied unseen would find the mark and return early.
#[tokio::test(start_paused = true)]
async fn obliterate_fails_internally_when_taking_the_mark_stays_shed() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("put should succeed");

    fake.fail(Fault::StateWrite);

    let error = store
        .obliterate(
            partition,
            address,
            Arc::new(StoreObliterateStats::default()),
        )
        .await
        .expect_err("a shedding state table must fail the obliteration");

    assert!(
        error.is_internal(),
        "expected an internal failure, got {error:?}"
    );
    assert!(!fake.has_association(partition, address));
    assert_eq!(fake.state_of(address.hash), Some(FragmentState::Stored));
}

/// Each step from taking the mark onwards is retried in place while it is shed, so sheds that clear
/// within the store ladder still complete the obliteration.
#[tokio::test(start_paused = true)]
async fn obliterate_retries_shed_steps_in_place() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let (fake, store, partition, address) = stored_payload().await;
            for fault in [
                Fault::StateWrite,
                Fault::AssociationCount,
                Fault::ObjectList,
                Fault::StateFinalize,
            ] {
                fake.fail_once(fault);
            }

            store
                .obliterate(
                    partition,
                    address,
                    Arc::new(StoreObliterateStats::default()),
                )
                .await
                .expect("sheds that clear must not fail the obliteration");

            assert!(!fake.has_association(partition, address));
            assert!(fake.object(address.hash).is_none());
            assert_eq!(
                fake.state_of(address.hash),
                Some(FragmentState::Obliterated)
            );
        })
        .await;
}

/// A partition obliterating a hash another obliteration holds still deletes its own association,
/// which is the obligation; the reclamation stays with the holder.
#[tokio::test]
async fn obliterate_of_a_marked_hash_deletes_only_this_partitions_association() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let (fake, store, partition, address) = stored_payload().await;
            fake.set_state(address.hash, FragmentState::Obliterating);

            store
                .obliterate(
                    partition,
                    address,
                    Arc::new(StoreObliterateStats::default()),
                )
                .await
                .expect("a held mark does not fail the obliteration");

            assert!(!fake.has_association(partition, address));
            assert!(fake.object(address.hash).is_some());
            assert_eq!(
                fake.state_of(address.hash),
                Some(FragmentState::Obliterating)
            );
        })
        .await;
}

/// A row already in the target state is the holder's own earlier attempt, applied although its
/// answer was lost, so clearing the mark again succeeds.
#[tokio::test]
async fn clearing_a_mark_already_in_its_target_state_succeeds() {
    let fake = Fake::default();
    let hash: Hash = random();

    fake.set_state(hash, FragmentState::Obliterated);

    store(&fake)
        .await
        .clear_mark(hash, FragmentState::Obliterated)
        .await
        .expect("an applied earlier attempt is not a conflict");
}

#[tokio::test]
async fn clearing_a_mark_from_another_state_is_a_conflict() {
    let fake = Fake::default();
    let hash: Hash = random();

    fake.set_state(hash, FragmentState::Stored);

    let error = store(&fake)
        .await
        .clear_mark(hash, FragmentState::Obliterated)
        .await
        .expect_err("the row left the mark by another route");

    assert!(
        error.is_internal(),
        "expected an internal failure, got {error:?}"
    );
}

#[tokio::test]
async fn obliterate_keeps_the_payload_while_another_partition_references_it() {
    let fake = Fake::default();
    let hash: Hash = random();
    let mine = Address {
        hash,
        context: random(),
    };
    let theirs = Address {
        hash,
        context: random(),
    };
    let partition: Partition = random();
    let other_partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, mine, fragment, Some(payload.clone()), false)
        .await
        .expect("put should succeed");
    store
        .clone()
        .put(other_partition, theirs, fragment, Some(payload), false)
        .await
        .expect("second put should succeed");

    store
        .obliterate(partition, mine, Arc::new(StoreObliterateStats::default()))
        .await
        .expect("obliterate should succeed");

    assert_eq!(fake.association_count(hash), 1);
    assert!(
        fake.object(hash).is_some(),
        "compliance only requires the obliterated partition's reference to be gone"
    );
    assert_eq!(
        fake.state_of(hash),
        Some(FragmentState::Stored),
        "the mark must be released so the hash stays writable"
    );
}

/// A timeout means the answer is unknown, so the caller is told to retry.
#[tokio::test]
async fn a_state_read_timeout_asks_the_caller_to_retry() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    fake.fail(Fault::StateReadTimeout);

    let error = store(&fake)
        .await
        .put(
            random::<Context>().into(),
            address,
            fragment,
            Some(payload),
            false,
        )
        .await
        .expect_err("a timeout must not be reported as a definite answer");

    assert!(error.is_slow_down(), "expected a retryable back-off");
}

/// A failure the service explained is not congestion.
///
/// `AccessDenied` reported as `SlowDown` would have the storage layer above retry a
/// misconfiguration through its whole ladder and then report it as a throttle.
#[tokio::test]
async fn a_refused_object_read_is_not_reported_as_congestion() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("the put should succeed");

    fake.fail(Fault::ObjectReadDenied);

    let error = store
        .get(partition, address)
        .await
        .expect_err("a refused read is not a payload");

    assert!(
        !error.is_slow_down(),
        "a permissions failure must not read as congestion"
    );
    assert!(
        !error.is_address_not_found(),
        "the object is there; we were refused"
    );
}

/// A body that stops mid-payload is a reset connection, not a damaged object, so the caller has
/// to be told to come back rather than handed an internal error it cannot act on.
#[tokio::test]
async fn a_body_that_stops_mid_payload_asks_the_caller_to_retry() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("the put should succeed");

    fake.fail(Fault::ObjectReadTruncated);

    let error = store
        .get(partition, address)
        .await
        .expect_err("a body that stops partway is not a payload");

    assert!(
        error.is_slow_down(),
        "a dropped connection must be retryable, not an internal failure"
    );
    assert!(
        !error.is_address_not_found(),
        "the object is there; only the read failed"
    );
}

#[tokio::test]
async fn a_throttled_state_read_asks_the_caller_to_retry() {
    let fake = Fake::default();
    fake.fail(Fault::StateRead);

    let error = store(&fake)
        .await
        .load_state(random())
        .await
        .expect_err("throttling is not an answer");

    assert!(error.is_slow_down(), "throttling must be retryable");
}

#[tokio::test]
async fn a_batch_read_left_unprocessed_asks_the_caller_to_retry() {
    let fake = Fake::default();
    fake.fail(Fault::BatchReadUnprocessed);
    let addresses = [Address {
        hash: random(),
        context: random(),
    }];
    let mut results = [StoreMatchResult::default()];

    let error = store(&fake)
        .await
        .query(random(), &addresses, &mut results)
        .await
        .expect_err("keys left unprocessed are not an answer");

    assert!(error.is_slow_down(), "unprocessed keys must be retryable");
}

/// A failed read must never read as a miss. A caller told "not found" for a hash it references
/// treats the content as lost, which counts data loss and clears the state row — off a
/// `DynamoDB` error that says nothing about whether the content is there.
#[tokio::test]
async fn a_broken_state_read_is_an_error_not_a_miss() {
    let fake = Fake::default();
    fake.fail(Fault::StateReadBroken);

    let error = store(&fake)
        .await
        .load_state(random())
        .await
        .expect_err("a broken read is not an empty table");

    assert!(!error.is_address_not_found(), "must not read as a miss");
    assert!(!error.is_slow_down(), "and is not retryable either");
}

/// The same rule on the fallback path, where the consequence is sharpest: this read only
/// happens for a hash a partition references, so a miss here is what triggers the repair.
#[tokio::test]
async fn a_failed_fragment_metadata_read_does_not_clear_the_state_row() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    preexisting_object(&fake, partition, address);

    let store = migrated_store(&fake).await;
    fake.fail(Fault::StateRead);

    let error = store
        .get(partition, address)
        .await
        .expect_err("the metadata read is throttled");

    assert!(error.is_slow_down());
    assert!(
        fake.state_of(address.hash).is_some(),
        "a throttle must not be mistaken for a lost payload and clear the row"
    );
}

/// Two writers reviving the same tombstone both want the hash stored. The one that loses the
/// compare-and-set has already uploaded its bytes and got the state it wanted, so it must not
/// fail.
#[tokio::test]
async fn reviving_a_hash_another_writer_already_revived_succeeds() {
    let fake = Fake::default();
    let hash: Hash = random();

    fake.set_state(hash, FragmentState::Stored);

    store(&fake)
        .await
        .revive_state(hash)
        .await
        .expect("losing the revival race is not a failure");
}

#[tokio::test]
async fn reviving_a_hash_an_obliteration_retook_backs_off() {
    let fake = Fake::default();
    let hash: Hash = random();

    fake.set_state(hash, FragmentState::Obliterating);

    let error = store(&fake)
        .await
        .revive_state(hash)
        .await
        .expect_err("an obliteration holds the hash again");

    assert!(error.is_slow_down(), "the mark is transient, so back off");
}

/// The upload and the state row survive an association failure, so a retry finishes the job
/// without re-uploading. Nothing is left that a later put would mistake for a complete write.
#[tokio::test]
async fn a_put_that_cannot_associate_leaves_the_payload_ready_for_a_retry() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    fake.fail(Fault::AssociationWrite);

    store(&fake)
        .await
        .put(partition, address, fragment, Some(payload.clone()), false)
        .await
        .expect_err("the association write fails");

    assert_eq!(fake.object(address.hash).unwrap().0, payload.as_ref());
    assert_eq!(fake.state_of(address.hash), Some(FragmentState::Stored));
    assert_eq!(fake.association_count(address.hash), 0);

    let recovered = Fake::default();
    recovered.set_state(address.hash, FragmentState::Stored);
    recovered.put_object_without_metadata(address.hash, payload.as_ref());
    store(&recovered)
        .await
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("a retry associates without re-uploading");
    assert_eq!(recovered.association_count(address.hash), 1);
}

/// Compliance is discharged by the association delete. If the count that follows fails, the
/// obliteration reports failure and holds its mark — but the reference is already gone.
#[tokio::test(start_paused = true)]
async fn an_obliterate_that_cannot_count_references_still_removed_the_reference() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("put should succeed");

    fake.fail(Fault::AssociationCount);

    store
        .obliterate(
            partition,
            address,
            Arc::new(StoreObliterateStats::default()),
        )
        .await
        .expect_err("counting references fails");

    assert_eq!(
        fake.association_count(address.hash),
        0,
        "the compliance obligation is discharged before anything that can fail after it"
    );
    assert_eq!(
        fake.state_of(address.hash),
        Some(FragmentState::Obliterating),
        "the mark is held, which is the known crashed-obliteration gap"
    );
}

#[tokio::test(start_paused = true)]
async fn an_obliterate_that_cannot_delete_the_payload_does_not_tombstone_it() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("put should succeed");

    fake.fail(Fault::ObjectDelete);

    store
        .obliterate(
            partition,
            address,
            Arc::new(StoreObliterateStats::default()),
        )
        .await
        .expect_err("deleting the payload fails");

    assert!(
        fake.object(address.hash).is_some(),
        "the payload survives a failed delete"
    );
    assert_eq!(
        fake.state_of(address.hash),
        Some(FragmentState::Obliterating),
        "it must not be tombstoned while its payload is still there"
    );
}

#[tokio::test(start_paused = true)]
async fn an_obliterate_that_cannot_list_versions_does_not_tombstone_the_payload() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("put should succeed");

    fake.fail(Fault::ObjectList);

    store
        .obliterate(
            partition,
            address,
            Arc::new(StoreObliterateStats::default()),
        )
        .await
        .expect_err("listing versions fails");

    assert_ne!(
        fake.state_of(address.hash),
        Some(FragmentState::Obliterated)
    );
}

/// Recursion must not report success when a sub-fragment could not be obliterated, or the
/// parent would be tombstoned over content that is still referenced.
#[tokio::test(start_paused = true)]
async fn obliterate_fails_when_a_sub_fragment_fails() {
    let fake = Fake::default();
    let partition: Partition = random();
    let context: Context = random();
    let leaves = [random::<Hash>(), random::<Hash>()];

    let store = store(&fake).await;
    let parent = store_fragmented(&store, partition, context, &leaves).await;

    fake.fail(Fault::ObjectDelete);

    store
        .obliterate(partition, parent, Arc::new(StoreObliterateStats::default()))
        .await
        .expect_err("a sub-fragment failure must fail the parent");

    assert_ne!(fake.state_of(parent.hash), Some(FragmentState::Obliterated));
}

/// The repair is best effort: failing to clear the row must not turn a not-found into an error,
/// because the caller's answer does not depend on the repair succeeding.
#[tokio::test]
async fn a_failed_repair_still_reports_the_lost_payload_as_not_found() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("put should succeed");

    fake.lose_object(address.hash);
    fake.fail(Fault::StateDelete);

    let error = store
        .get(partition, address)
        .await
        .expect_err("the payload is gone");

    assert!(error.is_address_not_found());
    assert_eq!(
        fake.state_of(address.hash),
        Some(FragmentState::Stored),
        "the row survives a failed repair, unchanged"
    );
}

/// The probe saw nothing, the upload landed, and an obliteration took the hash in between. The
/// put must not associate, or it would restore a reference the obliteration is removing.
#[tokio::test]
async fn a_put_that_loses_the_hash_mid_upload_backs_off_without_associating() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    fake.obliterate_during_upload(address.hash, FragmentState::Obliterating);

    let error = store(&fake)
        .await
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect_err("an obliteration holds the hash by the time the put publishes");

    assert!(
        error.is_slow_down(),
        "the mark is transient, so this is a back-off"
    );
    assert_eq!(
        fake.association_count(address.hash),
        0,
        "no reference may be created while an obliteration holds the hash"
    );
    assert_eq!(
        fake.state_of(address.hash),
        Some(FragmentState::Obliterating),
        "the put must not disturb the mark"
    );
}

/// Racing a *completed* obliteration is different: the tombstone is not a lock, and re-upload
/// over one is allowed, so the put finishes and revives the hash.
#[tokio::test]
async fn a_put_that_lands_on_a_fresh_tombstone_revives_it() {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    fake.obliterate_during_upload(address.hash, FragmentState::Obliterated);

    store(&fake)
        .await
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("re-upload over a tombstone is allowed");

    assert_eq!(fake.state_of(address.hash), Some(FragmentState::Stored));
    assert_eq!(fake.association_count(address.hash), 1);
}

/// The drain exists so a put that had already passed its state probe can land its association
/// and be counted. Without the wait the count runs immediately, sees nothing, and the payload
/// is deleted underneath a partition that legitimately stored it.
#[tokio::test]
async fn the_drain_lets_an_in_flight_association_be_counted() {
    let fake = Fake::default();
    let hash: Hash = random();
    let mine = Address {
        hash,
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, mine, fragment, Some(payload), false)
        .await
        .expect("put should succeed");

    let deleted = fake.association_deleted();
    let injector = fake.clone();
    let racing_partition: Partition = random();
    let racing = Address {
        hash,
        context: random(),
    };
    let mut tasks = JoinSet::new();
    lore_base::lore_spawn!(tasks, async move {
        deleted
            .await
            .expect("the obliteration must delete its association");
        injector.add_association(racing_partition, racing);
    });

    store
        .obliterate(partition, mine, Arc::new(StoreObliterateStats::default()))
        .await
        .expect("obliterate should succeed");

    while let Some(result) = tasks.join_next().await {
        result.expect("the racing writer should not panic");
    }

    assert!(
        fake.object(hash).is_some(),
        "an association that landed during the drain must keep the payload alive"
    );
    assert_eq!(fake.association_count(hash), 1);
    assert_eq!(
        fake.state_of(hash),
        Some(FragmentState::Stored),
        "the mark must be released so the surviving reference stays usable"
    );
}

#[tokio::test]
async fn copy_associates_the_destination_without_touching_the_payload() {
    let fake = Fake::default();
    let source = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, source, fragment, Some(payload.clone()), false)
        .await
        .expect("put should succeed");

    let destination_context: Context = random();
    store
        .copy(
            partition,
            source,
            partition,
            destination_context,
            CopyBehavior {
                durable: true,
                do_not_replicate: false,
            },
        )
        .await
        .expect("copy should succeed");

    assert_eq!(fake.association_count(source.hash), 2);
    assert_eq!(
        fake.object(source.hash).unwrap().0,
        payload.as_ref(),
        "copy must not rewrite the payload"
    );
    assert_eq!(fake.object_reads(), 0, "copy must not read S3");
}

#[tokio::test]
async fn copy_of_an_unknown_address_is_not_found() {
    let fake = Fake::default();
    let source = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();

    store(&fake)
        .await
        .copy(
            partition,
            source,
            partition,
            random::<Context>(),
            CopyBehavior {
                durable: true,
                do_not_replicate: false,
            },
        )
        .await
        .expect_err("nothing to copy");

    assert_eq!(fake.association_count(source.hash), 0);
}

/// A hash present in another context is not a full match, so it must not be copyable from this
/// one — the copy would otherwise fabricate a reference from a partial match.
#[tokio::test]
async fn copy_of_a_partial_match_is_not_found() {
    let fake = Fake::default();
    let hash: Hash = random();
    let stored = Address {
        hash,
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, stored, fragment, Some(payload), false)
        .await
        .expect("put should succeed");

    let other_context = Address {
        hash,
        context: random(),
    };
    store
        .copy(
            partition,
            other_context,
            partition,
            random::<Context>(),
            CopyBehavior {
                durable: true,
                do_not_replicate: false,
            },
        )
        .await
        .expect_err("a different context is not a full match");

    assert_eq!(fake.association_count(hash), 1);
}

/// A source naming no context is any association the partition holds, which is what a caller
/// acting on a partition match has. The payload is untouched, as for an exact source.
#[tokio::test]
async fn copy_from_an_unnamed_context_takes_any_association_in_the_partition() {
    let fake = Fake::default();
    let hash: Hash = random();
    let stored = Address {
        hash,
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, stored, fragment, Some(payload.clone()), false)
        .await
        .expect("put should succeed");

    store
        .copy(
            partition,
            Address::zero_context_hash(hash),
            partition,
            random::<Context>(),
            CopyBehavior {
                durable: true,
                do_not_replicate: false,
            },
        )
        .await
        .expect("a partition holding the hash answers a source naming no context");

    assert_eq!(fake.association_count(hash), 2);
    assert_eq!(
        fake.object(hash).unwrap().0,
        payload.as_ref(),
        "copy must not rewrite the payload"
    );
    assert_eq!(fake.object_reads(), 0, "copy must not read S3");
}

/// Naming no context widens the search inside one partition, never across them — this store
/// isolates, so a hash held only elsewhere is not the caller's to copy from.
#[tokio::test]
async fn copy_from_an_unnamed_context_does_not_reach_another_partition() {
    let fake = Fake::default();
    let hash: Hash = random();
    let stored = Address {
        hash,
        context: random(),
    };
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(
            random::<Partition>(),
            stored,
            fragment,
            Some(payload),
            false,
        )
        .await
        .expect("put should succeed");

    store
        .copy(
            random::<Partition>(),
            Address::zero_context_hash(hash),
            random::<Partition>(),
            random::<Context>(),
            CopyBehavior {
                durable: true,
                do_not_replicate: false,
            },
        )
        .await
        .expect_err("a partition holding nothing has no association to name");

    assert_eq!(fake.association_count(hash), 1);
}

#[tokio::test]
async fn exist_batch_reports_a_match_per_address_in_order() {
    let fake = Fake::default();
    let partition: Partition = random();
    let absent = Address {
        hash: random(),
        context: random(),
    };
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    let mut stored = Vec::new();
    for _ in 0..2 {
        let address = Address {
            hash: random(),
            context: random(),
        };
        store
            .clone()
            .put(partition, address, fragment, Some(payload.clone()), false)
            .await
            .expect("put should succeed");
        stored.push(address);
    }

    let addresses = [stored[0], absent, stored[1]];
    let mut results = [StoreMatchResult::default(); 3];
    store
        .query(partition, &addresses, &mut results)
        .await
        .expect("resolve should succeed");

    assert_eq!(
        results.map(|result| result.match_made),
        [
            StoreMatch::MatchFull,
            StoreMatch::MatchNone,
            StoreMatch::MatchFull
        ],
        "results must line up with the addresses given, misses included"
    );
}

/// The corruption this design exists to make impossible.
///
/// Writers race on one hash with different representations, each a valid encoding of the same
/// content. Under a model that stores the fragment separately from the bytes, an interleaving
/// can leave one writer's fragment describing another writer's payload. Here the fragment
/// travels on the object, so whichever upload lands last is the one that is read back — whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_cannot_tear_the_fragment_from_its_payload() {
    const CONTENT_SIZE: u64 = 4096;

    for _ in 0..64 {
        let fake = Fake::default();
        let hash: Hash = random();
        let store = store(&fake).await;

        let representations = [
            representation(FragmentFlags::PayloadCompressedZstd, 64, CONTENT_SIZE),
            representation(FragmentFlags::PayloadCompressedLZ4, 512, CONTENT_SIZE),
            representation(FragmentFlags::PayloadCompressedOodle2, 1024, CONTENT_SIZE),
            representation(FragmentFlags::PayloadFragmented, 2048, CONTENT_SIZE),
        ];

        let mut writers = JoinSet::new();
        for (fragment, payload) in representations {
            let store = store.clone();
            let address = Address {
                hash,
                context: random(),
            };
            let partition: Partition = random();

            lore_base::lore_spawn!(writers, async move {
                store
                    .put(partition, address, fragment, Some(payload), false)
                    .await
            });
        }

        while let Some(result) = writers.join_next().await {
            result
                .expect("writer task should not panic")
                .expect("every writer should succeed");
        }

        let (body, metadata) = fake.object(hash).expect("a payload must be stored");
        let stored =
            from_object_metadata(&metadata).expect("the stored object must carry a fragment");

        assert_eq!(
            stored.size_payload as usize,
            body.len(),
            "the fragment on the object must describe the bytes on that same object"
        );
        assert_eq!(stored.size_content, CONTENT_SIZE);
        assert_eq!(
            fake.state_of(hash),
            Some(FragmentState::Stored),
            "the state row must not be left mid-flight"
        );
        assert_eq!(
            fake.association_count(hash),
            4,
            "every writer's partition must end up referencing the payload"
        );

        let expected_byte = (stored.flags & PAYLOAD_FLAGS) as u8;
        assert!(
            body.iter().all(|byte| *byte == expected_byte),
            "the bytes must be the ones the winning writer uploaded, not a mix"
        );
    }
}

/// Store one payload in a fresh partition and return the fake, the store, the partition and the
/// address it was stored under.
async fn stored_payload() -> (Fake, Arc<AwsImmutableStore>, Partition, Address) {
    let fake = Fake::default();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let partition: Partition = random();
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);

    let store = store(&fake).await;
    store
        .clone()
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("put should succeed");

    (fake, store, partition, address)
}

/// Obliteration deletes every stored version of the payload's object, not only the latest.
#[tokio::test]
async fn obliterate_deletes_every_stored_version() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let (fake, store, partition, address) = stored_payload().await;
            let key = address.hash.to_string();
            fake.add_version(&key, "older", false);
            fake.add_version(&key, "oldest", false);

            store
                .obliterate(
                    partition,
                    address,
                    Arc::new(StoreObliterateStats::default()),
                )
                .await
                .expect("obliterate should succeed");

            assert!(
                fake.versions(&key).is_empty(),
                "every stored version is deleted"
            );
            assert!(fake.object(address.hash).is_none());
            assert_eq!(
                fake.state_of(address.hash),
                Some(FragmentState::Obliterated)
            );
        })
        .await;
}

/// Delete markers stay in the listing; the stored versions beside them are deleted.
#[tokio::test]
async fn obliterate_leaves_delete_markers_in_place() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let (fake, store, partition, address) = stored_payload().await;
            let key = address.hash.to_string();
            fake.add_version(&key, "marker", true);

            store
                .obliterate(
                    partition,
                    address,
                    Arc::new(StoreObliterateStats::default()),
                )
                .await
                .expect("obliterate should succeed");

            let remaining = fake.versions(&key);
            assert_eq!(remaining.len(), 1, "only the delete marker is left");
            assert!(remaining[0].is_delete_marker);
            assert!(fake.object(address.hash).is_none());
            assert_eq!(
                fake.state_of(address.hash),
                Some(FragmentState::Obliterated)
            );
        })
        .await;
}

/// A listing is by prefix, so a key that merely begins with the payload's key keeps its versions.
#[tokio::test]
async fn obliterate_deletes_only_the_payload_key() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let (fake, store, partition, address) = stored_payload().await;
            let key = address.hash.to_string();
            let sibling = format!("{key}-sibling");
            fake.add_version(&sibling, "kept", false);

            store
                .obliterate(
                    partition,
                    address,
                    Arc::new(StoreObliterateStats::default()),
                )
                .await
                .expect("obliterate should succeed");

            assert!(fake.versions(&key).is_empty());
            assert_eq!(
                fake.versions(&sibling).len(),
                1,
                "another key's versions are not this payload's to delete"
            );
        })
        .await;
}

/// A listing that stopped short cannot name every version, so once the store ladder runs out the
/// obliteration fails as internal with its mark held and the payload still in place.
#[tokio::test(start_paused = true)]
async fn obliterate_fails_internally_when_the_version_listing_stays_truncated() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let (fake, store, partition, address) = stored_payload().await;
            fake.fail(Fault::ObjectListTruncated);

            let error = store
                .obliterate(
                    partition,
                    address,
                    Arc::new(StoreObliterateStats::default()),
                )
                .await
                .expect_err("a truncated listing does not complete an obliteration");

            assert!(
                error.is_internal(),
                "expected an internal failure, got {error:?}"
            );
            assert!(fake.object(address.hash).is_some());
            assert_eq!(
                fake.state_of(address.hash),
                Some(FragmentState::Obliterating)
            );
        })
        .await;
}

/// The first step of an obliteration is the association delete; a shed one backs off and leaves
/// the hash exactly as it was.
#[tokio::test]
async fn obliterate_backs_off_when_the_association_delete_is_shed() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let (fake, store, partition, address) = stored_payload().await;
            fake.fail(Fault::AssociationDelete);

            let error = store
                .obliterate(
                    partition,
                    address,
                    Arc::new(StoreObliterateStats::default()),
                )
                .await
                .expect_err("a shed association delete fails the obliteration");

            assert!(error.is_slow_down(), "expected a back-off, got {error:?}");
            assert!(fake.has_association(partition, address));
            assert_eq!(fake.state_of(address.hash), Some(FragmentState::Stored));
            assert!(fake.object(address.hash).is_some());
        })
        .await;
}

/// Releasing the mark for a hash another partition still references is a conditional write, and one
/// still shed when the store ladder runs out fails as internal with the mark held.
#[tokio::test(start_paused = true)]
async fn obliterate_fails_internally_when_releasing_the_mark_stays_shed() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let (fake, store, partition, address) = stored_payload().await;
            let theirs = Address {
                hash: address.hash,
                context: random(),
            };
            let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);
            store
                .clone()
                .put(random(), theirs, fragment, Some(payload), false)
                .await
                .expect("second put should succeed");

            fake.fail(Fault::StateRelease);

            let error = store
                .obliterate(
                    partition,
                    address,
                    Arc::new(StoreObliterateStats::default()),
                )
                .await
                .expect_err("a shed release fails the obliteration");

            assert!(
                error.is_internal(),
                "expected an internal failure, got {error:?}"
            );
            assert!(!fake.has_association(partition, address));
            assert_eq!(fake.association_count(address.hash), 1);
            assert!(fake.object(address.hash).is_some());
            assert_eq!(
                fake.state_of(address.hash),
                Some(FragmentState::Obliterating)
            );
        })
        .await;
}

/// The tombstone is the last write of an obliteration; one still shed when the store ladder runs
/// out fails as internal after the payload is already deleted, with the mark held.
#[tokio::test(start_paused = true)]
async fn obliterate_fails_internally_when_finalizing_stays_shed() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let (fake, store, partition, address) = stored_payload().await;
            fake.fail(Fault::StateFinalize);

            let error = store
                .obliterate(
                    partition,
                    address,
                    Arc::new(StoreObliterateStats::default()),
                )
                .await
                .expect_err("a shed tombstone write fails the obliteration");

            assert!(
                error.is_internal(),
                "expected an internal failure, got {error:?}"
            );
            assert!(fake.object(address.hash).is_none());
            assert_eq!(
                fake.state_of(address.hash),
                Some(FragmentState::Obliterating)
            );
        })
        .await;
}

/// A refused `HeadObject` is a misconfiguration: neither congestion nor a missing payload, so it
/// does not repair the state row either.
#[tokio::test]
async fn a_refused_object_head_is_an_internal_failure() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let (fake, store, partition, address) = stored_payload().await;
            fake.fail(Fault::ObjectHeadDenied);

            let error = store
                .get_metadata(partition, address)
                .await
                .expect_err("a refused head is not a representation");

            assert!(
                error.is_internal(),
                "expected an internal failure, got {error:?}"
            );
            assert_eq!(fake.state_of(address.hash), Some(FragmentState::Stored));
        })
        .await;
}

/// A shed upload backs off, and nothing is published or associated for content that was not
/// stored.
#[tokio::test]
async fn a_shed_upload_backs_off_without_publishing() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let fake = Fake::default();
            let address = Address {
                hash: random(),
                context: random(),
            };
            let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);
            fake.fail(Fault::ObjectWriteShed);

            let error = store(&fake)
                .await
                .put(random(), address, fragment, Some(payload), false)
                .await
                .expect_err("a shed upload is not a stored payload");

            assert!(error.is_slow_down(), "expected a back-off, got {error:?}");
            assert_eq!(fake.state_of(address.hash), None);
            assert_eq!(fake.association_count(address.hash), 0);
        })
        .await;
}

/// A refused upload is an internal failure rather than congestion, and publishes nothing.
#[tokio::test]
async fn a_refused_upload_is_an_internal_failure() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let fake = Fake::default();
            let address = Address {
                hash: random(),
                context: random(),
            };
            let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);
            fake.fail(Fault::ObjectWriteDenied);

            let error = store(&fake)
                .await
                .put(random(), address, fragment, Some(payload), false)
                .await
                .expect_err("a refused upload is not a stored payload");

            assert!(
                error.is_internal(),
                "expected an internal failure, got {error:?}"
            );
            assert_eq!(fake.state_of(address.hash), None);
            assert_eq!(fake.association_count(address.hash), 0);
        })
        .await;
}

/// A shed association read is a back-off, never a miss: a miss on a referenced hash is what
/// triggers the lost-payload repair.
#[tokio::test]
async fn a_shed_association_read_asks_the_caller_to_retry() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let (fake, store, partition, address) = stored_payload().await;
            fake.fail(Fault::AssociationRead);

            let error = store
                .get(partition, address)
                .await
                .expect_err("a shed association read is not an answer");

            assert!(error.is_slow_down(), "expected a back-off, got {error:?}");
            assert!(!error.is_address_not_found());
            assert_eq!(fake.state_of(address.hash), Some(FragmentState::Stored));
        })
        .await;
}

/// The store contract, checked against this store the same way it is checked against every
/// other one.
///
/// The battery is `lore_storage::conformance`, the same one the local, replicated and remote
/// stores answer to, so a clause means here what it means there. The capabilities declare no
/// known violations, so every check has to pass.
///
/// It needs no service behind it because the substitution point is the `DynamoDb` and `S3` facades
/// rather than the wire: `test-util` replaces them with mocks, and these mocks are backed by a
/// [`Fake`] holding a bucket and the two tables in memory. Their expectations are stateful and
/// unbounded rather than a programmed call sequence, so the store issues whatever calls it would
/// really issue and the assertions land on what ends up stored. The fake models the semantics the
/// contract leans on — a conditional write that fails comes back carrying the row it found, and the
/// association query reports no more than the single row its limit allows.
///
/// What that cannot establish is whether the services agree with the fake, so the same battery
/// runs against `MinIO` and `DynamoDB` Local in `lore-integration-tests` behind the
/// `integration_tests` feature. Running it here keeps a contract regression one
/// `cargo test -p lore-aws` away instead of one container stack away.
///
/// Note what this does *not* reach. The store is built without the legacy metadata table, so
/// nothing here exercises the fallback resolution that table turns on, nor the gap that comes
/// with it: a hash with no state row is left associated by `obliterate` and goes on matching
/// through the fallback. Those paths are covered separately, on
/// [`store_with_separate_metadata_table`], where the two tables are genuinely distinct.
#[tokio::test]
async fn satisfies_the_immutable_store_contract() {
    let fake = Fake::default();
    let store = store(&fake).await;

    let execution = crate::store::setup_execution("test".to_string());
    lore_base::runtime::LORE_CONTEXT
        .scope(execution, async move {
            lore_storage::conformance::verify_immutable_store(
                store,
                lore_storage::conformance::Capabilities::new("AwsImmutableStore"),
            )
            .await;
        })
        .await;
}

mod malicious_s3_object_tests {
    use super::*;

    const MAX_OBJECT_SIZE: usize = FRAGMENT_SIZE_THRESHOLD + size_of::<Fragment>();

    #[tokio::test]
    async fn load_rejects_object_one_byte_over_max_size() {
        let fake = Fake::default();
        let hash: Hash = random();
        fake.put_object_without_metadata(hash, &vec![0u8; MAX_OBJECT_SIZE + 1]);
        assert!(matches!(
            store(&fake).await.load(hash).await,
            Err(StoreError::Oversized(_))
        ));
    }

    #[tokio::test]
    async fn load_rejects_object_far_over_max_size() {
        let fake = Fake::default();
        let hash: Hash = random();
        fake.put_object_without_metadata(hash, &vec![0u8; MAX_OBJECT_SIZE * 2]);
        assert!(matches!(
            store(&fake).await.load(hash).await,
            Err(StoreError::Oversized(_))
        ));
    }

    #[tokio::test]
    async fn load_accepts_object_exactly_at_max_size() {
        // An object at exactly MAX_OBJECT_SIZE must not be rejected by the size guard.
        // It has no fragment metadata header and no legacy row, so load will fail for a
        // different reason — but the error must not be Oversized.
        let fake = Fake::default();
        let hash: Hash = random();
        fake.put_object_without_metadata(hash, &vec![0u8; MAX_OBJECT_SIZE]);
        assert!(!matches!(
            store(&fake).await.load(hash).await,
            Err(StoreError::Oversized(_))
        ));
    }

    #[tokio::test]
    async fn load_rejects_oversized_s3_object_with_valid_fragment_header() {
        // Content-Length guard fires even when the object carries a valid lore-fragment
        // header — the raw S3 object size check runs before the body is read.
        let fake = Fake::default();
        let body = vec![0u8; MAX_OBJECT_SIZE + 1];
        let hash: Hash = random();
        let fragment = Fragment {
            flags: 0,
            size_payload: body.len() as u32,
            size_content: body.len() as u64,
        };
        fake.put_object_with_fragment_metadata(hash, &body, fragment);
        assert!(matches!(
            store(&fake).await.load(hash).await,
            Err(StoreError::Oversized(_))
        ));
    }

    /// The declared length is not the only bound.
    ///
    /// A chunked reply carries no `Content-Length`, so the header check never sees it. The bytes
    /// themselves are bounded too, so an endpoint that is not the one we think it is cannot have a
    /// body of any size read whole into this process.
    #[tokio::test]
    async fn load_rejects_an_undeclared_body_over_max_size() {
        let fake = Fake::default();
        let hash: Hash = random();
        fake.put_object_without_metadata(hash, &vec![0u8; MAX_OBJECT_SIZE * 4]);
        fake.fail(Fault::ObjectReadChunked);

        let error = store(&fake)
            .await
            .load(hash)
            .await
            .expect_err("a body past the maximum is not an answer");

        assert!(matches!(error, StoreError::Oversized(_)), "{error}");
    }

    #[tokio::test]
    async fn load_rejects_fragment_metadata_claiming_oversized_payload() {
        // A small S3 object (passes the content_length guard) but whose lore-fragment
        // header declares size_payload > FRAGMENT_SIZE_THRESHOLD is caught by
        // validate_fragment_size.
        let fake = Fake::default();
        let body = vec![0u8; 64];
        let hash: Hash = random();
        let malicious_fragment = Fragment {
            flags: 0,
            size_payload: (FRAGMENT_SIZE_THRESHOLD + 1) as u32,
            size_content: (FRAGMENT_SIZE_THRESHOLD + 1) as u64,
        };
        fake.put_object_with_fragment_metadata(hash, &body, malicious_fragment);
        assert!(matches!(
            store(&fake).await.load(hash).await,
            Err(StoreError::Oversized(_))
        ));
    }
}

/// A store containing one fragment inserted through its put operation.
async fn stored(fake: &Fake) -> (Arc<AwsImmutableStore>, Partition, Address, Fragment, Bytes) {
    let store = store(fake).await;
    let partition: Partition = random();
    let address = Address {
        hash: random(),
        context: random(),
    };
    let (fragment, payload) = representation(FragmentFlags::PayloadCompressedZstd, 64, 256);
    store
        .clone()
        .put(partition, address, fragment, Some(payload.clone()), false)
        .await
        .expect("put should succeed");
    (store, partition, address, fragment, payload)
}

mod shared_put {
    use lore_aws::store::single_flight::Role;
    use tokio::sync::oneshot;

    use super::*;
    use crate::store::poll_once;

    #[tokio::test]
    async fn takes_the_state_read_already_in_flight() {
        let fake = Fake::default();
        let (store, partition, address, fragment, payload) = stored(&fake).await;
        let state_reads = fake.state_reads();

        let (release, released) = oneshot::channel();
        let mut in_flight = Box::pin(
            store
                .state_reads()
                .run(address.hash, || async { released.await.expect("released") }),
        );
        assert!(poll_once(in_flight.as_mut()).await.is_pending());

        let mut put = store
            .clone()
            .put(partition, address, fragment, Some(payload), false);
        assert!(poll_once(put.as_mut()).await.is_pending());

        release
            .send(Ok(Some(FragmentState::Stored)))
            .expect("the read in flight is waiting");
        assert_eq!(in_flight.await.1, Role::Leader);
        put.await.expect("put should succeed");
        assert_eq!(fake.state_reads(), state_reads);
    }
}

mod shared_get {
    use std::task::Poll;

    use tokio::sync::oneshot;

    use super::*;
    use crate::store::poll_once;

    #[tokio::test]
    async fn after_a_copy_reads_the_association_the_copy_wrote() {
        let fake = Fake::default();
        let (store, partition, address, _, payload) = stored(&fake).await;
        let destination_partition: Partition = random();
        let destination = Address {
            hash: address.hash,
            context: random(),
        };

        let (_release, released) = oneshot::channel::<Result<bool, StoreError>>();
        let mut before_copy = Box::pin(
            store
                .association_reads()
                .run((destination_partition, destination), || async {
                    released.await.expect("released")
                }),
        );
        assert!(poll_once(before_copy.as_mut()).await.is_pending());

        store
            .clone()
            .copy(
                partition,
                address,
                destination_partition,
                destination.context,
                CopyBehavior {
                    durable: true,
                    do_not_replicate: false,
                },
            )
            .await
            .expect("copy should succeed");

        let mut get = store.clone().get(destination_partition, destination);
        let Poll::Ready(got) = poll_once(get.as_mut()).await else {
            panic!("the get joined a read that began before the copy");
        };
        assert_eq!(got.expect("get should succeed").payload, Some(payload));
    }
}

mod shared_exists {
    use std::task::Poll;

    use tokio::sync::oneshot;

    use super::*;
    use crate::store::poll_once;

    #[tokio::test]
    async fn after_a_delete_reads_the_association_gone() {
        let fake = Fake::default();
        let (store, partition, address, _, _) = stored(&fake).await;

        let (_release, released) = oneshot::channel::<Result<bool, StoreError>>();
        let mut before_delete = Box::pin(
            store
                .association_reads()
                .run((partition, address), || async {
                    released.await.expect("released")
                }),
        );
        assert!(poll_once(before_delete.as_mut()).await.is_pending());

        store
            .delete_association(partition, address)
            .await
            .expect("delete should succeed");

        let mut exists = Box::pin(store.exists(partition, address, SharedReadContext::Put));
        let Poll::Ready(exists) = poll_once(exists.as_mut()).await else {
            panic!("the read joined one that began before the delete");
        };
        assert!(!exists.expect("exists should succeed"));
    }
}

mod shared_state {
    use std::pin::Pin;
    use std::task::Poll;

    use lore_aws::store::single_flight::Role;
    use tokio::sync::oneshot;

    use super::*;
    use crate::store::poll_once;

    type HeldRead<'a> =
        Pin<Box<dyn Future<Output = (Result<Option<FragmentState>, StoreError>, Role)> + 'a>>;

    /// Keeps a state read for `hash` in flight so it predates the test's next operation.
    async fn hold_state_read(
        store: &AwsImmutableStore,
        hash: Hash,
    ) -> (
        oneshot::Sender<Result<Option<FragmentState>, StoreError>>,
        HeldRead<'_>,
    ) {
        let (release, released) = oneshot::channel();
        let mut in_flight: HeldRead<'_> = Box::pin(
            store
                .state_reads()
                .run(hash, || async { released.await.expect("released") }),
        );
        assert!(poll_once(in_flight.as_mut()).await.is_pending());
        (release, in_flight)
    }

    #[tokio::test]
    async fn after_an_obliteration_mark_reads_the_mark() {
        let fake = Fake::default();
        let store = store(&fake).await;
        let hash: Hash = random();
        fake.set_state(hash, FragmentState::Stored);
        let _before_mark = hold_state_read(&store, hash).await;

        store
            .take_mark(hash)
            .await
            .expect("the mark should be taken");

        let mut state = Box::pin(store.load_state_shared(hash, SharedReadContext::Put));
        let Poll::Ready(state) = poll_once(state.as_mut()).await else {
            panic!("the read joined one that began before the mark");
        };
        assert_eq!(
            state.expect("state read should succeed"),
            Some(FragmentState::Obliterating)
        );
    }

    #[tokio::test]
    async fn after_a_clear_reads_no_state() {
        let fake = Fake::default();
        let store = store(&fake).await;
        let hash: Hash = random();
        fake.set_state(hash, FragmentState::Stored);
        let _before_clear = hold_state_read(&store, hash).await;

        store.clear_state(hash).await.expect("clear should succeed");

        let mut state = Box::pin(store.load_state_shared(hash, SharedReadContext::Put));
        let Poll::Ready(state) = poll_once(state.as_mut()).await else {
            panic!("the read joined one that began before the clear");
        };
        assert_eq!(state.expect("state read should succeed"), None);
    }

    #[tokio::test]
    async fn after_a_publish_reads_the_published_state() {
        let fake = Fake::default();
        let store = store(&fake).await;
        let hash: Hash = random();
        let _before_publish = hold_state_read(&store, hash).await;

        assert_eq!(
            store
                .publish_state(hash)
                .await
                .expect("publish should succeed"),
            FragmentState::Stored
        );

        let mut state = Box::pin(store.load_state_shared(hash, SharedReadContext::Put));
        let Poll::Ready(state) = poll_once(state.as_mut()).await else {
            panic!("the read joined one that began before the publish");
        };
        assert_eq!(
            state.expect("state read should succeed"),
            Some(FragmentState::Stored)
        );
    }
}
