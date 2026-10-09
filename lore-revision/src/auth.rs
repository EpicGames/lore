// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use serde::Serialize;

use crate::interface::LoreString;

pub mod login;
pub mod mode;
pub mod userinfo;

pub use mode::AuthMode;
pub use mode::AuthPath;
pub use mode::UnknownAuthMode;

/////////////////////////////////
// General Notes for Auth token handling

// Check Token Recipient:
// An attacker sets up a URC repository on their own server, where their server environment info
// has the epic-controlled URC Auth service as the auth provider.
// An end user goes to clone the repository and the CLI dutifully uses the auth provider
// it is told to get an AuthN token (or loads a token from cache), and then subseqently sends it on to the attacker's server.
// Tokens should only be given to domains listed in the token's audience field

/////////////////////////////////

/// Event data carrying an authentication URL for the user to open.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreAuthUrlEventData {
    /// Authentication URL
    pub url: LoreString,
}

/// Event data for one wait in an interactive login: the user has not
/// approved it yet, and the client is about to wait `interval_secs` before
/// asking again. Emitted once per poll, so a consumer can show that the
/// login is still in progress against a provider with a long interval.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreAuthPendingEventData {
    /// Whole seconds since polling began.
    pub elapsed_secs: u64,
    /// Whole seconds until the next poll.
    pub interval_secs: u64,
    /// Whole seconds left before the session expires unapproved.
    pub remaining_secs: u64,
}
