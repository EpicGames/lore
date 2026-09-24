// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Smoke test verifying `lore.user.v1` carries the 3 RPCs' request /
//! response messages, with the partition listing as a server stream.

use lore_proto::lore::user::v1::PartitionListRequest;
use lore_proto::lore::user::v1::PartitionListResponse;
use lore_proto::lore::user::v1::User;
use lore_proto::lore::user::v1::UserFindRequest;
use lore_proto::lore::user::v1::UserFindResponse;
use lore_proto::lore::user::v1::UserGetRequest;
use lore_proto::lore::user::v1::UserGetResponse;
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
    let _ = PartitionListRequest::default();
    let _ = PartitionListResponse::default();
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
    let PartitionListRequest {} = PartitionListRequest::default();
    let PartitionListResponse {
        partition: _,
        permission: _,
    } = PartitionListResponse::default();
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

/// A `UserService` that answers nothing; it only lends the server type a
/// concrete implementation for the service-name check, and pins that
/// `PartitionList` is a server stream by having to name its stream type.
struct Unserved;

#[tonic::async_trait]
impl UserService for Unserved {
    type PartitionListStream = Empty<Result<PartitionListResponse, tonic::Status>>;

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

    async fn partition_list(
        &self,
        _: tonic::Request<PartitionListRequest>,
    ) -> Result<tonic::Response<Self::PartitionListStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("not served"))
    }
}
