// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The crate's tests that set process environment variables, as a binary of their own.
//!
//! The environment is process-wide and `#[serial]` orders only the tests that carry it, so in a
//! shared binary a test reading a variable could see another test's value. Every test here sets
//! variables and is serial with the others; the unit binary sets none.

mod client;
mod creds;
mod http;
mod imds;
mod sts;
mod telemetry;
#[path = "../unit/aws/test_server.rs"]
#[allow(
    dead_code,
    reason = "the unit binary's stub service, of which this binary uses a part"
)]
mod test_server;
