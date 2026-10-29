// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use lore_revision::cluster::peer::Locality;
use lore_server::plugins::PluginRegistry;
use lore_server::settings::*;
use lore_server::store::resolve_plugin_config_with_fallback;
use lore_server::topology::TopologyProvider;

/// Every required `[server.http]` field, so a test can add just the key it
/// cares about.
const MINIMAL_HTTP_SETTINGS: &str = r#"
        enabled = false
        host = "127.0.0.1"
        max_file_size = 1024
        port = 8080
        request_timeout_seconds = 30
        request_body_timeout_seconds = 30
        available_interval_seconds = 5
        available_timeout_seconds = 30
        store_health_check = false
    "#;

fn http_settings(extra_keys: &str) -> HttpSettings {
    toml::from_str(&format!("{MINIMAL_HTTP_SETTINGS}\n{extra_keys}\n"))
        .expect("[server.http] should deserialize")
}

const TEN_GIB: u64 = 10 * 1024 * 1024 * 1024;

#[test]
fn local_store_monitor_checks_every_thirty_seconds_below_ten_gibibytes() {
    let settings = LocalStoreMonitorSettings::default();

    assert_eq!(settings.check_interval_seconds, 30);
    assert_eq!(settings.low_space_threshold_bytes, TEN_GIB);
}

/// Existing config files carry no `[server.local_store_monitor]` table, so
/// an absent table has to leave the server running on the defaults.
#[test]
fn server_settings_default_the_local_store_monitor_table() {
    let server: ServerSettings =
        toml::from_str("").expect("[server] with no tables should deserialize");

    assert_eq!(server.local_store_monitor.check_interval_seconds, 30);
    assert_eq!(
        server.local_store_monitor.low_space_threshold_bytes,
        TEN_GIB
    );
}

#[test]
fn local_store_monitor_keys_are_optional_one_by_one() {
    let settings: LocalStoreMonitorSettings = toml::from_str(
        r#"
            check_interval_seconds = 5
        "#,
    )
    .expect("[server.local_store_monitor] should deserialize");

    assert_eq!(settings.check_interval_seconds, 5);
    assert_eq!(settings.low_space_threshold_bytes, TEN_GIB);
}

/// A bare-string `jwt_issuer` and a one-entry list are the same
/// configuration, so existing config files need no edit.
#[test]
fn jwt_issuer_accepts_a_bare_string_and_a_list() {
    let bare: AuthSettings = toml::from_str(
        r#"
            jwt_issuer = "LEGACY_AUTH_KEYWORD"
            jwt_audience = ["lore-service"]
        "#,
    )
    .expect("[server.auth] with a bare string should deserialize");
    let list: AuthSettings = toml::from_str(
        r#"
            jwt_issuer = ["LEGACY_AUTH_KEYWORD"]
            jwt_audience = ["lore-service"]
        "#,
    )
    .expect("[server.auth] with a list should deserialize");

    assert_eq!(bare.jwt_issuer, list.jwt_issuer);
    assert_eq!(bare.jwt_issuer, vec!["LEGACY_AUTH_KEYWORD".to_string()]);
}

#[test]
fn jwt_issuer_accepts_two_entries_for_a_cutover() {
    let auth: AuthSettings = toml::from_str(
        r#"
            jwt_issuer = ["LEGACY_AUTH_KEYWORD", "https://auth.example.com/realms/lore"]
            jwt_audience = ["lore-service"]
        "#,
    )
    .expect("[server.auth] with two issuers should deserialize");

    assert_eq!(
        auth.jwt_issuer,
        vec![
            "LEGACY_AUTH_KEYWORD".to_string(),
            "https://auth.example.com/realms/lore".to_string(),
        ]
    );
}

/// Every authorization field round-trips from TOML, including the
/// literal `urc-*` wildcard.
#[test]
fn auth_settings_authorization_fields_round_trip() {
    let auth: AuthSettings = toml::from_str(
        r#"
            jwt_issuer = "https://auth.example.com"
            jwt_audience = ["lore"]
            permission_claim = "realm_access.roles"
            resource_claim = "resources"
            resource_id_claim = "rsname"
            resource_id_template = "repo:{id}"
            resource_wildcard = "urc-*"
            baseline_access = "reachable"
            repository_catalog = "auth_service"
            repository_catalog_url = "https://catalog.example.com"
        "#,
    )
    .expect("[server.auth] with every authorization field should deserialize");

    assert_eq!(auth.permission_claim.as_deref(), Some("realm_access.roles"));
    assert_eq!(auth.resource_claim.as_deref(), Some("resources"));
    assert_eq!(auth.resource_id_claim, "rsname");
    assert_eq!(auth.resource_id_template, "repo:{id}");
    assert_eq!(auth.resource_wildcard, "urc-*");
    assert_eq!(auth.baseline_access, BaselineAccess::Reachable);
    assert_eq!(
        auth.repository_catalog,
        Some(RepositoryCatalogMode::AuthService)
    );
    assert_eq!(
        auth.repository_catalog_url.as_deref(),
        Some("https://catalog.example.com")
    );
}

/// A config setting none of the authorization fields gets the documented
/// defaults: the `urc-` template and wildcard literals, and no permission or
/// resource claim selected.
#[test]
fn auth_settings_authorization_fields_have_backward_compatible_defaults() {
    let auth: AuthSettings = toml::from_str(
        r#"
            jwt_issuer = "LEGACY_AUTH_KEYWORD"
            jwt_audience = ["lore-service"]
        "#,
    )
    .expect("[server.auth] without the authorization fields should deserialize");

    assert_eq!(auth.resource_id_template, "urc-{id}");
    assert_eq!(auth.resource_wildcard, "urc-*");
    assert_eq!(auth.resource_id_claim, "resource_id");
    assert_eq!(auth.baseline_access, BaselineAccess::Denied);
    assert_eq!(auth.permission_claim, None);
    assert_eq!(auth.resource_claim, None);
    assert_eq!(auth.repository_catalog, None);
    assert_eq!(auth.repository_catalog_url, None);
    assert_eq!(auth.jwt_typ, None);
}

/// The RFC 9068 rule is one bare string; a provider with its own
/// convention lists what it emits.
#[test]
fn jwt_typ_accepts_a_bare_string_and_a_list() {
    let bare: AuthSettings = toml::from_str(
        r#"
            jwt_issuer = "https://auth.example.com"
            jwt_audience = ["lore-service"]
            jwt_typ = "at+jwt"
        "#,
    )
    .expect("[server.auth] with a bare-string jwt_typ should deserialize");
    let list: AuthSettings = toml::from_str(
        r#"
            jwt_issuer = "https://auth.example.com"
            jwt_audience = ["lore-service"]
            jwt_typ = ["at+jwt", "JWT"]
        "#,
    )
    .expect("[server.auth] with a jwt_typ list should deserialize");

    assert_eq!(bare.jwt_typ, Some(vec!["at+jwt".to_string()]));
    assert_eq!(
        list.jwt_typ,
        Some(vec!["at+jwt".to_string(), "JWT".to_string()])
    );
}

/// An empty `jwt_typ` would refuse every token; leaving the key out is
/// how the check is skipped.
#[test]
fn auth_with_empty_jwt_typ_fails_validation() {
    let settings = settings_with_auth_keys(
        r#"
            jwt_issuer = "https://auth.example.com"
            jwt_audience = ["lore-service"]
            jwt_typ = []
        "#,
    )
    .expect("an empty jwt_typ list still parses");
    let error = validate_auth_config(&settings)
        .expect_err("[server.auth] with an empty jwt_typ must fail validation");
    assert!(
        error.to_string().contains("jwt_typ"),
        "the error must name the setting: {error}"
    );
}

