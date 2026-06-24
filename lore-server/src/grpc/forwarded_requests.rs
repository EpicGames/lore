// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use lore_base::types::RepositoryId;
use tonic::Request;
use tonic::Status;

use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;

const ON_BEHALF_OF_USER_ID_FIELD: &str = "on-behalf-of-user-id";

/// Reconstructed information about the end user client who has performed a particular
/// RPC. Used in place of directly reading from metadata to avoid it being brought into
/// scope and incorrect information being read from it.
#[derive(Clone, Debug)]
pub struct CallerContext {
    pub repository_id: RepositoryId,
    pub user_id: String,
    pub correlation_id: String,
}

impl CallerContext {
    /// Create the caller context from this original request. Call this function
    /// from the server that is the first one to receive an RPC from an end user client
    pub fn from_original_request<T>(request: &Request<T>) -> Result<Self, Status> {
        Ok(Self {
            repository_id: get_repository(request.metadata())?,
            user_id: get_user_id(request.extensions()),
            correlation_id: extract_correlation_id(request).unwrap_or_default(),
        })
    }

    /// Create the caller context from this forwarded request. Call this function
    /// from the server that has received this forwarded request from another Lore server
    pub fn from_forwarded_request<T>(request: &Request<T>) -> Result<Self, Status> {
        let user_id = request
            .metadata()
            .get(ON_BEHALF_OF_USER_ID_FIELD)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_string())
            .ok_or_else(|| Status::internal("missing/invalid `on-behalf-of-user-id` field"))?;

        Ok(Self {
            repository_id: get_repository(request.metadata())?,
            user_id,
            correlation_id: extract_correlation_id(request).unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod test {
    use lore_revision::lore::RepositoryId;
    use lore_transport::grpc::CORRELATION_ID_HEADER;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use rand::random;
    use tonic::Request;

    use super::*;
    use crate::auth::jwt::AuthorizationToken;

    fn insert_repository<T>(request: &mut Request<T>, repository: RepositoryId) {
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );
    }

    mod from_original_request {
        use super::*;

        #[test]
        fn extracts_all_fields() {
            let repository = random::<RepositoryId>();
            let mut request = Request::new(());
            insert_repository(&mut request, repository);
            request.extensions_mut().insert(AuthorizationToken {
                user_id: "alice".into(),
                ..AuthorizationToken::default()
            });
            request
                .metadata_mut()
                .insert(CORRELATION_ID_HEADER, "corr-123".parse().unwrap());

            let ctx = CallerContext::from_original_request(&request).unwrap();
            assert_eq!(ctx.repository_id, repository);
            assert_eq!(ctx.user_id, "alice");
            assert_eq!(ctx.correlation_id, "corr-123");
        }
    }

    mod from_forwarded_request {
        use super::*;

        #[test]
        fn missing_user_id_returns_internal_error() {
            let repository = random::<RepositoryId>();
            let mut request = Request::new(());
            insert_repository(&mut request, repository);

            let err = CallerContext::from_forwarded_request(&request).unwrap_err();
            assert_eq!(err.code(), tonic::Code::Internal);
            assert_eq!(
                err.message(),
                "missing/invalid `on-behalf-of-user-id` field"
            );
        }

        #[test]
        fn extracts_all_fields() {
            let repository = random::<RepositoryId>();
            let mut request = Request::new(());
            insert_repository(&mut request, repository);
            request
                .metadata_mut()
                .insert("on-behalf-of-user-id", "alice".parse().unwrap());
            request
                .metadata_mut()
                .insert(CORRELATION_ID_HEADER, "corr-456".parse().unwrap());

            let ctx = CallerContext::from_forwarded_request(&request).unwrap();
            assert_eq!(ctx.repository_id, repository);
            assert_eq!(ctx.user_id, "alice");
            assert_eq!(ctx.correlation_id, "corr-456");
        }
    }
}
