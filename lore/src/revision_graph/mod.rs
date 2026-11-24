// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Low-level revision graph API.
//!
//! The `lore_revision_graph_*` namespace reaches a repository's branches and revisions through a
//! storage handle and a repository id, with no repository instance and no handle of its own.
//! [`RevisionGraphError`] is the surface's error set, and [`LoreRevisionIdentifier`] is how a
//! caller names a revision.

pub mod branch_archive;
pub mod branch_create;
pub mod branch_list;
pub mod branch_metadata_get;
pub mod branch_metadata_set;
pub mod branch_push;
pub mod branch_resolve;
pub mod branch_sync;
#[cfg(not(feature = "test-util"))]
pub(crate) mod call;
#[cfg(feature = "test-util")]
pub mod call;
#[cfg(not(feature = "test-util"))]
pub(crate) mod locations;
#[cfg(feature = "test-util")]
pub mod locations;
#[cfg(not(feature = "test-util"))]
pub(crate) mod record;
#[cfg(feature = "test-util")]
pub mod record;
pub mod revision_resolve;

use std::convert::Infallible;

use lore_base::error::AddressNotFound;
use lore_base::error::BranchAdvanced;
use lore_base::error::BranchAlreadyExists;
use lore_base::error::BranchNotFound;
use lore_base::error::DeleteCurrent;
use lore_base::error::DeleteDefault;
use lore_base::error::DeleteProtected;
use lore_base::error::Disconnected;
use lore_base::error::Divergent;
use lore_base::error::InvalidArguments;
use lore_base::error::Maintenance;
use lore_base::error::NoRemote;
use lore_base::error::NotAuthenticated;
use lore_base::error::NotAuthorized;
use lore_base::error::NotConnected;
use lore_base::error::Oversized;
use lore_base::error::RevisionNotFound;
use lore_base::error::SlowDown;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_error_set::prelude::*;

/// The failures a revision graph verb reports.
#[error_set]
pub enum RevisionGraphError {
    InvalidArguments,
    NotAuthenticated,
    NotAuthorized,
    Disconnected,
    NotConnected,
    Maintenance,
    SlowDown,
    BranchAdvanced,
    Divergent,
    DeleteProtected,
    DeleteCurrent,
    DeleteDefault,
    BranchAlreadyExists,
    AddressNotFound,
    BranchNotFound,
    RevisionNotFound,
    NoRemote,
    Oversized,
}

impl RevisionGraphError {
    /// A rejection the arguments earned, alongside the generated `internal` constructor for a
    /// failure of ours.
    pub(crate) fn invalid(reason: impl Into<String>) -> Self {
        Self::from(InvalidArguments {
            reason: reason.into(),
        })
    }

    /// The failure that reports `err`, a refusal of a write at the remote no confirming read
    /// attributed: a refusal of the caller, an overloaded remote, a lost connection, a remote that
    /// can't be reached and a remote in maintenance keep their codes, and any other refusal is
    /// `Internal` with the remote's message, under `context`.
    pub(crate) fn remote_refusal<E: ErrorSet>(err: E, context: &'static str) -> Self {
        let Err(forwarded) = Err::<Infallible, _>(err).forward_any::<Self>(context);
        if forwarded.keeps_its_code_at_the_remote() || forwarded.is_internal() {
            return forwarded;
        }
        Self::internal_with_context(forwarded, context)
    }

    /// Whether this failure keeps its code wherever the remote answers it: a refusal of the
    /// caller, an overloaded remote, a lost connection, a remote that can't be reached and a remote
    /// in maintenance.
    pub(crate) fn keeps_its_code_at_the_remote(&self) -> bool {
        self.is_not_authorized()
            || self.is_not_authenticated()
            || self.is_slow_down()
            || self.is_disconnected()
            || self.is_not_connected()
            || self.is_maintenance()
    }
}

/// A revision identifier in its structured form: the revision with `hash`, or revision
/// `revision_number` on `branch`, where 0 names the branch's latest. A string identifier such as
/// `main@5` names its branch by name; `lore_revision_graph_branch_resolve` finds the branch's id.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema,
)]
pub struct LoreRevisionIdentifier {
    /// Non-zero: the revision with this hash. `branch` and `revision_number` must then be zero.
    pub hash: Hash,
    /// With a zero `hash`: the branch whose revision this names.
    pub branch: BranchId,
    /// With a zero `hash`: the revision number on `branch`; 0 names the branch's latest.
    pub revision_number: u64,
}

lore_base::carries_no_text!(LoreRevisionIdentifier);

/// The revision a [`LoreRevisionIdentifier`] names, as [`LoreRevisionIdentifier::form`] reads it.
#[lore_macro::test_pub]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum RevisionForm {
    /// The revision with this hash.
    Hash(Hash),
    /// The latest revision of the branch.
    Latest(BranchId),
    /// The revision with this number on the branch.
    Number(BranchId, u64),
}

impl LoreRevisionIdentifier {
    /// What this identifier names. A hash with a branch or a number, a number with neither a hash
    /// nor a branch, and an all-zero identifier are `InvalidArguments`.
    #[lore_macro::test_pub]
    pub(crate) fn form(&self) -> Result<RevisionForm, RevisionGraphError> {
        let malformed = |names: &str| {
            RevisionGraphError::invalid(format!("the revision identifier names {names}"))
        };
        match (
            !self.hash.is_zero(),
            !self.branch.is_zero(),
            self.revision_number != 0,
        ) {
            (true, false, false) => Ok(RevisionForm::Hash(self.hash)),
            (false, true, false) => Ok(RevisionForm::Latest(self.branch)),
            (false, true, true) => Ok(RevisionForm::Number(self.branch, self.revision_number)),
            (true, true, _) => Err(malformed("both a hash and a branch")),
            (true, false, true) => Err(malformed("a revision number with a hash")),
            (false, false, true) => Err(malformed("a revision number without a branch")),
            (false, false, false) => Err(malformed("no revision")),
        }
    }
}
