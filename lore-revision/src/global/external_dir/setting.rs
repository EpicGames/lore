// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The setting: `external-root` in the global data directory, which names the external root
//! together with the root ID and generation.

use std::num::NonZeroU64;
use std::path::Path;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use super::ExternalDirError;
use crate::util::config;

/// The setting's file, in the global data directory.
pub const EXTERNAL_ROOT: &str = "external-root";

/// The random value that identifies a global data directory's external roots, written as 32
/// lower-case hex digits. Held as 16 bytes, with an alignment of 1, so a future or a record that
/// holds one is aligned as it would be without it, where a `u128` would align it to 16.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RootId([u8; 16]);

impl RootId {
    /// A new random root ID.
    pub fn generate() -> RootId {
        RootId(uuid::Uuid::new_v4().into_bytes())
    }
}

impl std::fmt::Display for RootId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:032x}", u128::from_be_bytes(self.0))
    }
}

impl std::str::FromStr for RootId {
    type Err = String;

    /// Parses 32 lower-case hex digits, and nothing else.
    fn from_str(text: &str) -> Result<RootId, String> {
        if text.len() != 32
            || !text
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(format!("{text:?} is not 32 lower-case hex digits"));
        }
        u128::from_str_radix(text, 16)
            .map(|id| RootId(id.to_be_bytes()))
            .map_err(|err| err.to_string())
    }
}

impl Serialize for RootId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for RootId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<RootId, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// The contents of `external-root`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setting {
    /// The external root's absolute path, or empty for the default location.
    pub path: String,
    /// The root ID.
    pub id: RootId,
    /// The root generation, which starts at 1.
    pub generation: NonZeroU64,
}

impl Setting {
    /// Reads `external-root` in the global data directory `global`, or `None` where it is absent.
    pub fn load(global: &Path) -> Result<Option<Setting>, ExternalDirError> {
        config::load_blocking(global.join(EXTERNAL_ROOT))
            .forward::<ExternalDirError>("reading the external root setting")
    }

    /// Replaces `external-root` in the global data directory `global` with this setting, durably.
    pub async fn save(&self, global: &Path) -> Result<(), ExternalDirError> {
        config::save_strict(self, global.join(EXTERNAL_ROOT))
            .await
            .forward::<ExternalDirError>("writing the external root setting")
    }
}
