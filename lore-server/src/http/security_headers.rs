// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Content-type allowlisting and security headers for served content.
//!
//! Stored bytes are attacker-controlled, so serving them with a caller-chosen
//! `Content-Type` (`text/html`, `image/svg+xml`, ...) is a stored-XSS vector on
//! the Lore origin.

use std::collections::HashSet;

use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::header::CONTENT_SECURITY_POLICY;
use axum::http::header::X_CONTENT_TYPE_OPTIONS;

/// Served when the caller's `Content-Type` is not allowlisted.
const DEFAULT_SERVED_CONTENT_TYPE: &str = "application/octet-stream";

/// Deny-by-default allowlist of `Content-Type` values safe to serve verbatim.
///
/// Configurable in code via [`ContentTypeAllowlist::new`]; [`Default`] is the
/// safe built-in set.
#[derive(Clone, Debug)]
pub struct ContentTypeAllowlist {
    allowed: HashSet<String>,
}

impl ContentTypeAllowlist {
    pub fn new(types: impl IntoIterator<Item = String>) -> Self {
        Self {
            allowed: types.into_iter().map(|t| normalize(&t)).collect(),
        }
    }

    /// Matching is case- and parameter-insensitive: `text/plain; charset=utf-8`
    /// matches an allowlisted `text/plain`.
    pub fn is_allowed(&self, content_type: &str) -> bool {
        self.allowed.contains(&normalize(content_type))
    }

    /// The `Content-Type` to serve: the caller's value when allowlisted and a
    /// valid header, else `application/octet-stream`.
    pub fn coerce(&self, content_type: Option<String>) -> HeaderValue {
        match content_type {
            Some(ct) if self.is_allowed(&ct) => HeaderValue::try_from(ct)
                .unwrap_or_else(|_| HeaderValue::from_static(DEFAULT_SERVED_CONTENT_TYPE)),
            Some(ct) => {
                tracing::warn!(
                    requested = %ct,
                    served = DEFAULT_SERVED_CONTENT_TYPE,
                    "coerced disallowed content-type"
                );
                HeaderValue::from_static(DEFAULT_SERVED_CONTENT_TYPE)
            }
            None => HeaderValue::from_static(DEFAULT_SERVED_CONTENT_TYPE),
        }
    }
}

impl Default for ContentTypeAllowlist {
    fn default() -> Self {
        Self::new(
            [
                "application/octet-stream",
                "image/png",
                "image/jpeg",
                "image/gif",
                "image/webp",
                "application/pdf",
                "text/plain",
            ]
            .into_iter()
            .map(String::from),
        )
    }
}

