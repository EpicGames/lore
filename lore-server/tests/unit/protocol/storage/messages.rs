// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_server::protocol::storage::messages::MessageHandleError;
use tonic::Status;

#[test]
fn an_authorization_error_maps_by_whether_it_is_a_denial_or_can_clear() {
    for denial in [
        Status::permission_denied("denied"),
        Status::unauthenticated("token refused"),
    ] {
        assert!(matches!(
            MessageHandleError::from_authorization(denial),
            MessageHandleError::AuthorizationFailure(_)
        ));
    }
    assert!(matches!(
        MessageHandleError::from_authorization(Status::resource_exhausted("rate limited")),
        MessageHandleError::SlowDown
    ));
    for failure in [Status::internal("failed"), Status::unavailable("down")] {
        assert!(matches!(
            MessageHandleError::from_authorization(failure),
            MessageHandleError::InternalError
        ));
    }
}
