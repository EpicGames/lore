// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::sync::Arc;

use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::FragmentReference;
use lore_revision::filter::Filter;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_storage::hash;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

use crate::helper::RepositoryContextCreationArgsExt;
use crate::helper::default_repository_creation_args;

/// A repository over in-memory stores and no remote, writing its working tree at `path`.
pub async fn working_tree_repository(path: &Path) -> Arc<RepositoryContext> {
    working_tree_repository_filtered(path, Filter::default()).await
}

/// [`working_tree_repository`] under `filter`.
pub async fn working_tree_repository_filtered(
    path: &Path,
    filter: Filter,
) -> Arc<RepositoryContext> {
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
            default_repository_creation_args(immutable_store, mutable_store)
                .with_path(path)
                .with_filter(Arc::new(filter)),
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

/// Stores `payload` uncompressed as one fragment with `flags`, standing for `size_content` bytes of
/// content, and answers its address.
pub async fn store_fragment(
    repository: &Arc<RepositoryContext>,
    payload: &[u8],
    flags: FragmentFlags,
    size_content: u64,
) -> Address {
    let address = Address::zero_context_hash(hash::hash_slice(payload));
    lore_revision::immutable::store_raw_store_retry(
        repository.immutable_store(),
        repository.id,
        address,
        Fragment {
            flags: flags.bits(),
            size_payload: payload.len() as u32,
            size_content,
        },
        Some(bytes::Bytes::copy_from_slice(payload)),
    )
    .await
    .expect("store the fragment");
    address
}

/// A file node addressing `content` as a fragment list cut every `leaf_len` bytes, with the list
/// and every leaf stored.
pub async fn stored_as_list(
    repository: &Arc<RepositoryContext>,
    content: &[u8],
    leaf_len: usize,
) -> Node {
    let mut list = Vec::new();
    for (index, leaf) in content.chunks(leaf_len).enumerate() {
        list.push(FragmentReference {
            hash: store_fragment(repository, leaf, FragmentFlags::empty(), leaf.len() as u64)
                .await
                .hash,
            offset_content: (index * leaf_len) as u64,
        });
    }
    let mut node = file_node(content);
    node.address = store_fragment(
        repository,
        list.as_slice().as_bytes(),
        FragmentFlags::PayloadFragmented,
        content.len() as u64,
    )
    .await;
    node
}
