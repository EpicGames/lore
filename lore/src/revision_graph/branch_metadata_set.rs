// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_graph_branch_metadata_set`: edit a branch's metadata at each location a call
//! consults, the remote first.
//!
//! The call checks its arguments before any read (`validate`). At each location that holds the
//! branch, it reads the metadata and applies the edit to it (`apply`). Where the edit changes the
//! metadata, it checks the addresses of the result, writes the result, and swaps the branch's
//! metadata from the value it read, reading and editing again what another writer swapped in
//! first (`edit`). The remote checks every address the result names, and the local store only
//! those the entries set. Each location that holds the branch then reports one branch metadata
//! record per entry of the metadata it holds that a record can carry (see
//! `record::emit_branch_metadata`), and each location ends with its location complete event. A
//! failure at the remote stops the call before the local store.

use std::borrow::Cow;
use std::sync::Arc;

use futures::FutureExt;
use lore_base::error::AddressNotFound;
use lore_base::error::BranchAdvanced;
use lore_base::error::BranchNotFound;
use lore_base::error::Oversized;
use lore_base::types::Address;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::branch;
use lore_revision::immutable;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreMetadata;
use lore_revision::interface::LoreString;
use lore_revision::metadata::METADATA_MAX_SIZE;
use lore_revision::metadata::Metadata;
use lore_revision::metadata::MetadataType;
use lore_revision::metadata::branch::READ_ONLY_KEYS;
use lore_revision::metadata::branch::first_missing_remote;
use lore_revision::protocol::RemoteEndpoint;
use lore_revision::repository::RepositoryContext;
use lore_transport::ProtocolError;
use lore_transport::Revision;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_graph::RevisionGraphError;
use crate::revision_graph::branch_metadata_get::read_remote_metadata;
use crate::revision_graph::branch_metadata_get::remote_metadata_hash;
use crate::revision_graph::call::revision_graph_call;
use crate::revision_graph::locations::CallOutcome;
use crate::revision_graph::locations::LocationAnswer;
use crate::revision_graph::locations::LocationMiss;
use crate::revision_graph::locations::local_write_context;
use crate::revision_graph::locations::locations;
use crate::revision_graph::locations::remote_revision_service;
use crate::revision_graph::record::emit_branch_metadata;
use crate::revision_graph::record::emit_location_complete;
use crate::storage::handle::LoreStore;
use crate::storage::store::PerCallFlags;
use crate::storage::store::StoreInternal;

/// The most swaps a set makes at one location. The last one another writer overtakes fails the
/// location with `BranchAdvanced`.
const MAX_SWAPS: usize = 8;

/// One entry `lore_revision_graph_branch_metadata_set` sets in a branch's metadata.
#[repr(C)]
#[derive(
    Clone, Debug, PartialEq, ValidateText, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema,
)]
pub struct LoreRevisionGraphBranchMetadataEntry {
    /// Key to set, replacing the value it holds. Not empty, not a read-only key, and named once
    /// across `entries` and `remove`.
    pub key: LoreString,
    /// Value to store, with the type its tag names, which must be a `LORE_METADATA_TYPE_*` tag.
    /// A `protect` value's tag must be `LORE_METADATA_TYPE_BOOLEAN`.
    pub value: LoreMetadata,
}

/// Arguments for `lore_revision_graph_branch_metadata_set`.
#[repr(C)]
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    LoreArgs,
    bitcode::Encode,
    bitcode::Decode,
    lore_macro::WireSchema,
)]
#[handler(branch_metadata_set_impl)]
pub struct LoreRevisionGraphBranchMetadataSetArgs {
    /// Caller-chosen id echoed back in every event this surface defines for the call. Separate
    /// from `globals.correlation_id`.
    pub id: u64,
    /// Open storage handle the call reaches the repository through
    pub store: LoreStore,
    /// Repository the branch is in
    pub repository: Partition,
    /// Branch whose metadata is edited. Not zero.
    pub branch: BranchId,
    /// Entries to set, each replacing the value its key holds
    pub entries: LoreArray<LoreRevisionGraphBranchMetadataEntry>,
    /// Keys to remove. A key the metadata lacks is passed over.
    pub remove: LoreArray<LoreString>,
}

