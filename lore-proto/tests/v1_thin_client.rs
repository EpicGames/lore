// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Smoke test verifying `lore.thin_client.v1` carries the model types and
//! the 4 RPCs' request / response messages.

use lore_proto::lore::thin_client::v1::Action;
use lore_proto::lore::thin_client::v1::ContentDiffChunkResponse;
use lore_proto::lore::thin_client::v1::ContentDiffHeader;
use lore_proto::lore::thin_client::v1::ContentDiffRequest;
use lore_proto::lore::thin_client::v1::ContentDiffResponse;
use lore_proto::lore::thin_client::v1::DiffChange;
use lore_proto::lore::thin_client::v1::DiffConflict;
use lore_proto::lore::thin_client::v1::DiffPartition;
use lore_proto::lore::thin_client::v1::Metadata;
use lore_proto::lore::thin_client::v1::MetadataType;
use lore_proto::lore::thin_client::v1::NodeType;
use lore_proto::lore::thin_client::v1::Revision;
use lore_proto::lore::thin_client::v1::RevisionDiffHeader;
use lore_proto::lore::thin_client::v1::RevisionDiffRequest;
use lore_proto::lore::thin_client::v1::RevisionDiffResponse;
use lore_proto::lore::thin_client::v1::RevisionInfoRequest;
use lore_proto::lore::thin_client::v1::RevisionInfoResponse;
use lore_proto::lore::thin_client::v1::RevisionTreeHeader;
use lore_proto::lore::thin_client::v1::RevisionTreeRequest;
use lore_proto::lore::thin_client::v1::RevisionTreeResponse;
use lore_proto::lore::thin_client::v1::TreeNode;
use lore_proto::lore::thin_client::v1::content_diff_response::Payload as ContentDiffPayload;
use lore_proto::lore::thin_client::v1::revision::Parent as RevisionParent;
use lore_proto::lore::thin_client::v1::revision_diff_request::QueryFrom as RevisionDiffQueryFrom;
use lore_proto::lore::thin_client::v1::revision_diff_request::QueryTo as RevisionDiffQueryTo;
use lore_proto::lore::thin_client::v1::revision_diff_response::Payload as RevisionDiffPayload;
use lore_proto::lore::thin_client::v1::revision_info_request::Query as RevisionInfoQuery;
use lore_proto::lore::thin_client::v1::revision_tree_request::Query as RevisionTreeQuery;
use lore_proto::lore::thin_client::v1::revision_tree_response::Payload as RevisionTreePayload;

#[test]
fn v1_thin_client_model_types_default() {
    let _ = ContentDiffRequest::default();
    let _ = ContentDiffResponse::default();
    let _ = ContentDiffHeader::default();
    let _ = ContentDiffChunkResponse::default();
    let _ = DiffChange::default();
    let _ = DiffConflict::default();
    let _ = DiffPartition::default();
    let _ = TreeNode::default();
    let _ = Revision::default();
    let _ = Metadata::default();

    assert_eq!(NodeType::Directory as i32, 0);
    assert_eq!(Action::Keep as i32, 0);
    assert_eq!(MetadataType::Address as i32, 0);
}

#[test]
fn v1_thin_client_service_types_default() {
    let _ = RevisionInfoRequest::default();
    let _ = RevisionInfoResponse::default();
    let _ = RevisionDiffRequest::default();
    let _ = RevisionDiffResponse::default();
    let _ = RevisionDiffHeader::default();
    let _ = RevisionTreeRequest::default();
    let _ = RevisionTreeResponse::default();
    let _ = RevisionTreeHeader::default();
}

/// Field-shape regression net: destructuring each message + naming each
/// `oneof` variant asserts that every field name and variant on the
/// generated Rust types still exists. Renaming a proto field or
/// `oneof` variant breaks this test at compile time.
#[test]
fn v1_thin_client_field_shapes() {
    // ContentDiff
    let ContentDiffRequest {
        address_from: _,
        address_to: _,
        address_base: _,
        context_lines: _,
        ignore_whitespace_eol: _,
        ignore_whitespace_inline: _,
        max_diff_size: _,
    } = ContentDiffRequest::default();
    let ContentDiffResponse { payload: _ } = ContentDiffResponse::default();
    let _ = ContentDiffPayload::Header(Default::default());
    let _ = ContentDiffPayload::Chunk(Default::default());
    let ContentDiffHeader {
        lines_added: _,
        lines_deleted: _,
        binary: _,
        truncated: _,
        has_conflicts: _,
        conflict_count: _,
    } = ContentDiffHeader::default();
    let ContentDiffChunkResponse { diff: _ } = ContentDiffChunkResponse::default();

    // Diff vocabulary + tree
    let DiffChange {
        path: _,
        path_from: _,
        action: _,
        node_type: _,
        content_from: _,
        content_to: _,
        automerged: _,
        link_repository_index: _,
    } = DiffChange::default();
    let DiffConflict {
        change_from: _,
        change_to: _,
    } = DiffConflict::default();
    let DiffPartition {
        index: _,
        link_partition: _,
    } = DiffPartition::default();
    let TreeNode {
        path: _,
        node_type: _,
        address: _,
    } = TreeNode::default();

    // Revision + nested Parent + Metadata
    let Revision {
        signature: _,
        identifier: _,
        commit_message: _,
        timestamp: _,
        created_by: _,
        committed_by: _,
        metadata: _,
        parent_self: _,
        parent_other: _,
        number: _,
    } = Revision::default();
    let RevisionParent {
        signature: _,
        identifier: _,
    } = RevisionParent::default();
    let Metadata {
        key: _,
        value: _,
        metadata_type: _,
    } = Metadata::default();

    // RevisionInfo
    let RevisionInfoRequest { query: _ } = RevisionInfoRequest::default();
    let _ = RevisionInfoQuery::Identifier(Default::default());
    let _ = RevisionInfoQuery::Signature(Default::default());
    let RevisionInfoResponse { revision: _ } = RevisionInfoResponse::default();

    // RevisionDiff
    let RevisionDiffRequest {
        query_from: _,
        query_to: _,
        autoresolve: _,
    } = RevisionDiffRequest::default();
    let _ = RevisionDiffQueryFrom::IdentifierFrom(Default::default());
    let _ = RevisionDiffQueryFrom::SignatureFrom(Default::default());
    let _ = RevisionDiffQueryTo::IdentifierTo(Default::default());
    let _ = RevisionDiffQueryTo::SignatureTo(Default::default());
    let RevisionDiffHeader {
        identifier_from: _,
        signature_from: _,
        identifier_to: _,
        signature_to: _,
        identifier_base: _,
        signature_base: _,
    } = RevisionDiffHeader::default();
    let RevisionDiffResponse { payload: _ } = RevisionDiffResponse::default();
    let _ = RevisionDiffPayload::Header(Default::default());
    let _ = RevisionDiffPayload::Change(Default::default());
    let _ = RevisionDiffPayload::Conflict(Default::default());
    let _ = RevisionDiffPayload::Partition(Default::default());

    // RevisionTree
    let RevisionTreeRequest {
        query: _,
        path_prefix: _,
        max_depth: _,
    } = RevisionTreeRequest::default();
    let _ = RevisionTreeQuery::Identifier(Default::default());
    let _ = RevisionTreeQuery::Signature(Default::default());
    let RevisionTreeHeader {
        identifier: _,
        signature: _,
    } = RevisionTreeHeader::default();
    let RevisionTreeResponse { payload: _ } = RevisionTreeResponse::default();
    let _ = RevisionTreePayload::Header(Default::default());
    let _ = RevisionTreePayload::Node(Default::default());
}
