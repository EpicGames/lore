// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Runner for compile-fail tests using trybuild.
//!
//! These tests verify that incorrect usage of the error set API produces
//! helpful compile errors.
//!
//! # These snapshots are pinned to CI's exact rustc
//!
//! The `.stderr` files capture rustc diagnostics verbatim, so they break on any
//! toolchain whose output differs — and both axes bite:
//!
//! - **Channel.** Where a trait is reachable by more than one public path,
//!   which one rustc names differs between stable and nightly:
//!   `lore_error_set::WrapInternal` on one, `lore_error_set::prelude::WrapInternal`
//!   on the other.
//! - **Version.** Wording changes between stable releases. rustc 1.95 emits
//!   `help: the following other types implement trait `Has<V>``; later stables
//!   emit `help: `NarrowTarget` implements trait `Has<V>``.
//!
//! CI installs the *latest* stable (`dtolnay/rust-toolchain@stable`), so a local
//! stable that has not been updated recently will disagree with it, and a
//! snapshot blessed against nightly or an older stable passes locally and fails
//! in CI.
//!
//! Regenerate with an up-to-date stable:
//!
//! ```sh
//! rustup update stable
//! TRYBUILD=overwrite cargo +stable test -p lore-error-set --test compile_fail
//! ```
//!
//! If a mismatch is only the diagnostic's wording and the assertion still holds,
//! CI's "ACTUAL OUTPUT" block is authoritative — it is the exact text of the
//! toolchain that gates the build.

// Ignored: brittle. The snapshots capture rustc diagnostics verbatim and
// nothing pins a toolchain, so the text differs whenever CI and a developer
// machine resolve different rustc versions. Cached or AMI-baked runner
// toolchains are a suspected contributor, the install steps reusing an existing
// install rather than forcing a version, but CI's version is unconfirmed.
// Re-enable once the toolchain is pinned.
#[test]
#[ignore = "brittle under rustc version differences, possibly cached runner toolchains"]
fn compile_fail_tests() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/*.rs");
}
