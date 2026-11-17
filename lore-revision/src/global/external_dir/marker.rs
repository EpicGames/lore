// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The marker: `.lore-external-root` in an external root, which names the root ID and generation
//! the root belongs to.

use std::num::NonZeroU64;
use std::path::Path;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use super::ExternalDirError;
use super::setting::RootId;
use crate::util::config;

/// The marker's file, in an external root.
pub const MARKER: &str = ".lore-external-root";

/// The contents of the marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    /// The root ID of the global data directory the root belongs to.
    pub id: RootId,
    /// The root generation the root belongs to.
    pub generation: NonZeroU64,
}

impl Marker {
    /// Reads the marker in the external root `root`, or `None` where it is absent.
    pub fn load(root: &Path) -> Result<Option<Marker>, ExternalDirError> {
        config::load_blocking(root.join(MARKER))
            .forward::<ExternalDirError>("reading the external root marker")
    }

    /// Replaces the marker in the external root `root` with this one, durably.
    pub async fn save(&self, root: &Path) -> Result<(), ExternalDirError> {
        config::save_strict(self, root.join(MARKER))
            .await
            .forward::<ExternalDirError>("writing the external root marker")
    }
}
