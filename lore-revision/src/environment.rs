// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_storage::CompressionMode;
use lore_transport::Endpoint;
use lore_transport::Oidc;
use serde::Deserialize;

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct EnvironmentConfig {
    pub endpoint: Option<Endpoint>,
    pub config: Option<Config>,
    /// The OIDC provider info.
    #[serde(skip)]
    pub oidc: Option<Oidc>,
}

/// Server-tunable settings a server advertises in its environment.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct Config {
    pub max_query_batch: Option<usize>,
    pub compression_mode: Option<CompressionMode>,
    pub max_lock_batch: Option<usize>,
}
