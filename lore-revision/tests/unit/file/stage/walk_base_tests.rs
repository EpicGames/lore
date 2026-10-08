// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_revision::file::stage::*;
use lore_revision::util::fan_out::AncestorNodes;
use lore_revision::util::path::RelativePath;

const NODE_A: lore_revision::node::NodeID = 11;
const NODE_AB: lore_revision::node::NodeID = 22;

fn created<'a>(entries: &[(&'a str, lore_revision::node::NodeID)]) -> AncestorNodes<'a> {
    entries.iter().copied().collect()
}

fn resolved(entries: &[(&str, &str)]) -> Arc<lore_revision::util::fs::ResolvedPrefixes> {
    let mut prefixes = lore_revision::util::fs::ResolvedPrefixes::default();
    for (path, variation) in entries {
        prefixes.insert((*path).to_string(), (*variation).to_string());
    }
    Arc::new(prefixes)
}

fn path(path: &str) -> RelativePath {
    RelativePath::new_from_clean_parts(path, "")
}

fn below_ancestor(start: WalkStart) -> (RelativePath, lore_revision::node::NodeID, RelativePath) {
    match start {
        WalkStart::BelowAncestor {
            path,
            node,
            remainder,
        } => (path, node, remainder),
        WalkStart::FromRoot(_) => panic!("an ancestor was created"),
    }
}

#[test]
fn walk_start_starts_at_that_ancestor_with_the_rest_below_it() {
    let nodes = created(&[("a", NODE_A), ("a/b", NODE_AB)]);

    let (path_at, node, remainder) = below_ancestor(walk_start(path("a/b/c"), &nodes, None));
    assert_eq!(path_at.as_str(), "a/b");
    assert_eq!(node, NODE_AB);
    assert_eq!(remainder.as_str(), "c");

    let (path_at, node, remainder) = below_ancestor(walk_start(path("a/x/y/z"), &nodes, None));
    assert_eq!(path_at.as_str(), "a");
    assert_eq!(node, NODE_A);
    assert_eq!(remainder.as_str(), "x/y/z");
}

/// The remainder is a view of the target, so its lowercase form has to be
/// advanced along with it rather than left naming the whole path.
#[test]
fn walk_start_leaves_the_remainder_lowercased_from_the_start_down() {
    let nodes = created(&[("Assets", NODE_A)]);

    let (_, _, remainder) = below_ancestor(walk_start(path("Assets/Meshes/Rock"), &nodes, None));
    assert_eq!(remainder.as_str(), "Meshes/Rock");
    assert_eq!(remainder.as_lowercase_str(), "meshes/rock");
}

#[test]
fn walk_start_takes_the_whole_target_from_the_root_when_no_ancestor_was_created() {
    let nodes = created(&[("x", NODE_A)]);

    let WalkStart::FromRoot(returned) = walk_start(path("a/b"), &nodes, None) else {
        panic!("no ancestor was created");
    };
    assert_eq!(returned.as_str(), "a/b", "the target comes back untouched");

    assert!(matches!(
        walk_start(path("a"), &nodes, None),
        WalkStart::FromRoot(_)
    ));
    assert!(matches!(
        walk_start(path("a"), &created(&[]), None),
        WalkStart::FromRoot(_)
    ));
}

#[test]
fn walk_start_takes_the_case_the_prefix_resolved_to() {
    let nodes = created(&[("a", NODE_A), ("a/b", NODE_AB)]);
    let prefixes = resolved(&[("a", "A"), ("a/b", "A/B")]);

    let (path_at, node, remainder) =
        below_ancestor(walk_start(path("a/b/c"), &nodes, Some(&prefixes)));
    assert_eq!(path_at.as_str(), "A/B");
    assert_eq!(node, NODE_AB);
    assert_eq!(remainder.as_str(), "c", "the remainder is not recased");
}

/// A prefix resolves as a whole or not at all: the map answers for the
/// longest prefix it holds, and a shorter one answers for a shorter path.
#[test]
fn walk_start_ignores_a_resolution_covering_only_part_of_the_prefix() {
    let nodes = created(&[("a", NODE_A), ("a/b", NODE_AB)]);
    let prefixes = resolved(&[("a", "A")]);

    let (path_at, node, _) = below_ancestor(walk_start(path("a/b/c"), &nodes, Some(&prefixes)));
    assert_eq!(path_at.as_str(), "a/b");
    assert_eq!(node, NODE_AB);
}
