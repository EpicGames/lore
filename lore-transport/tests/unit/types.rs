// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
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

mod items_from {
    use bytes::Bytes;
    use lore_base::types::Hash;

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
