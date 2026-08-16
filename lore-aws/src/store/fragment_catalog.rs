// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Catalog semantics for object-backed immutable stores.
//!
//! A fragment catalog owns metadata, repository/context associations, and the
//! compare-and-swap transitions used by obliteration. Payload bytes are owned
//! by the object store and are deliberately outside this interface.

use async_trait::async_trait;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_storage::StoreError;
use lore_storage::StoreMatch;
use lore_storage::StoreQueryResult;

/// A successful claim on a fragment's obliteration state.
///
/// The catalog creates leases; callers can inspect them but cannot construct
/// arbitrary transitions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObliterationLease {
    original: Fragment,
    marker: Fragment,
}

impl ObliterationLease {
    /// Metadata as it was before obliteration began.
    pub fn original(&self) -> Fragment {
        self.original
    }

    /// Metadata containing the in-progress obliteration marker.
    pub fn marker(&self) -> Fragment {
        self.marker
    }

    /// Construct a lease from catalog-persisted metadata.
    ///
    /// This is intended for [`FragmentCatalog`] implementations. Store callers
    /// should use leases returned by [`FragmentCatalog::begin_obliteration`].
    pub fn new(original: Fragment, marker: Fragment) -> Self {
        Self { original, marker }
    }
}

/// Result of starting or resuming an obliteration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeginObliteration {
    /// A previous request already completed the obliteration.
    AlreadyObliterated,
    /// The caller owns, or is resuming, the in-progress transition.
    Acquired(ObliterationLease),
}

/// Result of releasing one repository/context association during obliteration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseAssociation {
    /// Other associations remain; the catalog restored the active metadata.
    ReferencesRemain,
    /// No associations remain; the marker is retained while the payload is deleted.
    PayloadUnreferenced,
}

