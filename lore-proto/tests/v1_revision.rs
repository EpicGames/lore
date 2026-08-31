// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Smoke test verifying `lore.revision.v1` carries the RPC request /
//! response messages.

use lore_proto::lore::revision::v1::BranchCreateRequest;
use lore_proto::lore::revision::v1::BranchCreateResponse;
use lore_proto::lore::revision::v1::BranchDeleteRequest;
use lore_proto::lore::revision::v1::BranchDeleteResponse;
use lore_proto::lore::revision::v1::BranchGetRequest;
use lore_proto::lore::revision::v1::BranchGetResponse;
use lore_proto::lore::revision::v1::BranchListRequest;
use lore_proto::lore::revision::v1::BranchListResponse;
use lore_proto::lore::revision::v1::BranchMergeConflict;
use lore_proto::lore::revision::v1::BranchMergeRequest;
use lore_proto::lore::revision::v1::BranchMergeResolution;
use lore_proto::lore::revision::v1::BranchMergeResponse;
use lore_proto::lore::revision::v1::BranchMergeSide;
use lore_proto::lore::revision::v1::BranchMetadataGetRequest;
use lore_proto::lore::revision::v1::BranchMetadataGetResponse;
use lore_proto::lore::revision::v1::BranchMetadataSetRequest;
use lore_proto::lore::revision::v1::BranchMetadataSetResponse;
use lore_proto::lore::revision::v1::BranchPushRequest;
use lore_proto::lore::revision::v1::BranchPushResponse;
use lore_proto::lore::revision::v1::RevisionCreateDeletePath;
use lore_proto::lore::revision::v1::RevisionCreateDirectory;
use lore_proto::lore::revision::v1::RevisionCreateMetadataEntry;
use lore_proto::lore::revision::v1::RevisionCreateMovePath;
use lore_proto::lore::revision::v1::RevisionCreateOperation;
use lore_proto::lore::revision::v1::RevisionCreatePutFile;
use lore_proto::lore::revision::v1::RevisionCreateRequest;
use lore_proto::lore::revision::v1::RevisionCreateResponse;
use lore_proto::lore::revision::v1::RevisionListRequest;
use lore_proto::lore::revision::v1::RevisionListResponse;
use lore_proto::lore::revision::v1::branch_get_request::Query as BranchGetQuery;
use lore_proto::lore::revision::v1::branch_merge_resolution::Resolution as BranchMergeResolutionKind;
use lore_proto::lore::revision::v1::revision_create_operation::Op as RevisionCreateOp;
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
    let _ = BranchMergeRequest::default();
    let _ = BranchMergeResponse::default();
    let _ = BranchMetadataGetRequest::default();
    let _ = BranchMetadataGetResponse::default();
    let _ = BranchMetadataSetRequest::default();
    let _ = BranchMetadataSetResponse::default();
    let _ = RevisionListRequest::default();
    let _ = RevisionListResponse::default();
    let _ = RevisionCreateRequest::default();
    let _ = RevisionCreateResponse::default();
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

    let BranchMergeRequest {
        request_id: _,
        branch_id_target: _,
        revision_signature_target: _,
        branch_id_source: _,
        revision_signature_source: _,
        commit_message: _,
        resolutions: _,
    } = BranchMergeRequest::default();
    let BranchMergeResolution {
        conflict_id: _,
        resolution: _,
    } = BranchMergeResolution::default();
    let _ = BranchMergeResolutionKind::Side(BranchMergeSide::Target as i32);
    let BranchMergeConflict {
        conflict_id: _,
        path_source: _,
        path_target: _,
    } = BranchMergeConflict::default();
    let BranchMergeResponse {
        outcome: _,
        revision_signature: _,
        revision_number: _,
        revision_signature_base: _,
        unresolved_conflicts: _,
    } = BranchMergeResponse::default();

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

    let RevisionCreateRequest {
        request_id: _,
        branch_id: _,
        revision_signature_base: _,
        commit_message: _,
        metadata: _,
        operations: _,
    } = RevisionCreateRequest::default();
    let RevisionCreateMetadataEntry {
        key: _,
        value: _,
        format: _,
    } = RevisionCreateMetadataEntry::default();
    let RevisionCreateOperation { op: _ } = RevisionCreateOperation::default();
    let _ = RevisionCreateOp::PutFile(Default::default());
    let _ = RevisionCreateOp::CreateDirectory(Default::default());
    let _ = RevisionCreateOp::DeletePath(Default::default());
    let _ = RevisionCreateOp::MovePath(Default::default());
    let RevisionCreatePutFile {
        path: _,
        mode: _,
        address: _,
    } = RevisionCreatePutFile::default();
    let RevisionCreateDirectory { path: _, mode: _ } = RevisionCreateDirectory::default();
    let RevisionCreateDeletePath { path: _ } = RevisionCreateDeletePath::default();
    let RevisionCreateMovePath {
        source: _,
        destination: _,
    } = RevisionCreateMovePath::default();
    let RevisionCreateResponse {
        revision_signature: _,
        revision_number: _,
    } = RevisionCreateResponse::default();
}
