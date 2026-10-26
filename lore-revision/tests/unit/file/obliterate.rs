// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashSet;
use std::sync::Arc;

use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::FragmentReference;
use lore_base::types::Hash;
use lore_revision::file::obliterate::impact::Budget;
use lore_revision::file::obliterate::impact::deletion_set;
use lore_revision::file::obliterate::impact::snapshot_affected;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use zerocopy::IntoBytes;

use crate::helper::test_store_create;

async fn leaf(repository: &Arc<RepositoryContext>, context: Context, byte: u8) -> Address {
    let payload = vec![byte; 128];
    put(repository, context, &payload, 128, 0).await
}

async fn put(
    repository: &Arc<RepositoryContext>,
    context: Context,
    payload: &[u8],
    size: u64,
    flags: u32,
) -> Address {
    let address = Address {
        hash: lore_storage::hash::hash_slice(payload),
        context,
    };
    repository
        .immutable_store()
        .put(
            repository.id,
            address,
            Fragment {
                flags,
                size_payload: payload.len() as u32,
                size_content: size,
            },
            Some(Bytes::copy_from_slice(payload)),
            false,
        )
        .await
        .expect("store fragment");
    address
}

async fn list(
    repository: &Arc<RepositoryContext>,
    context: Context,
    leaves: &[Address],
) -> Address {
    let references: Vec<_> = leaves
        .iter()
        .enumerate()
        .map(|(i, leaf)| FragmentReference {
            hash: leaf.hash,
            offset_content: (i * 128) as u64,
        })
        .collect();
    put(
        repository,
        context,
        references.as_bytes(),
        (leaves.len() * 128) as u64,
        FragmentFlags::PayloadFragmented.as_u32(),
    )
    .await
}

async fn snapshot(repository: &Arc<RepositoryContext>, address: Address) -> Hash {
    let state = State::new();
    state
        .node_add(
            repository.clone(),
            ROOT_NODE,
            Node {
                address,
                flags: NodeFlags::File.bits(),
                ..Default::default()
            },
            "probe.txt",
        )
        .await
        .expect("file node");
    state
        .serialize(repository.clone(), repository.try_write_token().unwrap())
        .await
        .expect("snapshot")
}

#[tokio::test]
async fn different_roots_sharing_a_child_affect_latest() {
    let (immutable, mutable, execution) = test_store_create().await.unwrap();
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable,
                mutable,
                Context::from([1; 16]).into(),
            ));
            let context = Context::from([2; 16]);
            let shared = leaf(&repository, context, 1).await;
            let added = leaf(&repository, context, 2).await;
            let old = list(&repository, context, &[shared]).await;
            let current = list(&repository, context, &[shared, added]).await;
            assert_ne!(old, current);
            let signature = snapshot(&repository, current).await;
            let mut budget = Budget::default();
            let mut deleted = HashSet::new();
            deletion_set(&repository, old, &mut budget, &mut deleted)
                .await
                .unwrap();
            assert!(deleted.contains(&shared));
            assert!(!deleted.contains(&current));
            assert!(
                snapshot_affected(&repository, signature, old, &deleted, &mut budget)
                    .await
                    .unwrap()
            );
        })
        .await;
}

#[tokio::test]
async fn superseded_content_and_other_contexts_do_not_match() {
    let (immutable, mutable, execution) = test_store_create().await.unwrap();
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable,
                mutable,
                Context::from([1; 16]).into(),
            ));
            let context = Context::from([2; 16]);
            let target = leaf(&repository, context, 1).await;
            let different = leaf(&repository, context, 2).await;
            let other_context = leaf(&repository, Context::from([3; 16]), 1).await;
            let mut budget = Budget::default();
            let mut deleted = HashSet::new();
            deletion_set(&repository, target, &mut budget, &mut deleted)
                .await
                .unwrap();
            for address in [different, other_context] {
                let signature = snapshot(&repository, address).await;
                assert!(
                    !snapshot_affected(&repository, signature, target, &deleted, &mut budget)
                        .await
                        .unwrap()
                );
            }
            let signature = snapshot(&repository, target).await;
            assert!(
                snapshot_affected(&repository, signature, target, &deleted, &mut budget)
                    .await
                    .unwrap()
            );
        })
        .await;
}

