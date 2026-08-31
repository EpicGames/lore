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
use thiserror::Error;

/// Served when the caller's `Content-Type` is not allowlisted.
const DEFAULT_SERVED_CONTENT_TYPE: &str = "application/octet-stream";

/// Built-in allowlist. Config adjusts it through [`ContentTypePolicy`] rather than
/// restating it.
pub const DEFAULT_ALLOWED_CONTENT_TYPES: &[&str] = &[
    "application/octet-stream",
    // S3 sets this on objects uploaded without a Content-Type, so callers that
    // forward S3 metadata send it here. It is not a registered type but means the
    // same as application/octet-stream. Kept as-is, not rewritten, so callers read
    // back what they sent. Browsers do not render it, and every response sets
    // nosniff.
    "binary/octet-stream",
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "application/pdf",
    "text/plain",
];

/// Types config may never allow: a browser executes these as a document, making
/// redeemed content a stored-XSS vector on the Lore origin.
const NEVER_ALLOWED_CONTENT_TYPES: &[&str] = &[
    "text/html",
    "application/xhtml+xml",
    "image/svg+xml",
    "text/xml",
    "application/xml",
    "application/xslt+xml",
    "text/javascript",
    "application/javascript",
    "application/ecmascript",
];

/// Configured adjustments to [`DEFAULT_ALLOWED_CONTENT_TYPES`].
#[derive(Clone, Debug, Default)]
pub struct ContentTypePolicy {
    pub extra: Vec<String>,
    /// Applied after `extra`, so deny wins.
    pub denied: Vec<String>,
}

/// Which list a rejected entry came from. The caller maps this to a config field
/// name, keeping this module independent of the config schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyField {
    Extra,
    Denied,
}

#[derive(Debug, Error)]
pub enum ContentTypePolicyError {
    #[error("entry {entry:?} is empty")]
    Empty { field: PolicyField, entry: String },
    #[error(
        "entry {entry:?} must be a single media type with no parameters; list each type on its own"
    )]
    HasParameterOrList { field: PolicyField, entry: String },
    #[error("entry {entry:?} must not contain a wildcard; list each type explicitly")]
    Wildcard { field: PolicyField, entry: String },
    #[error("entry {entry:?} is not a type/subtype media type")]
    NotTypeSubtype { field: PolicyField, entry: String },
    #[error("entry {entry:?} is not permitted: a browser can execute it as a document")]
    NeverAllowed { field: PolicyField, entry: String },
}

impl ContentTypePolicyError {
    /// Returns the list the rejected entry came from.
    pub fn field(&self) -> PolicyField {
        match self {
            Self::Empty { field, .. }
            | Self::HasParameterOrList { field, .. }
            | Self::Wildcard { field, .. }
            | Self::NotTypeSubtype { field, .. }
            | Self::NeverAllowed { field, .. } => *field,
        }
    }
}

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

    pub fn try_from_policy(policy: &ContentTypePolicy) -> Result<Self, ContentTypePolicyError> {
        for entry in &policy.extra {
            validate_shape(PolicyField::Extra, entry)?;

            if is_never_allowed(entry) {
                return Err(ContentTypePolicyError::NeverAllowed {
                    field: PolicyField::Extra,
                    entry: entry.clone(),
                });
            }
        }
        // A floor type here is a redundant no-op, not an error: it is already absent.
        for entry in &policy.denied {
            validate_shape(PolicyField::Denied, entry)?;
        }

        let denied: HashSet<String> = policy.denied.iter().map(|d| normalize(d)).collect();

        Ok(Self::new(
            DEFAULT_ALLOWED_CONTENT_TYPES
                .iter()
                .map(|content_type| (*content_type).to_string())
                .chain(policy.extra.iter().cloned())
                .filter(|content_type| !denied.contains(&normalize(content_type))),
        ))
    }

    /// The resolved set, sorted. Logged at startup.
    pub fn allowed_types(&self) -> Vec<String> {
        let mut types: Vec<String> = self.allowed.iter().cloned().collect();
        types.sort_unstable();
        types
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
            DEFAULT_ALLOWED_CONTENT_TYPES
                .iter()
                .map(|content_type| (*content_type).to_string()),
        )
    }
}

