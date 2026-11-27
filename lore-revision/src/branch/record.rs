// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Records stored the way branches are: a metadata key per id, holding the hash of a metadata blob
//! in the immutable store, and a mapping from each name, regardless of case, to the id that holds
//! it. Branches and scopes are such records.
//!
//! A record's writes are compare-and-swaps, ordered so that one that stops part way leaves a record
//! of what it held. A create swaps the record's metadata in from zero, then claims the name, and
//! swaps the metadata back where the claim fails. A claim of a name for a record that has metadata
//! records itself in the metadata as [`RESTORING`] before it claims, and finishes with a swap that
//! removes the entry. An archive stamps [`ARCHIVED`] and swaps the metadata before it releases the
//! name. A claim that stopped is settled by the next write of its record ([`settle`]), which marks
//! its name [`RELEASED`] and releases it. A settle and an archive move a time on, so a swap that
//! expects the metadata they replaced never lands. A release swaps the name's mapping from the
//! value of the claim it releases ([`NamedRecord::mapping`]): a kind whose claims carry a
//! generation releases only the claim it read, and a kind whose mappings hold the bare id releases
//! any claim by the record.
//!
//! A swap that finds the metadata key holding another value fails with the kind's own error
//! ([`NamedRecord::changed`]). The other refusals are left to the caller: the functions answer the
//! value a key held, or whether a release took the name.

use std::sync::Arc;

use lore_error_set::HasAll;
use lore_error_set::prelude::*;

use super::ARCHIVED;
use super::RELEASED;
use super::RESTORED;
use super::RESTORING;
use super::holds_name;
use super::is_archived;
use super::mutable_key;
use super::mutable_name_key;
use crate::errors::WriteRequired;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore_debug;
use crate::lore_info;
use crate::lore_warn;
use crate::metadata::Metadata;
use crate::metadata::MetadataError;
use crate::repository::RepositoryContext;
use crate::store::KeyType;
use crate::store::StoreError;

/// An error set the reads and writes of a [`NamedRecord`] forward into: one that declares every
/// variant of a store's and a metadata blob's errors, and [`WriteRequired`].
pub trait RecordError:
    ErrorSet
    + From<WriteRequired>
    + HasAll<<StoreError as ErrorSet>::Variants>
    + HasAll<<MetadataError as ErrorSet>::Variants>
{
}

impl<E> RecordError for E where
    E: ErrorSet
        + From<WriteRequired>
        + HasAll<<StoreError as ErrorSet>::Variants>
        + HasAll<<MetadataError as ErrorSet>::Variants>
{
}

/// The claim of a name that a record's metadata records the generation of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claimed {
    /// The claim of the name the metadata names the record.
    Name,
    /// The claim of the name the metadata records as [`RESTORING`].
    Restoring,
    /// The claim of the name the metadata marks [`RELEASED`].
    Released,
}

/// A kind of record stored the way branches are. An implementing type holds no data, so a future
/// generic over it holds nothing a future written for one kind doesn't.
pub trait NamedRecord {
    /// The error set the record's reads and writes fail with.
    type Error: RecordError;
    /// The generation of a claim of a name, which a mapping value carries beside the record's id
    /// so that a release swaps from the exact claim it releases. Zero-sized for a kind whose
    /// mappings hold the bare id.
    type Generation: Copy + Send + Sync + PartialEq + std::fmt::Debug;
    /// Whether a release stops at [`BranchStep::ReleaseChecked`](crate::test_util::BranchStep),
    /// between its read of the metadata and its swap. Only a kind whose mappings carry a
    /// generation stops there: a stale swap from a bare id erases a later claim by the record.
    #[cfg(feature = "test-util")]
    const STOPS_AT_RELEASE: bool;
    /// The function of a record's metadata key.
    const METADATA: &'static str;
    /// The function of a name's mapping key.
    const NAME_MAPPING: &'static str;

    /// The error that says the record `id` has no metadata.
    fn missing(id: Context) -> Self::Error;

    /// The error that says the metadata key of the record `id` holds a value other than the one a
    /// swap of it expected.
    fn changed(id: Context) -> Self::Error;

