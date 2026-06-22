// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_error_set::prelude::*;

use crate::event::EventError;
use crate::interface::LoreError;

#[error_set]
pub enum UnhandledError {
    InvalidArguments,
}

impl EventError for UnhandledError {
    fn translated(&self) -> LoreError {
        match self {
            Self::InvalidArguments(_) => LoreError::InvalidArguments,
            Self::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

// Re-export all FFI error types from lore-error (the single source of truth)
pub(crate) use lore_base::error::AddressNotFound;
pub(crate) use lore_base::error::AlreadyLinked;
pub(crate) use lore_base::error::BranchAdvanced;
pub(crate) use lore_base::error::BranchAlreadyExists;
pub(crate) use lore_base::error::BranchNotFound;
pub(crate) use lore_base::error::Conflict;
pub(crate) use lore_base::error::DeleteCurrent;
pub(crate) use lore_base::error::DeleteDefault;
pub(crate) use lore_base::error::DeleteProtected;
pub(crate) use lore_base::error::Disconnected;
pub(crate) use lore_base::error::Divergent;
pub(crate) use lore_base::error::FileNotFound;
pub(crate) use lore_base::error::IdenticalMetadata;
pub(crate) use lore_base::error::InvalidAddress;
pub(crate) use lore_base::error::InvalidArguments;
pub(crate) use lore_base::error::InvalidNodeHierarchy;
pub(crate) use lore_base::error::InvalidPath;
pub(crate) use lore_base::error::LayerNotFound;
pub(crate) use lore_base::error::LinkNotFound;
pub(crate) use lore_base::error::LinkPathNotFound;
pub(crate) use lore_base::error::LocalModifications;
pub(crate) use lore_base::error::LockNotFound;
pub(crate) use lore_base::error::LockNotOwned;
pub(crate) use lore_base::error::Maintenance;
pub(crate) use lore_base::error::MaxHistorySearchDepth;
pub(crate) use lore_base::error::MissingIdentity;
pub(crate) use lore_base::error::NoRemote;
pub(crate) use lore_base::error::NodeNotFound;
pub(crate) use lore_base::error::NotALayer;
pub(crate) use lore_base::error::NotALink;
pub(crate) use lore_base::error::NotAuthenticated;
pub(crate) use lore_base::error::NotAuthorized;
pub(crate) use lore_base::error::NotConnected;
pub(crate) use lore_base::error::NotFound;
pub(crate) use lore_base::error::NotSupported;
pub(crate) use lore_base::error::NothingStaged;
pub(crate) use lore_base::error::Oversized;
pub(crate) use lore_base::error::PayloadNotFound;
pub(crate) use lore_base::error::RepositoryAlreadyExists;
pub(crate) use lore_base::error::RepositoryNotFound;
pub(crate) use lore_base::error::RevisionNotFound;
pub(crate) use lore_base::error::SharedStoreNotFound;
pub(crate) use lore_base::error::SlowDown;
pub(crate) use lore_base::error::TokenNotFound;
pub(crate) use lore_base::error::WriteRequired;

#[error_set]
pub enum StateErrors {
    NodeNotFound,
    LinkNotFound,
    NotFound,
    RevisionNotFound,
    WriteRequired,
    Oversized,
    InvalidArguments,
    InvalidPath,
    InvalidNodeHierarchy,
    AddressNotFound,
    Disconnected,
    Maintenance,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NotSupported,
    PayloadNotFound,
    SlowDown,
    AlreadyLinked,
    BranchAdvanced,
    BranchAlreadyExists,
    BranchNotFound,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    FileNotFound,
    IdenticalMetadata,
    LayerNotFound,
    LinkPathNotFound,
    LocalModifications,
    LockNotFound,
    LockNotOwned,
    MaxHistorySearchDepth,
    NotALayer,
    NotALink,
    NothingStaged,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    MissingIdentity,
}

impl EventError for StateErrors {
    fn translated(&self) -> LoreError {
        LoreError::Internal
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}