/// An entry of a set in the form a metadata buffer stores it, as [`LoreMetadata::to_stored`]
/// answers it.
#[lore_macro::test_pub]
#[derive(Debug)]
pub(crate) struct StoredEntry<'a> {
    key: &'a str,
    value: Cow<'a, [u8]>,
    value_type: MetadataType,
}

/// What [`apply`] made of a location's metadata.
#[lore_macro::test_pub]
#[derive(Debug)]
pub(crate) enum Applied {
    /// The edits leave the metadata as it was.
    Unchanged(Metadata),
    /// The metadata after the edits.
    Changed(Metadata),
}

/// Checks the arguments of a set on `branch` that sets `entries` and removes `remove`, as a set
/// does before any read, and answers the stored form of each of `entries`, in order.
///
/// # Errors
///
/// `InvalidArguments` for a zero `branch`, an empty key, a read-only key ([`READ_ONLY_KEYS`]) in
/// either list, `protect` in `remove` or with a value that isn't Boolean, and a key the two lists
/// name more than once between them.
#[lore_macro::test_pub]
pub(crate) fn validate<'a>(
    branch: BranchId,
    entries: &'a [LoreRevisionGraphBranchMetadataEntry],
    remove: &[LoreString],
) -> Result<Vec<StoredEntry<'a>>, RevisionGraphError> {
    if branch.is_zero() {
        return Err(RevisionGraphError::invalid("the branch id is zero"));
    }
    let mut keys = Vec::with_capacity(entries.len() + remove.len());
    let mut stored = Vec::with_capacity(entries.len());
    for entry in entries {
        let key = entry.key.as_str();
        check_writable(key)?;
        let (value, value_type) = entry.value.to_stored();
        if key == branch::PROTECT && value_type != MetadataType::Boolean {
            return Err(RevisionGraphError::invalid(format!(
                "the value of `{key}` must be Boolean"
            )));
        }
        keys.push(key);
        stored.push(StoredEntry {
            key,
            value,
            value_type,
        });
    }
    for key in remove {
        let key = key.as_str();
        check_writable(key)?;
        if key == branch::PROTECT {
            return Err(RevisionGraphError::invalid(format!(
                "`{key}` can be set but not removed"
            )));
        }
        keys.push(key);
    }
    keys.sort_unstable();
    if let Some(pair) = keys.windows(2).find(|pair| pair[0] == pair[1]) {
        return Err(RevisionGraphError::invalid(format!(
            "the metadata key `{}` is named more than once",
            pair[0]
        )));
    }
    Ok(stored)
}

/// Refuses a key a set may not name: an empty or read-only one.
fn check_writable(key: &str) -> Result<(), RevisionGraphError> {
    if key.is_empty() {
        return Err(RevisionGraphError::invalid("a metadata key is empty"));
    }
    if READ_ONLY_KEYS.contains(&key) {
        return Err(RevisionGraphError::invalid(format!(
            "the metadata key `{key}` is read-only"
        )));
    }
    Ok(())
}

