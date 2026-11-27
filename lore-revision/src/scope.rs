// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Scopes of a repository, stored as branches are ([`record`]): metadata under a [`METADATA`] key
//! per scope id, and a mapping from each name, regardless of case, under an [`ID`] key.
//!
//! The metadata holds the scope's id, name, creator and creation time under the key names branch
//! metadata uses for them, and the archive and claim entries [`record`] writes. A scope's lifecycle
//! writes follow the order a branch's do: a create swaps the metadata in from zero, then claims the
//! name; a restore or a rename records the name it claims ([`RESTORING`]), claims it, then finishes
//! with a swap that removes the record; an archive stamps [`ARCHIVED`](branch::ARCHIVED) and swaps
//! the metadata, then releases the name. The next write of a scope settles a claim that stopped.
//!
//! The default scope has an id derived from its repository's ([`default_scope_id`]) and no stored
//! state. Its name, [`DEFAULT_NAME`], is reserved, and it takes no lifecycle write.

use std::sync::Arc;

use lore_base::text::fold_eq;
use lore_error_set::prelude::*;

use crate::branch;
use crate::branch::CREATED;
use crate::branch::CREATOR;
use crate::branch::NAME;
use crate::branch::RELEASED;
use crate::branch::RESTORED;
use crate::branch::RESTORING;
use crate::branch::is_archived;
use crate::branch::names_record;
use crate::branch::record;
use crate::branch::record::Claimed;
use crate::branch::record::NamedRecord;
use crate::branch::released;
use crate::branch::restoring;
use crate::branch::unmarked;
use crate::errors::AddressNotFound;
use crate::errors::AlreadyExists;
use crate::errors::Disconnected;
use crate::errors::FileNotFound;
use crate::errors::InvalidArguments;
use crate::errors::InvalidPath;
use crate::errors::LinkNotFound;
use crate::errors::Maintenance;
use crate::errors::NoRemote;
use crate::errors::NodeNotFound;
use crate::errors::NotAuthenticated;
use crate::errors::NotAuthorized;
use crate::errors::NotConnected;
use crate::errors::NotFound;
use crate::errors::NotSupported;
use crate::errors::Oversized;
use crate::errors::PayloadNotFound;
use crate::errors::SlowDown;
use crate::errors::WriteRequired;
use crate::hash;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore_debug;
use crate::lore_warn;
use crate::metadata::Metadata;
use crate::repository;
use crate::repository::RepositoryContext;

/// The function of a scope's metadata key.
pub const METADATA: &str = "scope-metadata";
/// The function of a scope name's mapping key.
pub const ID: &str = "scope-id";
/// The function the default scope's id derives under.
pub const DEFAULT_ID: &str = "scope-default";
/// The name of the default scope, reserved in any case.
pub const DEFAULT_NAME: &str = "default";
/// The key of a scope's id in its metadata.
const SCOPE_ID: &str = branch::ID;
/// The key of the last claim generation a scope's metadata issued.
const GENERATION: &str = "generation";
/// The key of the generation of the claim of the name a scope's metadata names it.
const CLAIM: &str = "claim";
/// The key of the generation of the claim a scope's metadata records as [`RESTORING`].
const RESTORING_CLAIM: &str = "restoring-claim";
/// The key of the generation of the claim a scope's metadata marks [`RELEASED`].
const RELEASED_CLAIM: &str = "released-claim";

/// A scope's identifier, a 16-byte value its creator generates.
pub type ScopeId = Context;

#[error_set]
pub enum ScopeError {
    AlreadyExists,
    InvalidArguments,
    WriteRequired,
    FileNotFound,
    Oversized,
    NodeNotFound,
    LinkNotFound,
    NotFound,
    InvalidPath,
    AddressNotFound,
    PayloadNotFound,
    Disconnected,
    SlowDown,
    Maintenance,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NotSupported,
}

/// A scope as its metadata describes it. The default scope has an empty creator and a zero
/// `created`, and an archived scope an empty name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub id: ScopeId,
    pub name: String,
    pub creator: String,
    /// The time the scope was created, in milliseconds since the Unix epoch.
    pub created: u64,
    pub archived: bool,
}