    /// The value a swap of the metadata key of `id` from `read`, the hash a read of the metadata
    /// found, expects the key to hold, where `read_locally` says whether that read was of the
    /// local store.
    fn expected(
        repository: Arc<RepositoryContext>,
        id: Context,
        read: Hash,
        read_locally: bool,
    ) -> impl Future<Output = Hash> + Send;

    /// The metadata of the record `id` that a release reads first, to tell whether the record
    /// keeps the name ([`release_unless`]).
    fn current_metadata(
        repository: Arc<RepositoryContext>,
        id: Context,
    ) -> impl Future<Output = Result<Metadata, Self::Error>> + Send;

    /// The value of a mapping that the claim of `generation` by `id` writes.
    fn mapping(id: Context, generation: Self::Generation) -> Hash;

    /// The generation of the `claimed` claim `metadata` records.
    fn generation(metadata: &Metadata, claimed: Claimed) -> Self::Generation;

    /// Records `generation` as that of the `claimed` claim in `metadata`, or removes it for `None`.
    fn set_generation(
        metadata: &mut Metadata,
        claimed: Claimed,
        generation: Option<Self::Generation>,
    ) -> Result<(), Self::Error>;
}

/// The value the metadata key of `id` holds, `None` where it is absent or holds zero.
#[inline]
pub fn metadata_key<R: NamedRecord>(
    repository: Arc<RepositoryContext>,
    id: Context,
) -> impl Future<Output = Result<Option<Hash>, R::Error>> {
    super::load_record_key(repository, R::METADATA, id, |value, _| Ok(value))
}

/// The id the mapping of `name` holds, zero for none.
///
/// Returns the read's future with its answer mapped, without a future of its own.
pub fn name_mapping<R: NamedRecord>(
    repository: Arc<RepositoryContext>,
    name: &str,
) -> impl Future<Output = Result<Context, R::Error>> {
    futures::FutureExt::map(mapping_value::<R>(repository, name), |held| {
        held.map(|held| held.to_context())
    })
}

/// The value the mapping of `name` holds, zero for none.
pub async fn mapping_value<R: NamedRecord>(
    repository: Arc<RepositoryContext>,
    name: &str,
) -> Result<Hash, R::Error> {
    let (key, key_type) = mutable_name_key(repository.salt(), R::NAME_MAPPING, name);
    match repository
        .read_mutable_store()
        .load(repository.id, key, key_type)
        .await
    {
        Ok(held) => Ok(held),
        Err(err) if err.is_address_not_found() || err.is_payload_not_found() => Ok(Hash::default()),
        Err(err) => Err(err).forward::<R::Error>("reading the mapping of a name"),
    }
}

/// Swaps the mapping of `name` from `expected` to `value`, answering the value the mapping held.
/// An absent mapping holds zero.
pub fn swap_mapping<R: NamedRecord>(
    repository: Arc<RepositoryContext>,
    name: &str,
    expected: Hash,
    value: Hash,
) -> impl Future<Output = Result<Hash, R::Error>> + use<R> {
    let (key, key_type) = mutable_name_key(repository.salt(), R::NAME_MAPPING, name);
    swap_key(repository, key, key_type, expected, value)
}

/// Swaps the metadata key of `id` from `expected` to `value`, answering the value it held. An
/// absent key holds zero.
pub fn swap_metadata_value<R: NamedRecord>(
    repository: Arc<RepositoryContext>,
    id: Context,
    expected: Hash,
    value: Hash,
) -> impl Future<Output = Result<Hash, R::Error>> + use<R> {
    let (key, key_type) = mutable_key(repository.salt(), R::METADATA, repository.id, id);
    swap_key(repository, key, key_type, expected, value)
}

/// Swaps the metadata key of `id` from `expected` to `value`, answering `expected`, the value it
/// replaced. A key that holds another value is left as it is, and the swap fails with
/// [`NamedRecord::changed`].
pub async fn swap_metadata_key<R: NamedRecord>(
    repository: Arc<RepositoryContext>,
    id: Context,
    expected: Hash,
    value: Hash,
) -> Result<Hash, R::Error> {
    let held = swap_metadata_value::<R>(repository, id, expected, value).await?;
    if held != expected {
        lore_debug!("Record {id} has metadata {held}, not {expected}, so it isn't rewritten");
        return Err(R::changed(id));
    }
    Ok(held)
}