/// A blank or whitespace-carrying entry beside a valid one is refused
/// too: it is no media type, and the verifier compares the header
/// exactly, so such an entry could only ever match a malformed header.
#[test]
fn auth_with_a_blank_jwt_typ_entry_fails_validation() {
    for entry in ["\"\"", "\"  \"", "\"at+jwt \"", "\" at+jwt\""] {
        let settings = settings_with_auth_keys(&format!(
            r#"
                jwt_issuer = "https://auth.example.com"
                jwt_audience = ["lore-service"]
                jwt_typ = ["at+jwt", {entry}]
            "#
        ))
        .expect("a blank jwt_typ entry still parses");
        let error = validate_auth_config(&settings)
            .expect_err("[server.auth] with a blank jwt_typ entry must fail validation");
        assert!(
            error.to_string().contains("jwt_typ"),
            "the error must name the setting: {error}"
        );
    }
}

#[test]
fn auth_with_a_jwt_typ_list_passes_validation() {
    let settings = settings_with_auth_keys(
        r#"
            jwt_issuer = "https://auth.example.com"
            jwt_audience = ["lore-service"]
            jwt_typ = ["at+jwt", "JWT"]

            [server.auth.oidc]
            client_id = "lore-cli"
        "#,
    )
    .expect("a jwt_typ list must parse");
    validate_auth_config(&settings).expect("a jwt_typ list must validate");
}

const TIER_1: &str = "";
const TIER_2: &str = r#"resource_claim = "resources""#;
const AUTH_SERVICE: &str = r#"
            [environment.endpoint]
            auth_url = "ucs-auth://auth.example.com"
            "#;

/// `authorizer_keys` selects the authorizer: [`TIER_1`], [`TIER_2`] or
/// [`AUTH_SERVICE`].
fn settings_with_oidc_keys(authorizer_keys: &str, oidc_keys: &str) -> Settings {
    settings_with_auth_keys(&format!(
        r#"
            jwt_issuer = "https://auth.example.com"
            jwt_audience = ["lore-service"]
            {authorizer_keys}

            [server.auth.oidc]
            client_id = "lore-cli"
            {oidc_keys}
        "#
    ))
    .expect("[server.auth.oidc] must parse")
}

#[test]
fn oidc_settings_default_to_no_scopes_and_not_preferred() {
    let settings = settings_with_oidc_keys(TIER_1, "");
    let oidc = settings
        .server
        .auth
        .and_then(|auth| auth.oidc)
        .expect("[server.auth.oidc] is present");
    assert_eq!(oidc.client_id, "lore-cli");
    assert!(oidc.scopes.is_empty());
    assert!(!oidc.preferred);
    assert_eq!(oidc.resource_template, None);
    assert_eq!(oidc.scope_template, None);
    assert_eq!(oidc.token_exchange_issuer, None);
}

#[test]
fn oidc_templates_that_match_the_authorizer_pass_validation() {
    let tier_2_forms = [
        r#"resource_template = "https://lore.example.com/partitions/{id}""#,
        r#"scope_template = "partition:{id}""#,
        r#"
            token_exchange_issuer = "https://sts.example.com"
            resource_template = "https://lore.example.com/partitions/{id}"
            "#,
        r#"
            token_exchange_issuer = "https://sts.example.com"
            scope_template = "partition:{id}"
            "#,
    ];
    let mut cases = vec![(TIER_1, ""), (AUTH_SERVICE, "")];
    for keys in tier_2_forms {
        cases.push((TIER_2, keys));
        cases.push((AUTH_SERVICE, keys));
    }
    for (authorizer_keys, oidc_keys) in cases {
        validate_auth_config(&settings_with_oidc_keys(authorizer_keys, oidc_keys)).unwrap_or_else(
            |error| panic!("{authorizer_keys} with {oidc_keys} must validate: {error}"),
        );
    }
}

/// Tier 1 and Tier 2 offer clients no login path besides OIDC.
#[test]
fn oidc_is_required_without_the_auth_service() {
    for authorizer_keys in [TIER_1, TIER_2] {
        let settings = settings_with_auth_keys(&format!(
            r#"
                jwt_issuer = "https://auth.example.com"
                jwt_audience = ["lore-service"]
                {authorizer_keys}
            "#
        ))
        .expect("[server.auth] must parse");
        let error = validate_auth_config(&settings)
            .expect_err("Tier 1 and Tier 2 must advertise the OIDC provider");
        assert!(
            error.to_string().contains("[server.auth.oidc]"),
            "the error must name the table: {error}"
        );
    }
}

#[test]
fn oidc_is_optional_beside_the_auth_service() {
    let settings = settings_with_auth_keys(&format!(
        r#"
            jwt_issuer = "https://auth.example.com"
            jwt_audience = ["lore-service"]
            {AUTH_SERVICE}
        "#
    ))
    .expect("[server.auth] must parse");
    validate_auth_config(&settings).expect("the auth service is a login path of its own");
}

/// Clients must ask for the tokens the authorizer reads: partition-scoped
/// under Tier 2, and not under Tier 1.
#[test]
fn oidc_templates_that_contradict_the_authorizer_fail_validation() {
    let cases = [
        (TIER_2, "", ["resource_claim", "resource_template"]),
        (
            TIER_1,
            r#"resource_template = "https://lore.example.com/partitions/{id}""#,
            ["resource_claim", "resource_template"],
        ),
        (
            TIER_1,
            r#"scope_template = "partition:{id}""#,
            ["resource_claim", "scope_template"],
        ),
    ];
    for (authorizer_keys, oidc_keys, named) in cases {
        let error = validate_auth_config(&settings_with_oidc_keys(authorizer_keys, oidc_keys))
            .expect_err("the templates must match the authorizer");
        let message = error.to_string();
        for setting in named {
            assert!(
                message.contains(setting),
                "the error must name {setting}: {message}"
            );
        }
    }
}

/// An exchange issuer without a template, or both templates, is a startup
/// error naming both sides, never a silent fall back to Tier 1.
#[test]
fn oidc_invalid_tier_2_fails_validation_naming_both_settings() {
    let cases = [
        (
            r#"token_exchange_issuer = "https://sts.example.com""#,
            ["token_exchange_issuer", "resource_template"],
        ),
        (
            r#"
                token_exchange_issuer = "https://sts.example.com"
                resource_template = "https://lore.example.com/partitions/{id}"
                scope_template = "partition:{id}"
                "#,
            ["resource_template", "scope_template"],
        ),
    ];
    for (keys, named) in cases {
        let error = validate_auth_config(&settings_with_oidc_keys(TIER_2, keys))
            .expect_err("an invalid Tier 2 configuration must fail validation");
        let message = error.to_string();
        for setting in named {
            assert!(
                message.contains(&format!("server.auth.oidc.{setting}")),
                "the error must name {setting}: {message}"
            );
        }
    }
}

