// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_notification::client::subscribe_retryable;
use tonic::Status;

/// A subscribe the server answers `Unauthenticated` fails with `NotAuthenticated`.
#[test]
fn an_unauthenticated_subscribe_fails_as_not_authenticated() {
    let failure = subscribe_retryable(Status::unauthenticated("no bearer token"));
    assert!(
        failure
            .as_ref()
            .is_err_and(|err| err.is_not_authenticated()),
        "{failure:?}"
    );
}

/// A subscribe the server answers `PermissionDenied` fails with `NotAuthorized`.
#[test]
fn a_permission_denied_subscribe_fails_as_not_authorized() {
    let failure = subscribe_retryable(Status::permission_denied("Not allowed"));
    assert!(
        failure.as_ref().is_err_and(|err| err.is_not_authorized()),
        "{failure:?}"
    );
}

/// A subscribe the server is unavailable for is tried again.
#[test]
fn an_unavailable_subscribe_is_retried() {
    let failure = subscribe_retryable(Status::unavailable("restarting"));
    assert!(failure.is_ok(), "{failure:?}");
}
