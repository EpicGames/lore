// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Subscribing to a repository's notifications.

use lore_base::error::Disconnected;
use lore_base::error::NotAuthorized;
use lore_revision::notification::NotificationError;
use lore_revision::notification::subscribe;
use lore_revision::repository::RemoteState;
use lore_revision::test_util::context_with_state;
use lore_transport::ProtocolError;

use crate::immutable::session_tests::under_execution_context;

/// A subscription whose connect the remote refuses fails with the refusal, and one whose connect
/// fails for any other reason fails as an internal error.
#[tokio::test]
async fn a_subscription_whose_connect_is_refused_fails_with_the_refusal() {
    type Failed = fn(&NotificationError) -> bool;
    let cases: [(ProtocolError, Failed); 2] = [
        (
            ProtocolError::from(NotAuthorized),
            NotificationError::is_not_authorized,
        ),
        (
            ProtocolError::from(Disconnected),
            NotificationError::is_internal,
        ),
    ];
    for (connect, failed) in cases {
        let repository = context_with_state(RemoteState::Failed(connect.clone())).await;

        let subscribed = under_execution_context(|| subscribe(repository)).await;

        assert!(
            subscribed.as_ref().is_err_and(failed),
            "a connect failing with {connect:?}: got {subscribed:?}"
        );
    }
}
