// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::sync::Arc;

use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_storage::hash;
use zerocopy::FromZeros;

use crate::helper::RepositoryContextCreationArgsExt;
use crate::helper::default_repository_creation_args;

/// A repository over in-memory stores and no remote, writing its working tree at `path`.
pub async fn working_tree_repository(path: &Path) -> Arc<RepositoryContext> {
    let immutable_store = lore_storage::local::immutable_store::create(
        None::<&str>,
        lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
        false,
        lore_storage::ImmutableStoreSettings::default(),
    )
    .await
    .expect("in-memory immutable store");
    let mutable_store = lore_storage::local::mutable_store::create(
        None::<&str>,
        lore_storage::MutableStoreSettings::default(),
        immutable_store.clone(),
    )
    .await
    .expect("in-memory mutable store");

    Arc::new(
        RepositoryContext::new(
            default_repository_creation_args(immutable_store, mutable_store).with_path(path),
        )
        .with_write_token(RepositoryWriteToken::acquire(path).await),
    )
}

/// A file node addressing `content`, which no store holds until it is written to one.
pub fn file_node(content: &[u8]) -> Node {
    let mut node = Node::new_zeroed();
    node.flags = NodeFlags::File.bits();
    node.address = lore_base::types::Address::zero_context_hash(hash::hash_slice(content));
    node.size = content.len() as u64;
    node
}