/// The state of a scope that a lifecycle write refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The scope is the default scope, which takes no lifecycle write.
    Default,
    /// The scope is archived, which a rename needs a restore for, and which a create leaves to
    /// [`restore`].
    Archived,
}

/// Scopes, whose names map through [`ID`] keys to their [`METADATA`] keys. A mapping value holds
/// the scope's id in its first 16 bytes and the generation of the claim that wrote it, little
/// endian, in the 8 after them: every claim of a name takes the next generation of the scope
/// ([`next_generation`]), so a release swaps from the one claim it releases, and a later claim of
/// the name by the same scope keeps the mapping.
#[lore_macro::test_pub]
struct Scopes;

impl NamedRecord for Scopes {
    type Error = ScopeError;
    type Generation = u64;
    #[cfg(feature = "test-util")]
    const STOPS_AT_RELEASE: bool = true;
    const METADATA: &'static str = METADATA;
    const NAME_MAPPING: &'static str = ID;

    fn mapping(id: Context, generation: u64) -> Hash {
        let mut value = [0u8; 32];
        value[..16].copy_from_slice(id.data());
        value[16..24].copy_from_slice(&generation.to_le_bytes());
        Hash::from(value)
    }

    fn generation(metadata: &Metadata, claimed: Claimed) -> u64 {
        metadata.get_u64(generation_key(claimed)).unwrap_or(0)
    }

    fn set_generation(
        metadata: &mut Metadata,
        claimed: Claimed,
        generation: Option<u64>,
    ) -> Result<(), ScopeError> {
        let Some(generation) = generation else {
            metadata.remove_key(generation_key(claimed));
            return Ok(());
        };
        metadata
            .set_u64(generation_key(claimed), generation)
            .forward::<ScopeError>("recording a claim generation in scope metadata")
    }

    fn missing(id: Context) -> Self::Error {
        missing(id)
    }

    fn changed(id: Context) -> Self::Error {
        exists(id)
    }

    fn expected(
        _repository: Arc<RepositoryContext>,
        _id: Context,
        read: Hash,
        _read_locally: bool,
    ) -> impl Future<Output = Hash> + Send {
        std::future::ready(read)
    }

    async fn current_metadata(
        repository: Arc<RepositoryContext>,
        id: Context,
    ) -> Result<Metadata, Self::Error> {
        read(repository, id).await.map(|(_, metadata)| metadata)
    }
}

/// The metadata key of the generation of the `claimed` claim.
fn generation_key(claimed: Claimed) -> &'static str {
    match claimed {
        Claimed::Name => CLAIM,
        Claimed::Restoring => RESTORING_CLAIM,
        Claimed::Released => RELEASED_CLAIM,
    }
}

/// Issues the next claim generation of the scope `metadata` describes, recording it in the
/// metadata: one past the last it issued, which the swap that records the claim writes.
fn next_generation(metadata: &mut Metadata) -> Result<u64, ScopeError> {
    let generation = metadata.get_u64(GENERATION).unwrap_or(0).saturating_add(1);
    metadata
        .set_u64(GENERATION, generation)
        .forward::<ScopeError>("issuing a claim generation in scope metadata")?;
    Ok(generation)
}

/// The generation of the claim a mapping value `held` records ([`Scopes::mapping`]).
fn held_generation(held: Hash) -> u64 {
    let mut generation = [0u8; 8];
    generation.copy_from_slice(&held.data()[16..24]);
    u64::from_le_bytes(generation)
}

/// The id of the default scope of `repository`: the first 16 bytes of the hash of the repository's
/// id under [`DEFAULT_ID`].
pub fn default_scope_id(repository: RepositoryId) -> ScopeId {
    let hash = hash::hash_function_arg_slice(repository::SALT_LORE, DEFAULT_ID, repository.data());
    let mut id = [0u8; 16];
    id.copy_from_slice(&hash.data()[..16]);
    ScopeId::from(id)
}

/// The default scope of `repository`, which has no stored record.
pub fn default_scope(repository: RepositoryId) -> Scope {
    Scope {
        id: default_scope_id(repository),
        name: DEFAULT_NAME.to_string(),
        creator: String::new(),
        created: 0,
        archived: false,
    }
}

