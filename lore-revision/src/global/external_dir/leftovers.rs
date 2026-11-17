// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The leftover list: `external.leftovers` in the global data directory, which names what is
//! left of old external roots to remove.

use std::path::Path;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use super::ExternalDirError;
use crate::util::config;

/// The leftover list's file, in the global data directory.
pub const LEFTOVERS: &str = "external.leftovers";

/// The contents of `external.leftovers`. A leftover in the global data directory is named by its
/// name alone, relative to that directory.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Leftovers {
    /// Leftovers whose marker is gone. One whose path is absent counts as removed.
    pub paths: Vec<String>,
    /// Leftovers listed while they couldn't be reached, which keep their marker and stay listed
    /// while their path is absent.
    pub marked: Vec<String>,
}

impl Leftovers {
    /// Reads `external.leftovers` in the global data directory `global`, which is empty where the
    /// file is absent.
    pub fn load(global: &Path) -> Result<Leftovers, ExternalDirError> {
        config::load_blocking(global.join(LEFTOVERS))
            .forward::<ExternalDirError>("reading the external root leftovers")
    }

    /// Replaces `external.leftovers` in the global data directory `global` with this list, durably.
    pub async fn save(&self, global: &Path) -> Result<(), ExternalDirError> {
        config::save_strict(self, global.join(LEFTOVERS))
            .await
            .forward::<ExternalDirError>("writing the external root leftovers")
    }
}
