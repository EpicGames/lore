// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use lore_base::types::Context;
use lore_notification::NotificationService as _;
use lore_proto::lore::notification::SubscribeRequest;
use lore_revision::lore::RepositoryId;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::RawToken;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::VerifiedToken;
use lore_server::grpc::authorization_timeout_status;
use lore_server::grpc::notification_service::*;
use lore_transport::grpc::PARTITION_ID_KEY;
use tonic::Code;
use tonic::Request;
use tonic::Status;

/// Denies everything and records which partition it was asked about.
struct DenyingAuthorizer {
    asked: Mutex<Vec<RepositoryId>>,
}

#[async_trait]
impl RepositoryAuthorizer for DenyingAuthorizer {
    async fn check_repository_access(
        &self,
        _token: Option<&VerifiedToken<'_>>,
        repository_id: RepositoryId,
        _action: Action,
    ) -> Result<(), Status> {
        self.asked.lock().unwrap().push(repository_id);
        Err(Status::permission_denied("no grant"))
    }
}

fn repository(id: u8) -> RepositoryId {
    let mut data = [0u8; 16];
    data[15] = id;
    Context::from(data).into()
}

/// A subscribe whose *metadata* names one partition and whose *body*
/// names another: only the body value — the one the stream registers
/// on — may decide, so the check must be asked about it.
#[tokio::test]
async fn subscribe_is_decided_by_the_body_partition_not_the_metadata() {
    let authorizer = Arc::new(DenyingAuthorizer {
        asked: Mutex::new(Vec::new()),
    });
    let service = NotificationService::new(
        Arc::new(lore_server::notification::local::NotificationSender::default()),
        authorizer.clone(),
        Duration::from_secs(5),
    );

    let body_partition = repository(1);
    let metadata_partition = repository(2);
    let mut request = Request::new(SubscribeRequest {
        repository: Context::from(body_partition).into(),
    });
    request.metadata_mut().insert_bin(
        PARTITION_ID_KEY,
        tonic::metadata::BinaryMetadataValue::from_bytes(metadata_partition.data()),
    );
    request
        .extensions_mut()
        .insert(AuthorizationToken::default());
    request.extensions_mut().insert(RawToken("raw.jwt".into()));

    let denied = match service.subscribe(request).await {
        Err(status) => status,
        Ok(_) => panic!("an ungranted subscribe must be denied"),
    };

    assert_eq!(denied.code(), Code::PermissionDenied);
    assert_eq!(*authorizer.asked.lock().unwrap(), vec![body_partition]);
}

/// A stalled authorizer cannot park a subscriber: the check elapses under
/// its own bound, and answers with the authorization timeout's status
/// rather than a denial or the handler timeout's.
#[tokio::test]
async fn a_stalled_authorizer_times_out_the_subscribe_check() {
    struct StalledAuthorizer;

    #[async_trait]
    impl RepositoryAuthorizer for StalledAuthorizer {
        async fn check_repository_access(
            &self,
            _token: Option<&VerifiedToken<'_>>,
            _repository_id: RepositoryId,
            _action: Action,
        ) -> Result<(), Status> {
            std::future::pending().await
        }
    }

    let service = NotificationService::new(
        Arc::new(lore_server::notification::local::NotificationSender::default()),
        Arc::new(StalledAuthorizer),
        Duration::from_millis(50),
    );

    let mut request = Request::new(SubscribeRequest {
        repository: Context::from(repository(1)).into(),
    });
    request
        .extensions_mut()
        .insert(AuthorizationToken::default());
    request.extensions_mut().insert(RawToken("raw.jwt".into()));

    let status = match service.subscribe(request).await {
        Err(status) => status,
        Ok(_) => panic!("a stalled authorization check must not admit a subscriber"),
    };

    assert_eq!(status.code(), Code::Cancelled);
    assert_eq!(status.message(), authorization_timeout_status().message());
}

/// A no-auth configuration selects `AllowAllRepositoryAuthorizer`, under
/// which subscribe stays open.
#[tokio::test]
async fn subscribe_stays_open_under_the_allow_all_authorizer() {
    let service = NotificationService::new(
        Arc::new(lore_server::notification::local::NotificationSender::default()),
        Arc::new(AllowAllRepositoryAuthorizer),
        Duration::from_secs(5),
    );
    let request = Request::new(SubscribeRequest {
        repository: Context::from(repository(1)).into(),
    });

    service
        .subscribe(request)
        .await
        .expect("a no-auth server's subscribe is open");
}
