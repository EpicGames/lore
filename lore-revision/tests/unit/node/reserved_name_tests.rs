// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::allocator::HeapBuf;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::ZeroHeapAlloc;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeBlockData;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::is_reserved_node_name;
use lore_revision::state::DirtyWalkOptions;
use lore_revision::state::Ext;
use lore_revision::state::Mark;
use lore_revision::state::State;
use lore_revision::state::StateNodeChildrenWithNameIterator;
use lore_revision::state::collect_dirty_paths_inner;
use lore_revision::util::path::RelativePath;
use lore_revision::util::path::RelativePathBuf;
use lore_storage::hash::hash_string;

use crate::fs::filesystem_provider::setup_test_execution;
use crate::fs::filesystem_provider::test_store_create;

/// The repository's own directory, in the spellings a filesystem that folds case treats as one,
/// and in those Windows resolves to it: with trailing dots and spaces, as its 8.3 short name, and
/// with a stream after a colon.
const RESERVED_NAMES: [&str; 18] = [
    ".urc",
    ".URC",
    ".Urc",
    ".uRC",
    ".lore",
    ".LORE",
    ".Lore",
    ".lOrE",
    ".lore.",
    ".LORE ",
    ".lore. .",
    ".urc..",
    "LORE~1",
    "lore~2",
    "Urc~1",
    "lore~1.",
    ".lore:stream",
    "LORE~1:x",
];

/// Names that share a prefix, a suffix or every letter with a reserved one without being it.
const UNRESERVED_NAMES: [&str; 17] = [
    ".urcignore",
    ".loreignore",
    "urc",
    "lore",
    ".url",
    ".lor",
    ".urc2",
    ".lore.bak",
    "..urc",
    ".ｕrc",
    "lore.x",
    "lore~",
    "lore~x",
    "lore~1x",
    "xlore~1",
    ".lore~1",
    "lore:x",
];

#[test]
fn the_reserved_names_are_matched_in_every_spelling() {
    for name in RESERVED_NAMES {
        assert!(is_reserved_node_name(name), "{name:?} is reserved");
    }
}

#[test]
fn a_name_that_only_resembles_a_reserved_one_is_not_reserved() {
    for name in UNRESERVED_NAMES {
        assert!(!is_reserved_node_name(name), "{name:?} is not reserved");
    }
}

#[test]
fn node_name_store_refuses_a_reserved_name() {
    for name in RESERVED_NAMES {
        let block = NodeBlock::new_zeroed();
        let err = block
            .write_for_tests()
            .node_name_store(name, 0, 0)
            .expect_err("a reserved name must be refused");
        assert!(err.is_invalid_arguments(), "{name:?}: {err:?}");
    }
}

#[test]
fn node_name_store_accepts_a_name_that_only_resembles_a_reserved_one() {
    for name in UNRESERVED_NAMES {
        let block = NodeBlock::new_zeroed();
        block
            .write_for_tests()
            .node_name_store(name, 0, 0)
            .unwrap_or_else(|err| panic!("{name:?} must be accepted: {err:?}"));
    }
}

/// A name table written before the names were reserved can carry one, so the read path refuses
/// it on its own, which is what makes every walk skip the node.
#[test]
fn a_reserved_name_planted_in_the_name_table_is_refused_on_read() {
    for name in RESERVED_NAMES {
        let mut data = NodeBlockData::new_from_heap_zeroed();
        data.node_count = 1;
        data.node[0].name_offset = 0;
        data.node[0].name_length = name.len() as u32;
        let block = NodeBlock::new_with_name(data, HeapBuf::from_slice(name.as_bytes()));

        assert!(
            block.node_name_ref(0).is_err(),
            "node_name_ref must refuse {name:?}"
        );
        assert!(
            block.node_name_clone(0).is_err(),
            "node_name_clone must refuse {name:?}"
        );
    }
}

fn directory(name: &str) -> Node {
    Node {
        flags: NodeFlags::NoFlags.bits(),
        mode: 0o755,
        name_hash: hash_string(name),
        ..Default::default()
    }
}