fn is_never_allowed(content_type: &str) -> bool {
    NEVER_ALLOWED_CONTENT_TYPES.contains(&normalize(content_type).as_str())
}

/// Rejects anything [`normalize`] would not reduce to a canonical `type/subtype`.
fn validate_shape(field: PolicyField, entry: &str) -> Result<(), ContentTypePolicyError> {
    let trimmed = entry.trim();

    if trimmed.is_empty() {
        return Err(ContentTypePolicyError::Empty {
            field,
            entry: entry.to_string(),
        });
    }
    if trimmed.contains(';') || trimmed.contains(',') {
        return Err(ContentTypePolicyError::HasParameterOrList {
            field,
            entry: entry.to_string(),
        });
    }
    if trimmed.contains('*') {
        return Err(ContentTypePolicyError::Wildcard {
            field,
            entry: entry.to_string(),
        });
    }

    let Some((media_type, subtype)) = trimmed.split_once('/') else {
        return Err(ContentTypePolicyError::NotTypeSubtype {
            field,
            entry: entry.to_string(),
        });
    };
    if !is_token(media_type) || !is_token(subtype) {
        return Err(ContentTypePolicyError::NotTypeSubtype {
            field,
            entry: entry.to_string(),
        });
    }

    Ok(())
}

/// RFC 9110 `token`, the character set a media type and subtype are drawn from.
/// Excludes whitespace, control characters, and everything non-ASCII.
fn is_token(part: &str) -> bool {
    !part.is_empty() && part.bytes().all(is_tchar)
}

