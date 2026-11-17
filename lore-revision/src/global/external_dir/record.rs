// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The move record: `external.move` in the global data directory, present while a move of the
//! external root is under way or interrupted.

use std::path::Path;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use super::ExternalDirError;
use crate::util::config;

/// The move record's file, in the global data directory.
pub const MOVE_RECORD: &str = "external.move";

/// What a move does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MoveKind {
    /// Renames the root within its volume.
    Rename,
    /// Copies the root to another volume.
    Copy,
    /// Creates an empty root where no default root exists.
    Create,
    /// Points the setting at a root that holds the current marker.
    Adopt,
    /// Gives up an unavailable root for a new, empty one of the next generation.
    Abandon,
}

/// The step a move is about to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MovePhase {
    /// Writes the marker into the current root, on a first move.
    Mark,
    /// Stores the root ID and generation, on a first move.
    Identify,
    /// Copies the root into a temporary directory beside the target.
    Copy,
    /// Places the new root at the target.
    Place,
    /// Switches the setting to the new root.
    Switch,
    /// Removes the old root's marker, after a copy.
    Unmark,
    /// Renames what is left of the old root aside, after a copy.
    SetAside,
    /// Makes the redirect at the default location match the setting.
    Redirect,
}

/// The contents of `external.move`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveRecord {
    /// The root the move leaves, empty for the default location.
    pub from: String,
    /// The root the move goes to, empty for the default location.
    pub to: String,
    /// What the move does.
    pub kind: MoveKind,
    /// The step the move is about to run.
    pub phase: MovePhase,
}

impl MoveRecord {
    /// Reads `external.move` in the global data directory `global`, or `None` where it is absent.
    pub fn load(global: &Path) -> Result<Option<MoveRecord>, ExternalDirError> {
        config::load_blocking(global.join(MOVE_RECORD))
            .forward::<ExternalDirError>("reading the external root move record")
    }

    /// Replaces `external.move` in the global data directory `global` with this record, durably.
    pub async fn save(&self, global: &Path) -> Result<(), ExternalDirError> {
        config::save_strict(self, global.join(MOVE_RECORD))
            .await
            .forward::<ExternalDirError>("writing the external root move record")
    }
}
