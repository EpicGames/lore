// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::sync::Arc;

use lore_base::error::Disconnected;
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::FragmentReference;
use lore_revision::filter::Filter;
use lore_revision::lore::RepositoryId;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::protocol::RemoteConnections;
use lore_revision::protocol::RemoteEndpoint;
use lore_revision::repository::RemoteState;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::test_util::context_built;
use lore_revision::test_util::context_with_state;
use lore_storage::hash;
use lore_transport::Connection;
use lore_transport::ProtocolError;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

use crate::helper::RepositoryContextCreationArgsExt;
use crate::helper::default_repository_creation_args;
use crate::immutable::serving::Serving;
use crate::immutable::serving::serving_remote;

/// An endpoint whose connection for `partition` reaches a remote that holds nothing
/// ([`serving_remote`] over an empty repository), so a read of it misses and is not retried, and
/// that connection, which counts the session pools started on it
/// ([`Connection::storage_session_starts`]).
pub async fn counted_endpoint(partition: RepositoryId) -> (Arc<RemoteEndpoint>, Arc<Connection>) {
    let empty = context_with_state(RemoteState::Offline).await;
    let connection = Connection::detached(vec![serving_remote(&empty, Serving::default())]);
    let connections = RemoteConnections::with_connection(partition, connection.clone());
    (Arc::new(RemoteEndpoint::over(connections)), connection)
}

/// A context on the zero partition, set up by `build`, whose connect failed, so it is not offline
/// and each read carries a session.
pub async fn built_context(
    build: impl FnOnce(RepositoryContext) -> RepositoryContext,
) -> Arc<RepositoryContext> {
    context_built(
        RemoteState::Failed(ProtocolError::from(Disconnected)),
        build,
    )
    .await
}

/// Whether `error`, or an error among its sources, is a `T`.
pub fn caused_by<T: std::error::Error + 'static>(
    error: &(dyn std::error::Error + 'static),
) -> bool {
    std::iter::successors(Some(error), |error| error.source()).any(|error| error.is::<T>())
}

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