fn is_tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
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
    use crate::http::test_utils::content_type_policy;

    #[test]
    fn default_allows_safe_types() {
        let allowlist = ContentTypeAllowlist::default();
        assert!(allowlist.is_allowed("image/png"));
        assert!(allowlist.is_allowed("image/jpeg"));
        assert!(allowlist.is_allowed("application/pdf"));
        assert!(allowlist.is_allowed("application/octet-stream"));
    }

    /// The S3 default type is allowed and served verbatim, in the same
    /// case- and parameter-insensitive way as every other entry.
    #[test]
    fn default_allows_s3_binary_octet_stream() {
        let allowlist = ContentTypeAllowlist::default();
        assert!(allowlist.is_allowed("binary/octet-stream"));
        assert!(allowlist.is_allowed("BINARY/OCTET-STREAM"));
        assert!(allowlist.is_allowed("binary/octet-stream; charset=utf-8"));
        assert_eq!(
            allowlist.coerce(Some("binary/octet-stream".to_string())),
            "binary/octet-stream"
        );
    }

    /// Allowing one `binary/*` subtype must not admit any other.
    #[test]
    fn binary_top_level_type_is_not_wildcarded() {
        let allowlist = ContentTypeAllowlist::default();
        assert!(!allowlist.is_allowed("binary/html"));
        assert!(!allowlist.is_allowed("binary/octet-stream,text/html"));
        assert_eq!(
            allowlist.coerce(Some("binary/octet-stream,text/html".to_string())),
            "application/octet-stream"
        );
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

    // ----- configured content-type policy -----

    fn builtin_sorted() -> Vec<String> {
        let mut types: Vec<String> = DEFAULT_ALLOWED_CONTENT_TYPES
            .iter()
            .map(|t| (*t).to_string())
            .collect();
        types.sort_unstable();
        types
    }

    /// An empty policy is what an absent config key resolves to, so it must leave
    /// the served set exactly as it was before this feature existed.
    #[test]
    fn empty_policy_resolves_to_builtin_set() {
        let allowlist = ContentTypeAllowlist::try_from_policy(&ContentTypePolicy::default())
            .expect("empty policy must resolve");
        assert_eq!(allowlist.allowed_types(), builtin_sorted());
    }

    /// `extra` adds to the built-in set rather than replacing it: an operator who
    /// names one type keeps all eight built-ins.
    #[test]
    fn extra_types_are_added_to_builtin_set() {
        let allowlist =
            ContentTypeAllowlist::try_from_policy(&content_type_policy(&["application/zip"], &[]))
                .unwrap();

        assert!(allowlist.is_allowed("application/zip"));
        for builtin in DEFAULT_ALLOWED_CONTENT_TYPES {
            assert!(
                allowlist.is_allowed(builtin),
                "extra must not drop built-in {builtin:?}"
            );
        }
    }

    /// `denied` removes a built-in type, and redeem then coerces it.
    #[test]
    fn denied_types_are_removed_from_builtin_set() {
        let allowlist =
            ContentTypeAllowlist::try_from_policy(&content_type_policy(&[], &["application/pdf"]))
                .unwrap();

        assert!(!allowlist.is_allowed("application/pdf"));
        assert_eq!(
            allowlist.coerce(Some("application/pdf".to_string())),
            "application/octet-stream"
        );
        assert!(
            allowlist.is_allowed("image/png"),
            "must remove only the named type"
        );
    }

    /// Deny is applied after extra, so a type in both lists is not allowed. Asserts
    /// the extra-only case first, so the test cannot pass just because the type is
    /// absent from the built-in set.
    #[test]
    fn denied_wins_over_extra_for_the_same_type() {
        let extra_only =
            ContentTypeAllowlist::try_from_policy(&content_type_policy(&["application/zip"], &[]))
                .unwrap();
        assert!(
            extra_only.is_allowed("application/zip"),
            "precondition: extra alone must allow the type"
        );

        let both = ContentTypeAllowlist::try_from_policy(&content_type_policy(
            &["application/zip"],
            &["application/zip"],
        ))
        .unwrap();
        assert!(!both.is_allowed("application/zip"));
    }

    /// Denying every built-in type is a legitimate strict policy: serve nothing
    /// verbatim.
    #[test]
    fn denying_every_builtin_type_yields_deny_all() {
        let denied: Vec<&str> = DEFAULT_ALLOWED_CONTENT_TYPES.to_vec();
        let allowlist =
            ContentTypeAllowlist::try_from_policy(&content_type_policy(&[], &denied)).unwrap();

        assert!(allowlist.allowed_types().is_empty());
        assert_eq!(
            allowlist.coerce(Some("image/png".to_string())),
            "application/octet-stream"
        );
    }

    /// Deny matching is case- and parameter-insensitive, like every other match.
    #[test]
    fn denied_matching_ignores_case_and_parameters() {
        let allowlist =
            ContentTypeAllowlist::try_from_policy(&content_type_policy(&[], &["IMAGE/PNG"]))
                .unwrap();

        assert!(!allowlist.is_allowed("image/png"));
        assert!(!allowlist.is_allowed("image/png; charset=utf-8"));
    }

    /// Naming a built-in in `extra` changes nothing; it must not produce a
    /// duplicate or otherwise alter the set.
    #[test]
    fn extra_entry_already_builtin_is_a_no_op() {
        let allowlist =
            ContentTypeAllowlist::try_from_policy(&content_type_policy(&["image/png"], &[]))
                .unwrap();

        assert_eq!(allowlist.allowed_types(), builtin_sorted());
    }

    /// A `denied` entry that matches nothing is a no-op, not an error: the same
    /// policy may be shared across environments with different `extra` lists.
    #[test]
    fn denied_entry_matching_nothing_is_a_no_op() {
        let allowlist =
            ContentTypeAllowlist::try_from_policy(&content_type_policy(&[], &["application/zip"]))
                .unwrap();

        assert_eq!(allowlist.allowed_types(), builtin_sorted());
    }

    /// The security floor: config cannot add a type a browser executes as a
    /// document. Startup fails rather than serving it.
    #[test]
    fn extra_rejects_every_never_allowed_type() {
        for never in [
            "text/html",
            "application/xhtml+xml",
            "image/svg+xml",
            "text/xml",
            "application/xml",
            "application/xslt+xml",
            "text/javascript",
            "application/javascript",
            "application/ecmascript",
        ] {
            let error = ContentTypeAllowlist::try_from_policy(&content_type_policy(&[never], &[]))
                .expect_err(&format!("extra must reject {never:?}"));
            assert!(
                error.to_string().contains(never),
                "error must name the rejected type, got: {error}"
            );
        }
    }

    /// The floor check normalizes, so case cannot smuggle a type past it.
    #[test]
    fn extra_rejects_never_allowed_type_in_any_case() {
        assert!(
            ContentTypeAllowlist::try_from_policy(&content_type_policy(&["TEXT/HTML"], &[]))
                .is_err()
        );
        assert!(
            ContentTypeAllowlist::try_from_policy(&content_type_policy(&["Image/SVG+XML"], &[]))
                .is_err()
        );
    }

    /// Denying a type that is already on the floor is redundant, not wrong.
    #[test]
    fn denied_accepts_never_allowed_type_as_redundant_no_op() {
        let allowlist =
            ContentTypeAllowlist::try_from_policy(&content_type_policy(&[], &["text/html"]))
                .unwrap();

        assert_eq!(allowlist.allowed_types(), builtin_sorted());
    }

    /// `normalize` truncates at the first `,`, so a comma-joined entry would allow
    /// less than written and could hide a floor type behind an allowed prefix.
    #[test]
    fn policy_rejects_comma_joined_entry() {
        assert!(
            ContentTypeAllowlist::try_from_policy(&content_type_policy(
                &["image/png,text/html"],
                &[]
            ))
            .is_err()
        );
        assert!(
            ContentTypeAllowlist::try_from_policy(&content_type_policy(
                &[],
                &["image/png,image/gif"]
            ))
            .is_err()
        );
    }

    /// Parameters are meaningless in the config list and `normalize` discards them,
    /// so writing one is a mistake worth surfacing at startup.
    #[test]
    fn policy_rejects_parameterized_entry() {
        assert!(
            ContentTypeAllowlist::try_from_policy(&content_type_policy(
                &["text/plain; charset=utf-8"],
                &[]
            ))
            .is_err()
        );
    }

    /// Whitespace around the `/` survives `normalize`, so without an explicit
    /// check a whitespace variant of a floor type is accepted and then matches a
    /// caller sending the same variant.
    #[test]
    fn policy_rejects_entry_with_whitespace_around_the_slash() {
        for malformed in [
            "text / html",
            "text /html",
            "text/ html",
            "image / svg+xml",
            "IMAGE / SVG+XML",
            "text\t/html",
            "image / png",
            // Inside a token, not at the slash boundary.
            "image/pn g",
            "tex t/plain",
        ] {
            assert!(
                ContentTypeAllowlist::try_from_policy(&content_type_policy(&[malformed], &[]))
                    .is_err(),
                "extra must reject {malformed:?}"
            );
            assert!(
                ContentTypeAllowlist::try_from_policy(&content_type_policy(&[], &[malformed]))
                    .is_err(),
                "denied must reject {malformed:?}"
            );
        }
    }

    /// A control character also survives `normalize` and would be stored as a key
    /// no valid header value can match.
    #[test]
    fn policy_rejects_entry_with_control_characters() {
        for malformed in ["text/ht\u{7}ml", "image/p\0ng", "text/html\u{7}"] {
            assert!(
                ContentTypeAllowlist::try_from_policy(&content_type_policy(&[malformed], &[]))
                    .is_err(),
                "must reject {malformed:?}"
            );
        }
    }

    /// Media type tokens are ASCII, so a non-ASCII entry can never match a real
    /// request and is always a config mistake.
    #[test]
    fn policy_rejects_non_ascii_entry() {
        for malformed in ["image/pñg", "tëxt/plain", "image/htmⅼ"] {
            assert!(
                ContentTypeAllowlist::try_from_policy(&content_type_policy(&[malformed], &[]))
                    .is_err(),
                "must reject {malformed:?}"
            );
        }
    }

    /// A wildcard matches nothing, so accepting one silently allows less than the
    /// operator wrote. `*/*` is the worst case: it reads as "allow everything".
    #[test]
    fn policy_rejects_wildcard_entry() {
        for wildcard in ["*/*", "image/*", "text/*", "*/plain", "image/pn*g"] {
            assert!(
                ContentTypeAllowlist::try_from_policy(&content_type_policy(&[wildcard], &[]))
                    .is_err(),
                "extra must reject {wildcard:?}"
            );
            assert!(
                ContentTypeAllowlist::try_from_policy(&content_type_policy(&[], &[wildcard]))
                    .is_err(),
                "denied must reject {wildcard:?}"
            );
        }
    }

    #[test]
    fn policy_rejects_empty_entry() {
        assert!(ContentTypeAllowlist::try_from_policy(&content_type_policy(&[""], &[])).is_err());
        assert!(
            ContentTypeAllowlist::try_from_policy(&content_type_policy(&["   "], &[])).is_err()
        );
    }

    #[test]
    fn policy_rejects_entry_that_is_not_type_subtype() {
        for malformed in ["png", "image/", "/png", "image/png/extra"] {
            assert!(
                ContentTypeAllowlist::try_from_policy(&content_type_policy(&[malformed], &[]))
                    .is_err(),
                "must reject {malformed:?}"
            );
        }
    }

    /// The caller needs to know which list a bad entry came from so it can name the
    /// config field. This module does not know config field names.
    #[test]
    fn policy_error_reports_which_list_the_entry_came_from() {
        let extra_error =
            ContentTypeAllowlist::try_from_policy(&content_type_policy(&["text/html"], &[]))
                .unwrap_err();
        assert_eq!(extra_error.field(), PolicyField::Extra);

        let denied_error = ContentTypeAllowlist::try_from_policy(&content_type_policy(
            &[],
            &["image/png,image/gif"],
        ))
        .unwrap_err();
        assert_eq!(denied_error.field(), PolicyField::Denied);
    }

    /// A config field name embedded here would go stale silently on a rename.
    #[test]
    fn policy_error_message_does_not_name_config_fields() {
        let error =
            ContentTypeAllowlist::try_from_policy(&content_type_policy(&["text/html"], &[]))
                .unwrap_err();

        assert!(
            !error.to_string().contains("presigned_url"),
            "field naming belongs to the caller, got: {error}"
        );
        assert!(
            error.to_string().contains("text/html"),
            "message must still name the entry, got: {error}"
        );
    }

    /// Invariant: the built-in set and the security floor must never intersect, or
    /// the default policy would fail startup.
    #[test]
    fn builtin_set_does_not_intersect_the_security_floor() {
        let builtin: HashSet<&&str> = DEFAULT_ALLOWED_CONTENT_TYPES.iter().collect();
        let floor: HashSet<&&str> = NEVER_ALLOWED_CONTENT_TYPES.iter().collect();

        let overlap: Vec<_> = builtin.intersection(&floor).collect();
        assert!(overlap.is_empty(), "types on both lists: {overlap:?}");
    }

    /// Both lists are compared against normalized input, so a non-canonical entry
    /// would silently never match.
    #[test]
    fn content_type_consts_are_canonical() {
        for content_type in DEFAULT_ALLOWED_CONTENT_TYPES
            .iter()
            .chain(NEVER_ALLOWED_CONTENT_TYPES.iter())
        {
            assert_eq!(
                &normalize(content_type),
                content_type,
                "{content_type:?} is not in normalized form"
            );
        }
    }

    /// `Default` and the const must not drift apart.
    #[test]
    fn default_impl_matches_builtin_const() {
        assert_eq!(
            ContentTypeAllowlist::default().allowed_types(),
            builtin_sorted()
        );
    }
}
