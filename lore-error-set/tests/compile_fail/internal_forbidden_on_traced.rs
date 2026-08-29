// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `.internal()` on a `Traced<_>` must not compile.
//!
//! `WrapInternal` never applied here, because `Traced` has no `Error` impl, so
//! this was already an error before the guard — but the message was a
//! confusing "`std::error::Error` is not implemented for `Traced<Internal>`".
//! The guard resolves uniquely instead and reports `guard::Forbidden`'s
//! `on_unimplemented` text, which names the right alternative.
use lore_error_set::prelude::*;

fn double_wraps_a_traced() -> Result<(), Traced<Internal>> {
    let result: Result<(), Traced<Internal>> = Err(Internal::msg("boom").into());
    // Should be `.chain_err(..)` / `.chain_err_from(..)`.
    result.internal("re-wrapping an already-traced error")
}

fn main() {}
