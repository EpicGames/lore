// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Integration tests for FFI error code delegation.
//!
//! Covers spec acceptance tests:
//! - #20: FFI code accessible on discrete type
//! - #21: FFI code inherited by error set

use std::error::Error;
use std::fmt;

use lore_error_set::error_set;
use lore_error_set::FfiError;

/// Opts this test module in to `#[ffi_code(N)]`.
///
/// The derive expands to a reference to this name, so only a module that
/// declares it can allocate a code — in the workspace proper that is
/// `lore-base/src/error.rs` and nowhere else. These fixtures exercise the
/// macro itself and their codes never reach an FFI boundary, so they declare
/// their own marker rather than joining the registry.
fn __ffi_code_registry_marker() {}

// ---------------------------------------------------------------------------
// Discrete error types with FfiError derive
// ---------------------------------------------------------------------------

#[derive(Debug, FfiError)]
#[ffi_code(1)]
pub struct NotFound;

impl fmt::Display for NotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not found")
    }
}

impl Error for NotFound {}

#[derive(Debug, FfiError)]
#[ffi_code(2)]
pub struct PermissionDenied;

impl fmt::Display for PermissionDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "permission denied")
    }
}

impl Error for PermissionDenied {}

#[derive(Debug, FfiError)]
#[ffi_code(3)]
pub struct Conflict;

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "conflict")
    }
}

impl Error for Conflict {}

// ---------------------------------------------------------------------------
// Error set
// ---------------------------------------------------------------------------

#[error_set]
pub enum FfiSet {
    NotFound,
    PermissionDenied,
    Conflict,
}

// ---------------------------------------------------------------------------
// Acceptance test #20: FFI code accessible on discrete type
// ---------------------------------------------------------------------------

#[test]
fn ffi_code_on_discrete_type() {
    let err = NotFound;
    assert_eq!(err.ffi_code(), 1);

    let err = PermissionDenied;
    assert_eq!(err.ffi_code(), 2);

    let err = Conflict;
    assert_eq!(err.ffi_code(), 3);
}

// ---------------------------------------------------------------------------
// Acceptance test #21: FFI code inherited by error set
// ---------------------------------------------------------------------------

#[test]
fn ffi_code_inherited_by_error_set() {
    let err: FfiSet = NotFound.into();
    assert_eq!(err.ffi_code(), 1);

    let err: FfiSet = PermissionDenied.into();
    assert_eq!(err.ffi_code(), 2);

    let err: FfiSet = Conflict.into();
    assert_eq!(err.ffi_code(), 3);
}

#[test]
fn ffi_code_for_internal_variant() {
    use lore_error_set::ErrorSet;
    use lore_error_set::Internal;

    let io_err = std::io::Error::other("boom");
    let traced_box = lore_error_set::TracedBox::new(Box::new(io_err), lore_error_set::Trace::new());
    let err = FfiSet::wrap_internal(traced_box, "test");

    assert_eq!(err.ffi_code(), Internal::FFI_CODE);
    assert_eq!(err.ffi_code(), -1);
}

// ---------------------------------------------------------------------------
// FFI codes are distinct per variant
// ---------------------------------------------------------------------------

#[test]
fn ffi_codes_are_distinct() {
    let nf: FfiSet = NotFound.into();
    let pd: FfiSet = PermissionDenied.into();
    let cf: FfiSet = Conflict.into();

    assert_ne!(nf.ffi_code(), pd.ffi_code());
    assert_ne!(pd.ffi_code(), cf.ffi_code());
    assert_ne!(nf.ffi_code(), cf.ffi_code());
}
