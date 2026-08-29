// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `.internal()` on an error set must not compile.
//!
//! Without the guard this silently collapses every handleable variant into
//! `Internal` and reports FFI -1. `WrapInternal` applies here (every
//! `#[error_set]` enum implements `std::error::Error`), so the guard shows up
//! as an E0034 ambiguity naming `InternalForbiddenOnErrorSets`.
use std::error::Error;
use std::fmt;

use lore_error_set::prelude::*;

#[derive(Debug)]
struct Oops;

impl fmt::Display for Oops {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "oops")
    }
}

impl Error for Oops {}

impl FfiError for Oops {
    fn ffi_code(&self) -> i32 {
        1
    }
}

#[error_set]
pub enum SetA {
    Oops,
}

fn collapses_an_error_set() -> Result<(), Traced<Internal>> {
    let result: Result<(), SetA> = Err(Oops.into());
    // Should be `.forward("...")`, which preserves the variant.
    result.internal("collapsing an error set")
}

fn main() {}