/// `metadata` with each of `entries` set and each key of `remove` removed, edited in place. An
/// entry whose key already holds its value with its type, and a key to remove that `metadata`
/// lacks, change nothing.
///
/// # Errors
///
/// `Oversized` where the edits change the metadata and leave it larger than
/// [`METADATA_MAX_SIZE`], and before any edit where `entries` alone would: their keys are
/// distinct, so the result holds every one of them.
#[lore_macro::test_pub]
pub(crate) fn apply(
    mut metadata: Metadata,
    entries: &[StoredEntry<'_>],
    remove: &[LoreString],
) -> Result<Applied, RevisionGraphError> {
    let pairs = entries
        .iter()
        .map(|entry| (entry.key, entry.value.as_ref()));
    if !Metadata::can_hold(pairs) {
        return Err(RevisionGraphError::from(Oversized {
            context: format!(
                "the metadata entries to set take more than the {METADATA_MAX_SIZE} byte \
                 metadata limit together; store large values as separate blobs and reference \
                 them via hash"
            ),
        }));
    }
    let mut changed = false;
    for entry in entries {
        changed |= metadata
            .set_typed(entry.key, &entry.value, entry.value_type)
            .forward_any::<RevisionGraphError>("setting a metadata entry")?;
    }
    for key in remove {
        changed |= metadata.remove_key(key.as_str());
    }
    if !changed {
        return Ok(Applied::Unchanged(metadata));
    }
    metadata
        .check_size()
        .forward_any::<RevisionGraphError>("checking the size of the edited metadata")?;
    Ok(Applied::Changed(metadata))
}

/// What a swap of a location's metadata answered.
enum Swapped {
    /// The swap stored this metadata.
    Won(Metadata),
    /// Another writer changed the metadata first, which now holds this hash: zero where the
    /// branch's metadata went.
    Lost(Hash),
}

/// Edit a branch's metadata at each location the call consults.
pub fn branch_metadata_set(
    globals: LoreGlobalArgs,
    args: LoreRevisionGraphBranchMetadataSetArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    dispatch_call(globals, args, callback, branch_metadata_set_impl)
}

fn branch_metadata_set_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionGraphBranchMetadataSetArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    let per_call = PerCallFlags::from_globals(&globals);
    revision_graph_call(
        globals,
        callback,
        args.store,
        args,
        branch_metadata_set,
        async move |store, args| {
            let entries = validate(args.branch, args.entries.as_slice(), args.remove.as_slice())?;
            let locations = locations(&store, per_call)?;
            let mut outcome = CallOutcome::default();
            if let Some(remote) = locations.remote() {
                let mut records = 0;
                let answer = set_remote(&mut records, &store, &args, &entries, remote).await;
                emit_location_complete(args.id, LoreBranchLocation::Remote, records, &answer);
                // A write verb stops at a failure at the remote, before it writes locally.
                outcome.add(Ok(answer?));
            }
            if locations.local() {
                let mut records = 0;
                let answer = set_local(&mut records, &store, &args, &entries).await;
                emit_location_complete(args.id, LoreBranchLocation::Local, records, &answer);
                outcome.add(answer);
            }
            outcome.finish(|_| {
                RevisionGraphError::from(BranchNotFound {
                    branch: args.branch.to_string(),
                })
            })
        },
    )
}

