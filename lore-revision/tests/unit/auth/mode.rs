// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::auth::AuthMode;
use lore_revision::auth::AuthPath;

#[test]
fn the_default_is_auto_and_is_zero() {
    assert_eq!(AuthMode::default(), AuthMode::Auto);
    assert_eq!(AuthMode::Auto as i32, 0);
}

#[test]
fn every_name_parses_whatever_its_case_or_surrounding_space() {
    assert_eq!("auto".parse(), Ok(AuthMode::Auto));
    assert_eq!("grpc".parse(), Ok(AuthMode::Grpc));
    assert_eq!("oidc".parse(), Ok(AuthMode::Oidc));
    assert_eq!("OIDC".parse(), Ok(AuthMode::Oidc));
    assert_eq!(" Grpc\n".parse(), Ok(AuthMode::Grpc));
}

#[test]
fn a_name_that_is_not_a_mode_is_refused_and_the_choices_are_named() {
    let error = "odic"
        .parse::<AuthMode>()
        .expect_err("a misspelt mode must not parse");
    let message = error.to_string();
    assert!(message.contains("'odic'"), "{message}");
    for name in ["auto", "grpc", "oidc"] {
        assert!(message.contains(name), "{message} must name {name}");
    }
    assert!("".parse::<AuthMode>().is_err());
}

#[test]
fn the_name_round_trips_through_parsing() {
    for mode in [AuthMode::Auto, AuthMode::Grpc, AuthMode::Oidc] {
        assert_eq!(mode.to_string().parse(), Ok(mode));
        assert_eq!(mode.name(), mode.to_string());
    }
}

#[test]
fn serde_uses_the_lowercase_names() {
    assert_eq!(
        serde_json::to_string(&AuthMode::Oidc).expect("serializable"),
        "\"oidc\""
    );
    assert_eq!(
        serde_json::from_str::<AuthMode>("\"grpc\"").expect("deserializable"),
        AuthMode::Grpc
    );
    assert!(serde_json::from_str::<AuthMode>("\"Oidc\"").is_err());
}

#[test]
fn auto_follows_the_servers_preference_and_an_explicit_mode_does_not() {
    assert_eq!(AuthMode::Auto.path(false), AuthPath::Grpc);
    assert_eq!(AuthMode::Auto.path(true), AuthPath::Oidc);

    assert_eq!(AuthMode::Grpc.path(false), AuthPath::Grpc);
    assert_eq!(AuthMode::Grpc.path(true), AuthPath::Grpc);

    assert_eq!(AuthMode::Oidc.path(false), AuthPath::Oidc);
    assert_eq!(AuthMode::Oidc.path(true), AuthPath::Oidc);
}