/// Whether `name` names the default scope, compared regardless of case.
pub fn is_default_name(name: &str) -> bool {
    fold_eq(name, DEFAULT_NAME)
}

/// Whether a scope can take `name`: a valid branch name ([`branch::is_valid_name`]) that isn't
/// the default scope's.
pub fn is_valid_name(name: &str) -> bool {
    branch::is_valid_name(name) && !is_default_name(name)
}

/// The scope `id` of `repository`, `None` where it has no metadata. The default scope's id
/// answers the default scope with no read.
pub async fn get(
    repository: Arc<RepositoryContext>,
    id: ScopeId,
) -> Result<Option<Scope>, ScopeError> {
    if id == default_scope_id(repository.id) {
        return Ok(Some(default_scope(repository.id)));
    }
    Ok(stored(repository, id)
        .await?
        .map(|(_, metadata)| scope_of(id, &metadata)))
}

/// The live scope of `repository` that `name` maps to, compared regardless of case, `None` where
/// there is none: a mapping to a scope whose metadata is archived, or names it otherwise, is
/// stale. The name [`DEFAULT_NAME`] answers the default scope with no read.
pub async fn get_by_name(
    repository: Arc<RepositoryContext>,
    name: &str,
) -> Result<Option<Scope>, ScopeError> {
    if is_default_name(name) {
        return Ok(Some(default_scope(repository.id)));
    }
    let held = record::name_mapping::<Scopes>(repository.clone(), name).await?;
    if held.is_zero() {
        return Ok(None);
    }
    Ok(stored(repository, held)
        .await?
        .filter(|(_, metadata)| !is_archived(metadata) && names_record(metadata, name))
        .map(|(_, metadata)| scope_of(held, &metadata)))
}

/// The scope `id` as `metadata` describes it.
fn scope_of(id: ScopeId, metadata: &Metadata) -> Scope {
    let archived = is_archived(metadata);
    let text = |key| metadata.get_string(key).unwrap_or_default().to_string();
    Scope {
        id,
        name: if archived { String::new() } else { text(NAME) },
        creator: text(CREATOR),
        created: metadata.get_u64(CREATED).unwrap_or_default(),
        archived,
    }
}

/// Fails with [`InvalidArguments`] unless a scope can take `name` ([`is_valid_name`]).
fn check_name(name: &str) -> Result<(), ScopeError> {
    if is_valid_name(name) {
        return Ok(());
    }
    Err(ScopeError::from(InvalidArguments {
        reason: format!("the scope name {name:?} is not valid"),
    }))
}

/// Creates scope `id` named `name` in `repository`, created by `creator` at `created`, in
/// milliseconds since the Unix epoch, and answers it. It swaps the metadata in from zero, then
/// claims the name from the value [`claim_expected`] reads. A claim that fails swaps the metadata
/// back to zero, which removes it, and fails with the claim's error: [`AlreadyExists`] naming
/// `name` where the mapping holds another value.
///
/// A scope `id` that is live under `name` already answers itself, as a repeated create does, and
/// claims the name first where it doesn't map to the scope, as a create that stopped after its
/// metadata leaves it. An archived scope `id` is refused with [`Refused::Archived`], with nothing
/// written, for the caller to restore ([`restore`]). Any other scope `id` fails with
/// [`AlreadyExists`] naming the id. A name that isn't valid, a zero id and the default scope's id
/// fail with [`InvalidArguments`] before anything is written.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn create(
    repository: Arc<RepositoryContext>,
    id: ScopeId,
    name: &str,
    creator: &str,
    created: u64,
) -> impl Future<Output = Result<Result<Scope, Refused>, ScopeError>> {
    async move {
        check_name(name)?;
        if id.is_zero() || id == default_scope_id(repository.id) {
            return Err(ScopeError::from(InvalidArguments {
                reason: format!("the scope id {id} can't be created"),
            }));
        }
        loop {
            if let Some(existing) = record::metadata_key::<Scopes>(repository.clone(), id).await? {
                return created_again(repository, id, name, existing).await;
            }
            let expected = claim_expected(repository.clone(), name, id).await?;
            #[cfg(feature = "test-util")]
            crate::test_util::branch_step(crate::test_util::BranchStep::CreateNameRead).await;
            let mut metadata = Metadata::new();
            populate(&mut metadata, id, name, creator, created)?;
            let generation = next_generation(&mut metadata)?;
            Scopes::set_generation(&mut metadata, Claimed::Name, Some(generation))?;
            let Some(stored) = swap(repository.clone(), id, Hash::default(), &metadata).await?
            else {
                continue;
            };
            #[cfg(feature = "test-util")]
            crate::test_util::branch_step(crate::test_util::BranchStep::CreateMetadataStored).await;
            if let Err(err) = claim(
                repository.clone(),
                name,
                expected,
                Scopes::mapping(id, generation),
            )
            .await
            {
                record::revert_metadata::<Scopes>(repository, id, stored, Hash::default()).await;
                return Err(err);
            }
            #[cfg(feature = "test-util")]
            crate::test_util::branch_step(crate::test_util::BranchStep::CreateNameClaimed).await;
            return Ok(Ok(scope_of(id, &metadata)));
        }
    }
}

