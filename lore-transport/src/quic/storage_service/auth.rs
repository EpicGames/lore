// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use lore_base::types::Partition;

use super::super::QuicClientError;
use super::super::client::AuthAdapter;
use super::super::client::CertificateSettings;
use super::super::client::QuicConnection;
use crate::error::ProtocolError;

/// Auth adapter for lore-storage/0.4. Connection-level authorization is a no-op;
/// per-session authorization is handled by `Storage::session_start()` which fetches
/// tokens via `auth_exchange` directly.
pub struct StorageClientAuth {
    #[allow(dead_code)]
    pub recipient_domain: String,
    #[allow(dead_code)]
    pub auth_url: String,
    #[allow(dead_code)]
    pub identity: String,
    #[allow(dead_code)]
    pub partition: Partition,
}

#[async_trait]
impl AuthAdapter for StorageClientAuth {
    type ErrorType = ProtocolError;

    async fn initial_authorize(
        &self,
        _connection: Arc<QuicConnection>,
    ) -> Result<(), Self::ErrorType> {
        // With lore-storage/0.4, authorization is per-session via Storage::session_start().
        // No connection-level authorize needed.
        Ok(())
    }

    async fn reconnect_authorize(
        &self,
        _connection: Arc<QuicConnection>,
    ) -> Result<(), QuicClientError> {
        // Same — reconnected connections get authorized per-session.
        Ok(())
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