/// Edits the metadata of the branch `args` names at the remote that `remote`, the storage
/// handle's endpoint, reaches, with `entries`, the stored form of its entries, and sends the
/// records of the metadata the remote then holds, adding the record events sent to `records`.
///
/// The remote's revision service answers the hash of the branch's metadata blob
/// ([`remote_metadata_hash`]), which is read through the remote's read context. Each swap goes
/// through [`swap_remote`].
async fn set_remote(
    records: &mut u64,
    store: &StoreInternal,
    args: &LoreRevisionGraphBranchMetadataSetArgs,
    entries: &[StoredEntry<'_>],
    remote: &Arc<RemoteEndpoint>,
) -> LocationAnswer<()> {
    let (ctx, service) = remote_revision_service(store, args.repository, remote).await?;
    let Some(expected) = remote_metadata_hash(service.as_ref(), args.branch).await? else {
        return Ok(Err(LocationMiss::BranchNotFound));
    };
    let edited = edit(
        expected,
        entries,
        args.remove.as_slice(),
        |edited, expected| swap_remote(&ctx, service.as_ref(), args.branch, edited, expected),
        |hash| read_remote_metadata(&ctx, hash).map(|read| read.map(Some)),
    )
    .await?;
    Ok(report(records, args, LoreBranchLocation::Remote, edited))
}

/// Edits the metadata of the branch `args` names in the local store, the blob its metadata key
/// names, with `entries`, the stored form of its entries, and sends the records of the metadata
/// the store then holds, adding the record events sent to `records`.
///
/// One context reads and writes. A metadata key that names a blob the store doesn't hold leaves
/// the store without metadata for the branch, as does a swap that another writer overtook with
/// such a blob. Each swap goes through [`swap_local`].
async fn set_local(
    records: &mut u64,
    store: &StoreInternal,
    args: &LoreRevisionGraphBranchMetadataSetArgs,
    entries: &[StoredEntry<'_>],
) -> LocationAnswer<()> {
    let ctx = local_write_context(store, args.repository);
    let Some(expected) = branch::mutable_value(ctx.clone(), branch::METADATA, args.branch)
        .await
        .forward_any::<RevisionGraphError>("loading the branch metadata key")?
    else {
        return Ok(Err(LocationMiss::BranchNotFound));
    };
    let addresses = set_addresses(args.entries.as_slice());
    let edited = edit(
        expected,
        entries,
        args.remove.as_slice(),
        |edited, expected| swap_local(&ctx, args.branch, &addresses, edited, expected),
        |hash| {
            branch::local_metadata_blob(ctx.clone(), hash).map(|read| {
                read.forward_any::<RevisionGraphError>("reading the branch metadata blob")
            })
        },
    )
    .await?;
    Ok(report(records, args, LoreBranchLocation::Local, edited))
}

/// Sends the records of `edited`, the metadata the set left at `location` where the location holds
/// the branch, adding the record events sent to `records`.
fn report(
    records: &mut u64,
    args: &LoreRevisionGraphBranchMetadataSetArgs,
    location: LoreBranchLocation,
    edited: Result<Metadata, LocationMiss>,
) -> Result<(), LocationMiss> {
    let metadata = edited?;
    *records += emit_branch_metadata(args.id, location, args.branch, &metadata, None);
    Ok(())
}

/// The metadata a location holds once `entries` are set in the metadata blob `expected` and the
/// keys of `remove` removed.
///
/// `read` reads the blob a hash names, and where the edit leaves the metadata as it was, nothing
/// is checked or written. Otherwise `swap` checks and writes the edited metadata and swaps the
/// branch's metadata from `expected` to it. Where another writer swapped first, the edit is applied
/// again to the metadata the swap answered, so the other writer's entries stay, at most
/// [`MAX_SWAPS`] swaps in all. A swap that answers zero, where the branch's metadata went, and a
/// `read` that finds no blob leave the location without the branch.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
///
/// # Errors
///
/// [`apply`]'s, `swap`'s and `read`'s errors, and `BranchAdvanced` where another writer overtook
/// every swap.
#[allow(clippy::manual_async_fn)]
fn edit<SwapFuture, ReadFuture>(
    mut expected: Hash,
    entries: &[StoredEntry<'_>],
    remove: &[LoreString],
    swap: impl Fn(Metadata, Hash) -> SwapFuture,
    read: impl Fn(Hash) -> ReadFuture,
) -> impl Future<Output = LocationAnswer<Metadata>>
where
    SwapFuture: Future<Output = Result<Swapped, RevisionGraphError>>,
    ReadFuture: Future<Output = Result<Option<Metadata>, RevisionGraphError>>,
{
    async move {
        for _ in 0..MAX_SWAPS {
            let Some(metadata) = read(expected).await? else {
                return Ok(Err(LocationMiss::BranchNotFound));
            };
            let edited = match apply(metadata, entries, remove)? {
                Applied::Unchanged(metadata) => return Ok(Ok(metadata)),
                Applied::Changed(edited) => edited,
            };
            expected = match swap(edited, expected).await? {
                Swapped::Won(metadata) => return Ok(Ok(metadata)),
                Swapped::Lost(held) if held.is_zero() => {
                    return Ok(Err(LocationMiss::BranchNotFound));
                }
                Swapped::Lost(held) => held,
            };
        }
        Err(overtaken())
    }
}

/// Checks that the remote holds each address `metadata` names, with at most one existence query
/// on the storage session of `ctx`, the remote's read context, writes `metadata` at the remote
/// through `ctx`, and swaps the metadata of `branch` from `expected` to it with `service`, the
/// remote's revision service.
///
/// The swap follows the write whatever the upload's outcome, which the write doesn't report: the
/// remote refuses a caller without write access first, and fails its own read of a blob that
/// didn't arrive. Lore Server answers `NotFound` for an address it lacks and for a missing
/// metadata key alike. After the existence query that means the branch's metadata went, which
/// [`metadata_went`] confirms.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
///
/// # Errors
///
/// `AddressNotFound` for the first address the remote lacks, a failed query or write, and the
/// remote's refusal of the swap.
#[allow(clippy::manual_async_fn)]
fn swap_remote<'a>(
    ctx: &'a Arc<RepositoryContext>,
    service: &'a dyn Revision,
    branch: BranchId,
    metadata: Metadata,
    expected: Hash,
) -> impl Future<Output = Result<Swapped, RevisionGraphError>> + 'a {
    async move {
        if let Some(missing) = first_missing_remote(ctx, &metadata.address_values())
            .await
            .forward_any::<RevisionGraphError>(
                "checking the new metadata's addresses at the remote",
            )?
        {
            return Err(RevisionGraphError::from(AddressNotFound::from(missing)));
        }
        let updated = metadata
            .serialize_remote(ctx.clone())
            .await
            .forward_any::<RevisionGraphError>("writing the branch metadata at the remote")?;
        match service.branch_metadata_set(branch, expected, updated).await {
            Ok(set) if set.success => Ok(Swapped::Won(metadata)),
            Ok(set) => Ok(Swapped::Lost(set.current_hash)),
            Err(err) if err.is_not_found() => metadata_went(service, branch, err).await,
            Err(err) => Err(err)
                .forward_any::<RevisionGraphError>("swapping the branch metadata at the remote"),
        }
    }
}

/// What the remote's `refusal`, a `NotFound`, of a swap of the metadata of `branch` means, from
/// a metadata get with `service`, the remote's revision service ([`remote_metadata_hash`]): a swap
/// lost to zero where the remote holds no metadata for the branch.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
///
/// # Errors
///
/// `Internal` where the remote still holds metadata for the branch, and the failure of that
/// metadata get.
#[allow(clippy::manual_async_fn)]
fn metadata_went(
    service: &dyn Revision,
    branch: BranchId,
    refusal: ProtocolError,
) -> impl Future<Output = Result<Swapped, RevisionGraphError>> + '_ {
    async move {
        match remote_metadata_hash(service, branch).await? {
            None => Ok(Swapped::Lost(Hash::default())),
            Some(_) => Err(RevisionGraphError::internal_with_context(
                refusal,
                "the remote answered the branch metadata swap with NotFound and still holds the \
                 branch's metadata",
            )),
        }
    }
}

