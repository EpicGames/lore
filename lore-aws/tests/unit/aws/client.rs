// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_aws::aws::*;

/// Every service client in a process resolves the same identity, so one cache serves them all: a
/// cache each would refresh each, and an invalidation learned from the service by one would not
/// reach the others.
#[test]
fn one_credential_chain_serves_every_client_in_a_region() {
    let first = default_credentials_chain("eu-west-1").expect("a chain builds");
    let again = default_credentials_chain("eu-west-1").expect("a chain builds");

    assert!(
        Arc::ptr_eq(&first, &again),
        "a second client must take the chain the first built"
    );

    let elsewhere = default_credentials_chain("us-east-1").expect("a chain builds");

    assert!(
        !Arc::ptr_eq(&first, &elsewhere),
        "the STS endpoint is regional, so a second region is a second chain"
    );
}

#[test]
fn endpoints_follow_the_regional_pattern() {
    assert_eq!(
        default_endpoint("dynamodb", "eu-west-1"),
        "https://dynamodb.eu-west-1.amazonaws.com"
    );
    assert_eq!(
        default_endpoint("dynamodb", "cn-north-1"),
        "https://dynamodb.cn-north-1.amazonaws.com.cn",
        "the China regions are a partition of their own"
    );
}

/// An endpoint behind a gateway can carry a path of its own, which every request keeps.
#[test]
fn a_request_path_follows_the_endpoints_own_path() {
    let core = |endpoint: &str| {
        ClientCore::new(
            http::HttpClient::new(&lore_aws::clients::HttpClientSettings::default())
                .expect("a client builds"),
            Arc::new(creds::CachedCredentials::new(Arc::new(
                creds::StaticProvider::new(creds::Credentials::new("AKID", "secret", None, None)),
            ))),
            Arc::from("eu-west-1"),
            url::Url::parse(endpoint).expect("a test endpoint parses"),
            std::time::Duration::from_secs(1),
            false,
        )
    };

    assert_eq!(
        core("https://dynamodb.eu-west-1.amazonaws.com")
            .url("/", None)
            .as_str(),
        "https://dynamodb.eu-west-1.amazonaws.com/"
    );
    assert_eq!(
        core("https://gw.example.com/dynamodb/")
            .url("/", None)
            .as_str(),
        "https://gw.example.com/dynamodb/"
    );
    assert_eq!(
        core("https://gw.example.com/dynamodb")
            .url("/table", Some("a=1"))
            .as_str(),
        "https://gw.example.com/dynamodb/table?a=1"
    );
}
