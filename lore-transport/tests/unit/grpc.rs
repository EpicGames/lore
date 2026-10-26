// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::atomic::Ordering;

use bytes::Bytes;
use lore_base::error::AddressNotFound;
use lore_transport::error::ProtocolError;
use tonic::Status;
use tower::ServiceBuilder;

mod environment_client;
mod revision_client;
mod storage_client;

use std::sync::atomic::AtomicUsize;

use lore_transport::grpc::*;

/// A connection that reaches nothing. `with_reconnect` never dials — it only reads the epoch
/// and defers to the supplied rebuild — so the tests drive it entirely offline.
fn test_connection() -> GRPCConnection {
    let endpoint = tonic::transport::Endpoint::from_shared("http://127.0.0.1:1".to_string())
        .expect("test endpoint");
    let channel = ServiceBuilder::new()
        .layer(RequestLoggerLayer {})
        .service(endpoint.connect_lazy());
    GRPCConnection::for_test("http://127.0.0.1:1".parse().expect("test url"), channel)
}

/// An operation the remote answers is returned as-is, without reconnecting.
///
/// Matching the QUIC client, where `NotFound` and friends bubble rather than provoking a
/// reconnect: only a lost channel is the transport's business.
#[tokio::test]
async fn a_server_verdict_is_returned_without_reconnecting() {
    let connection = test_connection();
    let rebuilds = AtomicUsize::new(0);

    let result: Result<(), ProtocolError> = with_reconnect(
        &connection,
        || async { Err(ProtocolError::from(lore_base::error::NotFound)) },
        |_| async {
            rebuilds.fetch_add(1, Ordering::Relaxed);
            Ok(())
        },
    )
    .await;

    assert!(
        result.is_err_and(|err| err.is_not_found()),
        "the remote's verdict must reach the caller unchanged",
    );
    assert_eq!(
        rebuilds.load(Ordering::Relaxed),
        0,
        "a verdict is not a lost channel, so nothing should reconnect",
    );
}

/// A remote that keeps reporting a lost channel is given up on rather than retried forever.
///
/// `GRPCConnection::reconnect` only gives up permanently when it cannot reach the remote at
/// all. One that accepts connections while failing every RPC rebuilds successfully every
/// round, so without this bound the loop would never terminate.
#[tokio::test]
async fn attempts_are_bounded_when_the_channel_never_recovers() {
    let connection = test_connection();
    let attempts = AtomicUsize::new(0);
    let rebuilds = AtomicUsize::new(0);

    let result: Result<(), ProtocolError> = with_reconnect(
        &connection,
        || async {
            attempts.fetch_add(1, Ordering::Relaxed);
            Err(ProtocolError::from(lore_base::error::Disconnected))
        },
        |_| async {
            rebuilds.fetch_add(1, Ordering::Relaxed);
            Ok(())
        },
    )
    .await;

    assert!(
        result.is_err_and(|err| err.is_disconnected()),
        "giving up must report a disconnect",
    );
    assert_eq!(attempts.load(Ordering::Relaxed), MAX_RECONNECTS_PER_OP);
    assert_eq!(rebuilds.load(Ordering::Relaxed), MAX_RECONNECTS_PER_OP);
}

/// Each attempt reads the epoch afresh, so a later attempt still drives a real reconnect.
///
/// Capturing it once outside the loop is the defect this pins: after the first rebuild the
/// epoch has moved, so every later attempt would hand `reconnect` a stale id, which it reads
/// as "somebody else already reconnected" and returns from without doing anything — no
/// reconnect, no backoff, and no route to giving up.
#[tokio::test]
async fn the_epoch_is_read_afresh_for_every_attempt() {
    let connection = test_connection();
    let seen = parking_lot::Mutex::new(Vec::new());

    let _: Result<(), ProtocolError> = with_reconnect(
        &connection,
        || async { Err(ProtocolError::from(lore_base::error::Disconnected)) },
        |reconnect_id| {
            let (seen, connection) = (&seen, &connection);
            async move {
                seen.lock().push(reconnect_id);
                connection.reconnect.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        },
    )
    .await;

    let seen = seen.lock().clone();
    assert_eq!(
        seen,
        (1..=MAX_RECONNECTS_PER_OP as u32).collect::<Vec<_>>(),
        "each attempt must observe the epoch left by the previous rebuild",
    );
}

/// A request's future does not hold the rebuild it runs only after a lost channel.
#[tokio::test]
async fn a_request_does_not_hold_its_rebuild() {
    let connection = test_connection();

    let request = with_reconnect(
        &connection,
        || async { Ok(()) },
        |_| async {
            let state = [0u8; 4096];
            tokio::task::yield_now().await;
            std::hint::black_box(state);
            Ok(())
        },
    );

    assert!(
        size_of_val(&request) < 4096,
        "a request holds {} bytes, its rebuild among them",
        size_of_val(&request)
    );
}

fn missing_address() -> AddressNotFound {
    AddressNotFound {
        address: std::array::from_fn(|index| index as u8),
    }
}

/// The address survives the round trip, so a caller can name the fragment
/// the peer is missing.
#[test]
fn a_missing_address_round_trips_through_a_status() {
    let status = Status::from(ProtocolError::from(missing_address()));
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);

    let error = ProtocolError::from(status);
    assert_eq!(
        error.as_address_not_found().map(|error| error.address),
        Some(missing_address().address),
    );
}

/// The details are a trailer, so the round trip has to hold across the
/// headers a peer actually reads.
#[test]
fn a_missing_address_round_trips_through_the_headers() {
    let mut headers = http::HeaderMap::new();
    Status::from(ProtocolError::from(missing_address()))
        .add_header(&mut headers)
        .expect("encode the status");

    let status = Status::from_header_map(&headers).expect("decode the status");
    let error = ProtocolError::from(status);
    assert_eq!(
        error.as_address_not_found().map(|error| error.address),
        Some(missing_address().address),
    );
}

/// `NotFound` names an absent object, which a caller recovers from by
/// creating it, and never an address.
#[test]
fn a_not_found_is_never_read_as_an_address() {
    let status = Status::with_details(
        tonic::Code::NotFound,
        "Branch not found",
        Bytes::copy_from_slice(&missing_address().address),
    );

    let error = ProtocolError::from(status);
    assert!(error.is_not_found(), "{error:?}");
}

/// A rejection naming no address is one of the other conditions the code
/// carries, so it stays an opaque failure.
#[test]
fn a_failed_precondition_without_details_is_not_an_address() {
    let error = ProtocolError::from(Status::failed_precondition(
        "Branch push is not a fast-forward",
    ));
    assert!(error.is_internal(), "{error:?}");
}

/// Details of any other length are not guessed at.
#[test]
fn details_of_another_length_are_not_read_as_an_address() {
    let status = Status::with_details(
        tonic::Code::FailedPrecondition,
        "Missing fragment",
        Bytes::from_static(&[1, 2, 3]),
    );

    let error = ProtocolError::from(status);
    assert!(error.is_internal(), "{error:?}");
}
