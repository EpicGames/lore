// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use lore_revision::util::url::same_server;

#[test]
fn the_same_url_is_the_same_server() {
    assert!(same_server(
        "lore://127.0.0.1:41337",
        "lore://127.0.0.1:41337"
    ));
}

#[test]
fn a_different_scheme_is_the_same_server() {
    assert!(same_server(
        "lore://127.0.0.1:41337",
        "lores://127.0.0.1:41337"
    ));
    assert!(same_server(
        "grpc://127.0.0.1:41337",
        "lores://127.0.0.1:41337"
    ));
}

#[test]
fn a_trailing_slash_is_the_same_server() {
    assert!(same_server(
        "lore://127.0.0.1:41337/",
        "lore://127.0.0.1:41337"
    ));
}

#[test]
fn host_case_is_ignored() {
    assert!(same_server(
        "lore://Lore.Example:41337",
        "lore://lore.example:41337"
    ));
}

#[test]
fn a_different_host_is_another_server() {
    assert!(!same_server(
        "lore://other.example:41337",
        "lore://127.0.0.1:41337"
    ));
}

#[test]
fn hosts_are_not_resolved() {
    assert!(!same_server(
        "lore://localhost:41337",
        "lore://127.0.0.1:41337"
    ));
}

#[test]
fn a_different_port_is_another_server() {
    assert!(!same_server("lore://127.0.0.1:1", "lore://127.0.0.1:41337"));
}

#[test]
fn a_port_named_on_one_side_only_is_the_same_server() {
    assert!(same_server("lore://127.0.0.1", "lore://127.0.0.1:41337"));
    assert!(same_server("lore://127.0.0.1:41337", "lore://127.0.0.1"));
}

#[test]
fn a_schemeless_remote_takes_the_default_scheme() {
    assert!(same_server("127.0.0.1:41337", "lore://127.0.0.1:41337"));
    assert!(!same_server(
        "other.example:41337",
        "lore://127.0.0.1:41337"
    ));
}

#[test]
fn a_remote_without_a_host_is_no_server() {
    assert!(!same_server("", "lore://127.0.0.1:41337"));
    assert!(!same_server("lore://", "lore://127.0.0.1:41337"));
}

#[test]
fn no_port_on_either_side_is_the_same_server() {
    assert!(same_server("lore://lore.example", "lores://lore.example"));
}

#[test]
fn ipv6_hosts_are_compared_canonically() {
    assert!(same_server("lore://[::1]:41337", "lore://[0:0::1]:41337"));
    assert!(!same_server("lore://[::1]:41337", "lore://[::2]:41337"));
}

#[test]
fn userinfo_and_path_are_ignored() {
    assert!(same_server(
        "lore://user@127.0.0.1:41337/a",
        "lore://127.0.0.1:41337/b"
    ));
}

#[test]
fn a_lore_unix_remote_names_its_upstream_host() {
    assert!(same_server(
        "lore-unix://lore.example/lib",
        "lores://lore.example:41337"
    ));
    assert!(!same_server(
        "lore-unix://other.example/lib",
        "lores://lore.example:41337"
    ));
}
