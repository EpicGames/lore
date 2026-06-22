// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use jsonwebtoken::DecodingKey;
use jsonwebtoken::jwk::Jwk;
use jsonwebtoken::jwk::JwkSet;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::LabelArray;
use lore_telemetry::METRICS_OPERATION_LATENCY_METRIC_NAME;
use lore_telemetry::timed;
use lore_telemetry::timer::TimedResult;
use lore_transport::grpc::user_agent;
use opentelemetry::KeyValue;
use serde::Deserialize;
use smallvec::SmallVec;
use thiserror::Error;
use tracing::warn;

#[derive(Clone)]
struct JWKServiceKey {
    #[allow(dead_code)]
    jwk: Jwk,
    decoding_key: DecodingKey,
    algorithm: jsonwebtoken::Algorithm,
}

#[derive(Clone, Default, Deserialize, Debug)]
pub struct JWKServiceSettings {
    pub endpoint: String,
}

#[derive(Error, Debug)]
pub enum JWKServiceError {
    #[error("Internal Error")]
    InternalError,
    #[error("Could not parse jwks endpoint response")]
    ParseError(#[from] serde_json::Error),
    #[error("Could not decode jwk key")]
    DecodingError(#[from] jsonwebtoken::errors::Error),
    #[error("Key for kid not found")]
    NotFound,
}

#[async_trait]
pub trait JWKService: Send + Sync {
    /// Get the public key for the specified key id. Note: this may potentially result in a network
    /// call if the key for key id is not already cached locally by the implementer of this trait.
    async fn get_key(
        &self,
        kid: &str,
    ) -> Result<(DecodingKey, jsonwebtoken::Algorithm), JWKServiceError>;
}

#[derive(Clone, Default)]
pub struct JwkServiceImpl {
    // allow to be refetched from different threads if needed
    cached_set: Arc<tokio::sync::RwLock<HashMap<String, JWKServiceKey>>>,
    #[allow(dead_code)]
    settings: JWKServiceSettings,
}

impl JwkServiceImpl {
    pub fn new(settings: JWKServiceSettings) -> Self {
        Self {
            cached_set: Default::default(),
            settings,
        }
    }

    async fn get_cached_key(
        &self,
        kid: &str,
    ) -> Result<(DecodingKey, jsonwebtoken::Algorithm), JWKServiceError> {
        let keys = self.cached_set.read().await;
        let res = keys.get(kid).ok_or(JWKServiceError::NotFound)?;
        Ok((res.decoding_key.clone(), res.algorithm))
    }

    /// Fetch the latest keys and replace the local cache. If `desired` is not-`None`,
    /// short-circuits if the key id is already present in the local cache.
    pub async fn fetch_new_keys(&self, desired: Option<&str>) -> Result<(), JWKServiceError> {
        let mut cache = self.cached_set.write().await;

        // Check to see if the desired key was fetched while we waited for the lock.
        if desired.map(|d| cache.get(d)).is_some() {
            return Ok(());
        }

        let client = reqwest::Client::builder()
            .user_agent(user_agent())
            .build()
            .map_err(|e| {
                warn!("Failed to construct HTTP client: {e:?}");
                JWKServiceError::InternalError
            })?;

        let response = timed!(
            self.latency_histogram_ms(METRICS_OPERATION_LATENCY_METRIC_NAME),
            &self.get_labels_for_operation_context("get_keys"),
            {
                client
                    .get(&self.settings.endpoint)
                    .send()
                    .await
                    .map_err(|e| {
                        warn!("Failed to fetch JWKS endpoint: {e:?}");
                        JWKServiceError::InternalError
                    })
            }
        )
        .result?;

        let status = response.status();
        let response_body = response.text().await.map_err(|e| {
            warn!("Failed to get response body from JWKS endpoint result: {e:?}");
            JWKServiceError::InternalError
        })?;

        if !status.is_success() {
            warn!("JWKS endpoint returned error. Status: {status}, response: {response_body}");

            return Err(JWKServiceError::InternalError);
        }

        let new_jwks: JwkSet = serde_json::from_str(response_body.as_str()).map_err(|e| {
            warn!("Failed to parse JWKS response: {response_body}");
            JWKServiceError::ParseError(e)
        })?;

        let mut new_set = HashMap::new();

        for jwk in new_jwks.keys {
            let kid = jwk
                .common
                .key_id
                .as_ref()
                .ok_or(JWKServiceError::InternalError)?;

            let algorithm = jwk
                .common
                .key_algorithm
                .ok_or(JWKServiceError::InternalError)?;
            let algorithm = jsonwebtoken::Algorithm::from_str(&algorithm.to_string())
                .map_err(JWKServiceError::DecodingError)?;

            new_set.insert(
                kid.clone(),
                JWKServiceKey {
                    decoding_key: DecodingKey::from_jwk(&jwk)
                        .map_err(JWKServiceError::DecodingError)?,
                    jwk,
                    algorithm,
                },
            );
        }

        *cache = new_set;

        Ok(())
    }
}

#[async_trait]
impl JWKService for JwkServiceImpl {
    async fn get_key(
        &self,
        kid: &str,
    ) -> Result<(DecodingKey, jsonwebtoken::Algorithm), JWKServiceError> {
        let key = self.get_cached_key(kid).await;

        match key {
            Ok(_) => key,
            Err(JWKServiceError::NotFound) => {
                // one more try after fetch
                self.fetch_new_keys(Some(kid)).await?;
                self.get_cached_key(kid).await
            }
            Err(e) => Err(e),
        }
    }
}

impl InstrumentProvider for JwkServiceImpl {
    fn namespace(&self) -> &'static str {
        "urc.auth.jwk_service"
    }
}