/// A revision written before the names were reserved can hold a node carrying one. The per-node
/// read refuses it, and the walk every listing and materialization stands on skips it, with the
/// siblings around it intact.
#[tokio::test]
async fn a_walk_skips_a_node_carrying_a_planted_reserved_name() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable_store, mutable_store, _execution) =
                test_store_create().await.expect("making test stores");
            let repository = Arc::new(RepositoryContext::new_null_context(
                immutable_store,
                mutable_store,
            ));
            let state = State::new();
            for name in ["alpha", "xurc", "omega"] {
                state
                    .node_add(repository.clone(), ROOT_NODE, directory(name), name)
                    .await
                    .expect("adding a directory must succeed");
            }
            let planted = state
                .find_subnode(repository.clone(), ROOT_NODE, hash_string("xurc"))
                .await
                .expect("the placeholder must resolve");

            // Overwrite the placeholder in place, bypassing the rules as an older writer did.
            let block_index = NodeBlock::index(planted);
            let block = state
                .block_with_nametable(repository.clone(), block_index)
                .await
                .expect("the block must read back");
            {
                let mut writer = block.write_for_tests();
                let node = writer.node(Node::index(planted));
                let (offset, length) = (node.name_offset, node.name_length);
                node.name_hash = hash_string(".URC");
                let (offset, length) = writer
                    .node_name_store_unchecked(".URC", offset, length)
                    .expect("the unchecked store must accept the name");
                let node = writer.node(Node::index(planted));
                node.name_offset = offset;
                node.name_length = length;
                writer.mark_dirty();
            }
            state.block_modified_for_tests(block, block_index);

            assert!(
                state
                    .node_name_clone(repository.clone(), planted)
                    .await
                    .is_err(),
                "a per-node read must refuse the planted name"
            );
            assert!(
                matches!(
                    state
                        .node_name_ref_or_skip(repository.clone(), planted)
                        .await,
                    Ok(None)
                ),
                "the skipping read must answer nothing for the planted name"
            );
            assert!(
                matches!(
                    state
                        .node_name_clone_or_skip(repository.clone(), planted)
                        .await,
                    Ok(None)
                ),
                "the skipping clone must answer nothing for the planted name"
            );

            let mut iter = StateNodeChildrenWithNameIterator::new(
                state.clone(),
                repository.clone(),
                ROOT_NODE,
            )
            .await
            .expect("the walk must open");
            let mut names = Vec::new();
            while let Some((_, _, name)) = iter.next().await.expect("the walk must not fail") {
                names.push(name.to_string());
            }
            names.sort();
            assert_eq!(names, vec!["alpha".to_string(), "omega".to_string()]);

            // Resolution is by name hash, which folds case, so a path naming the planted node
            // in any spelling would reach it: the resolver refuses the component instead.
            for path in [".URC", ".urc", ".Urc/anything"] {
                assert!(
                    state
                        .find_node_link(repository.clone(), path)
                        .await
                        .is_err(),
                    "{path:?} must resolve to nothing"
                );
            }
            assert!(
                state
                    .find_node_link(repository.clone(), "alpha")
                    .await
                    .is_ok(),
                "a sibling must still resolve"
            );

            // A dirty flag can predate the rule, so the dirty-path collections a stage, a
            // commit and a staged-anchor rebase stand on pass the node over as well.
            let alpha = state
                .find_subnode(repository.clone(), ROOT_NODE, hash_string("alpha"))
                .await
                .expect("alpha must resolve");
            for node in [alpha, planted] {
                state
                    .node_mark(
                        &repository,
                        node,
                        Mark::dirty(NodeFlags::DirtyAdd, Ext::Set(NodeExtFlags::NoFlags), true),
                    )
                    .await
                    .expect("marking dirty must succeed");
            }
            let collected = state
                .collect_dirty_paths(repository.clone(), ROOT_NODE, RelativePath::new())
                .await
                .expect("collecting dirty paths must not fail on the planted node");
            assert_eq!(
                collected
                    .iter()
                    .map(|path| path.as_str())
                    .collect::<Vec<_>>(),
                vec!["alpha"]
            );
            let mut walked = Vec::new();
            collect_dirty_paths_inner(
                state.clone(),
                repository.clone(),
                ROOT_NODE,
                &mut RelativePathBuf::new(),
                &mut walked,
                DirtyWalkOptions::default(),
            )
            .await
            .expect("the dirty walk must not fail on the planted node");
            assert_eq!(
                walked.iter().map(|path| path.as_str()).collect::<Vec<_>>(),
                vec!["alpha"]
            );
        })
        .await;
}