/// Checks that the local store holds the payload of each of `addresses`, the addresses the set's
/// entries name, with at most one query of the store of `ctx`, writes `metadata` to the store
/// through `ctx` with the options [`Metadata::serialize`] takes, reading the buffer in place where
/// it fits one fragment, and swaps the metadata key of `branch` from `expected` to it. The swap
/// answers the value the key held, `expected` where it won.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
///
/// # Errors
///
/// `AddressNotFound` for the first address whose payload the store lacks, a failed query, and the
/// write's and the swap's errors.
#[allow(clippy::manual_async_fn)]
fn swap_local<'a>(
    ctx: &'a Arc<RepositoryContext>,
    branch: BranchId,
    addresses: &'a [Address],
    metadata: Metadata,
    expected: Hash,
) -> impl Future<Output = Result<Swapped, RevisionGraphError>> + 'a {
    async move {
        if let Some(missing) = immutable::first_not_held_locally(ctx, addresses)
            .await
            .forward_any::<RevisionGraphError>(
            "querying the local store for the payloads the entries name",
        )? {
            return Err(RevisionGraphError::from(AddressNotFound::from(missing)));
        }
        let options =
            immutable::write_options_from_repository(ctx.clone()).with_local_cache_priority();
        let updated = metadata
            .serialize_borrowed(ctx.clone(), options)
            .await
            .forward_any::<RevisionGraphError>("writing the branch metadata")?;
        let held =
            branch::mutable_try_store(ctx.clone(), branch::METADATA, branch, expected, updated)
                .await
                .forward_any::<RevisionGraphError>("swapping the branch metadata")?;
        Ok(if held == expected {
            Swapped::Won(metadata)
        } else {
            Swapped::Lost(held)
        })
    }
}

/// The addresses `entries` set, without those with a zero hash, which name empty content.
fn set_addresses(entries: &[LoreRevisionGraphBranchMetadataEntry]) -> Vec<Address> {
    entries
        .iter()
        .filter_map(|entry| match &entry.value {
            LoreMetadata::Address(address) if !address.hash.is_zero() => Some(*address),
            _ => None,
        })
        .collect()
}

/// `BranchAdvanced` for a location whose every swap another writer overtook. Its trace names the
/// metadata swap, since the code's message speaks of a branch's latest.
fn overtaken() -> RevisionGraphError {
    let mut err = RevisionGraphError::from(BranchAdvanced);
    err.push_context("another writer changed the branch metadata before each swap");
    err
}
