// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::types::BranchId;
use lore_base::types::BranchPoint;
use lore_base::types::Hash;
use lore_proto::lore::model::v1 as model;
use lore_transport::types::*;

const FALLBACK: &str = "grpc://fallback.example:1234";

fn env_with(endpoint: Endpoint) -> EnvironmentConfig {
    EnvironmentConfig {
        endpoint: Some(endpoint),
        config: None,
        oidc: None,
    }
}

#[test]
fn service_url_returns_override_when_set() {
    let env = env_with(Endpoint {
        storage_url: Some("quic://storage.example:7000".into()),
        ..Default::default()
    });
    assert_eq!(env.storage_url(FALLBACK), "quic://storage.example:7000");
}

#[test]
fn service_url_falls_back_when_field_is_none() {
    let env = env_with(Endpoint::default());
    assert_eq!(env.storage_url(FALLBACK), FALLBACK);
    assert_eq!(env.revision_url(FALLBACK), FALLBACK);
    assert_eq!(env.lock_url(FALLBACK), FALLBACK);
    assert_eq!(env.repository_url(FALLBACK), FALLBACK);
    assert_eq!(env.notification_url(FALLBACK), FALLBACK);
}

#[test]
fn service_url_falls_back_when_field_is_empty_string() {
    // An empty Option<String> from proto decoding must behave identically
    // to None — the field is "unset."
    let env = env_with(Endpoint {
        storage_url: Some(String::new()),
        revision_url: Some(String::new()),
        ..Default::default()
    });
    assert_eq!(env.storage_url(FALLBACK), FALLBACK);
    assert_eq!(env.revision_url(FALLBACK), FALLBACK);
}

#[test]
fn service_url_falls_back_when_endpoint_section_missing() {
    let env = EnvironmentConfig {
        endpoint: None,
        config: None,
        oidc: None,
    };
    assert_eq!(env.storage_url(FALLBACK), FALLBACK);
    assert_eq!(env.repository_url(FALLBACK), FALLBACK);
}

#[test]
fn per_service_overrides_are_independent() {
    // Only some services have overrides; the others must fall back.
    let env = env_with(Endpoint {
        storage_url: Some("quic://storage.example:7000".into()),
        lock_url: Some("grpc://lock.example:8000".into()),
        ..Default::default()
    });
    assert_eq!(env.storage_url(FALLBACK), "quic://storage.example:7000");
    assert_eq!(env.lock_url(FALLBACK), "grpc://lock.example:8000");
    assert_eq!(env.revision_url(FALLBACK), FALLBACK);
    assert_eq!(env.repository_url(FALLBACK), FALLBACK);
    assert_eq!(env.notification_url(FALLBACK), FALLBACK);
}

/// A branch with every field set, and the record it converts to.
pub(crate) fn full_branch() -> (model::Branch, BranchRecord) {
    let record = BranchRecord {
        id: BranchId::from([1; 16]),
        name: "feature".to_string(),
        category: "release".to_string(),
        creator: "alice".to_string(),
        created: 1_700_000_000_123,
        latest: Hash::from([2; 32]),
        metadata: Hash::from([3; 32]),
        deleted: true,
        stack: vec![
            BranchPoint {
                branch: BranchId::from([4; 16]),
                revision: Hash::from([5; 32]),
            },
            BranchPoint {
                branch: BranchId::from([6; 16]),
                revision: Hash::from([7; 32]),
            },
        ],
    };
    let branch = model::Branch {
        id: record.id.into(),
        name: record.name.clone(),
        creator: record.creator.clone(),
        category: record.category.clone(),
        created: record.created,
        latest: record.latest.into(),
        deleted: record.deleted,
        metadata: record.metadata.into(),
        stack: record.stack.iter().map(model::BranchPoint::from).collect(),
    };
    (branch, record)
}

mod branch_record {
    use lore_transport::grpc::revision_client::branch_record_from_v1;

    use super::*;

    #[test]
    fn branch_record_from_v1_keeps_every_field() {
        let (branch, expected) = full_branch();
        assert_eq!(branch_record_from_v1(branch), expected);
    }
}

mod items_from {
    use bytes::Bytes;

    use super::*;

    fn page(numbers: &[u8]) -> RevisionListResponse {
        RevisionListResponse {
            items: numbers
                .iter()
                .map(|&number| RevisionItem {
                    number: u64::from(number),
                    signature: Hash::from([number; 32]),
                    metadata: Hash::default(),
                    state: Bytes::new(),
                })
                .collect(),
            next_revision: Hash::default(),
            previous_revision: Hash::default(),
        }
    }

    fn numbers(items: &[RevisionItem]) -> Vec<u64> {
        items.iter().map(|item| item.number).collect()
    }

    #[test]
    fn a_page_starting_at_its_anchor_is_kept_whole() {
        assert_eq!(
            numbers(page(&[3, 2, 1]).items_from(Hash::from([3; 32]))),
            [3, 2, 1]
        );
    }

    /// The revisions above the anchor in a page aligned on a wider boundary are not its
    /// ancestors.
    #[test]
    fn a_page_starting_above_its_anchor_loses_the_revisions_above_it() {
        assert_eq!(
            numbers(page(&[6, 5, 4, 3, 2, 1]).items_from(Hash::from([3; 32]))),
            [3, 2, 1]
        );
    }

    #[test]
    fn a_page_not_holding_its_anchor_holds_nothing_from_it() {
        assert!(page(&[6, 5, 4]).items_from(Hash::from([3; 32])).is_empty());
    }
}
