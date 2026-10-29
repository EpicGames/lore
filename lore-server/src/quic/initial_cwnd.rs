// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use serde::Deserialize;

/// Initial congestion window for a QUIC connection to or from another Lore server. The path
/// between peers is known to carry far more than the conservative default window.
pub const DEFAULT_PEER_INITIAL_CWND_BYTES: InitialCwndBytes = InitialCwndBytes(1024 * 1024);

/// Bytes a QUIC sender may have in flight before its congestion controller has measured the
/// path. Never less than two minimum-size QUIC datagrams: Cubic never lets its window fall below
/// that once running, but adopts the initial window unchecked.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(try_from = "u64")]
pub struct InitialCwndBytes(u64);

impl InitialCwndBytes {
    pub const MIN: u64 = 2 * 1200;

    pub fn get(self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for InitialCwndBytes {
    type Error = String;

    fn try_from(bytes: u64) -> Result<Self, Self::Error> {
        if bytes < Self::MIN {
            return Err(format!(
                "initial_cwnd_bytes must be at least {}, got {bytes}",
                Self::MIN
            ));
        }
        Ok(Self(bytes))
    }
}
