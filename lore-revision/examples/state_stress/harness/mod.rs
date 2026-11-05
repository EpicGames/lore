// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The harness of `State`'s node operations (`lore_revision::state::node`): a deterministic
//! scheduler that steps one task per operation through the operations' scheduling points, a chaos
//! scheduler that yields at random ones on many threads, a model that replays what each critical
//! section logged, a checker of the contract's invariants, and the catalogue of operations and the
//! mixes of them both drive.
//!
//! Everything here drives lore-revision's `state-hooks`, so it builds only with the `state-stress`
//! feature: this example, and the deterministic scenarios, lore-revision's `tests/state_scenarios`
//! and the C API's in the `lore` package, which include this file by its path. A module included
//! by its path resolves the modules it declares from its own directory, hence a `mod.rs` here.

pub mod catalogue;
pub mod chaos;
pub mod faults;
pub mod fixtures;
pub mod invariants;
pub mod log;
pub mod mixes;
pub mod model;
pub mod scheduler;