#[tokio::test]
async fn missing_fragment_and_exhausted_budget_are_incomplete() {
    let (immutable, mutable, execution) = test_store_create().await.unwrap();
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable,
                mutable,
                Context::from([1; 16]).into(),
            ));
            let missing = Address {
                hash: Hash::from([8; 32]),
                context: Context::from([2; 16]),
            };
            let mut deleted = HashSet::new();
            assert!(
                deletion_set(&repository, missing, &mut Budget::default(), &mut deleted)
                    .await
                    .is_err()
            );
            assert!(deleted.contains(&missing));
            let target = leaf(&repository, missing.context, 1).await;
            let signature = snapshot(&repository, target).await;
            let mut exhausted = Budget {
                nodes: 0,
                fragments: 0,
            };
            assert!(
                snapshot_affected(
                    &repository,
                    signature,
                    target,
                    &HashSet::from([target]),
                    &mut exhausted
                )
                .await
                .is_err()
            );
        })
        .await;
}

#[tokio::test]
async fn nested_fragments_missing_lists_and_depth_limit() {
    let (immutable, mutable, execution) = test_store_create().await.unwrap();
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable,
                mutable,
                Context::from([1; 16]).into(),
            ));
            let context = Context::from([2; 16]);
            let leaf = leaf(&repository, context, 1).await;
            let inner = list(&repository, context, &[leaf]).await;
            let outer = list(&repository, context, &[inner]).await;
            let mut deleted = HashSet::new();
            deletion_set(&repository, outer, &mut Budget::default(), &mut deleted)
                .await
                .unwrap();
            assert_eq!(deleted, HashSet::from([leaf, inner, outer]));
            let signature = snapshot(&repository, outer).await;
            assert!(
                snapshot_affected(
                    &repository,
                    signature,
                    leaf,
                    &HashSet::from([leaf]),
                    &mut Budget::default()
                )
                .await
                .unwrap()
            );
            let missing_list = Address {
                hash: Hash::from([9; 32]),
                context,
            };
            repository
                .immutable_store()
                .put(
                    repository.id,
                    missing_list,
                    Fragment {
                        flags: FragmentFlags::PayloadFragmented.as_u32(),
                        size_payload: std::mem::size_of::<FragmentReference>() as u32,
                        size_content: 128,
                    },
                    None,
                    false,
                )
                .await
                .unwrap();
            assert!(
                deletion_set(
                    &repository,
                    missing_list,
                    &mut Budget::default(),
                    &mut HashSet::new()
                )
                .await
                .is_err()
            );
            let mut deep = outer;
            for _ in 0..9 {
                deep = list(&repository, context, &[deep]).await;
            }
            assert!(
                deletion_set(
                    &repository,
                    deep,
                    &mut Budget::default(),
                    &mut HashSet::new()
                )
                .await
                .is_err()
            );
        })
        .await;
}

#[tokio::test]
async fn links_follow_recorded_snapshots_and_respect_repository_partition() {
    let (immutable, mutable, execution) = test_store_create().await.unwrap();
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable.clone(),
                mutable.clone(),
                Context::from([1; 16]).into(),
            ));
            let other = Arc::new(RepositoryContext::new_server_context(
                immutable,
                mutable,
                Context::from([4; 16]).into(),
            ));
            let target = leaf(&repository, Context::from([2; 16]), 1).await;
            let other_target = leaf(&other, target.context, 1).await;
            assert_eq!(target, other_target);
            for (linked, expected) in [(&repository, true), (&other, false)] {
                let linked_revision = snapshot(linked, target).await;
                let state = State::new();
                state
                    .node_add(
                        repository.clone(),
                        ROOT_NODE,
                        Node {
                            flags: NodeFlags::Link.bits(),
                            address: Address {
                                hash: linked_revision,
                                context: linked.id.into(),
                            },
                            child: ROOT_NODE,
                            ..Default::default()
                        },
                        "mounted",
                    )
                    .await
                    .unwrap();
                let signature = state
                    .serialize(repository.clone(), repository.try_write_token().unwrap())
                    .await
                    .unwrap();
                assert_eq!(
                    snapshot_affected(
                        &repository,
                        signature,
                        target,
                        &HashSet::from([target]),
                        &mut Budget::default()
                    )
                    .await
                    .unwrap(),
                    expected
                );
            }
        })
        .await;
}

