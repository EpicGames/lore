// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore::revision_graph::LoreRevisionIdentifier;
use lore::revision_graph::RevisionForm;
use lore_base::types::BranchId;
use lore_base::types::Hash;

const NUMBER: u64 = 7;

fn hash() -> Hash {
    Hash::from([0xA5; 32])
}

fn branch() -> BranchId {
    BranchId::from([0x5A; 16])
}

/// Asserts that `identifier` fails with `InvalidArguments`.
fn assert_malformed(identifier: LoreRevisionIdentifier) {
    let result = identifier.form();
    assert!(
        result.as_ref().is_err_and(|err| err.is_invalid_arguments()),
        "{identifier:?} must fail with InvalidArguments, got {result:?}"
    );
}

/// Asserts that `identifier` reads as `expected`.
fn assert_form(identifier: LoreRevisionIdentifier, expected: RevisionForm) {
    let form = identifier
        .form()
        .unwrap_or_else(|err| panic!("{identifier:?}: {err}"));
    assert_eq!(form, expected, "{identifier:?}");
}

#[test]
fn identifier_rejects_hash_with_branch() {
    for revision_number in [0, NUMBER] {
        assert_malformed(LoreRevisionIdentifier {
            hash: hash(),
            branch: branch(),
            revision_number,
        });
    }
}

#[test]
fn identifier_rejects_hash_with_number() {
    assert_malformed(LoreRevisionIdentifier {
        hash: hash(),
        revision_number: NUMBER,
        ..Default::default()
    });
}

#[test]
fn identifier_rejects_all_zero() {
    assert_malformed(LoreRevisionIdentifier::default());
}

#[test]
fn identifier_rejects_a_number_without_a_branch() {
    assert_malformed(LoreRevisionIdentifier {
        revision_number: NUMBER,
        ..Default::default()
    });
}

#[test]
fn identifier_forms_latest_number_and_hash() {
    assert_form(
        LoreRevisionIdentifier {
            branch: branch(),
            ..Default::default()
        },
        RevisionForm::Latest(branch()),
    );
    assert_form(
        LoreRevisionIdentifier {
            branch: branch(),
            revision_number: NUMBER,
            ..Default::default()
        },
        RevisionForm::Number(branch(), NUMBER),
    );
    assert_form(
        LoreRevisionIdentifier {
            hash: hash(),
            ..Default::default()
        },
        RevisionForm::Hash(hash()),
    );
}
