// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use async_trait::async_trait;
use lore_base::error::NotSupported;
use lore_base::types::RepositoryId;
use lore_proto::auth::ExchangeExternalTokenForUserTokenRequest;
use lore_proto::auth::ExchangeUserTokenForMultiresourceTokenRequest;
use lore_proto::auth::GetAuthSessionRequest;
use lore_proto::auth::GetUserIdRequest;
use lore_proto::auth::GetUserInfoRequest;
use lore_proto::auth::StartAuthSessionRequest;
use lore_proto::auth::urc_auth_api_client::UrcAuthApiClient;

use crate::error::ProtocolError;
use crate::grpc::CorrelationInterceptor;
use crate::traits::Authentication;
use crate::types::*;

/// Strips the custom scheme from an auth URL and returns an HTTPS URL
/// suitable for gRPC connection.
///
/// `ucs-auth://auth.example.com` -> `https://auth.example.com`
/// `https://auth.example.com` -> `https://auth.example.com` (unchanged)
fn grpc_endpoint(auth_url: &str) -> String {
    match auth_url.split_once("://") {
        Some(("https", _)) => auth_url.to_string(),
        Some((_, rest)) => format!("https://{rest}"),
        None => format!("https://{auth_url}"),
    }
}

/// Formats a `RepositoryId` as a UCS Auth resource identifier.
fn resource_id(repository: RepositoryId) -> String {
    format!("urc-{repository}")
}

/// Creates a gRPC client with correlation ID interceptor, connected to the
/// auth endpoint.
async fn connect_client(
    auth_url: &str,
) -> Result<
    UrcAuthApiClient<
        tonic::codegen::InterceptedService<tonic::transport::Channel, CorrelationInterceptor>,
    >,
    ProtocolError,
> {
    let endpoint = grpc_endpoint(auth_url);
    let endpoint = tonic::transport::Endpoint::new(endpoint)
        .map_err(|e| ProtocolError::internal(format!("invalid auth endpoint: {e}")))?;
    // Connect from a net-runtime task so the channel's driver tasks are
    // bound to the net runtime.
    let channel = lore_base::lore_spawn_net!(async move { endpoint.connect().await })
        .await
        .map_err(|e| ProtocolError::internal(format!("auth endpoint connect task: {e}")))?
        .map_err(|e| ProtocolError::internal(format!("failed to connect to auth endpoint: {e}")))?;
    Ok(UrcAuthApiClient::with_interceptor(
        channel,
        CorrelationInterceptor,
    ))
}

/// Sets the authorization header on a gRPC request.
fn set_auth_header<T>(request: &mut tonic::Request<T>, token: &str) -> Result<(), ProtocolError> {
    let mut header: tonic::metadata::MetadataValue<_> = format!("Bearer {token}")
        .parse()
        .map_err(|e| ProtocolError::internal(format!("invalid metadata value: {e}")))?;
    header.set_sensitive(true);
    request.metadata_mut().append("authorization", header);
    Ok(())
}

/// Authentication implementation using UCS Auth API gRPC service.
///
/// Registered under the `ucs-auth` scheme (and `https` during transition).
/// All `lore_proto::auth` imports are confined to this module.
///
/// The `correlation_id` parameter on trait methods is not used directly --
/// correlation IDs are injected into gRPC requests by `CorrelationInterceptor`,
/// which reads from the ambient context. Non-gRPC implementations
/// would use the parameter instead.
#[derive(Default)]
pub struct UcsAuthentication;

#[async_trait]
impl Authentication for UcsAuthentication {
    async fn start_auth_session(
        &self,
        auth_url: &str,
        client_state: &str,
        _correlation_id: &str,
    ) -> Result<AuthSession, ProtocolError> {
        let mut client = connect_client(auth_url).await?;

        let request = StartAuthSessionRequest {
            client_state: client_state.to_string(),
        };
        let res = client
            .start_auth_session(request)
            .await
            .map_err(ProtocolError::from)?;

        let inner = res.into_inner();
        Ok(AuthSession {
            session_code: inner.session_code,
            login_url: inner.login_url,
        })
    }

    async fn poll_auth_session(
        &self,
        auth_url: &str,
        client_state: &str,
        session_code: &str,
        _correlation_id: &str,
    ) -> Result<Option<AuthenticationToken>, ProtocolError> {
        let mut client = connect_client(auth_url).await?;

        let request = GetAuthSessionRequest {
            client_state: client_state.to_string(),
            session_code: session_code.to_string(),
        };
        let res = client
            .get_auth_session(request)
            .await
            .map_err(ProtocolError::from)?;

        match res.into_inner().user_token {
            Some(token) => Ok(Some(AuthenticationToken {
                token: token.user_token,
                user_id: token.user_id,
                user_name: token.user_name,
                expires_ms: token.expires_at.max(0) as u64,
                // Populated by orchestration layer via JWT decode, not the proto response
                acceptable_root_domains: Vec::new(),
                refresh_token: None,
            })),
            None => Ok(None),
        }
    }