/// A template without `{id}` names the same resource for every partition: the client
/// would ask for one token for all of them, and the server would match one claim entry
/// against every partition. Each is refused at startup, naming the setting.
#[test]
fn a_partition_template_without_its_placeholder_fails_validation() {
    let cases = [
        (
            settings_with_oidc_keys(
                TIER_2,
                r#"resource_template = "https://lore.example.com/partitions/{ID}""#,
            ),
            "server.auth.oidc.resource_template",
        ),
        (
            settings_with_oidc_keys(TIER_2, r#"scope_template = "partition""#),
            "server.auth.oidc.scope_template",
        ),
        (
            settings_with_oidc_keys(
                r#"
                    resource_claim = "resources"
                    resource_id_template = "urc-all"
                    "#,
                r#"resource_template = "https://lore.example.com/partitions/{id}""#,
            ),
            "server.auth.resource_id_template",
        ),
    ];
    for (settings, setting) in cases {
        let error = validate_auth_config(&settings)
            .expect_err("a template without {id} must fail validation");
        let message = error.to_string();
        assert!(
            message.contains(setting) && message.contains("{id}"),
            "the error must name {setting} and the placeholder: {message}"
        );
    }
}

#[test]
fn oidc_with_a_keyword_jwt_issuer_fails_validation() {
    let settings = settings_with_auth_keys(
        r#"
            jwt_issuer = ["LEGACY_AUTH_KEYWORD", "https://auth.example.com"]
            jwt_audience = ["lore-service"]

            [server.auth.oidc]
            client_id = "lore-cli"
        "#,
    )
    .expect("[server.auth.oidc] must parse");
    let error = validate_auth_config(&settings)
        .expect_err("advertising OIDC needs an issuer URL to discover");
    assert!(
        error.to_string().contains("jwt_issuer"),
        "the error must name the setting: {error}"
    );
}

#[test]
fn oidc_client_id_is_required_and_not_empty() {
    let missing = settings_with_auth_keys(
        r#"
            jwt_issuer = "https://auth.example.com"
            jwt_audience = ["lore-service"]

            [server.auth.oidc]
            preferred = true
        "#,
    )
    .expect_err("[server.auth.oidc] without client_id must fail to parse");
    assert!(missing.to_string().contains("client_id"), "{missing}");

    let settings = settings_with_auth_keys(
        r#"
            jwt_issuer = "https://auth.example.com"
            jwt_audience = ["lore-service"]

            [server.auth.oidc]
            client_id = ""
        "#,
    )
    .expect("an empty client_id still parses");
    let empty = validate_auth_config(&settings).expect_err("an empty client_id must fail");
    assert!(empty.to_string().contains("client_id"), "{empty}");
}

/// Minimal loadable settings with the given `[server.auth]` keys, for the
/// startup-validation tests. `Settings` deserializes with a `'static`
/// bound, so the assembled TOML is leaked; each test builds one.
fn settings_with_auth_keys(auth_keys: &str) -> Result<Settings, toml::de::Error> {
    let config = format!(
        r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [immutable_store]
            mode = "local"

            [mutable_store]
            mode = "local"

            [server.auth]
            {auth_keys}
        "#
    );
    toml::from_str(Box::leak(config.into_boxed_str()))
}

#[test]
fn auth_without_jwt_audience_fails_to_parse_naming_the_setting() {
    let error = settings_with_auth_keys(r#"jwt_issuer = "LEGACY_AUTH_KEYWORD""#)
        .expect_err("[server.auth] without jwt_audience must fail to parse");
    assert!(
        error.to_string().contains("jwt_audience"),
        "the error must name the missing setting: {error}"
    );
}

#[test]
fn auth_without_jwt_issuer_fails_to_parse_naming_the_setting() {
    let error = settings_with_auth_keys(r#"jwt_audience = ["lore-service"]"#)
        .expect_err("[server.auth] without jwt_issuer must fail to parse");
    assert!(
        error.to_string().contains("jwt_issuer"),
        "the error must name the missing setting: {error}"
    );
}

/// An empty list parses but would reject every token, which is never what
/// was configured on purpose, so validation refuses it.
#[test]
fn auth_with_empty_jwt_audience_fails_validation() {
    let settings = settings_with_auth_keys(
        r#"
            jwt_issuer = "LEGACY_AUTH_KEYWORD"
            jwt_audience = []
        "#,
    )
    .expect("an empty jwt_audience list still parses");
    validate_auth_config(&settings)
        .expect_err("[server.auth] with an empty jwt_audience must fail validation");
}

#[test]
fn auth_with_empty_jwt_issuer_fails_validation() {
    let settings = settings_with_auth_keys(
        r#"
            jwt_issuer = []
            jwt_audience = ["lore-service"]
        "#,
    )
    .expect("an empty jwt_issuer list still parses");
    validate_auth_config(&settings)
        .expect_err("[server.auth] with an empty jwt_issuer must fail validation");
}

#[test]
fn auth_with_issuer_and_audience_passes_validation() {
    let settings = settings_with_auth_keys(
        r#"
            jwt_issuer = "LEGACY_AUTH_KEYWORD"
            jwt_audience = ["lore-service", ".example.net"]

            [environment.endpoint]
            auth_url = "ucs-auth://auth.example.com"
        "#,
    )
    .expect("a complete [server.auth] must parse");
    validate_auth_config(&settings).expect("a complete [server.auth] must validate");
}

/// The loader reads `user_url` from `[environment.endpoint]` for the catalog
/// selection, so `repository_catalog = "user_service"` loads with it and is
/// refused without it.
#[test]
fn user_service_catalog_needs_user_url_at_load() {
    let settings_with = |endpoint_keys: &str| {
        settings_with_auth_keys(&format!(
            r#"
                jwt_issuer = "LEGACY_AUTH_KEYWORD"
                jwt_audience = ["lore-service"]
                repository_catalog = "user_service"

                [environment.endpoint]
                auth_url = "ucs-auth://auth.example.com"
                {endpoint_keys}
            "#
        ))
        .expect("settings must parse")
    };
    validate_auth_config(&settings_with(
        r#"user_url = "ucs-auth://directory.example.com""#,
    ))
    .expect("the user_service catalog with user_url must validate");
    let error = validate_auth_config(&settings_with(""))
        .expect_err("the user_service catalog without user_url must fail validation");
    assert!(error.to_string().contains("user_url"), "{error}");
}

/// No `[server.auth]` at all keeps starting: verification stays off and
/// nothing is mandatory.
#[test]
fn no_auth_table_passes_validation() {
    let settings: Settings = toml::from_str(
        r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [immutable_store]
            mode = "local"

            [mutable_store]
            mode = "local"
        "#,
    )
    .expect("settings deserialize");
    validate_auth_config(&settings).expect("no [server.auth] must stay valid");
}

/// `auth_url` names an authorization service, but without `[server.auth]`
/// nothing verifies tokens and the server would run open. The loader
/// refuses it, naming both settings.
#[test]
fn auth_url_without_server_auth_fails_validation() {
    let settings: Settings = toml::from_str(
        r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [immutable_store]
            mode = "local"

            [mutable_store]
            mode = "local"

            [environment.endpoint]
            auth_url = "https://legacy-auth.example.com"
        "#,
    )
    .expect("settings deserialize");
    let error = validate_auth_config(&settings)
        .expect_err("auth_url without [server.auth] must fail validation");
    assert!(error.to_string().contains("auth_url"), "{error}");
    assert!(error.to_string().contains("[server.auth]"), "{error}");
}

/// The authorizer-selection conflict bails at config load: setting
/// `resource_claim` while `auth_url` is still configured is refused
/// before any initialization, naming both settings.
#[test]
fn auth_url_with_resource_claim_fails_validation() {
    // The trailing table is appended after the `[server.auth]` keys the
    // helper writes, which TOML reads as a sibling table.
    let settings = settings_with_auth_keys(
        r#"
            jwt_issuer = "LEGACY_AUTH_KEYWORD"
            jwt_audience = ["lore-service"]
            resource_claim = "resources"

            [environment.endpoint]
            auth_url = "https://legacy-auth.example.com"
        "#,
    )
    .expect("the conflicting pairing still parses");
    let error = validate_auth_config(&settings)
        .expect_err("auth_url with resource_claim must fail validation");
    assert!(error.to_string().contains("auth_url"), "{error}");
    assert!(error.to_string().contains("resource_claim"), "{error}");
}

/// `repository_catalog = "auth_service"` with no endpoint to ask is
/// refused at load, naming both settings that could supply one.
#[test]
fn auth_service_catalog_without_an_endpoint_fails_validation() {
    let settings = settings_with_auth_keys(
        r#"
            jwt_issuer = "https://issuer.example.com"
            jwt_audience = ["lore-service"]
            repository_catalog = "auth_service"
        "#,
    )
    .expect("the incomplete pairing still parses");
    let error = validate_auth_config(&settings)
        .expect_err("auth_service without an endpoint must fail validation");
    assert!(
        error.to_string().contains("repository_catalog_url"),
        "{error}"
    );
    assert!(error.to_string().contains("auth_url"), "{error}");
}

/// Both keys absent means an empty policy, which resolves to the built-in set.
#[test]
fn presign_content_type_lists_default_to_empty() {
    let http = http_settings("");
    assert!(http.presigned_url_extra_content_types.is_empty());
    assert!(http.presigned_url_denied_content_types.is_empty());
}

#[test]
fn presign_extra_content_types_are_read() {
    let http =
        http_settings(r#"presigned_url_extra_content_types = ["application/zip", "audio/mpeg"]"#);
    assert_eq!(
        http.presigned_url_extra_content_types,
        ["application/zip", "audio/mpeg"]
    );
}

#[test]
fn presign_denied_content_types_are_read() {
    let http = http_settings(r#"presigned_url_denied_content_types = ["application/pdf"]"#);
    assert_eq!(http.presigned_url_denied_content_types, ["application/pdf"]);
}

#[test]
fn test_settings_with_plugin_sections() {
    let config = r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [server.http]
            enabled = false
            host = "127.0.0.1"
            max_file_size = 1024
            port = 8080
            request_timeout_seconds = 30
            request_body_timeout_seconds = 30
            available_interval_seconds = 5
            available_timeout_seconds = 30
            store_health_check = false

            [immutable_store]
            mode = "aws"

            [mutable_store]
            mode = "aws"

            [topology]
            provider = "consul"

            [plugins.aws]
            s3_bucket = "my-bucket"
            region = "us-east-1"

            [plugins.consul]
            address = "localhost:8500"

            [hooks.compliance]
            enabled = true
            webhook_url = "https://example.com/notify"
        "#;

    let settings: Settings = toml::from_str(config).unwrap();
    assert_eq!(settings.immutable_store.mode, "aws");
    assert_eq!(settings.mutable_store.mode, "aws");
    assert!(settings.plugins.contains_key("aws"));
    assert!(settings.plugins.contains_key("consul"));

    // Verify aws plugin settings
    let aws_plugin = settings.plugins.get("aws").unwrap();
    assert_eq!(
        aws_plugin.get("s3_bucket").unwrap().as_str().unwrap(),
        "my-bucket"
    );
    assert_eq!(
        aws_plugin.get("region").unwrap().as_str().unwrap(),
        "us-east-1"
    );

    // Verify consul plugin settings
    let consul_plugin = settings.plugins.get("consul").unwrap();
    assert_eq!(
        consul_plugin.get("address").unwrap().as_str().unwrap(),
        "localhost:8500"
    );

    // Verify hooks
    let compliance_hook = settings.hooks.get("compliance").unwrap();
    assert!(compliance_hook.enabled);
    assert_eq!(
        compliance_hook
            .config
            .get("webhook_url")
            .unwrap()
            .as_str()
            .unwrap(),
        "https://example.com/notify"
    );

    // Verify topology
    let topology = settings.topology.unwrap();
    assert!(matches!(
        topology.provider,
        lore_server::topology::TopologyProvider::Consul
    ));
}

#[test]
fn a_disabled_service_is_parsed() {
    let config = r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [server.grpc_public_services.storage_service]
            enabled = false

            [server.grpc_public_services.lock_service]
            enabled = false

            [immutable_store]
            mode = "local"

            [mutable_store]
            mode = "local"
        "#;

    let settings: Settings = toml::from_str(config).expect("settings deserialize");
    let services = &settings.server.grpc_public_services;

    assert!(!services.storage_service.enabled);
    assert!(!services.lock_service.enabled);
    assert!(services.thin_client_service.enabled);
    assert!(services.admin_service.enabled);
}

/// An absent table means every service registers.
#[test]
fn an_absent_table_registers_every_service() {
    let config = r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [immutable_store]
            mode = "local"

            [mutable_store]
            mode = "local"
        "#;

    let settings: Settings = toml::from_str(config).expect("settings deserialize");
    let services = &settings.server.grpc_public_services;

    assert!(services.admin_service.enabled);
    assert!(services.storage_service.enabled);
    assert!(services.lock_service.enabled);
    assert!(services.notification_service.enabled);
}

/// `general` nests under the service block.
#[test]
fn general_settings_parse_under_the_service_block() {
    let config = r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [server.grpc_public_services.lock_service.general]
            max_encoding_message_size = 16777216

            [immutable_store]
            mode = "local"

            [mutable_store]
            mode = "local"
        "#;

    let settings: Settings = toml::from_str(config).expect("settings deserialize");

    assert_eq!(
        settings
            .server
            .grpc_public_services
            .lock_service
            .general
            .max_encoding_message_size,
        Some(16_777_216)
    );
    assert!(
        settings.server.grpc_public_services.lock_service.enabled,
        "a block carrying only `general` must stay enabled"
    );
}

/// `mode = "none"` is how a layered config opts out of the `[lock_store]`
/// table that `default.toml` sets.
#[test]
fn a_lock_store_mode_of_none_parses() {
    let config = r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [lock_store]
            mode = "none"

            [immutable_store]
            mode = "local"

            [mutable_store]
            mode = "local"
        "#;

    let settings: Settings = toml::from_str(config).expect("settings deserialize");

    assert_eq!(
        settings.lock_store.expect("lock_store present").mode,
        "none"
    );
}

/// Unknown keys are ignored, so a misspelled disable leaves the service
/// registered.
#[test]
fn a_misspelled_disable_leaves_the_service_registered() {
    let config = r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [server.grpc_public_services.thin_cleint_service]
            enabled = false

            [server.grpc_public_services.storage_service]
            enabld = false

            [immutable_store]
            mode = "local"

            [mutable_store]
            mode = "local"
        "#;

    let settings: Settings = toml::from_str(config).expect("settings deserialize");
    let services = &settings.server.grpc_public_services;

    assert!(services.thin_client_service.enabled);
    assert!(services.storage_service.enabled);
}

/// The pre-`general` spelling of `max_encoding_message_size` deserializes
/// but is silently dropped.
#[test]
fn the_pre_general_spelling_of_max_encoding_message_size_is_dropped() {
    let config = r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [server.grpc_public_services.lock_service]
            max_encoding_message_size = 16777216

            [immutable_store]
            mode = "local"

            [mutable_store]
            mode = "local"
        "#;

    let settings: Settings = toml::from_str(config).expect("settings deserialize");

    assert_eq!(
        settings
            .server
            .grpc_public_services
            .lock_service
            .general
            .max_encoding_message_size,
        None
    );
}

/// A configuration disabling every public gRPC service loads.
#[test]
fn disabling_every_service_still_loads() {
    let config = r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [server.grpc_public_services.admin_service]
            enabled = false

            [server.grpc_public_services.storage_service]
            enabled = false

            [server.grpc_public_services.revision_service]
            enabled = false

            [server.grpc_public_services.repository_service]
            enabled = false

            [server.grpc_public_services.environment_service]
            enabled = false

            [server.grpc_public_services.thin_client_service]
            enabled = false

            [server.grpc_public_services.lock_service]
            enabled = false

            [server.grpc_public_services.notification_service]
            enabled = false

            [immutable_store]
            mode = "local"

            [mutable_store]
            mode = "local"
        "#;

    let settings: Settings = toml::from_str(config).expect("settings deserialize");
    let services = &settings.server.grpc_public_services;

    assert!(!services.admin_service.enabled);
    assert!(!services.storage_service.enabled);
    assert!(!services.revision_service.enabled);
    assert!(!services.repository_service.enabled);
    assert!(!services.environment_service.enabled);
    assert!(!services.thin_client_service.enabled);
    assert!(!services.lock_service.enabled);
    assert!(!services.notification_service.enabled);
}

#[test]
fn test_settings_empty_plugins_and_hooks() {
    let config = r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [server.http]
            enabled = false
            host = "127.0.0.1"
            max_file_size = 1024
            port = 8080
            request_timeout_seconds = 30
            request_body_timeout_seconds = 30
            available_interval_seconds = 5
            available_timeout_seconds = 30
            store_health_check = false

            [immutable_store]
            mode = "local"

            [immutable_store.local]
            path = "/tmp/immutable"
            flush_delay_seconds = 5

            [mutable_store]
            mode = "local"

            [mutable_store.local]
            path = "/tmp/mutable"
            flush_delay_seconds = 5
        "#;

    let settings: Settings = toml::from_str(config).unwrap();
    assert_eq!(settings.immutable_store.mode, "local");
    assert_eq!(settings.mutable_store.mode, "local");
    assert!(settings.plugins.is_empty());
    assert!(settings.hooks.is_empty());
}

#[test]
fn test_hook_settings_disabled_by_default() {
    let config = r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [server.http]
            enabled = false
            host = "127.0.0.1"
            max_file_size = 1024
            port = 8080
            request_timeout_seconds = 30
            request_body_timeout_seconds = 30
            available_interval_seconds = 5
            available_timeout_seconds = 30
            store_health_check = false

            [immutable_store]
            mode = "local"

            [immutable_store.local]
            path = "/tmp/immutable"
            flush_delay_seconds = 5

            [mutable_store]
            mode = "local"

            [mutable_store.local]
            path = "/tmp/mutable"
            flush_delay_seconds = 5

            [hooks.some_hook]
            custom_field = "value"
        "#;

    let settings: Settings = toml::from_str(config).unwrap();
    let some_hook = settings.hooks.get("some_hook").unwrap();
    // enabled defaults to false
    assert!(!some_hook.enabled);
    assert_eq!(
        some_hook
            .config
            .get("custom_field")
            .unwrap()
            .as_str()
            .unwrap(),
        "value"
    );
}

#[test]
fn test_settings_with_lock_store() {
    let config = r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [server.http]
            enabled = false
            host = "127.0.0.1"
            max_file_size = 1024
            port = 8080
            request_timeout_seconds = 30
            request_body_timeout_seconds = 30
            available_interval_seconds = 5
            available_timeout_seconds = 30
            store_health_check = false

            [immutable_store]
            mode = "local"

            [immutable_store.local]
            path = "/tmp/immutable"
            flush_delay_seconds = 5

            [mutable_store]
            mode = "local"

            [mutable_store.local]
            path = "/tmp/mutable"
            flush_delay_seconds = 5

            [lock_store]
            mode = "local"
        "#;

    let settings: Settings = toml::from_str(config).unwrap();
    assert!(settings.lock_store.is_some());
    // "local" mode uses the built-in LocalLockStore (in-memory lock store)
    assert_eq!(settings.lock_store.unwrap().mode, "local");
}

#[test]
fn test_settings_with_builtin_fixed_topology() {
    let config = r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [server.http]
            enabled = false
            host = "127.0.0.1"
            max_file_size = 1024
            port = 8080
            request_timeout_seconds = 30
            request_body_timeout_seconds = 30
            available_interval_seconds = 5
            available_timeout_seconds = 30
            store_health_check = false

            [immutable_store]
            mode = "local"

            [immutable_store.local]
            path = "/tmp/immutable"
            flush_delay_seconds = 5

            [mutable_store]
            mode = "local"

            [mutable_store.local]
            path = "/tmp/mutable"
            flush_delay_seconds = 5

            [topology]
            provider = "fixed"

            [topology.fixed]
            peers = [{ address = "192.168.1.10", port = 9090, locality = "SameRegion" }]
        "#;

    let settings: Settings = toml::from_str(config).unwrap();

    // Verify topology is configured with fixed provider
    let topology = settings.topology.as_ref().unwrap();
    assert!(matches!(
        topology.provider,
        lore_server::topology::TopologyProvider::Fixed
    ));

    // Verify the built-in fixed topology configuration is present
    let fixed = topology.fixed.as_ref().unwrap();
    assert_eq!(fixed.peers.len(), 1);
    assert_eq!(fixed.peers[0].address, "192.168.1.10");
    assert_eq!(fixed.peers[0].port, 9090);
    assert_eq!(fixed.peers[0].locality, Locality::SameRegion);

    // No plugins should be needed for built-in fixed topology
    assert!(!settings.plugins.contains_key("fixed"));
}

#[test]
fn test_settings_with_no_topology() {
    let config = r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [server.http]
            enabled = false
            host = "127.0.0.1"
            max_file_size = 1024
            port = 8080
            request_timeout_seconds = 30
            request_body_timeout_seconds = 30
            available_interval_seconds = 5
            available_timeout_seconds = 30
            store_health_check = false

            [immutable_store]
            mode = "local"

            [immutable_store.local]
            path = "/tmp/immutable"
            flush_delay_seconds = 5

            [mutable_store]
            mode = "local"

            [mutable_store.local]
            path = "/tmp/mutable"
            flush_delay_seconds = 5
        "#;

    let settings: Settings = toml::from_str(config).unwrap();

    // Verify topology is not configured
    assert!(settings.topology.is_none());
}

#[test]
fn test_settings_with_none_topology_provider() {
    let config = r#"
            [server]
            runtime_shutdown_timeout_seconds = 0

            [server.http]
            enabled = false
            host = "127.0.0.1"
            max_file_size = 1024
            port = 8080
            request_timeout_seconds = 30
            request_body_timeout_seconds = 30
            available_interval_seconds = 5
            available_timeout_seconds = 30
            store_health_check = false

            [immutable_store]
            mode = "local"

            [immutable_store.local]
            path = "/tmp/immutable"
            flush_delay_seconds = 5

            [mutable_store]
            mode = "local"

            [mutable_store.local]
            path = "/tmp/mutable"
            flush_delay_seconds = 5

            [topology]
            provider = "none"
        "#;

    let settings: Settings = toml::from_str(config).unwrap();

    // Verify topology has 'none' provider
    let topology = settings.topology.as_ref().unwrap();
    assert!(matches!(
        topology.provider,
        lore_server::topology::TopologyProvider::None
    ));
}

// =========================================================================
// Config File Validation Tests
// =========================================================================
//
// These tests validate all configuration files in `lore-server/config/` to ensure:
// 1. All config files can be loaded and parsed successfully
// 2. Plugin configurations are correctly structured
// 3. Plugins referenced in configs can be loaded by the current binary
//
// TODO(mjansson): The Settings and ServerSettings structs SHOULD use `#[serde(deny_unknown_fields)]`
//                 to catch invalid config sections (e.g., `[server.lock_store]` instead of `[lock_store]`).

/// Store modes that are handled directly (not via plugins)
const CORE_STORE_MODES: &[&str] = &["local", "composite", "remote"];

/// Finds the config directory relative to the workspace root.
fn find_config_dir() -> PathBuf {
    // Try multiple potential locations for the config directory
    let potential_paths = [
        PathBuf::from("lore-server/config"),
        PathBuf::from("config"),
        PathBuf::from("../lore-server/config"),
        PathBuf::from("../../lore-server/config"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config"),
    ];

    for path in &potential_paths {
        if path.exists() && path.is_dir() {
            return path.clone();
        }
    }

    // Last resort: use the CARGO_MANIFEST_DIR relative path
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config")
}

/// Discovers all config files in the config directory.
/// Excludes `.example` files which are templates.
///
/// Test-only, and the `current_dir` read is diagnostic text in the panic
/// message identifying where the search started from.
#[allow(clippy::disallowed_methods)]
fn discover_standalone_config_files() -> Vec<PathBuf> {
    let config_dir = find_config_dir();

    if !config_dir.exists() {
        panic!(
            "Config directory not found. Tried: {:?}. Current dir: {:?}",
            config_dir,
            std::env::current_dir()
        );
    }

    let mut config_files = Vec::new();
    let region_suffixes = ["us-east-2", "ap-southeast-1", "eu-west-3"];

    for entry in fs::read_dir(&config_dir).expect("Failed to read config directory") {
        let entry = entry.expect("Failed to read directory entry");
        let path = entry.path();

        if let Some(extension) = path.extension()
            && extension == "toml"
        {
            let file_name = path.file_name().unwrap_or_default().to_string_lossy();
            // skip intentionally partial regional-override configs
            let is_region_override = region_suffixes
                .iter()
                .any(|r| file_name.ends_with(&format!("_{r}.toml")));
            // Skip example files
            let is_example = file_name.ends_with(".example") || file_name.contains(".example.");

            if !is_example && !is_region_override {
                config_files.push(path);
            }
        }
    }

    config_files.sort();
    config_files
}

/// Loads and merges config files similar to how the binary does it.
/// This loads default.toml and merges with the environment-specific config.
fn load_merged_config(config_path: &Path) -> Result<Settings, String> {
    let config_dir = config_path.parent().unwrap_or(Path::new("."));
    let default_path = config_dir.join("default.toml");

    // Load default config
    let default_content = fs::read_to_string(&default_path)
        .map_err(|e| format!("Failed to read default.toml at {default_path:?}: {e}"))?;

    let mut default_settings: toml::Value = toml::from_str(&default_content)
        .map_err(|e| format!("Failed to parse default.toml at {default_path:?}: {e}"))?;

    // If this is not the default.toml itself, merge with the environment config
    if config_path.file_name().unwrap_or_default() != "default.toml" {
        let env_content = fs::read_to_string(config_path)
            .map_err(|e| format!("Failed to read config at {config_path:?}: {e}"))?;

        let env_settings: toml::Value = toml::from_str(&env_content)
            .map_err(|e| format!("Failed to parse config at {config_path:?}: {e}"))?;

        // Deep merge env_settings into default_settings
        merge_toml_values(&mut default_settings, &env_settings);
    }

    // Deserialize the merged config
    default_settings
        .clone()
        .try_into()
        .map_err(|e| format!("Failed to deserialize merged config for {config_path:?}: {e}"))
}

/// Deep merges two TOML values. Source values override target values.
fn merge_toml_values(target: &mut toml::Value, source: &toml::Value) {
    match (target, source) {
        (toml::Value::Table(target_table), toml::Value::Table(source_table)) => {
            for (key, source_value) in source_table {
                if let Some(target_value) = target_table.get_mut(key) {
                    merge_toml_values(target_value, source_value);
                } else {
                    target_table.insert(key.clone(), source_value.clone());
                }
            }
        }
        (target, source) => {
            *target = source.clone();
        }
    }
}

/// Checks if a mode requires plugin configuration.
fn requires_plugin_config(mode: &str) -> bool {
    !CORE_STORE_MODES.contains(&mode)
}

/// Validates that a plugin configuration exists and is properly structured.
fn validate_plugin_config(
    plugins: &HashMap<String, toml::Value>,
    mode: &str,
    store_type: &str,
) -> Result<(), String> {
    if !requires_plugin_config(mode) {
        return Ok(());
    }

    // Check if plugin config exists
    if !plugins.contains_key(mode) {
        return Err(format!(
            "Mode '{mode}' requires plugin configuration [plugins.{mode}], but it's missing"
        ));
    }

    // For AWS plugin, verify the store-specific section exists or can be inferred
    if let Some(plugin_config) = plugins.get(mode) {
        // Check if there's a store-specific section
        let has_store_section = plugin_config.get(store_type).is_some();

        // For AWS plugin, we require store-specific config sections
        if mode == "aws" && !has_store_section {
            return Err(format!(
                "AWS plugin config [plugins.aws] is missing [plugins.aws.{store_type}] section"
            ));
        }
    }

    Ok(())
}

/// Creates a plugin registry with all compiled-in plugins registered.
fn create_test_registry() -> PluginRegistry {
    let mut registry = PluginRegistry::new();
    lore_server::plugins::register_all_plugins(&mut registry);
    registry
}

#[test]
fn test_discover_config_files() {
    let config_files = discover_standalone_config_files();

    // We should find at least the default.toml and one environment config
    assert!(
        !config_files.is_empty(),
        "No config files found in config directory"
    );

    // Verify default.toml is present
    let has_default = config_files
        .iter()
        .any(|p| p.file_name().unwrap_or_default() == "default.toml");
    assert!(has_default, "default.toml not found in config files");

    println!("Discovered {} config files:", config_files.len());
    for file in &config_files {
        println!("  - {}", file.display());
    }
}

#[test]
fn test_all_config_files_load_successfully() {
    let config_files = discover_standalone_config_files();
    let mut failures: Vec<String> = Vec::new();

    println!("\n=== Config Loading Validation ===\n");

    for config_path in &config_files {
        let config_name = config_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();

        match load_merged_config(config_path) {
            Ok(settings) => {
                println!("✓ {config_name} - loaded successfully");
                println!(
                    "    immutable_store.mode = {}, mutable_store.mode = {}",
                    settings.immutable_store.mode, settings.mutable_store.mode
                );
            }
            Err(e) => {
                let msg = format!("✗ {config_name} - FAILED: {e}");
                println!("{msg}");
                failures.push(msg);
            }
        }
    }

    if !failures.is_empty() {
        panic!("\n\nConfig loading failures:\n{}\n", failures.join("\n"));
    }

    println!(
        "\n=== All {} config files loaded successfully ===\n",
        config_files.len()
    );
}

#[test]
fn test_all_config_files_have_valid_plugin_configs() {
    let config_files = discover_standalone_config_files();
    let mut failures: Vec<String> = Vec::new();

    println!("\n=== Plugin Configuration Validation ===\n");

    for config_path in &config_files {
        let config_name = config_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();

        let settings = match load_merged_config(config_path) {
            Ok(s) => s,
            Err(e) => {
                failures.push(format!("{config_name}: Failed to load - {e}"));
                continue;
            }
        };

        let mut config_errors: Vec<String> = Vec::new();

        // Validate immutable store configuration
        let immutable_mode = &settings.immutable_store.mode;
        if let Err(e) = validate_plugin_config(&settings.plugins, immutable_mode, "immutable_store")
        {
            config_errors.push(format!("immutable_store: {e}"));
        }

        // Handle composite store - check durable tier
        if immutable_mode == "composite"
            && let Some(composite) = &settings.immutable_store.composite
            && let Some(durable) = &composite.durable
            && let Err(e) =
                validate_plugin_config(&settings.plugins, &durable.mode, "immutable_store")
        {
            config_errors.push(format!("composite.durable: {e}"));
        }

        // Validate mutable store configuration
        let mutable_mode = &settings.mutable_store.mode;
        if let Err(e) = validate_plugin_config(&settings.plugins, mutable_mode, "mutable_store") {
            config_errors.push(format!("mutable_store: {e}"));
        }

        // Validate topology configuration
        if let Some(topology) = &settings.topology
            && let Some(plugin_name) = topology.provider.plugin_name()
            && !settings.plugins.contains_key(plugin_name)
        {
            // Check if inline config is available as fallback (only for fixed topology)
            let has_inline = match topology.provider {
                TopologyProvider::Fixed => topology.fixed.is_some(),
                TopologyProvider::RotatingIdFixed => topology.rotating_id_fixed.is_some(),
                TopologyProvider::Composite => topology.composite.is_some(),
                // Consul requires plugin configuration
                TopologyProvider::Consul => false,
                TopologyProvider::None => true,
            };

            if !has_inline {
                config_errors.push(format!(
                    "topology: Provider '{plugin_name}' requires [plugins.{plugin_name}] configuration"
                ));
            }
        }

        if config_errors.is_empty() {
            println!("✓ {config_name} - plugin configs valid");
        } else {
            let error_msg = format!(
                "✗ {} - INVALID:\n    {}",
                config_name,
                config_errors.join("\n    ")
            );
            println!("{error_msg}");
            failures.push(error_msg);
        }
    }

    if !failures.is_empty() {
        panic!(
            "\n\nPlugin configuration validation failures:\n{}\n",
            failures.join("\n")
        );
    }

    println!("\n=== All config files have valid plugin configurations ===\n");
}

#[test]
fn test_registered_plugins_match_config_requirements() {
    let registry = create_test_registry();

    println!("\n=== Registered Plugins ===\n");
    println!(
        "Immutable store plugins: {:?}",
        registry.list_immutable_store_plugins()
    );
    println!(
        "Mutable store plugins: {:?}",
        registry.list_mutable_store_plugins()
    );
    println!(
        "Lock store plugins: {:?}",
        registry.list_lock_store_plugins()
    );
    println!("Topology plugins: {:?}", registry.list_topology_plugins());
    println!(
        "\nNote: Core modes (local, composite, remote) are handled directly, not via plugins."
    );
    println!(
        "Note: External plugins (aws, consul) are registered in derived crates (e.g., lore-server-epic)."
    );

    let immutable_plugins = registry.list_immutable_store_plugins();
    let mutable_plugins = registry.list_mutable_store_plugins();
    let topology_plugins = registry.list_topology_plugins();

    // Local stores should NOT be in the plugin list - they are core modes
    // handled directly in the server code, not via the plugin system
    assert!(
        !immutable_plugins.contains(&"local".to_string()),
        "Local immutable store should NOT be registered as a plugin (it's a core mode)"
    );
    assert!(
        !mutable_plugins.contains(&"local".to_string()),
        "Local mutable store should NOT be registered as a plugin (it's a core mode)"
    );

    // Fixed topology is a built-in feature, NOT a plugin.
    assert!(
        !topology_plugins.contains(&"fixed".to_string()),
        "Fixed topology should NOT be registered as a plugin (it's a built-in feature)"
    );
}

#[test]
fn test_plugin_configs_can_be_parsed() {
    let config_files = discover_standalone_config_files();
    let registry = create_test_registry();
    let mut results: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    println!("\n=== Plugin Config Parsing Validation ===\n");
    println!("Note: External plugins (aws, consul) are registered in derived");
    println!("crates (e.g., lore-server-epic). Only registered plugins are validated.\n");

    for config_path in &config_files {
        let start = Instant::now();

        let config_name = config_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();

        let settings = match load_merged_config(config_path) {
            Ok(s) => s,
            Err(e) => {
                failures.push(format!("{config_name}: Failed to load - {e}"));
                continue;
            }
        };

        let mut parse_results: Vec<String> = Vec::new();
        let mut parse_errors: Vec<String> = Vec::new();

        // Test immutable store plugin config parsing
        let immutable_mode = &settings.immutable_store.mode;
        if requires_plugin_config(immutable_mode) {
            if registry
                .list_immutable_store_plugins()
                .contains(&immutable_mode.clone())
            {
                if let Some(plugin_config) = resolve_plugin_config_with_fallback(
                    &settings.plugins,
                    immutable_mode,
                    "immutable_store",
                ) {
                    match registry.validate_immutable_store_config(immutable_mode, &plugin_config) {
                        Ok(()) => {
                            parse_results.push(format!("immutable_store[{immutable_mode}]: ✓"));
                        }
                        Err(e) => {
                            parse_errors.push(format!("immutable_store[{immutable_mode}]: {e}"));
                        }
                    }
                }
            } else {
                parse_results.push(format!(
                    "immutable_store[{immutable_mode}]: ✓ (external plugin)"
                ));
            }
        } else {
            parse_results.push(format!("immutable_store[{immutable_mode}]: ✓ (core mode)"));
        }

        // Test composite durable tier
        if immutable_mode == "composite"
            && let Some(composite) = &settings.immutable_store.composite
            && let Some(durable) = &composite.durable
        {
            let durable_mode = &durable.mode;
            if requires_plugin_config(durable_mode) {
                if registry
                    .list_immutable_store_plugins()
                    .contains(&durable_mode.clone())
                {
                    if let Some(plugin_config) = resolve_plugin_config_with_fallback(
                        &settings.plugins,
                        durable_mode,
                        "immutable_store",
                    ) {
                        match registry.validate_immutable_store_config(durable_mode, &plugin_config)
                        {
                            Ok(()) => {
                                parse_results.push(format!("composite.durable[{durable_mode}]: ✓"));
                            }
                            Err(e) => {
                                parse_errors
                                    .push(format!("composite.durable[{durable_mode}]: {e}"));
                            }
                        }
                    }
                } else {
                    parse_results.push(format!(
                        "composite.durable[{durable_mode}]: ✓ (external plugin)"
                    ));
                }
            }
        }

        // Test mutable store plugin config parsing
        let mutable_mode = &settings.mutable_store.mode;
        if requires_plugin_config(mutable_mode) {
            if registry
                .list_mutable_store_plugins()
                .contains(&mutable_mode.clone())
            {
                if let Some(plugin_config) = resolve_plugin_config_with_fallback(
                    &settings.plugins,
                    mutable_mode,
                    "mutable_store",
                ) {
                    match registry.validate_mutable_store_config(mutable_mode, &plugin_config) {
                        Ok(()) => {
                            parse_results.push(format!("mutable_store[{mutable_mode}]: ✓"));
                        }
                        Err(e) => {
                            parse_errors.push(format!("mutable_store[{mutable_mode}]: {e}"));
                        }
                    }
                }
            } else {
                parse_results.push(format!(
                    "mutable_store[{mutable_mode}]: ✓ (external plugin)"
                ));
            }
        } else {
            parse_results.push(format!("mutable_store[{mutable_mode}]: ✓ (core mode)"));
        }

        // Test topology plugin config parsing
        if let Some(topology) = &settings.topology
            && let Some(plugin_name) = topology.provider.plugin_name()
            && let Some(plugin_config) = settings.plugins.get(plugin_name)
        {
            if registry
                .list_topology_plugins()
                .contains(&plugin_name.to_string())
            {
                match registry.validate_topology_config(plugin_name, plugin_config) {
                    Ok(()) => {
                        parse_results.push(format!("topology[{plugin_name}]: ✓"));
                    }
                    Err(e) => {
                        let error_msg = e.to_string();
                        let is_config_error = error_msg.contains("configuration error");
                        if is_config_error {
                            let is_expected_env_injected_field =
                                plugin_name == "consul" && error_msg.contains("address");

                            if is_expected_env_injected_field {
                                parse_results.push(format!(
                                    "topology[{plugin_name}]: ✓ (address provided via env)"
                                ));
                            } else {
                                parse_errors.push(format!("topology[{plugin_name}]: {e}"));
                            }
                        } else {
                            parse_errors.push(format!("topology[{plugin_name}]: {e}"));
                        }
                    }
                }
            } else {
                parse_results.push(format!("topology[{plugin_name}]: ✓ (external plugin)"));
            }
        }

        if parse_errors.is_empty() {
            let result = format!("✓ {}\n    {}", config_name, parse_results.join("\n    "));
            println!("{} ({:.2}s)", result, start.elapsed().as_secs_f32());
            results.push(result);
        } else {
            let error_msg = format!(
                "✗ {}\n    Passed: {}\n    Failed: {}",
                config_name,
                parse_results.join(", "),
                parse_errors.join("\n    ")
            );
            println!("{error_msg}");
            failures.push(error_msg);
        }
    }

    if !failures.is_empty() {
        panic!(
            "\n\nPlugin config parsing failures:\n{}\n",
            failures.join("\n")
        );
    }

    println!("\n=== All plugin configs can be parsed ===\n");
}

#[test]
fn test_default_config_is_local_only() {
    let config_dir = find_config_dir();
    let default_path = config_dir.join("default.toml");

    let settings = load_merged_config(&default_path).expect("default.toml should load");

    // Default config should use local stores (no external dependencies)
    assert_eq!(
        settings.immutable_store.mode, "local",
        "default.toml should use local immutable store"
    );
    assert_eq!(
        settings.mutable_store.mode, "local",
        "default.toml should use local mutable store"
    );

    // Should not have AWS plugin config (or if it does, it's optional)
    // This is intentional - default should work without any cloud services
}

#[test]
fn test_gha_config_is_local_only() {
    let config_dir = find_config_dir();
    let gha_path = config_dir.join("gha.toml");

    if !gha_path.exists() {
        println!("Skipping test - gha.toml not found");
        return;
    }

    let settings = load_merged_config(&gha_path).expect("gha.toml should load");

    // GHA config should use local stores for CI testing
    assert_eq!(
        settings.immutable_store.mode, "local",
        "gha.toml should use local immutable store for CI"
    );
    assert_eq!(
        settings.mutable_store.mode, "local",
        "gha.toml should use local mutable store for CI"
    );
}

#[test]
fn test_production_configs() {
    let config_files = discover_standalone_config_files();

    // Production configs that should use AWS
    let production_configs = [
        "ci.toml",
        "gamedev.toml",
        "benchmark.toml",
        "uefn-live.toml",
        "uefn-canary.toml",
        "uefn-livetesting.toml",
        "live-internal.toml",
    ];

    for config_name in &production_configs {
        let config_path = config_files
            .iter()
            .find(|p| p.file_name().unwrap_or_default().to_string_lossy() == *config_name);

        if let Some(config_path) = config_path {
            let settings = load_merged_config(config_path)
                .unwrap_or_else(|_| panic!("{config_name} should load"));

            // Production configs should use composite or aws for immutable store
            let immutable_mode = &settings.immutable_store.mode;
            assert!(
                immutable_mode == "composite" || immutable_mode == "aws",
                "{config_name} should use composite or aws immutable store, found: {immutable_mode}"
            );

            // Production configs should use aws for mutable store
            assert_eq!(
                settings.mutable_store.mode, "aws",
                "{config_name} should use aws mutable store"
            );

            // Should have AWS plugin configuration
            assert!(
                settings.plugins.contains_key("aws"),
                "{config_name} should have [plugins.aws] configuration"
            );

            println!("✓ {config_name} correctly uses AWS stores");
        }
    }
}

#[test]
fn test_composite_store_durable_modes_are_valid() {
    let config_files = discover_standalone_config_files();
    let registry = create_test_registry();
    let registered_plugins = registry.list_immutable_store_plugins();

    println!("\n=== Composite Store Durable Mode Validation ===\n");
    println!("Note: External plugin modes (e.g., aws) are accepted without validation.");
    println!("They are validated in the derived crate (e.g., lore-server-epic).\n");

    for config_path in &config_files {
        let config_name = config_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();

        let settings = match load_merged_config(config_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        if settings.immutable_store.mode != "composite" {
            continue;
        }

        if let Some(composite) = &settings.immutable_store.composite {
            // Validate local tier mode - must be core or a known plugin
            let local_mode = &composite.local.mode;
            let local_valid = CORE_STORE_MODES.contains(&local_mode.as_str())
                || registered_plugins.contains(&local_mode.clone());

            // Validate durable tier mode if present
            if let Some(durable) = &composite.durable {
                let durable_mode = &durable.mode;
                let durable_valid = CORE_STORE_MODES.contains(&durable_mode.as_str())
                    || registered_plugins.contains(&durable_mode.clone());

                let local_label = if local_valid {
                    "✓".to_string()
                } else {
                    "external".to_string()
                };
                let durable_label = if durable_valid {
                    "✓".to_string()
                } else {
                    "external".to_string()
                };
                println!(
                    "✓ {config_name} - composite store: local={local_mode} ({local_label}), durable={durable_mode} ({durable_label})"
                );
            } else {
                println!("✓ {config_name} - composite store: local={local_mode}, durable=none");
            }
        }
    }
}

#[test]
fn test_config_validation_summary() {
    let config_files = discover_standalone_config_files();
    let registry = create_test_registry();

    println!("\n");
    println!("╔════════════════════════════════════════════════════════════════╗");
    println!("║         Lore Server Configuration Validation Summary            ║");
    println!("╠════════════════════════════════════════════════════════════════╣");
    println!("║                                                                ║");
    println!(
        "║  Config files found: {:>3}                                      ║",
        config_files.len()
    );
    println!("║                                                                ║");
    println!("║  Registered Plugins:                                           ║");
    println!(
        "║    - Immutable stores: {:?}",
        registry.list_immutable_store_plugins()
    );
    println!(
        "║    - Mutable stores:   {:?}",
        registry.list_mutable_store_plugins()
    );
    println!(
        "║    - Lock stores:      {:?}",
        registry.list_lock_store_plugins()
    );
    println!(
        "║    - Topology:         {:?}",
        registry.list_topology_plugins()
    );
    println!("║                                                                ║");
    println!("╠════════════════════════════════════════════════════════════════╣");
    println!("║  Config Files:                                                 ║");

    for config_path in &config_files {
        let config_name = config_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let status = match load_merged_config(config_path) {
            Ok(settings) => {
                format!(
                    "✓ {} | imm: {}, mut: {}",
                    config_name, settings.immutable_store.mode, settings.mutable_store.mode
                )
            }
            Err(e) => format!("✗ {config_name} | ERROR: {e}"),
        };
        println!("║  {status}  ");
    }

    println!("║                                                                ║");
    println!("╚════════════════════════════════════════════════════════════════╝");
    println!("\n");
}

#[test]
fn http_health_only_is_opt_in() {
    assert!(!http_settings("").health_only);
    assert!(!http_settings("health_only = false").health_only);
    assert!(http_settings("health_only = true").health_only);
}

mod deserialize_quic_internal {
    use lore_server::quic::initial_cwnd::DEFAULT_PEER_INITIAL_CWND_BYTES;

    use super::*;

    const ENDPOINT_KEYS: &str = "host = \"0.0.0.0\"\nport = 41337\nnum_listeners = 10";

    fn parse(extra_keys: &str) -> ServerSettings {
        toml::from_str(&format!(
            "[quic]\n{ENDPOINT_KEYS}\n[quic_internal]\n{ENDPOINT_KEYS}\n{extra_keys}"
        ))
        .expect("[server] should deserialize")
    }

    #[test]
    fn internal_endpoint_defaults_to_the_peer_window() {
        let quic_internal = parse("")
            .quic_internal
            .expect("[server.quic_internal] present");

        assert_eq!(
            quic_internal.initial_cwnd_bytes,
            Some(DEFAULT_PEER_INITIAL_CWND_BYTES)
        );
    }

    #[test]
    fn internal_endpoint_keeps_a_configured_window() {
        let quic_internal = parse("initial_cwnd_bytes = 65536")
            .quic_internal
            .expect("[server.quic_internal] present");

        let initial_cwnd = quic_internal
            .initial_cwnd_bytes
            .expect("initial_cwnd_bytes set");
        assert_eq!(initial_cwnd.get(), 65_536);
    }

    #[test]
    fn absent_internal_endpoint_stays_absent() {
        let server: ServerSettings =
            toml::from_str("").expect("[server] with no tables should deserialize");

        assert!(server.quic_internal.is_none());
    }
}

mod quic_settings_initial_cwnd_bytes {
    use super::*;

    #[test]
    fn omitted_initial_cwnd_is_left_to_the_controller() {
        let quic: QuicSettings =
            toml::from_str("host = \"0.0.0.0\"\nport = 41337\nnum_listeners = 10")
                .expect("[server.quic] should deserialize");

        assert_eq!(quic.initial_cwnd_bytes, None);
    }
}
