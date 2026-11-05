// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Tests for link targets: the pin a link records, its linked revision in `address.hash` and the
//! node it exposes in the linked State in `child`, as the writers that repin a link set them
//! (H17); and the content the same writers give a file in place.

use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_revision::filter::FilterMode;
use lore_revision::node::*;
use lore_revision::state::State;
use lore_revision::util::path::RelativePath;
use lore_storage::hash::hash_string;

use super::null_repository;
use super::record;
use crate::fs::filesystem_provider::setup_test_execution;

/// A link named `name` pinned at `revision`, exposing node `child` of the linked State.
fn link(name: &str, revision: u8, child: NodeID) -> Node {
    Node {
        flags: NodeFlags::Link.bits(),
        name_hash: hash_string(name),
        address: Address {
            hash: Hash::from([revision; 32]),
            ..Address::default()
        },
        child,
        ..Default::default()
    }
}

/// H17: staging a link over the link already at its path takes the whole pin: the linked
/// revision and the node it exposes, not the revision alone over a stale `child`; and the
/// staged link's size and mode.
#[tokio::test]
async fn staging_a_link_over_a_link_takes_its_revision_and_its_node() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let repository = null_repository().await;
            let state = State::new();
            let existing = state
                .node_add(repository.clone(), ROOT_NODE, link("lnk", 1, 7), "lnk")
                .await
                .expect("the link adds");

            let staged = lore_revision::stage::stage_single_node(
                repository.clone(),
                state.clone(),
                RelativePath::new().join("lnk"),
                Node {
                    size: 42,
                    mode: 0o755,
                    ..link("lnk", 2, 9)
                },
                Arc::default(),
                None,
                FilterMode::Full,
            )
            .await
            .expect("the stage of the link");

            assert_eq!(staged.node, existing, "the link is staged in place");
            let pinned = record(&state, &repository, existing).await;
            assert_eq!(pinned.address.hash, Hash::from([2u8; 32]));
            assert_eq!(pinned.child, 9, "the link exposes the staged node");
            assert_eq!((pinned.size, pinned.mode), (42, 0o755));
            assert!(pinned.is_link());
        })
        .await;
}

/// H17: a merge grafting a source link over the staged link takes the source's whole pin, its
/// node included, and the source's size and mode.
#[tokio::test]
async fn a_merge_grafting_a_link_takes_the_sources_node() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let repository = null_repository().await;
            let state = State::new();
            let staged = state
                .node_add(repository.clone(), ROOT_NODE, link("lnk", 1, 7), "lnk")
                .await
                .expect("the link adds");

            lore_revision::branch::merge::graft_overwrite_node(
                repository.clone(),
                state.clone(),
                staged,
                &Node {
                    size: 24,
                    mode: 0o644,
                    ..link("lnk", 3, 11)
                },
            )
            .await
            .expect("the graft");

            let grafted = record(&state, &repository, staged).await;
            assert_eq!(grafted.address.hash, Hash::from([3u8; 32]));
            assert_eq!(grafted.child, 11, "the link exposes the source's node");
            assert_eq!((grafted.size, grafted.mode), (24, 0o644));
            assert!(grafted.is_staged_merge());
        })
        .await;
}

/// A file whose content `address` holds, of `size` and `mode`.
fn file(name: &str, address: Address, size: u64, mode: u16) -> Node {
    Node {
        flags: NodeFlags::File.bits(),
        name_hash: hash_string(name),
        address,
        size,
        mode,
        ..Default::default()
    }
}

/// An address of content `hash` in file context `context`.
fn address(hash: u8, context: u8) -> Address {
    Address {
        hash: Hash::from([hash; 32]),
        context: lore_base::types::Context::from([context; 16]),
    }
}

/// Staging a file over the file at its path, and a merge grafting one, write the new content in
/// place: the whole address, its file context included, the size and the mode.
#[tokio::test]
async fn staging_or_grafting_a_file_over_a_file_takes_its_whole_content() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let repository = null_repository().await;
            let state = State::new();
            let existing = state
                .node_add(
                    repository.clone(),
                    ROOT_NODE,
                    file("f", address(1, 1), 1, 0),
                    "f",
                )
                .await
                .expect("the file adds");

            let staged = lore_revision::stage::stage_single_node(
                repository.clone(),
                state.clone(),
                RelativePath::new().join("f"),
                file("f", address(2, 2), 22, 0o755),
                Arc::default(),
                None,
                FilterMode::Full,
            )
            .await
            .expect("the stage of the file");
            assert_eq!(staged.node, existing, "the file is staged in place");
            let held = record(&state, &repository, existing).await;
            assert_eq!(held.address, address(2, 2));
            assert_eq!((held.size, held.mode), (22, 0o755));

            lore_revision::branch::merge::graft_overwrite_node(
                repository.clone(),
                state.clone(),
                existing,
                &file("f", address(3, 3), 33, 0o644),
            )
            .await
            .expect("the graft");
            let held = record(&state, &repository, existing).await;
            assert_eq!(held.address, address(3, 3));
            assert_eq!((held.size, held.mode), (33, 0o644));
            assert!(held.is_file() && held.is_staged_merge());
        })
        .await;
}
