// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The guard must hold under UFCS.
//!
//! E0034's own help text suggests disambiguating with UFCS, so the poisoned
//! methods carry an unsatisfiable `Self: guard::Forbidden` bound that makes
//! them uncallable by any spelling. (The other arm of that suggestion, the
//! one naming `WrapInternal` instead, does still compile — nothing in the
//! type system can stop it, which is why the `no-wrapinternal-ufcs`
//! pre-commit hook greps for it.)
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

fn ufcs_cannot_reach_it() {
    let result: Result<(), SetA> = Err(Oops.into());
    let _ = InternalForbiddenOnErrorSets::internal(result, "via UFCS");
}

fn main() {}
