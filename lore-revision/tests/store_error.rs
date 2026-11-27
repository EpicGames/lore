// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::error::AddressNotFound;
use lore_base::error::LoreErrorCode;
use lore_base::error::Oversized;
use lore_base::error::PayloadNotFound;
use lore_base::error::SlowDown;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_error_set::FfiError;
use lore_storage::StoreError;

#[test]
fn store_error_exhaustive_match() {
    let errors: Vec<StoreError> = vec![
        StoreError::from(AddressNotFound::from(Address::default())),
        StoreError::from(PayloadNotFound::from(Hash::default())),
        StoreError::from(SlowDown),
        StoreError::from(Oversized {
            context: "test".to_string(),
        }),
        StoreError::internal("test"),
    ];

    for err in errors {
        match err {
            StoreError::AddressNotFound(_)
            | StoreError::PayloadNotFound(_)
            | StoreError::SlowDown(_)
            | StoreError::Oversized(_)
            | StoreError::NotFound(_)
            | StoreError::Disconnected(_)
            | StoreError::NotAuthorized(_)
            | StoreError::NotAuthenticated(_)
            | StoreError::Maintenance(_)
            | StoreError::NoRemote(_)
            | StoreError::NotSupported(_)
            | StoreError::Internal(_) => {}
        }
    }
}

#[test]
fn store_error_implements_std_error() {
    fn assert_std_error(_: &impl std::error::Error) {}

    assert_std_error(&StoreError::from(AddressNotFound::from(Address::default())));
    assert_std_error(&StoreError::from(PayloadNotFound::from(Hash::default())));
    assert_std_error(&StoreError::from(SlowDown));
    assert_std_error(&StoreError::from(Oversized {
        context: "test".to_string(),
    }));
    assert_std_error(&StoreError::internal("test"));
}

#[test]
fn store_error_ffi_codes() {
    assert_eq!(
        StoreError::from(AddressNotFound::from(Address::default())).ffi_code(),
        LoreErrorCode::AddressNotFound as i32
    );
    assert_eq!(
        StoreError::from(PayloadNotFound::from(Hash::default())).ffi_code(),
        LoreErrorCode::PayloadNotFound as i32
    );
    assert_eq!(
        StoreError::from(SlowDown).ffi_code(),
        LoreErrorCode::SlowDown as i32
    );
    assert_eq!(
        StoreError::from(Oversized {
            context: "test".to_string()
        })
        .ffi_code(),
        LoreErrorCode::Oversized as i32
    );
    assert_eq!(
        StoreError::internal("test").ffi_code(),
        LoreErrorCode::Internal as i32
    );
}

#[test]
fn store_error_internal_convenience() {
    let err = StoreError::internal("something went wrong");
    assert!(err.is_internal());
    assert_eq!(err.to_string(), "something went wrong");
    assert_eq!(err.ffi_code(), LoreErrorCode::Internal as i32);
}

#[test]
fn store_error_internal_with_context_convenience() {
    let source = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
    let err = StoreError::internal_with_context(source, "loading config");
    assert!(err.is_internal());
    assert_eq!(err.to_string(), "loading config: file missing");
    assert_eq!(err.ffi_code(), LoreErrorCode::Internal as i32);
}
