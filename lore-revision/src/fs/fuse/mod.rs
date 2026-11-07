// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! FUSE-backed filesystem provider: every request is answered from one filesystem `State`
//! ([`served::Served`]), the mounted revision with its local changes applied, which a sync
//! changes in place.

use lore_error_set::prelude::*;

use crate::errors::Disconnected;
use crate::errors::NotSupported;

pub mod access_recorder;
pub mod attr;
#[cfg(not(feature = "test-util"))]
mod clock;
#[cfg(feature = "test-util")]
pub mod clock;
pub mod deletions;
pub mod discover;
pub mod filesystem;
pub mod fragment_cache;
pub mod inode;
pub mod inode_number;
pub mod listing;
pub mod listing_cache;
#[cfg(not(feature = "test-util"))]
mod local_tree;
#[cfg(feature = "test-util")]
pub mod local_tree;
pub mod mount;
pub mod mount_manager;
pub mod notifications;
pub mod operation;
pub mod prefetch;
pub mod read;
pub mod served;
pub mod spelling;
pub mod swap;
pub mod view;
#[cfg(not(feature = "test-util"))]
mod write;
#[cfg(feature = "test-util")]
pub mod write;

#[error_set]
pub enum FuseProviderError {
    NotSupported,
    Disconnected,
}