/// Answers scope `id`, whose metadata key holds `existing`, to a create of it under `name`:
/// [`Refused::Archived`] where it is archived, the scope where it is live under `name` with no
/// pending claim, after a claim of `name` where the name doesn't map to it, and otherwise
/// [`AlreadyExists`] naming the id.
async fn created_again(
    repository: Arc<RepositoryContext>,
    id: ScopeId,
    name: &str,
    existing: Hash,
) -> Result<Result<Scope, Refused>, ScopeError> {
    let metadata = record::load_metadata::<ScopeError>(repository.clone(), existing).await?;
    if is_archived(&metadata) {
        return Ok(Err(Refused::Archived));
    }
    if restoring(&metadata).is_some() || metadata.get_string(NAME).ok() != Some(name) {
        lore_debug!("Scope {id} exists other than as a create of {name} leaves it");
        return Err(exists(id));
    }
    let claimed = Scopes::mapping(id, Scopes::generation(&metadata, Claimed::Name));
    let expected = claim_expected(repository.clone(), name, id).await?;
    if expected != claimed {
        claim(repository, name, expected, claimed).await?;
    }
    Ok(Ok(scope_of(id, &metadata)))
}

/// Writes the descriptive fields of a new scope into `metadata`.
fn populate(
    metadata: &mut Metadata,
    id: ScopeId,
    name: &str,
    creator: &str,
    created: u64,
) -> Result<(), ScopeError> {
    metadata
        .set_context(SCOPE_ID, id)
        .forward::<ScopeError>("setting the scope id")?;
    metadata
        .set_string(NAME, name)
        .forward::<ScopeError>("setting the scope name")?;
    metadata
        .set_string(CREATOR, creator)
        .forward::<ScopeError>("setting the scope creator")?;
    metadata
        .set_u64(CREATED, created)
        .forward::<ScopeError>("setting the scope creation time")
}

/// The value a claim of `name` for `claimant` swaps from: the value of the name's mapping, zero for
/// none. A mapping to another scope with metadata fails with [`AlreadyExists`] naming `name`,
/// whatever its metadata says: only that scope's own writes release it, as the next write of an
/// archive or a rename that stopped before its release does. A mapping to an id without metadata
/// is stale, as a create whose claim landed and whose metadata was set back leaves it, and the
/// claim swaps from it. A mapping to `claimant` answers its value: a claim of the scope, which a
/// later claim swaps from.
async fn claim_expected(
    repository: Arc<RepositoryContext>,
    name: &str,
    claimant: ScopeId,
) -> Result<Hash, ScopeError> {
    let value = record::mapping_value::<Scopes>(repository.clone(), name).await?;
    let held = value.to_context();
    if held.is_zero() || held == claimant {
        return Ok(value);
    }
    if record::metadata_key::<Scopes>(repository, held)
        .await?
        .is_some()
    {
        lore_debug!("Scope name {name} maps to {held}");
        return Err(taken(name));
    }
    lore_debug!("Scope name {name} maps to {held}, which has no metadata");
    Ok(value)
}

