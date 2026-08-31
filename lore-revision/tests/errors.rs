// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::error::AddressNotFound;
use lore_base::error::FileNotFound;
use lore_base::error::InvalidArguments;
use lore_base::error::PayloadNotFound;
use lore_base::error::SlowDown;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_error_set::FfiError;

#[test]
fn address_not_found_display() {
    let err = AddressNotFound::from(Address::default());
    assert_eq!(
        err.to_string(),
        format!("Address not found: {}", Address::default())
    );
}

#[test]
fn address_not_found_ffi_code() {
    let err = AddressNotFound::from(Address::default());
    assert_eq!(err.ffi_code(), 80);
}

#[test]
fn payload_not_found_ffi_code() {
    let err = PayloadNotFound::from(Hash::default());
    assert_eq!(err.ffi_code(), 81);
}

#[test]
fn payload_not_found_display() {
    let err = PayloadNotFound::from(Hash::default());
    assert_eq!(
        err.to_string(),
        format!("Payload not found: {}", Hash::default())
    );
}

#[test]
fn slow_down_display_and_ffi_code() {
    let err = SlowDown;
    assert_eq!(err.to_string(), "Store overloaded, slow down");
    assert_eq!(err.ffi_code(), 31);
}

#[test]
fn invalid_arguments_display() {
    let err = InvalidArguments {
        reason: "bad input".to_string(),
    };
    assert_eq!(err.to_string(), "invalid arguments: bad input");
}

#[test]
fn invalid_arguments_ffi_code() {
    let err = InvalidArguments {
        reason: "bad input".to_string(),
    };
    assert_eq!(err.ffi_code(), 3);
}

#[test]
fn file_not_found_display() {
    let err = FileNotFound {
        resource: "src/main.rs".to_string(),
    };
    assert_eq!(err.to_string(), "file not found: src/main.rs");
}

#[test]
fn file_not_found_ffi_code() {
    let err = FileNotFound {
        resource: "src/main.rs".to_string(),
    };
    assert_eq!(err.ffi_code(), 82);
}

#[test]
fn all_discrete_types_implement_std_error() {
    fn assert_std_error(_: &impl std::error::Error) {}

    assert_std_error(&AddressNotFound::from(Address::default()));
    assert_std_error(&FileNotFound {
        resource: "test".to_string(),
    });
    assert_std_error(&InvalidArguments {
        reason: "test".to_string(),
    });
    assert_std_error(&PayloadNotFound::from(Hash::default()));
    assert_std_error(&SlowDown);
}
