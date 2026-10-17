// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Smoke test verifying `lore.repository.v1` carries the 7 RPCs' request /
//! response messages.

use lore_proto::lore::repository::v1::RepositoryCountRequest;
use lore_proto::lore::repository::v1::RepositoryCountResponse;
use lore_proto::lore::repository::v1::RepositoryCreateRequest;
use lore_proto::lore::repository::v1::RepositoryCreateResponse;
use lore_proto::lore::repository::v1::RepositoryDeleteRequest;
use lore_proto::lore::repository::v1::RepositoryDeleteResponse;
use lore_proto::lore::repository::v1::RepositoryGetRequest;
use lore_proto::lore::repository::v1::RepositoryGetResponse;
use lore_proto::lore::repository::v1::RepositoryListRequest;
use lore_proto::lore::repository::v1::RepositoryListResponse;
use lore_proto::lore::repository::v1::RepositoryMetadataGetRequest;
use lore_proto::lore::repository::v1::RepositoryMetadataGetResponse;
use lore_proto::lore::repository::v1::RepositoryMetadataSetRequest;
use lore_proto::lore::repository::v1::RepositoryMetadataSetResponse;
use lore_proto::lore::repository::v1::repository_get_request::Query as RepositoryGetQuery;

#[test]
fn v1_repository_request_response_types_default() {
    let _ = RepositoryCreateRequest::default();
    let _ = RepositoryCreateResponse::default();
    let _ = RepositoryDeleteRequest::default();
    let _ = RepositoryDeleteResponse::default();
    let _ = RepositoryGetRequest::default();
    let _ = RepositoryGetResponse::default();
    let _ = RepositoryListRequest::default();
    let _ = RepositoryListResponse::default();
    let _ = RepositoryCountRequest::default();
    let _ = RepositoryCountResponse::default();
    let _ = RepositoryMetadataGetRequest::default();
    let _ = RepositoryMetadataGetResponse::default();
    let _ = RepositoryMetadataSetRequest::default();
    let _ = RepositoryMetadataSetResponse::default();
}

/// Field-shape regression net: destructuring each message + naming each
/// `oneof` variant asserts that every field name and variant on the
/// generated Rust types still exists. Renaming a proto field or
/// `oneof` variant breaks this test at compile time.
#[test]
fn v1_repository_field_shapes() {
    let RepositoryCreateRequest {
        id: _,
        name: _,
        description: _,
        default_branch_id: _,
        default_branch_name: _,
        creator: _,
    } = RepositoryCreateRequest::default();
    let RepositoryCreateResponse { repository: _ } = RepositoryCreateResponse::default();

    let RepositoryDeleteRequest { id: _ } = RepositoryDeleteRequest::default();
    let RepositoryDeleteResponse { repository: _ } = RepositoryDeleteResponse::default();

    let RepositoryGetRequest { query: _ } = RepositoryGetRequest::default();
    let _ = RepositoryGetQuery::Id(Default::default());
    let _ = RepositoryGetQuery::Name(Default::default());
    let RepositoryGetResponse { repository: _ } = RepositoryGetResponse::default();

    let RepositoryListRequest { creator: _ } = RepositoryListRequest::default();
    let RepositoryListResponse { repository: _ } = RepositoryListResponse::default();

    let RepositoryCountRequest { creator: _ } = RepositoryCountRequest::default();
    let RepositoryCountResponse { count: _ } = RepositoryCountResponse::default();

    let RepositoryMetadataGetRequest { id: _ } = RepositoryMetadataGetRequest::default();
    let RepositoryMetadataGetResponse { metadata: _ } = RepositoryMetadataGetResponse::default();

    let RepositoryMetadataSetRequest {
        id: _,
        expected: _,
        updated: _,
    } = RepositoryMetadataSetRequest::default();
    let RepositoryMetadataSetResponse { metadata: _ } = RepositoryMetadataSetResponse::default();
}
