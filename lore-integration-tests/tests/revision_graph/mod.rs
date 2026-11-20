// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Integration tests for the revision graph API, run against an in-process Lore Server.

mod archive;
mod concurrency;
mod counting;
mod create;
mod isolation;
mod list;
mod metadata;
mod perf;
mod push;
mod push_core;
mod push_refusals;
mod read_costs;
mod resolve;
mod session_reuse;
mod support;
mod sync;
mod write_costs;
