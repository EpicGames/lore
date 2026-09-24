// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Lint / negative checks for the v1 proto surface. Walks the source
//! `.proto` files and asserts the unified-naming + structural rules
//! that the v1 RFC pins.

use std::fs;
use std::path::PathBuf;

const V1_PROTO_FILES: &[&str] = &[
    "proto/lore/model/v1/model.proto",
    "proto/lore/repository/v1/repository.proto",
    "proto/lore/revision/v1/revision.proto",
    "proto/lore/thin_client/v1/model.proto",
    "proto/lore/thin_client/v1/thin_client.proto",
    "proto/lore/user/v1/user.proto",
];

fn read_v1(rel: &str) -> String {
    let crate_dir = env!("CARGO_MANIFEST_DIR");
    let path = PathBuf::from(crate_dir).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn non_comment_lines(content: &str) -> impl Iterator<Item = &str> {
    content.lines().filter(|line| {
        let trimmed = line.trim();
        !trimmed.starts_with("//") && !trimmed.is_empty()
    })
}

#[test]
fn no_urc_references() {
    for file in V1_PROTO_FILES {
        let content = read_v1(file);
        for line in non_comment_lines(&content) {
            assert!(
                !line.contains("urc."),
                "{file}: line `{line}` references a `urc.` type; v1 must not depend on urc"
            );
            assert!(
                !line.starts_with("import \"model.proto\"")
                    && !line.starts_with("import \"revision.proto\""),
                "{file}: imports a top-level urc proto; v1 must only import `lore/...` paths"
            );
        }
    }
}

#[test]
fn no_repository_id_field() {
    for file in V1_PROTO_FILES {
        let content = read_v1(file);
        for line in non_comment_lines(&content) {
            assert!(
                !line.contains("repository_id"),
                "{file}: line `{line}` mentions `repository_id`; v1 requests must keep scoping out-of-band"
            );
        }
    }
}

#[test]
fn no_urc_deprecated_field_names() {
    let forbidden = [
        "revision_deprecated",
        "parent_deprecated",
        "branch_point_deprecated",
    ];
    for file in V1_PROTO_FILES {
        let content = read_v1(file);
        for line in non_comment_lines(&content) {
            for name in &forbidden {
                assert!(
                    !line.contains(name),
                    "{file}: line `{line}` uses deprecated urc field name `{name}`"
                );
            }
        }
    }
}

#[test]
fn no_adjective_first_side_qualifiers() {
    // Field-decl detection: `<type> <name> = <num>;`. For each field name,
    // assert it does not start with a side qualifier followed by `_`.
    let qualifiers = ["from", "to", "self", "other", "base"];
    for file in V1_PROTO_FILES {
        let content = read_v1(file);
        for line in non_comment_lines(&content) {
            let trimmed = line
                .trim()
                .trim_start_matches("optional ")
                .trim_start_matches("repeated ");
            let Some((decl, _)) = trimmed.split_once('=') else {
                continue;
            };
            // decl looks like "<type> <name> "
            let Some(name) = decl.split_whitespace().nth(1) else {
                continue;
            };
            for q in &qualifiers {
                let prefix = format!("{q}_");
                assert!(
                    !name.starts_with(&prefix),
                    "{file}: field `{name}` uses adjective-first side qualifier `{q}_*`; \
                     v1 expects noun-first form (e.g. `signature_{q}` not `{q}_signature`)"
                );
            }
        }
    }
}

#[test]
fn no_dropped_rpcs_or_messages() {
    let dropped = [
        "BranchProtect",
        "BranchUnprotect",
        "BranchRevisionList",
        "RevisionStateHistory",
    ];
    for file in V1_PROTO_FILES {
        let content = read_v1(file);
        for d in &dropped {
            for marker in [format!("rpc {d}"), format!("message {d}")] {
                assert!(
                    !content.contains(&marker),
                    "{file}: contains `{marker}`; this RPC/message was dropped from v1"
                );
            }
        }
    }
}

#[test]
fn doc_comments_on_messages_and_enums() {
    for file in V1_PROTO_FILES {
        let content = read_v1(file);
        let lines: Vec<&str> = content.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            let introduces_top_level = (trimmed.starts_with("message ")
                || trimmed.starts_with("enum ")
                || trimmed.starts_with("service "))
                && trimmed.ends_with('{');
            // Skip nested types — keep the check on top-level / one-level-nested
            // declarations (covers Revision.Parent at one nest level too).
            if !introduces_top_level {
                continue;
            }
            // Find the previous non-blank line; it must start with `//`.
            let mut found_doc = false;
            for j in (0..i).rev() {
                let prev = lines[j].trim();
                if prev.is_empty() {
                    continue;
                }
                found_doc = prev.starts_with("//");
                break;
            }
            assert!(
                found_doc,
                "{file}: declaration on line {} is missing a `//` doc comment: `{trimmed}`",
                i + 1
            );
        }
    }
}
