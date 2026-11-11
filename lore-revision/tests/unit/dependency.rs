// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashSet;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_revision::dependency::resolve::resolve_dependency_file_set;
use lore_revision::dependency::*;
use lore_revision::lore::RepositoryId;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;

use crate::fs::filesystem_provider::setup_test_execution;
use crate::fs::filesystem_provider::test_store_create;
use crate::helper::in_memory;

/// Build a raw dependency blob header (16 bytes) with the given
/// `entry_count`. Used by tests to craft malformed inputs without going
/// through the well-formed `serialize` path.
fn blob_header(entry_count: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_SIZE);
    buf.extend_from_slice(&MAGIC.to_le_bytes());
    buf.extend_from_slice(&VERSION.to_le_bytes());
    buf.extend_from_slice(&entry_count.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // reserved
    buf
}

#[test]
fn deserialize_roundtrip() {
    let mut data = DependencyData::new();
    data.add(7, &["foo", "bar"]);
    data.add(42, &["baz"]);
    let bytes = data.serialize();
    let parsed = DependencyData::deserialize(&bytes).expect("roundtrip");
    assert_eq!(parsed, data);
}

#[test]
fn deserialize_empty_blob_ok() {
    let bytes = DependencyData::new().serialize();
    let parsed = DependencyData::deserialize(&bytes).expect("empty roundtrip");
    assert!(parsed.entries.is_empty());
}

#[test]
fn deserialize_rejects_entry_count_exceeding_buffer() {
    // Header declares 1_000_000 entries but buffer only has the header.
    // Without the cap, this triggers a ~32 MiB Vec::with_capacity.
    let bytes = blob_header(1_000_000);
    let err = DependencyData::deserialize(&bytes).expect_err("should reject");
    assert!(
        err.to_string().contains("entry_count exceeds"),
        "unexpected error: {err}"
    );
}

#[test]
fn deserialize_rejects_u32_max_entry_count() {
    let bytes = blob_header(u32::MAX);
    let err = DependencyData::deserialize(&bytes).expect_err("should reject");
    assert!(
        err.to_string().contains("entry_count exceeds"),
        "unexpected error: {err}"
    );
}

#[test]
fn deserialize_rejects_tag_count_exceeding_remaining_buffer() {
    // Header says 1 entry. Entry header says tag_count = 65535. Remaining
    // buffer (0 bytes after the entry header) can't fit 65535 * 2 bytes.
    // Without the cap, this triggers a ~1 MiB Vec::with_capacity for tags.
    let mut bytes = blob_header(1);
    bytes.extend_from_slice(&7u32.to_le_bytes()); // node_id
    bytes.extend_from_slice(&u16::MAX.to_le_bytes()); // tag_count
    bytes.extend_from_slice(&0u16.to_le_bytes()); // reserved
    let err = DependencyData::deserialize(&bytes).expect_err("should reject");
    assert!(
        err.to_string().contains("tag_count exceeds"),
        "unexpected error: {err}"
    );
}

#[test]
fn deserialize_accepts_legitimate_tag_counts() {
    // Confirm the tag_count cap doesn't break well-formed blobs with
    // multiple tags.
    let mut data = DependencyData::new();
    data.add(1, &["a", "b", "c", "d", "e"]);
    let bytes = data.serialize();
    let parsed = DependencyData::deserialize(&bytes).expect("should accept");
    assert_eq!(parsed, data);
}

#[test]
fn deserialize_rejects_bad_magic() {
    let mut bytes = blob_header(0);
    bytes[0..4].copy_from_slice(&0xDEADBEEFu32.to_le_bytes());
    assert!(DependencyData::deserialize(&bytes).is_err());
}

#[test]
fn deserialize_rejects_unsupported_version() {
    let mut bytes = blob_header(0);
    bytes[4..8].copy_from_slice(&999u32.to_le_bytes());
    assert!(DependencyData::deserialize(&bytes).is_err());
}

#[test]
fn deserialize_rejects_blob_shorter_than_header() {
    let bytes = vec![0u8; HEADER_SIZE - 1];
    assert!(DependencyData::deserialize(&bytes).is_err());
}

/// A revision holding `a.txt` and a link at `vendor` to a revision holding `l.txt`.
async fn state_with_link(repository: &Arc<RepositoryContext>) -> Arc<State> {
    let linked = RepositoryId::from(uuid::Uuid::now_v7());
    let pin = in_memory::linked_revision(repository, linked).await;
    let state = State::new();
    let file = in_memory::file("a.txt");
    in_memory::add(&state, repository.clone(), ROOT_NODE, file, "a.txt").await;
    let link = in_memory::link_node("vendor", linked, pin);
    in_memory::add(&state, repository.clone(), ROOT_NODE, link, "vendor").await;
    let revision =
        in_memory::commit(repository, &state, Hash::default(), in_memory::branch_id()).await;
    in_memory::revision_state(repository, revision).await
}

/// The dependency set of the root files `roots` in `state`.
async fn file_set(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    roots: &[&str],
) -> Result<HashSet<NodeID>, DependencyError> {
    let roots: Vec<String> = roots.iter().map(|root| root.to_string()).collect();
    resolve_dependency_file_set(repository.clone(), state.clone(), &roots, &[], false, 0).await
}

/// A root file inside a link is skipped, and refused when every root file is. The link itself
/// is a root file like any other, a path missing inside a link is not found, and root files are
/// read in any spelling a clone accepts.
#[tokio::test]
async fn a_root_file_inside_a_link_is_skipped_or_refused() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable, mutable, _execution) =
                test_store_create().await.expect("making test stores");
            let repository = in_memory::repository(immutable, mutable);
            let state = state_with_link(&repository).await;
            let node = async |path: &str| {
                state
                    .find_node_link(repository.clone(), path)
                    .await
                    .expect("finding the node")
                    .node
            };
            let (file, link) = (node("a.txt").await, node("vendor").await);

            let kept = file_set(&repository, &state, &["./a.txt", "vendor/l.txt"]).await;
            assert_eq!(kept.expect("the file set"), HashSet::from([file]));
            let mount = file_set(&repository, &state, &["vendor"]).await;
            assert_eq!(mount.expect("the file set"), HashSet::from([link]));
            let refused = file_set(&repository, &state, &["vendor/l.txt"]).await;
            assert!(
                refused.as_ref().is_err_and(|err| err.is_not_supported()),
                "{refused:?}"
            );
            let missing = file_set(&repository, &state, &["vendor/missing.txt"]).await;
            assert!(
                missing.as_ref().is_err_and(|err| err.is_file_not_found()),
                "{missing:?}"
            );
            let none = file_set(&repository, &state, &[]).await;
            assert_eq!(none.expect("the file set"), HashSet::new());
        })
        .await;
}
