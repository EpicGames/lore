// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use bytes::Bytes;
use lore_proto::Conflict;
use lore_proto::Path;
use lore_proto::PathDiff;
use lore_proto::PathType;
use lore_revision::change::FileAction;
use lore_revision::change::NodeChange;
use lore_revision::change::NodeChangeState;
use lore_revision::lore::RepositoryId;
use lore_revision::node::NodeFlags;
use tracing::warn;

pub fn node_flags_to_type(flags: NodeFlags) -> i32 {
    if flags.contains(NodeFlags::File) {
        PathType::File as i32
    } else if flags.contains(NodeFlags::Link) {
        PathType::Link as i32
    } else {
        PathType::Directory as i32
    }
}

/// The side the change resolves to: `from` for a delete, `to` otherwise.
fn resolved_side(change: &NodeChange) -> &NodeChangeState {
    match change.action {
        FileAction::Delete => &change.from,
        _ => &change.to,
    }
}

/// The linked repository a change resolves under (empty when it is the
/// request's own repository), and whether a link tracks its parent branch.
async fn link_partition_and_tracking(
    change: &NodeChange,
    parent_repository_id: RepositoryId,
) -> (Bytes, bool) {
    let side = resolved_side(change);
    if !side.flags.contains(NodeFlags::Link) {
        return (Bytes::new(), false);
    }
    let target: RepositoryId = side.address.context.into();
    let link_partition = if target == parent_repository_id {
        Bytes::new()
    } else {
        Bytes::from(target)
    };
    let tracking = side
        .state
        .link_find(side.repository.clone(), target, side.node)
        .await
        .is_ok_and(|link_ref| link_ref.is_tracking());
    (link_partition, tracking)
}

pub async fn map_to_path_diff(
    change: &NodeChange,
    parent_repository_id: RepositoryId,
) -> Option<PathDiff> {
    let (link_partition, tracking) =
        link_partition_and_tracking(change, parent_repository_id).await;
    match change.action {
        FileAction::Delete => Some(PathDiff {
            from: Some(Path {
                path: change.path.to_string(),
                address: change.from.address.into(),
                r#type: node_flags_to_type(change.from.flags),
                tracking,
            }),
            to: None,
            automerged: change.flags.is_conflict_automerged(),
            link_partition,
            tracking,
        }),
        FileAction::Add => Some(PathDiff {
            from: None,
            to: Some(Path {
                path: change.path.to_string(),
                address: change.to.address.into(),
                r#type: node_flags_to_type(change.to.flags),
                tracking,
            }),
            automerged: change.flags.is_conflict_automerged(),
            link_partition,
            tracking,
        }),
        FileAction::Keep => Some(PathDiff {
            from: Some(Path {
                path: change.path.to_string(),
                address: change.from.address.into(),
                r#type: node_flags_to_type(change.from.flags),
                tracking,
            }),
            to: Some(Path {
                path: change.path.to_string(),
                address: change.to.address.into(),
                r#type: node_flags_to_type(change.to.flags),
                tracking,
            }),
            automerged: change.flags.is_conflict_automerged(),
            link_partition,
            tracking,
        }),
        _ => {
            // TODO(mjansson): handle MOVE, for which we need to have 2 paths, so the existing NodeChange doesn't work
            // TODO(parroyo): do we want to handle Copy ?
            warn!("unhandled action {:?}", change.action);
            None
        }
    }
}