/// Claims `name` with a compare-and-swap of its mapping from `expected` to `claimed`, the value of
/// the claim ([`Scopes::mapping`]), which the caller recorded in the scope's metadata first. The
/// claim succeeds where the mapping held `expected`, or holds `claimed` already, as the same claim
/// by a repeated create leaves it. A mapping that holds none, or an older claim of the same scope,
/// as a release of the scope that raced the claim leaves it, is swapped from again. A mapping that
/// holds another value is left as it is, and the claim fails with [`AlreadyExists`] naming `name`.
async fn claim(
    repository: Arc<RepositoryContext>,
    name: &str,
    mut expected: Hash,
    claimed: Hash,
) -> Result<(), ScopeError> {
    let held = loop {
        let held =
            record::swap_mapping::<Scopes>(repository.clone(), name, expected, claimed).await?;
        if held == expected || held == claimed {
            return Ok(());
        }
        if !superseded_by(held, claimed) {
            break held;
        }
        expected = held;
    };
    lore_debug!(
        "Scope name {name} maps to {}, not {}, so {} doesn't claim it",
        held.to_context(),
        expected.to_context(),
        claimed.to_context()
    );
    Err(taken(name))
}

/// Whether a mapping that holds `held` is one the claim `claimed` takes over: none, or an older
/// claim of the same scope.
fn superseded_by(held: Hash, claimed: Hash) -> bool {
    held.is_zero()
        || (held.to_context() == claimed.to_context()
            && held_generation(held) < held_generation(claimed))
}

/// [`AlreadyExists`], with a context naming the scope name `name`: another scope holds it.
fn taken(name: &str) -> ScopeError {
    let mut err = ScopeError::from(AlreadyExists);
    err.push_context(format!("scope name {name}"));
    err
}

/// [`AlreadyExists`], with a context naming scope `id`: the scope exists other than as the write
/// expects it, as a create of it under another request or a claim of it that recorded itself
/// first finds it.
fn exists(id: ScopeId) -> ScopeError {
    let mut err = ScopeError::from(AlreadyExists);
    err.push_context(format!("scope {id}"));
    err
}

/// [`NotFound`], with a context naming scope `id`, which has no metadata.
fn missing(id: ScopeId) -> ScopeError {
    let mut err = ScopeError::from(NotFound);
    err.push_context(format!("scope {id}"));
    err
}

/// The metadata hash of scope `id` and the metadata it names, `None` where the scope has no
/// metadata.
async fn stored(
    repository: Arc<RepositoryContext>,
    id: ScopeId,
) -> Result<Option<(Hash, Metadata)>, ScopeError> {
    let Some(hash) = record::metadata_key::<Scopes>(repository.clone(), id).await? else {
        return Ok(None);
    };
    let metadata = record::load_metadata::<ScopeError>(repository, hash).await?;
    Ok(Some((hash, metadata)))
}

/// The metadata hash of scope `id` and the metadata it names. A scope without metadata fails with
/// [`NotFound`].
async fn read(
    repository: Arc<RepositoryContext>,
    id: ScopeId,
) -> Result<(Hash, Metadata), ScopeError> {
    stored(repository, id).await?.ok_or_else(|| missing(id))
}

/// [`record::swap_metadata`] of scope `id`, answering `None` where the metadata key holds a value
/// other than `expected`.
async fn swap(
    repository: Arc<RepositoryContext>,
    id: ScopeId,
    expected: Hash,
    metadata: &Metadata,
) -> Result<Option<Hash>, ScopeError> {
    match record::swap_metadata::<Scopes>(repository, id, expected, metadata).await {
        Ok(hash) => Ok(Some(hash)),
        Err(err) if err.is_already_exists() => Ok(None),
        Err(err) => Err(err),
    }
}

/// Releases the name a settled claim of scope `id` had claimed, which `metadata` marks
/// [`RELEASED`] with the claim's generation, unless the metadata no longer marks it ([`unmarked`]).
/// The release swaps from that generation alone, so it leaves the claim of the name the scope holds,
/// whatever the name.
async fn release_marked(
    repository: Arc<RepositoryContext>,
    id: ScopeId,
    metadata: &Metadata,
) -> Result<(), ScopeError> {
    if let Some(marked) = released(metadata) {
        let generation = Scopes::generation(metadata, Claimed::Released);
        record::release_unless::<Scopes>(repository, id, marked, unmarked, generation).await?;
    }
    Ok(())
}

