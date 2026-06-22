// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fmt::Display;
use std::fmt::Formatter;

use serde::Deserialize;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq)]
pub enum Locality {
    SameRegion,
    OtherRegion,
}

impl Locality {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SameRegion => "SameRegion",
            Self::OtherRegion => "OtherRegion",
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PeerInfo {
    pub id: String,
    pub address: String,
    pub port: u16,
    pub locality: Locality,
    /// ID of this peer that is safe for metrics
    pub metric_id: String,
}

impl Display for Locality {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let value = self.as_str();
        write!(f, "{value}")
    }
}

impl Display for PeerInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({}) {}:{}",
            self.id, self.locality, self.address, self.port
        )
    }
}
