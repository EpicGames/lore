// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Smoke test verifying `lore.user.v1` carries the request / response
//! messages of `UserService` and `RepositoryRegistryService`, with the
//! repository listing as a server stream.

use lore_proto::lore::user::v1::RepositoryListRequest;
use lore_proto::lore::user::v1::RepositoryListResponse;
use lore_proto::lore::user::v1::RepositoryRegisterRequest;
use lore_proto::lore::user::v1::RepositoryRegisterResponse;
use lore_proto::lore::user::v1::RepositoryUnregisterRequest;
use lore_proto::lore::user::v1::RepositoryUnregisterResponse;
use lore_proto::lore::user::v1::User;
use lore_proto::lore::user::v1::UserFindRequest;
use lore_proto::lore::user::v1::UserFindResponse;
use lore_proto::lore::user::v1::UserGetRequest;
use lore_proto::lore::user::v1::UserGetResponse;
use lore_proto::lore::user::v1::repository_registry_service_client::RepositoryRegistryServiceClient;
use lore_proto::lore::user::v1::repository_registry_service_server::RepositoryRegistryService;
use lore_proto::lore::user::v1::repository_registry_service_server::RepositoryRegistryServiceServer;
use lore_proto::lore::user::v1::user_find_request::Query as UserFindQuery;
use lore_proto::lore::user::v1::user_service_client::UserServiceClient;
use lore_proto::lore::user::v1::user_service_server::UserService;
use lore_proto::lore::user::v1::user_service_server::UserServiceServer;
use tonic::codegen::tokio_stream::Empty;

#[test]
fn v1_user_request_response_types_default() {
    let _ = User::default();
    let _ = UserGetRequest::default();
    let _ = UserGetResponse::default();
    let _ = UserFindRequest::default();
    let _ = UserFindResponse::default();
    let _ = RepositoryListRequest::default();
    let _ = RepositoryListResponse::default();
    let _ = RepositoryRegisterRequest::default();
    let _ = RepositoryRegisterResponse::default();
    let _ = RepositoryUnregisterRequest::default();
    let _ = RepositoryUnregisterResponse::default();
}

/// Field-shape regression net: destructuring each message asserts that every
/// field name on the generated Rust types still exists, so a renamed proto
/// field breaks this test at compile time.
#[test]
fn v1_user_field_shapes() {
    let User {
        id: _,
        display_name: _,
        username: _,
    } = User::default();
    let UserGetRequest {
        partition: _,
        id: _,
    } = UserGetRequest::default();
    let UserGetResponse { user: _ } = UserGetResponse::default();
    let UserFindRequest {
        partition: _,
        query: _,
    } = UserFindRequest::default();
    let _ = UserFindQuery::Username(Default::default());
    let _ = UserFindQuery::DisplayName(Default::default());
    let UserFindResponse { user: _ } = UserFindResponse::default();
    let RepositoryListRequest {} = RepositoryListRequest::default();
    let RepositoryListResponse {
        id: _,
        permission: _,
    } = RepositoryListResponse::default();
    let RepositoryRegisterRequest { id: _, name: _ } = RepositoryRegisterRequest::default();
    let RepositoryRegisterResponse {} = RepositoryRegisterResponse::default();
    let RepositoryUnregisterRequest { id: _ } = RepositoryUnregisterRequest::default();
    let RepositoryUnregisterResponse {} = RepositoryUnregisterResponse::default();
}

/// A partition is carried as the repository id bytes, as `Repository.id`
/// and the partition metadata key are; a `UserFind` query names one field
/// to match; and an empty `user` list on `UserFind` is the "no such user"
/// answer, several entries a display-name collision.
#[test]
fn v1_user_field_types() {
    let request = UserGetRequest {
        partition: prost::bytes::Bytes::from_static(&[7; 16]),
        id: vec!["u-1".to_string()],
    };
    assert_eq!(request.partition.len(), 16);
    let request = UserFindRequest {
        partition: prost::bytes::Bytes::from_static(&[7; 16]),
        query: Some(UserFindQuery::DisplayName("Ada".to_string())),
    };
    assert!(matches!(request.query, Some(UserFindQuery::DisplayName(_))));
    let response = UserFindResponse::default();
    assert!(response.user.is_empty());
}

/// Both stubs build, and the service is addressed under the new package so
/// an endpoint can serve it beside the legacy directory RPCs.
#[test]
fn v1_user_service_stubs_exist() {
    fn assert_client<T>(_: fn(T) -> UserServiceClient<T>) {}
    assert_client::<tonic::transport::Channel>(UserServiceClient::new);
    assert_eq!(
        <UserServiceServer<Unserved> as tonic::server::NamedService>::NAME,
        "lore.user.v1.UserService"
    );
}

/// The registry is a service of its own, so an endpoint can serve it beside
/// `UserService`.
#[test]
fn v1_repository_registry_service_stubs_exist() {
    fn assert_client<T>(_: fn(T) -> RepositoryRegistryServiceClient<T>) {}
    assert_client::<tonic::transport::Channel>(RepositoryRegistryServiceClient::new);
    assert_eq!(
        <RepositoryRegistryServiceServer<Unserved> as tonic::server::NamedService>::NAME,
        "lore.user.v1.RepositoryRegistryService"
    );
}

/// A `UserService` and `RepositoryRegistryService` that answers nothing; it
/// only lends the server types a concrete implementation for the
/// service-name checks, and pins that `RepositoryList` is a server stream by
/// having to name its stream type.
struct Unserved;

#[tonic::async_trait]
impl UserService for Unserved {
    async fn user_get(
        &self,
        _: tonic::Request<UserGetRequest>,
    ) -> Result<tonic::Response<UserGetResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("not served"))
    }

    async fn user_find(
        &self,
        _: tonic::Request<UserFindRequest>,
    ) -> Result<tonic::Response<UserFindResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("not served"))
    }
}

#[tonic::async_trait]
impl RepositoryRegistryService for Unserved {
    type RepositoryListStream = Empty<Result<RepositoryListResponse, tonic::Status>>;

    async fn repository_list(
        &self,
        _: tonic::Request<RepositoryListRequest>,
    ) -> Result<tonic::Response<Self::RepositoryListStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("not served"))
    }

    async fn repository_register(
        &self,
        _: tonic::Request<RepositoryRegisterRequest>,
    ) -> Result<tonic::Response<RepositoryRegisterResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("not served"))
    }

    async fn repository_unregister(
        &self,
        _: tonic::Request<RepositoryUnregisterRequest>,
    ) -> Result<tonic::Response<RepositoryUnregisterResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("not served"))
    }
}