#[tokio::test]
async fn nonzero_offset_lists_preserve_deletion_and_head_dependencies() {
    let (immutable, mutable, execution) = test_store_create().await.unwrap();
    LORE_CONTEXT
        .scope(
            execution,
            Box::pin(async move {
                let repository = Arc::new(RepositoryContext::new_server_context(
                    immutable,
                    mutable,
                    Context::from([1; 16]).into(),
                ));
                let context = Context::from([2; 16]);
                let shared = leaf(&repository, context, 1).await;
                let other = leaf(&repository, context, 2).await;
                let refs = [
                    FragmentReference {
                        hash: shared.hash,
                        offset_content: 1024,
                    },
                    FragmentReference {
                        hash: other.hash,
                        offset_content: 1152,
                    },
                ];
                let inner = put(
                    &repository,
                    context,
                    refs.as_bytes(),
                    256,
                    FragmentFlags::PayloadFragmented.as_u32(),
                )
                .await;
                let outer_refs = [
                    FragmentReference {
                        hash: other.hash,
                        offset_content: 0,
                    },
                    FragmentReference {
                        hash: inner.hash,
                        offset_content: 1024,
                    },
                ];
                let outer = put(
                    &repository,
                    context,
                    outer_refs.as_bytes(),
                    1280,
                    FragmentFlags::PayloadFragmented.as_u32(),
                )
                .await;
                let mut deleted = HashSet::new();
                deletion_set(&repository, outer, &mut Budget::default(), &mut deleted)
                    .await
                    .expect("valid nonzero-offset dependency list");
                assert_eq!(deleted, HashSet::from([outer, inner, shared, other]));
                let signature = snapshot(&repository, outer).await;
                assert!(
                    snapshot_affected(
                        &repository,
                        signature,
                        shared,
                        &HashSet::from([shared]),
                        &mut Budget::default()
                    )
                    .await
                    .unwrap()
                );
            }),
        )
        .await;
}

#[tokio::test]
async fn invalid_fragment_dependency_bounds_are_incomplete() {
    let (immutable, mutable, execution) = test_store_create().await.unwrap();
    LORE_CONTEXT
        .scope(
            execution,
            Box::pin(async move {
                let repository = Arc::new(RepositoryContext::new_server_context(
                    immutable,
                    mutable,
                    Context::from([1; 16]).into(),
                ));
                let context = Context::from([2; 16]);
                let child = leaf(&repository, context, 1).await;
                let empty = repository
                    .immutable_store()
                    .put(
                        repository.id,
                        Address {
                            hash: Hash::from([9; 32]),
                            context,
                        },
                        Fragment {
                            flags: FragmentFlags::PayloadFragmented.as_u32(),
                            size_payload: 0,
                            size_content: 256,
                        },
                        Some(Bytes::new()),
                        false,
                    )
                    .await;
                assert!(
                    empty.is_err(),
                    "empty fragment payloads must be rejected by storage"
                );
                for offsets in [
                    vec![u64::MAX - 1, u64::MAX],
                    vec![1024, 1024],
                    vec![1024, 1280],
                ] {
                    let refs: Vec<_> = offsets
                        .into_iter()
                        .map(|offset_content| FragmentReference {
                            hash: child.hash,
                            offset_content,
                        })
                        .collect();
                    let address = put(
                        &repository,
                        context,
                        refs.as_bytes(),
                        256,
                        FragmentFlags::PayloadFragmented.as_u32(),
                    )
                    .await;
                    let mut deleted = HashSet::new();
                    let result =
                        deletion_set(&repository, address, &mut Budget::default(), &mut deleted)
                            .await;
                    assert!(
                        result.is_err(),
                        "malformed dependency list must be incomplete"
                    );
                    assert!(deleted.contains(&address));
                }
            }),
        )
        .await;
}
