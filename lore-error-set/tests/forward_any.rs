// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Integration tests for the lenient `forward_any`.
//!
//! `forward_any` drops the `Target: HasAll<Source::Variants>` bound that makes
//! `forward` strict, so it compiles where the source declares variants the
//! target does not. Runtime behaviour is unchanged: variants the target
//! declares are preserved, and only unmatched ones collapse to
//! `Target::Internal`.
//!
//! It exists for sources that over-declare — where satisfying the strict bound
//! would mean widening the target with variants the call site cannot reach.
//! `tests/forward_strict.rs` covers the strict form, which remains the default.

use std::error::Error;
use std::fmt;

use lore_error_set::prelude::*;

// ---------------------------------------------------------------------------
// Discrete error types
// ---------------------------------------------------------------------------

macro_rules! discrete {
    ($name:ident, $code:expr, $msg:expr) => {
        #[derive(Debug, Clone, PartialEq)]
        pub struct $name;

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str($msg)
            }
        }

        impl Error for $name {}

        impl FfiError for $name {
            fn ffi_code(&self) -> i32 {
                $code
            }
        }
    };
}

discrete!(NotFound, 10, "not found");
discrete!(Disconnected, 11, "disconnected");
discrete!(DeleteDefault, 12, "cannot delete the default branch");

// ---------------------------------------------------------------------------
// Wide declares three variants; Narrow declares two of them.
//
// `DeleteDefault` is the variant Narrow does not declare — it stands in for
// the domain errors a broad set carries that a narrow consumer cannot act on.
// ---------------------------------------------------------------------------

#[error_set]
pub enum Wide {
    NotFound,
    Disconnected,
    DeleteDefault,
}

#[error_set]
pub enum Narrow {
    NotFound,
    Disconnected,
}

fn wide_err(which: u8) -> Result<(), Wide> {
    match which {
        0 => Err(NotFound.into()),
        1 => Err(Disconnected.into()),
        _ => Err(DeleteDefault.into()),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn declared_variant_is_preserved() {
    let err = wide_err(0)
        .forward_any::<Narrow>("loading")
        .expect_err("should be an error");
    assert!(err.is_not_found(), "NotFound must survive the hop");
    assert!(!err.is_internal(), "must not collapse a declared variant");
}

#[test]
fn every_declared_variant_is_preserved() {
    let err = wide_err(1)
        .forward_any::<Narrow>("loading")
        .expect_err("should be an error");
    assert!(err.is_disconnected(), "Disconnected must survive the hop");
}

#[test]
fn undeclared_variant_collapses_to_internal() {
    let err = wide_err(2)
        .forward_any::<Narrow>("loading")
        .expect_err("should be an error");
    assert!(
        err.is_internal(),
        "a variant the target does not declare has nowhere to go"
    );
}

#[test]
fn context_lands_on_the_trace() {
    let err = wide_err(0)
        .forward_any::<Narrow>("loading the thing")
        .expect_err("should be an error");
    assert_eq!(
        err.trace().locations().last().and_then(|l| l.context()),
        Some("loading the thing"),
    );
}

#[test]
fn collapsed_variant_still_carries_context() {
    // The unmatched path is the one most likely to drop the trace, so assert
    // it explicitly rather than trusting the matched case to cover it.
    let err = wide_err(2)
        .forward_any::<Narrow>("loading the thing")
        .expect_err("should be an error");
    assert_eq!(
        err.trace().locations().last().and_then(|l| l.context()),
        Some("loading the thing"),
    );
}

#[test]
fn lazy_context_is_only_built_on_error() {
    let mut called = false;
    let ok: Result<(), Narrow> = Ok::<(), Wide>(()).forward_any_with(|| {
        called = true;
        "unused".to_string()
    });
    assert!(ok.is_ok());
    assert!(!called, "closure must not run on the success path");
}

#[test]
fn lazy_context_lands_on_the_trace() {
    let err = wide_err(0)
        .forward_any_with::<Narrow, _>(|| format!("loading {}", "thing"))
        .expect_err("should be an error");
    assert_eq!(
        err.trace().locations().last().and_then(|l| l.context()),
        Some("loading thing"),
    );
}

#[test]
fn success_passes_through() {
    let ok: Result<u32, Narrow> = Ok::<u32, Wide>(7).forward_any("unused");
    assert_eq!(ok.expect("should be Ok"), 7);
}