    async fn exchange_external_token(
        &self,
        auth_url: &str,
        token: &str,
        token_type: &str,
        _correlation_id: &str,
    ) -> Result<AuthenticationToken, ProtocolError> {
        let mut client = connect_client(auth_url).await?;

        let request = ExchangeExternalTokenForUserTokenRequest {
            external_token: token.to_string(),
            token_type: token_type.to_string(),
        };
        let res = client
            .exchange_external_token_for_user_token(request)
            .await
            .map_err(ProtocolError::from)?;

        let user_token = res
            .into_inner()
            .user_token
            .ok_or_else(|| ProtocolError::internal("empty user token in exchange response"))?;

        Ok(AuthenticationToken {
            token: user_token.user_token,
            user_id: user_token.user_id,
            user_name: user_token.user_name,
            expires_ms: user_token.expires_at.max(0) as u64,
            // Populated by orchestration layer via JWT decode, not the proto response
            acceptable_root_domains: Vec::new(),
            refresh_token: None,
        })
    }

    async fn refresh_authentication(
        &self,
        _auth_url: &str,
        _refresh_token: &str,
        _correlation_id: &str,
    ) -> Result<AuthenticationToken, ProtocolError> {
        Err(ProtocolError::from(NotSupported {
            operation: "refresh_authentication".to_string(),
        }))
    }

    async fn exchange_for_repository(
        &self,
        auth_url: &str,
        authn_token: &str,
        repository: RepositoryId,
        correlation_id: &str,
    ) -> Result<AuthorizationToken, ProtocolError> {
        self.exchange_for_custom_resource(
            auth_url,
            authn_token,
            &resource_id(repository),
            correlation_id,
        )
        .await
    }

    async fn exchange_for_custom_resource(
        &self,
        auth_url: &str,
        authn_token: &str,
        resource_id: &str,
        _correlation_id: &str,
    ) -> Result<AuthorizationToken, ProtocolError> {
        let mut client = connect_client(auth_url).await?;

        let mut request = tonic::Request::new(ExchangeUserTokenForMultiresourceTokenRequest {
            resource_id: vec![resource_id.to_string()],
        });
        set_auth_header(&mut request, authn_token)?;

        let res = client
            .exchange_user_token_for_multiresource_token(request)
            .await
            .map_err(ProtocolError::from)?;

        let token = res
            .into_inner()
            .token
            .ok_or_else(|| ProtocolError::internal("empty token in exchange response"))?;

        Ok(AuthorizationToken {
            token: token.user_token,
            expires_ms: token.expires_at.max(0) as u64,
            // Populated by orchestration layer via JWT decode, not the proto response
            acceptable_root_domains: Vec::new(),
        })
    }

    async fn get_user_info(
        &self,
        auth_url: &str,
        authz_token: &str,
        repository: RepositoryId,
        user_ids: &[String],
        _correlation_id: &str,
    ) -> Result<Vec<ResolvedUser>, ProtocolError> {
        let mut client = connect_client(auth_url).await?;

        let mut request = tonic::Request::new(GetUserInfoRequest {
            resource_id: resource_id(repository),
            user_id: user_ids.to_vec(),
        });
        set_auth_header(&mut request, authz_token)?;

        let res = client
            .get_user_info(request)
            .await
            .map_err(ProtocolError::from)?;

        Ok(res
            .into_inner()
            .user_info
            .into_iter()
            .map(|u| ResolvedUser {
                user_id: u.user_id,
                user_name: u.display_name,
            })
            .collect())
    }

    async fn get_user_id(
        &self,
        auth_url: &str,
        authz_token: &str,
        repository: RepositoryId,
        display_name: &str,
        _correlation_id: &str,
    ) -> Result<Option<ResolvedUser>, ProtocolError> {
        let mut client = connect_client(auth_url).await?;

        let mut request = tonic::Request::new(GetUserIdRequest {
            resource_id: resource_id(repository),
            user_display_name: display_name.to_string(),
        });
        set_auth_header(&mut request, authz_token)?;

        let res = client
            .get_user_id(request)
            .await
            .map_err(ProtocolError::from)?;

        Ok(res.into_inner().user_info.map(|u| ResolvedUser {
            user_id: u.user_id,
            user_name: u.display_name,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grpc_endpoint_ucs_auth() {
        assert_eq!(
            grpc_endpoint("ucs-auth://auth.example.com"),
            "https://auth.example.com"
        );
    }

    #[test]
    fn grpc_endpoint_https() {
        assert_eq!(
            grpc_endpoint("https://auth.example.com"),
            "https://auth.example.com"
        );
    }

    #[test]
    fn grpc_endpoint_no_scheme() {
        assert_eq!(
            grpc_endpoint("auth.example.com"),
            "https://auth.example.com"
        );
    }

    #[test]
    fn grpc_endpoint_custom_scheme() {
        assert_eq!(
            grpc_endpoint("custom://auth.example.com:8443/path"),
            "https://auth.example.com:8443/path"
        );
    }

    #[test]
    fn resource_id_format() {
        let repo_id = RepositoryId::default();
        let rid = resource_id(repo_id);
        assert!(rid.starts_with("urc-"));
        // Default RepositoryId is all zeros, displayed as hex
        assert_eq!(rid, "urc-00000000000000000000000000000000");
    }

    #[tokio::test]
    async fn refresh_returns_not_supported() {
        let auth = UcsAuthentication;
        let result = auth
            .refresh_authentication("ucs-auth://auth.example.com", "refresh-tok", "corr-1")
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().is_not_supported());
    }
}