pub async fn map_to_conflict(
    conflict: &(NodeChange, NodeChange),
    parent_repository_id: RepositoryId,
) -> Option<Conflict> {
    Some(Conflict {
        diff_base: map_to_path_diff(&conflict.0, parent_repository_id).await,
        diff_compare: map_to_path_diff(&conflict.1, parent_repository_id).await,
    })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::Arc;

    use bytes::Bytes;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_proto::Path;
    use lore_proto::PathDiff;
    use lore_proto::PathType;
    use lore_revision::change::Flags;
    use lore_revision::change::NodeChange;
    use lore_revision::change::NodeChangeState;
    use lore_revision::lore::RepositoryId;
    use lore_revision::node::NodeFlags;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryFormat;
    use lore_revision::state;
    use lore_revision::util::path::RelativePath;
    use lore_transport::ProtocolError;

    use crate::grpc::handlers::path_diff::map_to_path_diff;

    pub async fn new_test_context() -> Arc<lore_revision::repository::RepositoryContext> {
        let immutable = lore_storage::local::immutable_store::LocalImmutableStore::new(
            None,
            lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
        )
        .await
        .expect("Failed to create store");
        let mutable = Arc::new(
            lore_storage::local::mutable_store::LocalMutableStore::new(
                None::<&std::path::Path>,
                lore_storage::MutableStoreSettings::default(),
                immutable.clone(),
            )
            .await
            .expect("Failed to create store"),
        );
        Arc::new(RepositoryContext::new(
            None,
            immutable,
            mutable,
            Context::default().into(),
            lore_revision::instance::InstanceId::generate(),
            Err(ProtocolError::from(lore_base::error::NoRemote)),
            Arc::default(),
            RepositoryFormat::Lore,
        ))
    }

    #[tokio::test]
    async fn test_mapping_addition() {
        let a_context = Context::default();
        let a_hash = Hash::hash_buffer(&[0, 1, 2, 3]);
        let address_to = Address {
            hash: a_hash,
            context: a_context,
        };

        let repository = new_test_context().await;
        let state = Arc::new(state::State::new());

        let addition = NodeChange {
            action: lore_revision::change::FileAction::Add,
            path: RelativePath::from_str("Samples/Content/file.uasset").unwrap(),
            from_path: None,
            flags: Flags::None,
            from: NodeChangeState {
                node: 1,
                repository: repository.clone(),
                state: state.clone(),
                address: Address::default(),
                flags: NodeFlags::NoFlags,
            },
            to: NodeChangeState {
                node: 2,
                repository: repository.clone(),
                state: state.clone(),
                address: address_to,
                flags: NodeFlags::File,
            },
        };
        let mapped = map_to_path_diff(&addition, repository.id).await;
        assert_eq!(
            mapped,
            Some(PathDiff {
                from: None,
                to: Some(Path {
                    path: "Samples/Content/file.uasset".to_string(),
                    address: address_to.into(),
                    r#type: PathType::File as i32,
                    tracking: false,
                }),
                automerged: false,
                link_partition: Bytes::new(),
                tracking: false,
            })
        );
    }

    #[tokio::test]
    async fn test_mapping_deletion() {
        let a_context = Context::default();
        let a_hash = Hash::hash_buffer(&[0, 1, 2, 3]);
        let address_from = Address {
            hash: a_hash,
            context: a_context,
        };

        let repository = new_test_context().await;
        let state = Arc::new(state::State::new());

        let deletion = NodeChange {
            action: lore_revision::change::FileAction::Delete,
            path: RelativePath::from_str("Samples/Content/file.uasset").unwrap(),
            from_path: None,
            flags: Flags::None,
            from: NodeChangeState {
                node: 1,
                repository: repository.clone(),
                state: state.clone(),
                address: address_from,
                flags: NodeFlags::File,
            },
            to: NodeChangeState {
                node: 2,
                repository: repository.clone(),
                state: state.clone(),
                address: Address::default(),
                flags: NodeFlags::File,
            },
        };
        let mapped = map_to_path_diff(&deletion, repository.id).await;
        assert_eq!(
            mapped,
            Some(PathDiff {
                from: Some(Path {
                    path: "Samples/Content/file.uasset".to_string(),
                    address: address_from.into(),
                    r#type: PathType::File as i32,
                    tracking: false,
                }),
                to: None,
                automerged: false,
                link_partition: Bytes::new(),
                tracking: false,
            })
        );
    }

    #[tokio::test]
    async fn test_mapping_modification() {
        let a_context = Context::default();
        let a_hash = Hash::hash_buffer(&[0, 1, 2, 3]);
        let address_from = Address {
            hash: a_hash,
            context: a_context,
        };
        let address_to = Address {
            hash: a_hash,
            context: a_context,
        };

        let repository = new_test_context().await;
        let state = Arc::new(state::State::new());

        let modification = NodeChange {
            action: lore_revision::change::FileAction::Keep,
            path: RelativePath::from_str("Samples/Content/file.uasset").unwrap(),
            from_path: None,
            flags: Flags::None,
            from: NodeChangeState {
                node: 1,
                repository: repository.clone(),
                state: state.clone(),
                address: address_from,
                flags: NodeFlags::File,
            },
            to: NodeChangeState {
                node: 2,
                repository: repository.clone(),
                state: state.clone(),
                address: address_to,
                flags: NodeFlags::File,
            },
        };
        let mapped = map_to_path_diff(&modification, repository.id).await;
        assert_eq!(
            mapped,
            Some(PathDiff {
                from: Some(Path {
                    path: "Samples/Content/file.uasset".to_string(),
                    address: address_from.into(),
                    r#type: PathType::File as i32,
                    tracking: false,
                }),
                to: Some(Path {
                    path: "Samples/Content/file.uasset".to_string(),
                    address: address_to.into(),
                    r#type: PathType::File as i32,
                    tracking: false,
                }),
                automerged: false,
                link_partition: Bytes::new(),
                tracking: false,
            })
        );
    }

    #[tokio::test]
    async fn test_mapping_empty_files_with_zero_hash() {
        let a_context = Context::default();
        let a_hash = Hash::default();
        let address_to = Address {
            hash: a_hash,
            context: a_context,
        };

        let repository = new_test_context().await;
        let state = Arc::new(state::State::new());

        let addition = NodeChange {
            action: lore_revision::change::FileAction::Add,
            path: RelativePath::from_str("Samples/Content/file.uasset").unwrap(),
            from_path: None,
            flags: Flags::None,
            from: NodeChangeState {
                node: 1,
                repository: repository.clone(),
                state: state.clone(),
                address: Address::default(),
                flags: NodeFlags::NoFlags,
            },
            to: NodeChangeState {
                node: 2,
                repository: repository.clone(),
                state: state.clone(),
                address: address_to,
                flags: NodeFlags::File,
            },
        };
        let mapped = map_to_path_diff(&addition, repository.id).await;
        assert_eq!(
            mapped,
            Some(PathDiff {
                from: None,
                to: Some(Path {
                    path: "Samples/Content/file.uasset".to_string(),
                    address: address_to.into(),
                    r#type: PathType::File as i32,
                    tracking: false,
                }),
                automerged: false,
                link_partition: Bytes::new(),
                tracking: false,
            })
        );
    }

    #[tokio::test]
    async fn test_mapping_link_addition() {
        let a_context = Context::default();
        let a_hash = Hash::hash_buffer(&[4, 5, 6, 7]);
        let address_to = Address {
            hash: a_hash,
            context: a_context,
        };

        let repository = new_test_context().await;
        let state = Arc::new(state::State::new());

        let link_addition = NodeChange {
            action: lore_revision::change::FileAction::Add,
            path: RelativePath::from_str("Samples/Content/submodule").unwrap(),
            from_path: None,
            flags: Flags::None,
            from: NodeChangeState {
                node: 1,
                repository: repository.clone(),
                state: state.clone(),
                address: Address::default(),
                flags: NodeFlags::NoFlags,
            },
            to: NodeChangeState {
                node: 2,
                repository: repository.clone(),
                state: state.clone(),
                address: address_to,
                flags: NodeFlags::Link,
            },
        };
        let mapped = map_to_path_diff(&link_addition, repository.id).await;
        assert_eq!(
            mapped,
            Some(PathDiff {
                from: None,
                to: Some(Path {
                    path: "Samples/Content/submodule".to_string(),
                    address: address_to.into(),
                    r#type: PathType::Link as i32,
                    tracking: false,
                }),
                automerged: false,
                link_partition: Bytes::new(),
                tracking: false,
            })
        );
    }

    #[tokio::test]
    async fn test_mapping_link_deletion() {
        let a_context = Context::default();
        let a_hash = Hash::hash_buffer(&[8, 9, 10, 11]);
        let address_from = Address {
            hash: a_hash,
            context: a_context,
        };

        let repository = new_test_context().await;
        let state = Arc::new(state::State::new());

        let link_deletion = NodeChange {
            action: lore_revision::change::FileAction::Delete,
            path: RelativePath::from_str("Samples/Content/submodule").unwrap(),
            from_path: None,
            flags: Flags::None,
            from: NodeChangeState {
                node: 1,
                repository: repository.clone(),
                state: state.clone(),
                address: address_from,
                flags: NodeFlags::Link,
            },
            to: NodeChangeState {
                node: 2,
                repository: repository.clone(),
                state: state.clone(),
                address: Address::default(),
                flags: NodeFlags::Link,
            },
        };
        let mapped = map_to_path_diff(&link_deletion, repository.id).await;
        assert_eq!(
            mapped,
            Some(PathDiff {
                from: Some(Path {
                    path: "Samples/Content/submodule".to_string(),
                    address: address_from.into(),
                    r#type: PathType::Link as i32,
                    tracking: false,
                }),
                to: None,
                automerged: false,
                link_partition: Bytes::new(),
                tracking: false,
            })
        );
    }

    #[tokio::test]
    async fn test_mapping_link_modification() {
        let a_context = Context::default();
        let a_hash_from = Hash::hash_buffer(&[12, 13, 14, 15]);
        let a_hash_to = Hash::hash_buffer(&[16, 17, 18, 19]);
        let address_from = Address {
            hash: a_hash_from,
            context: a_context,
        };
        let address_to = Address {
            hash: a_hash_to,
            context: a_context,
        };

        let repository = new_test_context().await;
        let state = Arc::new(state::State::new());

        let link_modification = NodeChange {
            action: lore_revision::change::FileAction::Keep,
            path: RelativePath::from_str("Samples/Content/submodule").unwrap(),
            from_path: None,
            flags: Flags::None,
            from: NodeChangeState {
                node: 1,
                repository: repository.clone(),
                state: state.clone(),
                address: address_from,
                flags: NodeFlags::Link,
            },
            to: NodeChangeState {
                node: 2,
                repository: repository.clone(),
                state: state.clone(),
                address: address_to,
                flags: NodeFlags::Link,
            },
        };
        let mapped = map_to_path_diff(&link_modification, repository.id).await;
        assert_eq!(
            mapped,
            Some(PathDiff {
                from: Some(Path {
                    path: "Samples/Content/submodule".to_string(),
                    address: address_from.into(),
                    r#type: PathType::Link as i32,
                    tracking: false,
                }),
                to: Some(Path {
                    path: "Samples/Content/submodule".to_string(),
                    address: address_to.into(),
                    r#type: PathType::Link as i32,
                    tracking: false,
                }),
                automerged: false,
                link_partition: Bytes::new(),
                tracking: false,
            })
        );
    }

    #[tokio::test]
    async fn test_mapping_automerged_conflict() {
        let a_context = Context::default();
        let a_hash_from = Hash::hash_buffer(&[20, 21, 22, 23]);
        let a_hash_to = Hash::hash_buffer(&[24, 25, 26, 27]);
        let address_from = Address {
            hash: a_hash_from,
            context: a_context,
        };
        let address_to = Address {
            hash: a_hash_to,
            context: a_context,
        };

        let repository = new_test_context().await;
        let state = Arc::new(state::State::new());

        let automerged_change = NodeChange {
            action: lore_revision::change::FileAction::Keep,
            path: RelativePath::from_str("Samples/Content/merged.txt").unwrap(),
            from_path: None,
            flags: Flags::ConflictAutomerged,
            from: NodeChangeState {
                node: 1,
                repository: repository.clone(),
                state: state.clone(),
                address: address_from,
                flags: NodeFlags::File,
            },
            to: NodeChangeState {
                node: 2,
                repository: repository.clone(),
                state: state.clone(),
                address: address_to,
                flags: NodeFlags::File,
            },
        };
        let mapped = map_to_path_diff(&automerged_change, repository.id).await;
        assert_eq!(
            mapped,
            Some(PathDiff {
                from: Some(Path {
                    path: "Samples/Content/merged.txt".to_string(),
                    address: address_from.into(),
                    r#type: PathType::File as i32,
                    tracking: false,
                }),
                to: Some(Path {
                    path: "Samples/Content/merged.txt".to_string(),
                    address: address_to.into(),
                    r#type: PathType::File as i32,
                    tracking: false,
                }),
                automerged: true,
                link_partition: Bytes::new(),
                tracking: false,
            })
        );
    }

    /// A link into a different repository stamps `link_partition` with that
    /// target repository id.
    #[tokio::test]
    async fn test_mapping_cross_link_sets_partition() {
        let repository = new_test_context().await;
        let state = Arc::new(state::State::new());

        let target_repository = RepositoryId::from(uuid::Uuid::now_v7());
        let a_hash = Hash::hash_buffer(&[30, 31, 32, 33]);
        let address_to = Address {
            hash: a_hash,
            context: target_repository.into(),
        };

        let link_addition = NodeChange {
            action: lore_revision::change::FileAction::Add,
            path: RelativePath::from_str("Samples/Content/submodule").unwrap(),
            from_path: None,
            flags: Flags::None,
            from: NodeChangeState {
                node: 1,
                repository: repository.clone(),
                state: state.clone(),
                address: Address::default(),
                flags: NodeFlags::NoFlags,
            },
            to: NodeChangeState {
                node: 2,
                repository: repository.clone(),
                state: state.clone(),
                address: address_to,
                flags: NodeFlags::Link,
            },
        };

        let mapped = map_to_path_diff(&link_addition, repository.id)
            .await
            .expect("link addition maps to a diff");
        assert_eq!(
            mapped.link_partition,
            Bytes::from(target_repository),
            "cross-repository link change carries the target repository partition",
        );
        // No link reference is recorded, so tracking falls back to pinned.
        assert!(!mapped.tracking);
    }

    /// A link into the request's own repository leaves `link_partition` empty.
    #[tokio::test]
    async fn test_mapping_same_repo_partition_empty() {
        let repository = new_test_context().await;
        let state = Arc::new(state::State::new());

        let a_hash = Hash::hash_buffer(&[40, 41, 42, 43]);
        let address_to = Address {
            hash: a_hash,
            // new_test_context uses the default context as its repository id.
            context: Context::default(),
        };

        let link_addition = NodeChange {
            action: lore_revision::change::FileAction::Add,
            path: RelativePath::from_str("Samples/Content/submodule").unwrap(),
            from_path: None,
            flags: Flags::None,
            from: NodeChangeState {
                node: 1,
                repository: repository.clone(),
                state: state.clone(),
                address: Address::default(),
                flags: NodeFlags::NoFlags,
            },
            to: NodeChangeState {
                node: 2,
                repository: repository.clone(),
                state: state.clone(),
                address: address_to,
                flags: NodeFlags::Link,
            },
        };

        let mapped = map_to_path_diff(&link_addition, repository.id)
            .await
            .expect("link addition maps to a diff");
        assert!(
            mapped.link_partition.is_empty(),
            "same-repository change leaves the partition empty",
        );
        assert!(!mapped.tracking);
    }
}
