// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_transport::auth::require_custom_resources;

#[test]
fn one_or_more_resources_are_accepted() {
    assert!(require_custom_resources(&["urc-user-service"]).is_ok());
    assert!(require_custom_resources(&["urc-user-service", "stream:a:*"]).is_ok());
}

#[test]
fn no_resource_is_refused() {
    assert!(require_custom_resources(&[]).is_err());
}

#[test]
fn an_empty_resource_id_is_refused() {
    assert!(require_custom_resources(&[""]).is_err());
    assert!(require_custom_resources(&["urc-user-service", ""]).is_err());
}
