// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Smoke test verifying `lore.revision.v1` carries the 8 RPCs' request /
//! response messages.

use lore_proto::lore::revision::v1::BranchCreateRequest;
use lore_proto::lore::revision::v1::BranchCreateResponse;
use lore_proto::lore::revision::v1::BranchDeleteRequest;
use lore_proto::lore::revision::v1::BranchDeleteResponse;
use lore_proto::lore::revision::v1::BranchGetRequest;
use lore_proto::lore::revision::v1::BranchGetResponse;
use lore_proto::lore::revision::v1::BranchListRequest;
use lore_proto::lore::revision::v1::BranchListResponse;
use lore_proto::lore::revision::v1::BranchMetadataGetRequest;
use lore_proto::lore::revision::v1::BranchMetadataGetResponse;
use lore_proto::lore::revision::v1::BranchMetadataSetRequest;
use lore_proto::lore::revision::v1::BranchMetadataSetResponse;
use lore_proto::lore::revision::v1::BranchPushRequest;
use lore_proto::lore::revision::v1::BranchPushResponse;
use lore_proto::lore::revision::v1::RevisionListRequest;
use lore_proto::lore::revision::v1::RevisionListResponse;
use lore_proto::lore::revision::v1::branch_get_request::Query as BranchGetQuery;
use lore_proto::lore::revision::v1::revision_list_request::Start as RevisionListStart;

#[test]
fn v1_revision_request_response_types_default() {
    let _ = BranchCreateRequest::default();
    let _ = BranchCreateResponse::default();
    let _ = BranchDeleteRequest::default();
    let _ = BranchDeleteResponse::default();
    let _ = BranchGetRequest::default();
    let _ = BranchGetResponse::default();
    let _ = BranchListRequest::default();
    let _ = BranchListResponse::default();
    let _ = BranchPushRequest::default();
    let _ = BranchPushResponse::default();
    let _ = BranchMetadataGetRequest::default();
    let _ = BranchMetadataGetResponse::default();
    let _ = BranchMetadataSetRequest::default();
    let _ = BranchMetadataSetResponse::default();
    let _ = RevisionListRequest::default();
    let _ = RevisionListResponse::default();
}

/// Field-shape regression net: destructuring each message + naming each
/// `oneof` variant asserts that every field name and variant on the
/// generated Rust types still exists. Renaming a proto field or
/// `oneof` variant breaks this test at compile time.
#[test]
fn v1_revision_field_shapes() {
    let BranchCreateRequest {
        id: _,
        name: _,
        creator: _,
        category: _,
        stack: _,
        scope: _,
    } = BranchCreateRequest::default();
    let BranchCreateResponse { branch: _ } = BranchCreateResponse::default();

    let BranchDeleteRequest { id: _ } = BranchDeleteRequest::default();
    let BranchDeleteResponse { branch: _ } = BranchDeleteResponse::default();

    let BranchGetRequest { query: _ } = BranchGetRequest::default();
    let _ = BranchGetQuery::Id(Default::default());
    let _ = BranchGetQuery::Name(Default::default());
    let BranchGetResponse { branch: _ } = BranchGetResponse::default();

    let BranchListRequest {
        creator: _,
        include_deleted: _,
    } = BranchListRequest::default();
    let BranchListResponse { branch: _ } = BranchListResponse::default();

    let BranchPushRequest {
        id: _,
        revision_signature: _,
        force: _,
        fast_forward_merge: _,
    } = BranchPushRequest::default();
    let BranchPushResponse {
        revision_signature: _,
        revision_number: _,
        fast_forward_merged: _,
        message: _,
    } = BranchPushResponse::default();

    let BranchMetadataGetRequest { id: _ } = BranchMetadataGetRequest::default();
    let BranchMetadataGetResponse { metadata: _ } = BranchMetadataGetResponse::default();
    let BranchMetadataSetRequest {
        id: _,
        expected: _,
        updated: _,
    } = BranchMetadataSetRequest::default();
    let BranchMetadataSetResponse { metadata: _ } = BranchMetadataSetResponse::default();

    let RevisionListRequest { start: _ } = RevisionListRequest::default();
    let _ = RevisionListStart::Identifier(Default::default());
    let _ = RevisionListStart::Signature(Default::default());
    let RevisionListResponse {
        items: _,
        signature_forward: _,
        signature_backward: _,
    } = RevisionListResponse::default();
}