/// Swaps the value of `key` from `expected` to `value`, answering the value the key held. An
/// absent key holds zero.
pub async fn swap_key<E: RecordError>(
    repository: Arc<RepositoryContext>,
    key: Hash,
    key_type: KeyType,
    expected: Hash,
    value: Hash,
) -> Result<Hash, E> {
    let handle = repository
        .try_write_mutable_store()
        .ok_or_else(|| E::from(WriteRequired))?;
    handle
        .compare_and_swap(repository.id, key, expected, value, key_type)
        .await
        .forward_with::<E, _>(|| format!("compare-and-swap of a {key_type:?} key"))
}

/// Serializes `metadata` and swaps it into the metadata key of `id` from `expected`, answering
/// its hash, as [`swap_metadata_key`] swaps it.
pub async fn swap_metadata<R: NamedRecord>(
    repository: Arc<RepositoryContext>,
    id: Context,
    expected: Hash,
    metadata: &Metadata,
) -> Result<Hash, R::Error> {
    let hash = serialize_metadata::<R::Error>(repository.clone(), metadata).await?;
    swap_metadata_key::<R>(repository, id, expected, hash).await?;
    Ok(hash)
}

/// Stores `metadata` in the immutable store, answering its hash.
pub async fn serialize_metadata<E: RecordError>(
    repository: Arc<RepositoryContext>,
    metadata: &Metadata,
) -> Result<Hash, E> {
    metadata
        .serialize(repository)
        .await
        .forward::<E>("serializing record metadata")
}

/// The metadata blob `hash`.
pub async fn load_metadata<E: RecordError>(
    repository: Arc<RepositoryContext>,
    hash: Hash,
) -> Result<Metadata, E> {
    Metadata::deserialize(repository, hash)
        .await
        .forward::<E>("deserializing record metadata")
}

/// Sets the time `key` holds in `metadata` to the time now in milliseconds, or one past the time
/// it holds where that is later, so a swap of the metadata key to the result writes a value other
/// than the one it replaces.
pub fn stamp_time<E: RecordError>(metadata: &mut Metadata, key: &str) -> Result<(), E> {
    let after = metadata
        .get_u64(key)
        .map_or(0, |time| time.saturating_add(1));
    metadata
        .set_u64(key, crate::util::time::timestamp().max(after))
        .forward::<E>("setting a time in record metadata")
}

/// Swaps the metadata of `id` from `stored` back to `previous`, after a claim that failed. The key
/// keeps any value but `stored`, so a value another writer stored since stays unless it is
/// `stored` too. A failed write is logged.
pub async fn revert_metadata<R: NamedRecord>(
    repository: Arc<RepositoryContext>,
    id: Context,
    stored: Hash,
    previous: Hash,
) {
    if let Err(err) = swap_metadata_value::<R>(repository, id, stored, previous).await {
        lore_warn!(
            "Failed to set the metadata of {id}, whose claim of its name failed, back to {previous}: {err}"
        );
    }
}

/// Releases `name` from the claim of `generation` by `id` with a compare-and-swap of the name's
/// mapping from that claim's value ([`NamedRecord::mapping`]) to zero, answering whether the
/// mapping held it. A mapping that holds another value, as another record's claim or a later claim
/// by `id` leaves it, or none, is left as it is, and so is every mapping for a zero `id`.
///
/// [`branch::release_name`](super::release_name) makes the same swap in its own future rather
/// than awaiting this one, which would hold its arguments a second time.
pub async fn release_held<R: NamedRecord>(
    repository: Arc<RepositoryContext>,
    id: Context,
    name: &str,
    generation: R::Generation,
) -> Result<bool, R::Error> {
    if id.is_zero() {
        return Ok(false);
    }
    let claim = R::mapping(id, generation);
    let held = swap_mapping::<R>(repository, name, claim, Hash::default()).await?;
    if held != claim {
        lore_debug!(
            "Name {name} maps to {}, not the claim {generation:?} by {id}, so it stays unchanged",
            held.to_context()
        );
    }
    Ok(held == claim)
}