/// The write a [`claim_name`] makes: what it checks of the scope, and the metadata it finishes
/// with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Claim {
    /// A restore of an archived scope.
    Restore,
    /// A rename of a live scope.
    Rename,
}

/// Archives scope `id` of `repository`, and answers it archived. It swaps the metadata to one
/// that says the scope is archived, then releases the name ([`record::archive`]); the metadata,
/// with the scope's name, stays. A scope that is archived already answers itself, once a mapping
/// of its name to it, as an archive that stopped before its release leaves it, is released.
///
/// A pending claim the metadata records ([`RESTORING`]) is settled first ([`record::settle`]), and
/// a [`RELEASED`] name released ([`release_marked`]). A metadata key
/// that changed since the archive read it sends the archive back to its read. The default scope is
/// refused with [`Refused::Default`], and a scope without metadata fails with [`NotFound`].
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn archive(
    repository: Arc<RepositoryContext>,
    id: ScopeId,
) -> impl Future<Output = Result<Result<Scope, Refused>, ScopeError>> {
    async move {
        if id == default_scope_id(repository.id) {
            return Ok(Err(Refused::Default));
        }
        loop {
            let (read, metadata) = read(repository.clone(), id).await?;
            #[cfg(feature = "test-util")]
            crate::test_util::branch_step(crate::test_util::BranchStep::ArchiveMetadataRead).await;
            if let Some(pending) = restoring(&metadata) {
                let pending = pending.to_string();
                record::settle::<Scopes>(repository.clone(), id, read, true, metadata, &pending)
                    .await?;
                continue;
            }
            let name = metadata.get_string(NAME).unwrap_or_default().to_string();
            release_marked(repository.clone(), id, &metadata).await?;
            if is_archived(&metadata) {
                let generation = Scopes::generation(&metadata, Claimed::Name);
                record::release_held::<Scopes>(repository, id, &name, generation).await?;
                return Ok(Ok(scope_of(id, &metadata)));
            }
            #[cfg(feature = "test-util")]
            crate::test_util::branch_step(crate::test_util::BranchStep::ArchiveLookedUp).await;
            let scope = Scope {
                name: String::new(),
                archived: true,
                ..scope_of(id, &metadata)
            };
            match record::archive::<Scopes>(repository.clone(), id, &name, read, metadata).await {
                Ok(_) => return Ok(Ok(scope)),
                Err(err) if err.is_already_exists() => {
                    lore_debug!("Scope {id} changed since the archive read it");
                }
                Err(err) => return Err(err),
            }
        }
    }
}

/// Restores the archived scope `id` of `repository` under `name`, and answers it live, as
/// [`claim_name`] describes. A scope live under exactly `name`, with no pending claim, answers
/// itself, as a repeated create does, so a restore sent again after it succeeded answers the
/// scope. A scope live under any other name fails with [`AlreadyExists`] naming the scope, as a
/// create of it under that name does.
///
/// Returns the claim's future, without a future of its own.
pub fn restore(
    repository: Arc<RepositoryContext>,
    id: ScopeId,
    name: &str,
) -> impl Future<Output = Result<Result<Scope, Refused>, ScopeError>> {
    claim_name(repository, id, name, Claim::Restore)
}

/// Renames the live scope `id` of `repository` to `name`, and answers it under that name, as
/// [`claim_name`] describes. An archived scope is refused with [`Refused::Archived`]. A rename to
/// the name the scope holds answers the scope, and one that changes only the name's case swaps the
/// name in the metadata alone, since the name's mapping key is the same.
///
/// Returns the claim's future, without a future of its own.
pub fn rename(
    repository: Arc<RepositoryContext>,
    id: ScopeId,
    name: &str,
) -> impl Future<Output = Result<Result<Scope, Refused>, ScopeError>> {
    claim_name(repository, id, name, Claim::Rename)
}