/// Metadata and association index used by an object-backed immutable store.
///
/// Implementations must serialize `associate_fragment`, `begin_obliteration`,
/// `release_association`, and `finalize_obliteration` for a hash. No method may
/// make an obliterating or obliterated fragment active again.
#[async_trait]
pub trait FragmentCatalog: Send + Sync {
    /// Return the association match without requiring callers to load metadata.
    async fn exist(
        &self,
        repository: Context,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<StoreMatch, StoreError> {
        self.query(repository, address, match_requested)
            .await
            .map(|result| result.match_made)
    }

    /// Return the best match available at or below `match_requested`.
    async fn query(
        &self,
        repository: Context,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<StoreQueryResult, StoreError>;

    /// Return association matches in the same order as `addresses`.
    async fn query_batch(
        &self,
        repository: Context,
        addresses: &[Address],
        match_requested: StoreMatch,
    ) -> Result<Vec<StoreMatch>, StoreError>;

    /// Load metadata by payload hash, including obliteration markers.
    async fn load_metadata(&self, hash: Hash) -> Result<Fragment, StoreError>;

    /// Register metadata and its first association idempotently.
    ///
    /// Existing active metadata must exactly match `fragment`. Implementations
    /// must reject hash collisions and any attempt to resurrect an obliterating
    /// or obliterated fragment.
    async fn register_fragment(
        &self,
        repository: Context,
        address: Address,
        fragment: Fragment,
    ) -> Result<(), StoreError>;

    /// Add an association to existing active metadata idempotently.
    async fn associate_fragment(
        &self,
        repository: Context,
        address: Address,
    ) -> Result<(), StoreError>;

    /// Atomically start or resume obliteration for `hash`.
    async fn begin_obliteration(&self, hash: Hash) -> Result<BeginObliteration, StoreError>;

    /// Remove one association and decide whether the payload is still referenced.
    ///
    /// If references remain, this operation also restores the original active
    /// metadata before returning.
    async fn release_association(
        &self,
        repository: Context,
        address: Address,
        lease: ObliterationLease,
    ) -> Result<ReleaseAssociation, StoreError>;

    /// Atomically replace an in-progress marker with terminal obliterated metadata.
    async fn finalize_obliteration(
        &self,
        hash: Hash,
        lease: ObliterationLease,
    ) -> Result<(), StoreError>;

    /// Maximum supported batch lookup size, if bounded by the backend.
    fn max_query_batch(&self) -> Option<usize> {
        None
    }
}

/// Backend-independent behavioral contract for catalog implementations.
#[cfg(any(test, feature = "catalog-contract-tests"))]
pub mod contract {
    use lore_base::types::FragmentFlags;

    use super::*;

    /// Exercise the externally observable fragment catalog state machine.
    pub async fn run<C>(catalog: &C)
    where
        C: FragmentCatalog + ?Sized,
    {
        let repository_a = Context::from([0x11; 16]);
        let repository_b = Context::from([0x22; 16]);
        let address_a = Address {
            hash: Hash::from([0x33; 32]),
            context: Context::from([0x44; 16]),
        };
        let address_b = Address {
            hash: address_a.hash,
            context: Context::from([0x55; 16]),
        };
        let fragment = Fragment {
            flags: FragmentFlags::PayloadCompressedZstd.bits(),
            size_payload: 128,
            size_content: 1024,
        };

        catalog
            .register_fragment(repository_a, address_a, fragment)
            .await
            .expect("initial registration failed");
        catalog
            .register_fragment(repository_a, address_a, fragment)
            .await
            .expect("registration was not idempotent");

        let exact = catalog
            .query(repository_a, address_a, StoreMatch::MatchFull)
            .await
            .expect("exact query failed");
        assert_eq!(exact.match_made, StoreMatch::MatchFull);
        assert_eq!(exact.fragment, fragment);

        let partition = catalog
            .query(repository_a, address_b, StoreMatch::MatchPartition)
            .await
            .expect("partition query failed");
        assert_eq!(partition.match_made, StoreMatch::MatchPartition);

        let hash = catalog
            .query(repository_b, address_b, StoreMatch::MatchHash)
            .await
            .expect("hash query failed");
        assert_eq!(hash.match_made, StoreMatch::MatchHash);

        let batch = catalog
            .query_batch(repository_a, &[address_a, address_b], StoreMatch::MatchFull)
            .await
            .expect("batch query failed");
        assert_eq!(batch, vec![StoreMatch::MatchFull, StoreMatch::MatchNone]);

        let collision = Fragment {
            size_content: fragment.size_content + 1,
            ..fragment
        };
        assert!(
            catalog
                .register_fragment(repository_b, address_b, collision)
                .await
                .is_err(),
            "hash collision was accepted"
        );

        catalog
            .associate_fragment(repository_b, address_b)
            .await
            .expect("second association failed");
        catalog
            .associate_fragment(repository_b, address_b)
            .await
            .expect("association was not idempotent");

        let first_lease = match catalog
            .begin_obliteration(address_a.hash)
            .await
            .expect("failed to begin obliteration")
        {
            BeginObliteration::AlreadyObliterated => {
                panic!("new fragment was already obliterated")
            }
            BeginObliteration::Acquired(lease) => lease,
        };
        assert_eq!(first_lease.original(), fragment);
        assert_ne!(
            first_lease.marker().flags & FragmentFlags::PayloadObliterating.bits(),
            0
        );
        assert!(
            catalog
                .associate_fragment(repository_b, address_a)
                .await
                .is_err(),
            "association was added while obliteration was in progress"
        );

        let resumed = catalog
            .begin_obliteration(address_a.hash)
            .await
            .expect("failed to resume obliteration");
        assert_eq!(resumed, BeginObliteration::Acquired(first_lease));

        assert_eq!(
            catalog
                .release_association(repository_a, address_a, first_lease)
                .await
                .expect("failed to release first association"),
            ReleaseAssociation::ReferencesRemain
        );
        assert_eq!(
            catalog
                .load_metadata(address_a.hash)
                .await
                .expect("failed to reload restored metadata"),
            fragment
        );

        let last_lease = match catalog
            .begin_obliteration(address_a.hash)
            .await
            .expect("failed to begin final obliteration")
        {
            BeginObliteration::AlreadyObliterated => {
                panic!("fragment became terminal before its last association was released")
            }
            BeginObliteration::Acquired(lease) => lease,
        };
        assert_eq!(
            catalog
                .release_association(repository_b, address_b, last_lease)
                .await
                .expect("failed to release last association"),
            ReleaseAssociation::PayloadUnreferenced
        );
        assert_eq!(
            catalog
                .load_metadata(address_a.hash)
                .await
                .expect("failed to load in-progress marker"),
            last_lease.marker()
        );
        assert!(
            catalog
                .associate_fragment(repository_a, address_a)
                .await
                .is_err(),
            "terminal-path association resurrected the fragment"
        );

        catalog
            .finalize_obliteration(address_a.hash, last_lease)
            .await
            .expect("failed to finalize obliteration");
        let terminal = catalog
            .load_metadata(address_a.hash)
            .await
            .expect("failed to load terminal metadata");
        assert_eq!(terminal.flags, FragmentFlags::PayloadObliterated.bits());
        assert_eq!(terminal.size_payload, 0);
        assert_eq!(terminal.size_content, 0);
        assert_eq!(
            catalog
                .begin_obliteration(address_a.hash)
                .await
                .expect("terminal retry failed"),
            BeginObliteration::AlreadyObliterated
        );
    }
}
