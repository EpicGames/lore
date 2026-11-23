// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod client;
mod client_container;
mod peer_responses;
mod server;

use lore_base::error::AddressNotFound;
use lore_base::error::PayloadNotFound;
use lore_base::error::SlowDown;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_server::quic::replication_store_service::*;
use lore_storage::StoreError;

#[test]
fn store_error_address_not_found_maps_to_address_not_found_code() {
    let err = StoreError::from(AddressNotFound::from(Address::default()));
    let code = ReplicationServiceErrorCode::from(&err);
    assert_eq!(code, ReplicationServiceErrorCode::AddressNotFound);
    assert_eq!(code as u32, 201);
}

#[test]
fn store_error_internal_maps_to_internal_code() {
    let err = StoreError::internal("test");
    let code = ReplicationServiceErrorCode::from(&err);
    assert_eq!(code, ReplicationServiceErrorCode::Internal);
    assert_eq!(code as u32, 200);
}

#[test]
fn store_error_slow_down_maps_to_slow_down_code() {
    let err = StoreError::from(SlowDown);
    let code = ReplicationServiceErrorCode::from(&err);
    assert_eq!(code, ReplicationServiceErrorCode::SlowDown);
    assert_eq!(code as u32, 202);
}

#[test]
fn store_error_payload_not_found_maps_to_payload_not_found_code() {
    let err = StoreError::from(PayloadNotFound::from(Hash::default()));
    let code = ReplicationServiceErrorCode::from(&err);
    assert_eq!(code, ReplicationServiceErrorCode::PayloadNotFound);
    assert_eq!(code as u32, 203);
}

#[test]
fn error_code_address_not_found_converts_to_client_service_error() {
    let client_err =
        lore_server::quic::replication_store_service::client::ReplicationStoreClientError::from(
            ReplicationServiceErrorCode::AddressNotFound,
        );
    assert!(matches!(
        client_err,
        lore_server::quic::replication_store_service::client::ReplicationStoreClientError::ServiceError(
            ReplicationServiceErrorCode::AddressNotFound
        )
    ));
}

#[test]
fn error_code_slow_down_converts_to_client_service_error() {
    let client_err =
        lore_server::quic::replication_store_service::client::ReplicationStoreClientError::from(
            ReplicationServiceErrorCode::SlowDown,
        );
    assert!(matches!(
        client_err,
        lore_server::quic::replication_store_service::client::ReplicationStoreClientError::ServiceError(
            ReplicationServiceErrorCode::SlowDown
        )
    ));
}

#[test]
fn error_code_internal_converts_to_client_service_error() {
    let client_err =
        lore_server::quic::replication_store_service::client::ReplicationStoreClientError::from(
            ReplicationServiceErrorCode::Internal,
        );
    assert!(matches!(
        client_err,
        lore_server::quic::replication_store_service::client::ReplicationStoreClientError::ServiceError(
            ReplicationServiceErrorCode::Internal
        )
    ));
}

#[test]
fn error_code_payload_not_found_converts_to_client_service_error() {
    let client_err =
        lore_server::quic::replication_store_service::client::ReplicationStoreClientError::from(
            ReplicationServiceErrorCode::PayloadNotFound,
        );
    assert!(matches!(
        client_err,
        lore_server::quic::replication_store_service::client::ReplicationStoreClientError::ServiceError(
            ReplicationServiceErrorCode::PayloadNotFound
        )
    ));
}