/// Claims `name` for scope `id` as `kind` says, in three writes, so that one that stops part way
/// leaves a record of what it held. It swaps the metadata from the value it read to one that
/// records `name` as [`RESTORING`], then claims `name` from the value its mapping held
/// ([`claim_expected`]), then finishes with a swap to metadata without the record that names the
/// scope `name`: a restore sets [`RESTORED`] to the time now, which says the scope is live, and a
/// rename marks the name it held [`RELEASED`] and releases it.
///
/// Every claim takes the next generation of the scope ([`next_generation`]) in the swap that
/// records it, and every release swaps from the generation the metadata it read records, so a
/// release that races a later claim of the same name by the scope leaves that claim. A name a
/// [`RELEASED`] mark names is released first ([`release_marked`]), and a restore under a name other
/// than the scope's old one first releases the old name's claim, which an archive that stopped
/// before its release leaves mapped. A mapping of `name` to an older claim of the scope is swapped
/// from.
///
/// Of claims of one scope that race, one makes the first swap, and the others, which find the
/// metadata changed and a claim recorded, fail with [`AlreadyExists`] naming the scope before they
/// claim anything. A metadata change of another kind sends the claim back to its read. A pending
/// claim the metadata records is settled first ([`record::settle`]), unless the claim lost its
/// first swap or settled one already: that claim started since. A claim of `name` that finds the
/// mapping holding another value fails with [`AlreadyExists`] naming `name`, and the first swap is
/// set back. A final swap that finds the metadata changed, as a settle leaves it, releases the
/// claim, and fails with [`AlreadyExists`] naming the scope.
///
/// A name that isn't valid fails with [`InvalidArguments`], the default scope is refused with
/// [`Refused::Default`], and a scope without metadata fails with [`NotFound`].
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn claim_name(
    repository: Arc<RepositoryContext>,
    id: ScopeId,
    name: &str,
    kind: Claim,
) -> impl Future<Output = Result<Result<Scope, Refused>, ScopeError>> {
    async move {
        check_name(name)?;
        if id == default_scope_id(repository.id) {
            return Ok(Err(Refused::Default));
        }
        let mut yields = false;
        loop {
            let (read, mut metadata) = read(repository.clone(), id).await?;
            #[cfg(feature = "test-util")]
            crate::test_util::branch_step(crate::test_util::BranchStep::RestoreMetadataRead).await;
            if let Some(pending) = restoring(&metadata) {
                if yields {
                    lore_debug!("A claim of {pending} by scope {id} recorded itself first");
                    return Err(exists(id));
                }
                let pending = pending.to_string();
                record::settle::<Scopes>(repository.clone(), id, read, true, metadata, &pending)
                    .await?;
                yields = true;
                continue;
            }
            let held = metadata.get_string(NAME).unwrap_or_default().to_string();
            match (kind, is_archived(&metadata)) {
                (_, false) if held == name => return Ok(Ok(scope_of(id, &metadata))),
                (Claim::Restore, false) => {
                    lore_debug!("Scope {id} is live under another name than {name}");
                    return Err(exists(id));
                }
                (Claim::Rename, true) => return Ok(Err(Refused::Archived)),
                (Claim::Rename, false) if fold_eq(&held, name) => {
                    if let Some(renamed) =
                        rename_case(repository.clone(), id, read, metadata, name).await?
                    {
                        return Ok(Ok(renamed));
                    }
                    continue;
                }
                _ => {}
            }
            release_marked(repository.clone(), id, &metadata).await?;
            let held_generation = Scopes::generation(&metadata, Claimed::Name);
            if kind == Claim::Restore && !fold_eq(&held, name) {
                record::release_held::<Scopes>(repository.clone(), id, &held, held_generation)
                    .await?;
            }
            let expected = claim_expected(repository.clone(), name, id).await?;
            #[cfg(feature = "test-util")]
            crate::test_util::branch_step(crate::test_util::BranchStep::RestoreChecked).await;

            metadata.remove_key(RELEASED);
            Scopes::set_generation(&mut metadata, Claimed::Released, None)?;
            let generation = next_generation(&mut metadata)?;
            metadata
                .set_string(RESTORING, name)
                .forward::<ScopeError>("recording the claim in scope metadata")?;
            Scopes::set_generation(&mut metadata, Claimed::Restoring, Some(generation))?;
            let Some(recorded) = swap(repository.clone(), id, read, &metadata).await? else {
                yields = true;
                continue;
            };
            #[cfg(feature = "test-util")]
            crate::test_util::branch_step(crate::test_util::BranchStep::RestoreRecorded).await;

            if let Err(err) = claim(
                repository.clone(),
                name,
                expected,
                Scopes::mapping(id, generation),
            )
            .await
            {
                record::revert_metadata::<Scopes>(repository, id, recorded, read).await;
                return Err(err);
            }
            #[cfg(feature = "test-util")]
            crate::test_util::branch_step(crate::test_util::BranchStep::RestoreNameClaimed).await;

            let claims = Claims {
                held_generation,
                generation,
                kind,
            };
            return finish_claim(repository, id, name, &held, claims, recorded, metadata)
                .await
                .map(Ok);
        }
    }
}

