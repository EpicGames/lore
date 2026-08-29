// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Shared helpers for the HTTP module's tests.

use crate::http::security_headers::ContentTypeAllowlist;
use crate::http::security_headers::ContentTypePolicy;
use crate::http::server::PresignConfig;

pub(crate) fn content_type_policy(extra: &[&str], denied: &[&str]) -> ContentTypePolicy {
    ContentTypePolicy {
        extra: extra.iter().map(|t| (*t).to_string()).collect(),
        denied: denied.iter().map(|t| (*t).to_string()).collect(),
    }
}

pub(crate) fn presign_config() -> PresignConfig {
    presign_config_with_policy(ContentTypePolicy::default())
}

pub(crate) fn presign_config_with_policy(policy: ContentTypePolicy) -> PresignConfig {
    let key_bytes = [0u8; 32];
    PresignConfig {
        hmac_key: ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &key_bytes),
        key_id: "test_key_id_1234".to_string(),
        min_ttl_seconds: 1,
        default_ttl_seconds: 3600,
        max_ttl_seconds: 86400,
        content_type_allowlist: ContentTypeAllowlist::try_from_policy(&policy)
            .expect("test policy should resolve"),
    }
}
