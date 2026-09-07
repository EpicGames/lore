// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use lore_base::types::Partition;

use super::super::QuicClientError;
use super::super::QuicOpCode;
use super::super::client::AuthAdapter;
use super::super::client::CertificateSettings;
use super::super::client::QuicConnection;
use super::super::client::send_client_identify;
use super::Command;
use crate::error::ProtocolError;

/// Auth adapter for lore-storage/0.4. Grants no authority of its own: per-session authorization is
/// handled by `Storage::session_start()`, which fetches tokens via `auth_exchange` directly. The
/// authorize hooks are used only to announce the client's user agent on connect and reconnect.
pub struct StorageClientAuth {
    #[allow(dead_code)]
    pub recipient_domain: String,
    #[allow(dead_code)]
    pub auth_url: String,
    #[allow(dead_code)]
    pub identity: String,
    #[allow(dead_code)]
    pub partition: Partition,
    pub user_agent: String,
}

impl StorageClientAuth {
    async fn announce_user_agent(
        &self,
        connection: Arc<QuicConnection>,
    ) -> Result<(), QuicClientError> {
        send_client_identify(
            connection,
            Command::ClientIdentify as QuicOpCode,
            true,
            &self.user_agent,
        )
        .await
    }
}

#[async_trait]
impl AuthAdapter for StorageClientAuth {
    type ErrorType = ProtocolError;

    async fn initial_authorize(
        &self,
        connection: Arc<QuicConnection>,
    ) -> Result<(), Self::ErrorType> {
        self.announce_user_agent(connection)
            .await
            .map_err(|e| ProtocolError::internal(format!("sending ClientIdentify: {e}")))
    }

    async fn reconnect_authorize(
        &self,
        connection: Arc<QuicConnection>,
    ) -> Result<(), QuicClientError> {
        self.announce_user_agent(connection).await
    }

    fn client_certs(&self) -> CertificateSettings {
        CertificateSettings {
            // storage service uses a public/known CA that can be found natively
            custom_ca: None,
            // storage service clients doesn't need to provide certs, and are gated
            // by an Auth token instead
            client: None,
        }
    }
}
