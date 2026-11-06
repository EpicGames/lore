// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::error::Disconnected;
use lore_revision::lore::BranchId;
use lore_revision::repository::RemoteState;
use lore_revision::repository::status::*;
use lore_revision::test_util::context_with_state;
use lore_transport::ProtocolError;

fn disconnected() -> ProtocolError {
    ProtocolError::from(Disconnected)
}

#[tokio::test]
async fn offline_remote_resolves_to_unavailable() {
    let ctx = context_with_state(RemoteState::Offline).await;

    let result = resolve_remote_latest(&ctx, BranchId::default()).await;

    assert_eq!(
        result,
        (None, false, false),
        "offline should degrade to unavailable"
    );
}

#[tokio::test]
async fn failed_remote_resolves_to_unavailable() {
    let ctx = context_with_state(RemoteState::Failed(disconnected())).await;

    let result = resolve_remote_latest(&ctx, BranchId::default()).await;

    assert_eq!(
        result,
        (None, false, false),
        "failed remote should degrade to unavailable"
    );
}
