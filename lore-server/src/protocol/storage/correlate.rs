// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ops::Deref;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use lore_storage::ImmutableStore;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::correlation::CorrelationId;
use crate::protocol::attribute_map::AttributeMap;
use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::messages::Message;
use crate::protocol::storage::messages::MessageHandleError;
use crate::protocol::storage::messages::MessageParseError;
use crate::protocol::storage::messages::Response;

#[derive(Clone, Debug, PartialEq)]
pub struct Correlate {
    pub correlation_id: Option<String>,
}

const MIN_CORRELATION_ID_LENGTH: usize = 4;
const MAX_CORRELATION_ID_LENGTH: usize = 150;

impl Correlate {
    pub fn parse(bytes: Bytes) -> Result<Self, MessageParseError>
    where
        Self: Sized,
    {
        let correlation_id = if !bytes.is_ascii()
            || (bytes.len() < MIN_CORRELATION_ID_LENGTH || bytes.len() > MAX_CORRELATION_ID_LENGTH)
        {
            warn!(
                "Correlation id was invalid, either it contained non-ascii characters, or had an invalid length."
            );
            None
        } else {
            // SAFETY: We've already verified above that the bytes are ASCII.
            Some(unsafe { String::from_utf8_unchecked(bytes.to_vec()) })
        };

        Ok(Self { correlation_id })
    }
}

#[async_trait]
impl Message for Correlate {
    #[tracing::instrument(name = "Correlate::handle", skip_all)]
    async fn handle(
        &self,
        context: Arc<AttributeMap>,
        _immutable_store: Arc<dyn ImmutableStore>,
        _repository_authorizer: Arc<dyn RepositoryAuthorizer>,
    ) -> Result<LoreResponse, MessageHandleError> {
        let correlation_id = match self.correlation_id.as_ref() {
            Some(correlation_id) => {
                info!(correlation_id, "Setting correlation id for connection.");
                context.insert(CorrelationId::new(correlation_id));
                correlation_id.clone()
            }
            None => {
                if let Some(correlation_id) = context.get::<CorrelationId>() {
                    info!(
                        correlation_id = correlation_id.0,
                        "No correlation id found on message, using the default value from the connection."
                    );
                    correlation_id.0.clone()
                } else {
                    let correlation_id = CorrelationId::default();
                    let inner = correlation_id.deref().to_string();
                    warn!(
                        correlation_id = inner,
                        "No correlation id found on message, and no default value present. Generating a new value."
                    );
                    context.insert(correlation_id);

                    inner
                }
            }
        };

        if let Some(span) = context.get::<tracing::Span>() {
            debug!(correlation_id, "Recording correlation id.");
            span.record("correlation_id", &correlation_id);
        }

        Ok(LoreResponse::Correlate(CorrelateResponse {
            correlation_id,
        }))
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct CorrelateResponse {
    pub correlation_id: String,
}

impl Response for CorrelateResponse {
    fn data(&self) -> Vec<Bytes> {
        vec![Bytes::copy_from_slice(self.correlation_id.as_bytes())]
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::store::test_store_create;

    fn allow_all() -> Arc<dyn RepositoryAuthorizer> {
        Arc::new(crate::authnz::repository_authorizer::AllowAllRepositoryAuthorizer)
    }

    #[test]
    fn test_parse() {
        let correlation_id = uuid::Uuid::new_v4().to_string();

        let bytes = bytes::Bytes::from(correlation_id.clone());

        assert_eq!(
            Correlate::parse(bytes),
            Ok(Correlate {
                correlation_id: Some(correlation_id)
            })
        );
    }

    #[test]
    fn test_parse_correlation_id_too_short() {
        assert_eq!(
            Correlate::parse(bytes::Bytes::from(
                "a".repeat(MIN_CORRELATION_ID_LENGTH - 1),
            )),
            Ok(Correlate {
                correlation_id: None
            })
        );
    }

    #[test]
    fn test_parse_correlation_id_too_long() {
        assert_eq!(
            Correlate::parse(bytes::Bytes::from(
                "a".repeat(MAX_CORRELATION_ID_LENGTH + 1),
            )),
            Ok(Correlate {
                correlation_id: None
            })
        );
    }

    #[test]
    fn test_parse_correlation_id_non_ascii() {
        assert_eq!(
            Correlate::parse(bytes::Bytes::from("🙅‍♂️🙋‍♂️🔑".repeat(3),)),
            Ok(Correlate {
                correlation_id: None
            })
        );
    }

    #[tokio::test]
    async fn test_handle() {
        let correlation_id = uuid::Uuid::new_v4().to_string();

        let message = Correlate {
            correlation_id: Some(correlation_id.clone()),
        };

        let context = Arc::new(AttributeMap::default());

        let (immutable_store, _mutable_store, _execution) =
            test_store_create().await.expect("Failed to create stores");

        let response = message
            .handle(context.clone(), immutable_store, allow_all())
            .await
            .unwrap();
        assert_eq!(
            LoreResponse::Correlate(CorrelateResponse {
                correlation_id: correlation_id.clone()
            }),
            response
        );

        assert_eq!(correlation_id, **context.get::<CorrelationId>().unwrap());

        assert_eq!(
            vec![Bytes::copy_from_slice(correlation_id.as_bytes())],
            response.data()
        );
    }

    #[tokio::test]
    async fn test_handle_correlation_id_already_set() {
        let context = Arc::new(AttributeMap::default());
        context.insert(CorrelationId::new(uuid::Uuid::new_v4()));

        let correlation_id = uuid::Uuid::new_v4().to_string();
        let message = Correlate {
            correlation_id: Some(correlation_id.clone()),
        };

        let (immutable_store, _mutable_store, _execution) =
            test_store_create().await.expect("Failed to create stores");

        assert_eq!(
            LoreResponse::Correlate(CorrelateResponse {
                correlation_id: correlation_id.clone()
            }),
            message
                .handle(context.clone(), immutable_store, allow_all())
                .await
                .unwrap()
        );

        assert_eq!(correlation_id, **context.get::<CorrelationId>().unwrap());
    }

    #[tokio::test]
    async fn test_handle_correlation_id_missing() {
        let message = Correlate {
            correlation_id: None,
        };

        let context = Arc::new(AttributeMap::default());
        let correlation_id = uuid::Uuid::new_v4();
        context.insert(CorrelationId::new(correlation_id));

        let (immutable_store, _mutable_store, _execution) =
            test_store_create().await.expect("Failed to create stores");

        assert_eq!(
            LoreResponse::Correlate(CorrelateResponse {
                correlation_id: correlation_id.to_string()
            }),
            message
                .handle(context.clone(), immutable_store, allow_all())
                .await
                .unwrap()
        );

        assert_eq!(
            correlation_id.to_string(),
            **context.get::<CorrelationId>().unwrap()
        );
    }

    #[tokio::test]
    async fn test_handle_correlation_id_missing_not_set_in_context() {
        let message = Correlate {
            correlation_id: None,
        };

        let context = Arc::new(AttributeMap::default());

        let (immutable_store, _mutable_store, _execution) =
            test_store_create().await.expect("Failed to create stores");

        match message
            .handle(context.clone(), immutable_store, allow_all())
            .await
            .unwrap()
        {
            LoreResponse::Correlate(CorrelateResponse { correlation_id }) => {
                uuid::Uuid::from_str(&correlation_id).expect("Correlation id was invalid");
            }
            default => {
                panic!("Unexpected response from Correlate::handle: {default:?}");
            }
        }
    }
}
