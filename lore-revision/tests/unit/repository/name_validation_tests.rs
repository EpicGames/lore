// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Coverage for [`is_valid_name`], in particular the segment rules that keep
//! a name stable through the URL parsing in [`parse_url`].
use lore_revision::repository::MAX_NAME_LEN;
use lore_revision::repository::is_valid_name;
use lore_revision::repository::parse_url;

#[test]
fn accepts_names_with_dots_inside_segments() {
    for name in ["my-repo", "org/my-repo", "my.repo", "org/v1.2/repo_a"] {
        assert!(is_valid_name(name), "{name} should be valid");
    }
}

#[test]
fn rejects_dot_segments() {
    for name in [".", "..", "./repo", "org/../other", "org/repo/..", "../.."] {
        assert!(!is_valid_name(name), "{name} should be rejected");
    }
}

#[test]
fn rejects_dot_leading_segments() {
    // Not just the traversal-like `.` and `..`: any leading dot is rejected.
    for name in [
        "...",
        ".hidden",
        ".lore",
        "org/...",
        "org/.git",
        "org/.git/repo",
    ] {
        assert!(!is_valid_name(name), "{name} should be rejected");
    }
}

#[test]
fn rejects_empty_segments() {
    for name in ["", "/", "//", "/repo", "repo/", "repo//", "org//repo"] {
        assert!(!is_valid_name(name), "{name} should be rejected");
    }
}

#[test]
fn rejects_disallowed_characters_and_oversized_names() {
    for name in ["repo name", "org\\repo", "repo!", "org/repö"] {
        assert!(!is_valid_name(name), "{name} should be rejected");
    }
    assert!(is_valid_name(&"a".repeat(MAX_NAME_LEN)));
    assert!(!is_valid_name(&"a".repeat(MAX_NAME_LEN + 1)));
}

#[test]
fn every_valid_name_survives_url_parsing() {
    // The point of the segment rules: a valid name is returned unchanged by
    // `parse_url`, so the repository a client asks for is the one it created.
    for name in ["repo", "org/repo", "org/v1.2/repo_a", "org/a.b.c/d-e"] {
        let (_remote_url, parsed) = parse_url(&format!("lores://host/{name}"), false)
            .unwrap_or_else(|err| panic!("{name} should parse: {err}"));
        assert_eq!(parsed, name);
    }
}

#[test]
fn rejected_names_are_the_ones_url_parsing_rewrites() {
    // Each of these either loses the name or resolves to a different one.
    // Tested because the parse_url logic dictates what kind of names we can allow.
    // If parse_url changes, the is_valid_name logic needs to be re-evaluated.
    for (url_name, parsed_as) in [
        (".", ""),
        ("..", ""),
        ("org/../other", "other"),
        ("./repo", "repo"),
        ("repo/", "repo"),
        ("/repo/test//", "repo/test"),
    ] {
        assert!(!is_valid_name(url_name), "{url_name} should be rejected");
        let parsed = parse_url(&format!("lores://host/{url_name}"), false)
            .map_or_default(|(_remote_url, name)| name);
        assert_eq!(parsed, parsed_as, "for {url_name}");
    }
}
