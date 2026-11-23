// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use bytes::Bytes;
use lore_transport::quic::QuicClientError;
use lore_transport::quic::QuicServiceError;
use lore_transport::quic::response_reader::*;

#[test]
fn maps_known_service_errors_to_typed_variants() {
    assert!(matches!(
        handle_error(QuicServiceError::NotAuthorized as u32),
        QuicClientError::NotAuthorized
    ));
    assert!(matches!(
        handle_error(QuicServiceError::SlowDown as u32),
        QuicClientError::SlowDown
    ));
    assert!(matches!(
        handle_error(QuicServiceError::NotFound as u32),
        QuicClientError::NotFound
    ));
    assert!(matches!(
        handle_error(QuicServiceError::Oversized as u32),
        QuicClientError::Oversized
    ));
}

#[test]
fn unknown_status_falls_back_to_server_error() {
    assert!(matches!(handle_error(42), QuicClientError::ServerError(42)));
}

#[test]
fn every_answer_of_the_server_shows_its_frame_written() {
    assert!(shows_frame_written(&Ok(Bytes::new())));
    for status in [
        QuicServiceError::InvalidCommand as u32,
        QuicServiceError::NotAuthorized as u32,
        QuicServiceError::Failed as u32,
        QuicServiceError::NotFound as u32,
        QuicServiceError::Oversized as u32,
        QuicServiceError::SlowDown as u32,
        QuicServiceError::ImplementationReserved as u32,
    ] {
        assert!(
            shows_frame_written(&Err(handle_error(status))),
            "status {status}"
        );
    }
}

#[test]
fn a_failure_on_the_client_side_does_not_show_its_frame_written() {
    for error in [
        QuicClientError::Terminated,
        QuicClientError::CrytpoError,
        QuicClientError::Read,
        QuicClientError::WriteChunks,
        QuicClientError::StreamOpen,
    ] {
        assert!(!shows_frame_written(&Err(error.clone())), "{error:?}");
    }
}