/// The claims a [`finish_claim`] finishes: the generation of the claim of the name the scope held,
/// that of the claim of the new name, and the kind of write.
#[derive(Clone, Copy)]
struct Claims {
    held_generation: u64,
    generation: u64,
    kind: Claim,
}

/// Finishes the claim of `name` by scope `id` that recorded itself as `recorded`, holding
/// `metadata`, and claimed the name, as [`claim_name`] describes: a swap from the record to
/// `metadata` without [`RESTORING`], named `name` under the claim's generation, with [`RESTORED`]
/// stamped for a restore, and for a rename `held`, the name the scope held, marked [`RELEASED`]
/// with its claim's generation and then released. A final swap that finds the metadata changed
/// releases the claim, and fails with [`AlreadyExists`] naming the scope. A release swaps from the
/// claim's own generation, so it leaves any later claim of the name by the scope.
async fn finish_claim(
    repository: Arc<RepositoryContext>,
    id: ScopeId,
    name: &str,
    held: &str,
    claims: Claims,
    recorded: Hash,
    mut metadata: Metadata,
) -> Result<Scope, ScopeError> {
    metadata.remove_key(RESTORING);
    Scopes::set_generation(&mut metadata, Claimed::Restoring, None)?;
    metadata
        .set_string(NAME, name)
        .forward::<ScopeError>("naming the scope in its metadata")?;
    Scopes::set_generation(&mut metadata, Claimed::Name, Some(claims.generation))?;
    match claims.kind {
        Claim::Restore => record::stamp_time::<ScopeError>(&mut metadata, RESTORED)?,
        Claim::Rename => {
            metadata
                .set_string(RELEASED, held)
                .forward::<ScopeError>("marking the scope's old name released")?;
            Scopes::set_generation(
                &mut metadata,
                Claimed::Released,
                Some(claims.held_generation),
            )?;
        }
    }
    if swap(repository.clone(), id, recorded, &metadata)
        .await?
        .is_none()
    {
        if let Err(err) =
            record::release_held::<Scopes>(repository, id, name, claims.generation).await
        {
            lore_warn!(
                "Failed to release the name {name} of scope {id}, whose claim didn't finish: {err}"
            );
        }
        return Err(exists(id));
    }
    if claims.kind == Claim::Rename {
        #[cfg(feature = "test-util")]
        crate::test_util::branch_step(crate::test_util::BranchStep::RenameReleasing).await;
        record::release_held::<Scopes>(repository, id, held, claims.held_generation).await?;
    }
    Ok(scope_of(id, &metadata))
}

/// Renames scope `id`, whose metadata key holds `read` as `metadata`, to `name`, which differs
/// from the name it holds only in case, with a swap of the metadata alone: the name's mapping key
/// is the same. Answers `None` where the key holds another value.
async fn rename_case(
    repository: Arc<RepositoryContext>,
    id: ScopeId,
    read: Hash,
    mut metadata: Metadata,
    name: &str,
) -> Result<Option<Scope>, ScopeError> {
    metadata
        .set_string(NAME, name)
        .forward::<ScopeError>("naming the scope in its metadata")?;
    Ok(swap(repository, id, read, &metadata)
        .await?
        .map(|_| scope_of(id, &metadata)))
}
