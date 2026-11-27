// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Smoke test verifying lore.model.v1 carries the v1 baseline types
//! (`Branch`, `BranchPoint`, `Repository`, `RevisionIdentifier`,
//! `RevisionItem`).

use lore_proto::lore::model::v1::Branch;
use lore_proto::lore::model::v1::BranchPoint;
use lore_proto::lore::model::v1::Repository;
use lore_proto::lore::model::v1::RevisionIdentifier;
use lore_proto::lore::model::v1::RevisionItem;

#[test]
fn v1_model_types_default() {
    let branch = Branch::default();
    assert_eq!(branch.name, "");
    assert!(!branch.deleted);
    assert!(branch.stack.is_empty());

    let _ = BranchPoint::default();
    let _ = Repository::default();
    let _ = RevisionIdentifier::default();
    let _ = RevisionItem::default();
}

/// Field-shape regression net: destructuring each message asserts every
/// field name still exists on the generated Rust type. Renaming a proto
/// field (or accidentally introducing one) breaks this test at compile
/// time.
#[test]
fn v1_model_field_shapes() {
    let Branch {
        id: _,
        name: _,
        creator: _,
        category: _,
        created: _,
        latest: _,
        deleted: _,
        metadata: _,
        stack: _,
        scope: _,
    } = Branch::default();

    let BranchPoint {
        branch_id: _,
        revision_signature: _,
    } = BranchPoint::default();

    let Repository {
        id: _,
        name: _,
        description: _,
        default_branch_id: _,
        default_branch_name: _,
        creator: _,
        created: _,
        metadata: _,
    } = Repository::default();

    let RevisionIdentifier {
        branch_id: _,
        number: _,
    } = RevisionIdentifier::default();

    let RevisionItem {
        number: _,
        signature: _,
        metadata: _,
        state: _,
    } = RevisionItem::default();
}
