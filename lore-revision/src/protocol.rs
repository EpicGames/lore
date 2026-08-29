// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Bridge the connect() function to maintain the 3-argument signature.
pub async fn connect(
    remote_url: &str,
    identity: &str,
    repository: crate::lore::RepositoryId,
) -> Result<std::sync::Arc<lore_transport::Connection>, lore_transport::ProtocolError> {
    let execution = crate::lore::execution_context();
    let globals = execution.globals();
    lore_transport::connect(
        remote_url,
        identity,
        repository,
        globals.max_connections as usize,
        globals.identity_token(),
        globals.access_token(),
    )
    .await
}
