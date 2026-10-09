// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore::branch::*;
use lore::interface::LoreString;

#[test]
fn cascade_scope_maps_each_field_combination() {
    assert!(matches!(
        CascadeScope::new(&LoreString::from(""), 0, "link", "include_links"),
        Ok(CascadeScope::OuterOnly)
    ));
    assert!(matches!(
        CascadeScope::new(&LoreString::from(""), 1, "link", "include_links"),
        Ok(CascadeScope::All)
    ));
    assert!(matches!(
        CascadeScope::new(&LoreString::from("lnk"), 0, "link", "include_links"),
        Ok(CascadeScope::Single(path)) if path == "lnk"
    ));
}

#[test]
fn cascade_scope_rejects_a_path_together_with_include_all() {
    let scope = CascadeScope::new(&LoreString::from("lnk"), 1, "link", "include_links");

    let err = scope.expect_err("a path with include_all must be refused");
    assert!(
        err.to_string().contains("link and include_links"),
        "expected the conflicting fields to be named, got: {err}"
    );
}