/// Releases `name` from the claim of `generation` by `id` ([`release_held`]) unless the record's
/// metadata, read again first with [`NamedRecord::current_metadata`], `keeps` it, and answers
/// whether it released it. A mapping that holds another value, or none, is left as it is, and
/// answers false. A read or release that fails otherwise fails.
pub async fn release_unless<R: NamedRecord>(
    repository: Arc<RepositoryContext>,
    id: Context,
    name: &str,
    keeps: fn(&Metadata, &str) -> bool,
    generation: R::Generation,
) -> Result<bool, R::Error> {
    if keeps(&R::current_metadata(repository.clone(), id).await?, name) {
        return Ok(false);
    }
    #[cfg(feature = "test-util")]
    if R::STOPS_AT_RELEASE {
        crate::test_util::branch_step(crate::test_util::BranchStep::ReleaseChecked).await;
    }
    release_held::<R>(repository, id, name, generation).await
}

/// Releases `name` from the claim of `generation` by `id`, that of a write whose final swap found
/// the metadata changed, unless the metadata holds it ([`release_unless`], [`holds_name`]),
/// logging a failure. A settle releases the name itself where the write had claimed it when the
/// settle read the metadata; this frees one claimed after.
pub async fn release_unrecorded_claim<R: NamedRecord>(
    repository: Arc<RepositoryContext>,
    id: Context,
    name: &str,
    generation: R::Generation,
) {
    if let Err(err) = release_unless::<R>(repository, id, name, holds_name, generation).await {
        lore_warn!("Failed to release the name {name} of {id}, whose claim didn't finish: {err}");
    }
}

/// Settles the claim of `pending` that `metadata`, the metadata of `id` a read found as `read`,
/// records ([`RESTORING`]): it swaps the metadata from the value [`NamedRecord::expected`] answers
/// to one that names `pending` as [`RELEASED`] in place of the record, with the claim's generation,
/// and with the time that keeps the record as it was moved on, [`ARCHIVED`] for an archived record
/// and [`RESTORED`] for a live one, which fails the claim's final swap, then releases `pending`
/// from the record while the metadata still marks it ([`unmarked`](super::unmarked)). A metadata
/// key that holds another value is left as it is, and answers `None`; a settled claim answers the
/// hash its swap wrote, and whether its release took `pending` from the record. A write that fails
/// otherwise fails.
///
/// The swap comes first, so the claim can't finish once its name is released, and it moves the
/// time on, so no later record of a claim makes the metadata the settled claim swaps from again.
/// The [`RELEASED`] entry outlives the settle, so a settle that stops before its release, or a
/// claim the write makes after it and then stops, leaves the name for the next write of the record
/// to release. A write that claims after the release and goes on finds its final swap refused, and
/// releases its claim itself.
pub async fn settle<R: NamedRecord>(
    repository: Arc<RepositoryContext>,
    id: Context,
    read: Hash,
    read_locally: bool,
    mut metadata: Metadata,
    pending: &str,
) -> Result<Option<(Hash, bool)>, R::Error> {
    let generation = R::generation(&metadata, Claimed::Restoring);
    metadata.remove_key(RESTORING);
    R::set_generation(&mut metadata, Claimed::Restoring, None)?;
    metadata
        .set_string(RELEASED, pending)
        .forward::<R::Error>("recording the settled claim in record metadata")?;
    R::set_generation(&mut metadata, Claimed::Released, Some(generation))?;
    let stamp = if is_archived(&metadata) {
        ARCHIVED
    } else {
        RESTORED
    };
    stamp_time::<R::Error>(&mut metadata, stamp)?;
    let expected = R::expected(repository.clone(), id, read, read_locally).await;
    let settled = serialize_metadata::<R::Error>(repository.clone(), &metadata).await?;
    drop(metadata);
    if swap_metadata_value::<R>(repository.clone(), id, expected, settled).await? != expected {
        lore_debug!("Record {id} changed since its pending claim of {pending} was read");
        return Ok(None);
    }
    lore_info!("Settled a claim of '{pending}' by {id} that didn't finish");
    #[cfg(feature = "test-util")]
    crate::test_util::branch_step(crate::test_util::BranchStep::PendingRestoreSettled).await;
    let released =
        release_unless::<R>(repository, id, pending, super::unmarked, generation).await?;
    Ok(Some((settled, released)))
}