fn normalize(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// `nosniff` stops MIME sniffing; the sandboxed CSP blocks script execution
/// even if a bad type slips through.
pub fn apply_security_headers(headers: &mut HeaderMap) {
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; sandbox"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_allows_safe_types() {
        let allowlist = ContentTypeAllowlist::default();
        assert!(allowlist.is_allowed("image/png"));
        assert!(allowlist.is_allowed("image/jpeg"));
        assert!(allowlist.is_allowed("application/pdf"));
        assert!(allowlist.is_allowed("application/octet-stream"));
    }

    #[test]
    fn default_disallows_executable_types() {
        let allowlist = ContentTypeAllowlist::default();
        assert!(!allowlist.is_allowed("text/html"));
        assert!(!allowlist.is_allowed("image/svg+xml"));
        assert!(!allowlist.is_allowed("application/xhtml+xml"));
    }

    #[test]
    fn is_allowed_ignores_case() {
        let allowlist = ContentTypeAllowlist::default();
        assert!(allowlist.is_allowed("IMAGE/PNG"));
        assert!(!allowlist.is_allowed("TEXT/HTML"));
    }

    #[test]
    fn is_allowed_strips_parameters() {
        let allowlist = ContentTypeAllowlist::default();
        assert!(allowlist.is_allowed("text/plain; charset=utf-8"));
        assert!(!allowlist.is_allowed("text/html; charset=utf-8"));
    }

    #[test]
    fn new_accepts_custom_set() {
        let allowlist = ContentTypeAllowlist::new(["application/wasm".to_string()]);
        assert!(allowlist.is_allowed("application/wasm"));
        assert!(!allowlist.is_allowed("image/png"));
    }

    #[test]
    fn apply_security_headers_sets_nosniff_and_csp() {
        let mut headers = HeaderMap::new();
        apply_security_headers(&mut headers);
        assert_eq!(headers.get(X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
        assert_eq!(
            headers.get(CONTENT_SECURITY_POLICY).unwrap(),
            "default-src 'none'; sandbox"
        );
    }

    // ----- adversarial: allowlist bypass attempts -----

    /// Dangerous types must be rejected regardless of case, surrounding
    /// whitespace, or trailing parameters.
    #[test]
    fn adversarial_dangerous_types_rejected_in_all_forms() {
        let a = ContentTypeAllowlist::default();
        for t in [
            "TEXT/HTML",
            "  text/html  ",
            "text/html; charset=utf-8",
            "text/html ;junk",
            "\ttext/html\t",
            "image/svg+xml",
            "IMAGE/SVG+XML",
            "image/svg+xml; charset=utf-8",
            "application/xhtml+xml",
            "text/xml",
            "application/xml",
        ] {
            assert!(!a.is_allowed(t), "is_allowed must reject {t:?}");
            assert_eq!(
                a.coerce(Some(t.to_string())),
                "application/octet-stream",
                "coerce must neutralize {t:?}"
            );
        }
    }

    /// A leading `;` yields an empty media type — must fail closed, and must not
    /// let a disallowed type hidden in the "parameters" through.
    #[test]
    fn adversarial_leading_semicolon_fails_closed() {
        let a = ContentTypeAllowlist::default();
        assert!(!a.is_allowed(";text/html"));
        assert!(!a.is_allowed(";image/png"));
        assert_eq!(
            a.coerce(Some(";text/html".to_string())),
            "application/octet-stream"
        );
    }

    /// Empty / whitespace-only / missing content types must never be served
    /// verbatim (the MIME-sniff vector).
    #[test]
    fn adversarial_empty_and_missing_coerced() {
        let a = ContentTypeAllowlist::default();
        assert_eq!(a.coerce(None), "application/octet-stream");
        assert_eq!(a.coerce(Some(String::new())), "application/octet-stream");
        assert_eq!(
            a.coerce(Some("   ".to_string())),
            "application/octet-stream"
        );
    }

    /// A disallowed type in the *parameters* of an allowed media type is
    /// harmless: the browser keys on the media type (image/png) just as the
    /// allowlist does. Allowed, served as-is.
    #[test]
    fn adversarial_param_smuggling_stays_at_media_type() {
        let a = ContentTypeAllowlist::default();
        assert!(a.is_allowed("image/png; x=text/html"));
        assert_eq!(
            a.coerce(Some("image/png; x=text/html".to_string())),
            "image/png; x=text/html"
        );
    }

    /// Comma-separated smuggling is not a valid single media type and must not
    /// be treated as the allowed prefix.
    #[test]
    fn adversarial_comma_smuggling_rejected() {
        let a = ContentTypeAllowlist::default();
        assert!(!a.is_allowed("image/png,text/html"));
        assert_eq!(
            a.coerce(Some("image/png,text/html".to_string())),
            "application/octet-stream"
        );
    }

    /// A pre-poisoned header map (attacker-weakened CSP appended earlier) must
    /// be fully replaced — never left with a permissive extra CSP value.
    #[test]
    fn adversarial_security_headers_replace_poisoned_values() {
        let mut headers = HeaderMap::new();
        headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nope"));
        headers.append(
            CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("default-src *"),
        );
        headers.append(
            CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("script-src 'unsafe-inline'"),
        );

        apply_security_headers(&mut headers);

        assert_eq!(headers.get(X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
        let csp: Vec<_> = headers.get_all(CONTENT_SECURITY_POLICY).iter().collect();
        assert_eq!(csp.len(), 1, "must not leave permissive CSP values behind");
        assert_eq!(csp[0], "default-src 'none'; sandbox");
    }
}