/// Archives `id`, named `name`, whose metadata the metadata key holds as `expected`. It swaps the
/// metadata from `expected` to `metadata` with [`ARCHIVED`] set past the time it holds and
/// [`RESTORED`] removed, which says the record is archived, then releases the name from the record
/// ([`release_archived_name`]). Answers `true` where the release took the name, or a claim of the
/// record holds it since, and `false` where the mapping holds another value: another record's
/// claim, another claim of the record, or none. A metadata key that holds another value is left as
/// it is, and the archive fails with [`NamedRecord::changed`], releasing nothing.
///
/// The swap comes first, so a claim that read the metadata before it can't finish once the name is
/// released. `metadata` must hold no pending claim ([`RESTORING`]): a caller settles one first. A
/// [`RELEASED`] entry stays.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn archive<R: NamedRecord>(
    repository: Arc<RepositoryContext>,
    id: Context,
    name: &str,
    expected: Hash,
    metadata: Metadata,
) -> impl Future<Output = Result<bool, R::Error>> {
    async move {
        let generation = R::generation(&metadata, Claimed::Name);
        let archived = {
            let mut metadata = metadata;
            stamp_time::<R::Error>(&mut metadata, ARCHIVED)?;
            metadata.remove_key(RESTORED);
            swap_metadata::<R>(repository.clone(), id, expected, &metadata).await?
        };
        #[cfg(feature = "test-util")]
        crate::test_util::branch_step(crate::test_util::BranchStep::ArchiveReleasing).await;
        release_archived_name::<R>(repository, id, name, archived, generation).await
    }
}

/// Releases `name` from the claim of `generation` by `id` once an archive has swapped the record's
/// metadata to `archived`, unless a claim holds the name since, and answers whether the release
/// took the name or the claim holds it: the metadata key is read again, and where it holds another
/// value, the metadata it holds records a claim of `name` or names the record live under it
/// ([`holds_name`]). A claim that read the archive's metadata claims the name only where it no
/// longer maps to the record. A key that still holds `archived` is released from without a metadata
/// read. A read that fails fails, and an absent key fails with [`NamedRecord::missing`].
async fn release_archived_name<R: NamedRecord>(
    repository: Arc<RepositoryContext>,
    id: Context,
    name: &str,
    archived: Hash,
    generation: R::Generation,
) -> Result<bool, R::Error> {
    let current = metadata_key::<R>(repository.clone(), id)
        .await?
        .ok_or_else(|| R::missing(id))?;
    if current != archived
        && holds_name(
            &load_metadata::<R::Error>(repository.clone(), current).await?,
            name,
        )
    {
        lore_debug!("A claim of {name} by {id} holds it since its archive");
        return Ok(true);
    }
    release_held::<R>(repository, id, name, generation).await
}

/// Branches, whose names map through [`ID`](super::ID) keys to their [`METADATA`](super::METADATA)
/// keys.
pub struct Branches;

/// A branch's mapping holds the bare id, as every server writes and releases it, so its claims
/// carry no generation.
impl NamedRecord for Branches {
    type Error = super::BranchError;
    type Generation = ();
    #[cfg(feature = "test-util")]
    const STOPS_AT_RELEASE: bool = false;
    const METADATA: &'static str = super::METADATA;
    const NAME_MAPPING: &'static str = super::ID;

    fn missing(id: Context) -> Self::Error {
        Self::Error::from(crate::errors::BranchNotFound {
            branch: id.to_string(),
        })
    }

    fn changed(id: Context) -> Self::Error {
        Self::Error::from(crate::errors::BranchAlreadyExists {
            branch: id.to_string(),
        })
    }

    fn expected(
        repository: Arc<RepositoryContext>,
        id: Context,
        read: Hash,
        read_locally: bool,
    ) -> impl Future<Output = Hash> + Send {
        super::local_metadata_expected(repository, id, read, read_locally)
    }

    fn current_metadata(
        repository: Arc<RepositoryContext>,
        id: Context,
    ) -> impl Future<Output = Result<Metadata, Self::Error>> + Send {
        super::metadata_local(repository, id)
    }

    fn mapping(id: Context, _generation: ()) -> Hash {
        Hash::from_context(id)
    }

    fn generation(_metadata: &Metadata, _claimed: Claimed) {}

    fn set_generation(
        _metadata: &mut Metadata,
        _claimed: Claimed,
        _generation: Option<()>,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}
