// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use lore_base::types::Address;
    use lore_base::types::Hash;
    use lore_revision::change;
    use lore_revision::change::NodeChange;
    use lore_revision::change::NodeChangeState;
    use lore_revision::node::NodeFlags;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::state::State;
    use lore_revision::util::path::RelativePath;
    use lore_revision::util::path::RelativePathBuf;
    use lore_storage::local::immutable_store;
    use lore_storage::local::mutable_store;

    include!("helper.rs");

    pub async fn new_test_context() -> Arc<lore_revision::repository::RepositoryContext> {
        let immutable = immutable_store::LocalImmutableStore::new(
            None,
            immutable_store::ImmutableStoreSettings::default(),
        )
        .await
        .expect("Failed to create store");
        let mutable = Arc::new(
            mutable_store::LocalMutableStore::new(
                None::<&Path>,
                lore_storage::MutableStoreSettings::default(),
                immutable.clone(),
            )
            .await
            .expect("Failed to create store"),
        );
        Arc::new(RepositoryContext::new(default_repository_creation_args(
            immutable, mutable,
        )))
    }

    #[test]
    fn changes_empty() {
        let mut changes: Vec<NodeChange> = vec![];
        change::reverse(&mut changes);
        assert!(changes.is_empty());
    }

    #[tokio::test]
    async fn changes_odd_three() {
        let mut changes: Vec<NodeChange> = vec![];

        let repository = new_test_context().await;
        let state = Arc::new(State::new());

        let hashes = [
            Hash::from(rand::random::<[u8; 32]>()),
            Hash::from(rand::random::<[u8; 32]>()),
            Hash::from(rand::random::<[u8; 32]>()),
            Hash::from(rand::random::<[u8; 32]>()),
        ];

        changes.push(NodeChange {
            action: change::FileAction::Add,
            from: NodeChangeState {
                repository: repository.clone(),
                state: state.clone(),
                node: 1,
                address: Address::zero_context_hash(hashes[0]),
                flags: NodeFlags::NoFlags,
            },
            to: NodeChangeState {
                repository: repository.clone(),
                state: state.clone(),
                node: 2,
                address: Address::default(),
                flags: NodeFlags::NoFlags,
            },
            flags: change::Flags::None,
            path: RelativePath::default(),
            from_path: None,
        });

        changes.push(NodeChange {
            action: change::FileAction::Keep,
            from: NodeChangeState {
                repository: repository.clone(),
                state: state.clone(),
                node: 4,
                address: Address::default(),
                flags: NodeFlags::NoFlags,
            },
            to: NodeChangeState {
                repository: repository.clone(),
                state: state.clone(),
                node: 3,
                address: Address::zero_context_hash(hashes[1]),
                flags: NodeFlags::NoFlags,
            },
            flags: change::Flags::Modify | change::Flags::Conflict,
            path: RelativePathBuf::new().push_and_freeze("name"),
            from_path: None,
        });

        changes.push(NodeChange {
            action: change::FileAction::Copy,
            from: NodeChangeState {
                repository: repository.clone(),
                state: state.clone(),
                node: 5,
                address: Address::zero_context_hash(hashes[3]),
                flags: NodeFlags::NoFlags,
            },
            to: NodeChangeState {
                repository: repository.clone(),
                state: state.clone(),
                node: 6,
                address: Address::zero_context_hash(hashes[2]),
                flags: NodeFlags::NoFlags,
            },
            flags: change::Flags::Merge,
            path: RelativePath::default(),
            from_path: None,
        });

        let changes_ref = changes.clone();

        change::reverse(&mut changes);

        assert_eq!(changes.len(), 3);

        assert_eq!(changes[0].action, change::FileAction::Delete);
        assert_eq!(changes[0].from.node, changes_ref[2].to.node);
        assert_eq!(changes[0].to.node, changes_ref[2].from.node);
        assert_eq!(changes[0].flags, change::Flags::None);
        assert_eq!(changes[0].from.address, changes_ref[2].to.address);
        assert_eq!(changes[0].to.address, changes_ref[2].from.address);
        assert_eq!(changes[0].path.as_str(), changes_ref[2].path.as_str());

        assert_eq!(changes[1].action, changes_ref[1].action);
        assert_eq!(changes[1].from.node, changes_ref[1].to.node);
        assert_eq!(changes[1].to.node, changes_ref[1].from.node);
        assert_eq!(changes[1].flags, change::Flags::Modify);
        assert_eq!(changes[1].from.address, changes_ref[1].to.address);
        assert_eq!(changes[1].to.address, changes_ref[1].from.address);
        assert_eq!(changes[1].path.as_str(), changes_ref[1].path.as_str());

        assert_eq!(changes[2].action, change::FileAction::Delete);
        assert_eq!(changes[2].from.node, changes_ref[0].to.node);
        assert_eq!(changes[2].to.node, changes_ref[0].from.node);
        assert_eq!(changes[2].flags, change::Flags::None);
        assert_eq!(changes[2].from.address, changes_ref[0].to.address);
        assert_eq!(changes[2].to.address, changes_ref[0].from.address);
        assert_eq!(changes[2].path.as_str(), changes_ref[0].path.as_str());
    }
    /*
    #[test]
    fn changes_odd_five() {
        let mut changes: Vec<NodeChange> = vec![];

        let hashes = [
            Hash {
                data: rand::random(),
            },
            Hash {
                data: rand::random(),
            },
            Hash {
                data: rand::random(),
            },
            Hash {
                data: rand::random(),
            },
            Hash {
                data: rand::random(),
            },
        ];

        changes.push(NodeChange {
            action: change::FileAction::Add,
            from.node: 1,
            to.node: 2,
            flags: change::Flags::None,
            from.address: Some(Address::zero_context_hash(hashes[0])),
            to.address: None,
            path: RelativePath::default(),
        });

        changes.push(NodeChange {
            action: change::FileAction::Keep,
            from.node: 4,
            to.node: 3,
            flags: change::Flags::Modify | change::Flags::Conflict,
            from.address: None,
            to.address: Some(Address::zero_context_hash(hashes[1])),
            path: RelativePathBuf::new().push_and_freeze("name"),
        });

        changes.push(NodeChange {
            action: change::FileAction::Copy,
            from.node: 5,
            to.node: 6,
            flags: change::Flags::Merge,
            from.address: Some(Address::zero_context_hash(hashes[3])),
            to.address: Some(Address::zero_context_hash(hashes[2])),
            path: RelativePath::default(),
        });

        changes.push(NodeChange {
            action: change::FileAction::Delete,
            from.node: 7,
            to.node: 7,
            flags: change::Flags::Conflict | change::Flags::Merge,
            from.address: None,
            to.address: None,
            path: RelativePath::default(),
        });

        changes.push(NodeChange {
            action: change::FileAction::Move,
            from.node: 8,
            to.node: 9,
            flags: change::Flags::ConflictAutomerged | change::Flags::Merge,
            from.address: Some(Address::zero_context_hash(hashes[4])),
            to.address: Some(Address::zero_context_hash(hashes[0])),
            path: RelativePath::default(),
        });

        let changes_ref = changes.clone();

        change::reverse(&mut changes);

        assert_eq!(changes.len(), 5);

        assert_eq!(changes[0].action, changes_ref[4].action);
        assert_eq!(changes[0].from.node, changes_ref[4].to.node);
        assert_eq!(changes[0].to.node, changes_ref[4].from.node);
        assert_eq!(changes[0].flags, change::Flags::None);
        assert_eq!(changes[0].from.address, changes_ref[4].to.address);
        assert_eq!(changes[0].to.address, changes_ref[4].from.address);
        assert_eq!(changes[0].path.as_str(), changes_ref[4].path.as_str());

        assert_eq!(changes[1].action, change::FileAction::Add);
        assert_eq!(changes[1].from.node, changes_ref[3].to.node);
        assert_eq!(changes[1].to.node, changes_ref[3].from.node);
        assert_eq!(changes[1].flags, change::Flags::None);
        assert_eq!(changes[1].from.address, changes_ref[3].to.address);
        assert_eq!(changes[1].to.address, changes_ref[3].from.address);
        assert_eq!(changes[1].path.as_str(), changes_ref[3].path.as_str());

        assert_eq!(changes[2].action, change::FileAction::Delete);
        assert_eq!(changes[2].from.node, changes_ref[2].to.node);
        assert_eq!(changes[2].to.node, changes_ref[2].from.node);
        assert_eq!(changes[2].flags, change::Flags::None);
        assert_eq!(changes[2].from.address, changes_ref[2].to.address);
        assert_eq!(changes[2].to.address, changes_ref[2].from.address);
        assert_eq!(changes[2].path.as_str(), changes_ref[2].path.as_str());

        assert_eq!(changes[3].action, changes_ref[1].action);
        assert_eq!(changes[3].from.node, changes_ref[1].to.node);
        assert_eq!(changes[3].to.node, changes_ref[1].from.node);
        assert_eq!(changes[3].flags, change::Flags::Modify);
        assert_eq!(changes[3].from.address, changes_ref[1].to.address);
        assert_eq!(changes[3].to.address, changes_ref[1].from.address);
        assert_eq!(changes[3].path.as_str(), changes_ref[1].path.as_str());

        assert_eq!(changes[4].action, change::FileAction::Delete);
        assert_eq!(changes[4].from.node, changes_ref[0].to.node);
        assert_eq!(changes[4].to.node, changes_ref[0].from.node);
        assert_eq!(changes[4].flags, changes_ref[0].flags);
        assert_eq!(changes[4].from.address, changes_ref[0].to.address);
        assert_eq!(changes[4].to.address, changes_ref[0].from.address);
        assert_eq!(changes[4].path.as_str(), changes_ref[0].path.as_str());
    }

    #[test]
    fn changes_even() {
        let mut changes: Vec<NodeChange> = vec![];

        let hashes = [
            Hash::from(rand::random::<[u8; 32]>()),
            Hash::from(rand::random::<[u8; 32]>()),
            Hash::from(rand::random::<[u8; 32]>()),
            Hash::from(rand::random::<[u8; 32]>()),
        ];

        changes.push(NodeChange {
            action: change::FileAction::Add,
            from.node: 1,
            to.node: 2,
            flags: change::Flags::Modify,
            from.address: Some(Address::zero_context_hash(hashes[0])),
            to.address: None,
            path: RelativePath::default(),
        });

        changes.push(NodeChange {
            action: change::FileAction::Copy,
            from.node: 5,
            to.node: 6,
            flags: change::Flags::Merge,
            from.address: Some(Address::zero_context_hash(hashes[3])),
            to.address: Some(Address::zero_context_hash(hashes[2])),
            path: RelativePath::default(),
        });

        changes.push(NodeChange {
            action: change::FileAction::Delete,
            from.node: 7,
            to.node: 7,
            flags: change::Flags::Conflict | change::Flags::Merge,
            from.address: None,
            to.address: None,
            path: RelativePath::default(),
        });

        changes.push(NodeChange {
            action: change::FileAction::Move,
            from.node: 8,
            to.node: 9,
            flags: change::Flags::ConflictAutomerged | change::Flags::Merge,
            from.address: Some(Address::zero_context_hash(hashes[1])),
            to.address: Some(Address::zero_context_hash(hashes[0])),
            path: RelativePath::default(),
        });

        let changes_ref = changes.clone();

        change::reverse(&mut changes);

        assert_eq!(changes.len(), 4);

        assert_eq!(changes[0].action, changes_ref[3].action);
        assert_eq!(changes[0].from.node, changes_ref[3].to.node);
        assert_eq!(changes[0].to.node, changes_ref[3].from.node);
        assert_eq!(changes[0].flags, change::Flags::None);
        assert_eq!(changes[0].from.address, changes_ref[3].to.address);
        assert_eq!(changes[0].to.address, changes_ref[3].from.address);
        assert_eq!(changes[0].path.as_str(), changes_ref[3].path.as_str());

        assert_eq!(changes[1].action, change::FileAction::Add);
        assert_eq!(changes[1].from.node, changes_ref[2].to.node);
        assert_eq!(changes[1].to.node, changes_ref[2].from.node);
        assert_eq!(changes[1].flags, change::Flags::None);
        assert_eq!(changes[1].from.address, changes_ref[2].to.address);
        assert_eq!(changes[1].to.address, changes_ref[2].from.address);
        assert_eq!(changes[1].path.as_str(), changes_ref[2].path.as_str());

        assert_eq!(changes[2].action, change::FileAction::Delete);
        assert_eq!(changes[2].from.node, changes_ref[1].to.node);
        assert_eq!(changes[2].to.node, changes_ref[1].from.node);
        assert_eq!(changes[2].flags, change::Flags::None);
        assert_eq!(changes[2].from.address, changes_ref[1].to.address);
        assert_eq!(changes[2].to.address, changes_ref[1].from.address);
        assert_eq!(changes[2].path.as_str(), changes_ref[1].path.as_str());

        assert_eq!(changes[3].action, change::FileAction::Delete);
        assert_eq!(changes[3].from.node, changes_ref[0].to.node);
        assert_eq!(changes[3].to.node, changes_ref[0].from.node);
        assert_eq!(changes[3].flags, changes_ref[0].flags);
        assert_eq!(changes[3].from.address, changes_ref[0].to.address);
        assert_eq!(changes[3].to.address, changes_ref[0].from.address);
        assert_eq!(changes[3].path.as_str(), changes_ref[0].path.as_str());
    }
    */

    #[tokio::test]
    async fn content_repository_id_resolves_link_nodes_to_their_target() {
        let parent = new_test_context().await;
        let mounted = new_test_context().await;
        let unmounted = new_test_context().await;
        let state = Arc::new(State::new());

        let link_side = |target: &Arc<RepositoryContext>| NodeChangeState {
            repository: parent.clone(),
            state: state.clone(),
            node: 1,
            address: Address {
                hash: Hash::from(rand::random::<[u8; 32]>()),
                context: target.id.into(),
            },
            flags: NodeFlags::Link,
        };
        let file_side = || NodeChangeState {
            repository: parent.clone(),
            state: state.clone(),
            node: 2,
            address: Address::zero_context_hash(Hash::from(rand::random::<[u8; 32]>())),
            flags: NodeFlags::File,
        };

        let added_link = NodeChange {
            action: change::FileAction::Add,
            from: file_side(),
            to: link_side(&mounted),
            flags: change::Flags::None,
            path: RelativePath::default(),
            from_path: None,
        };
        assert_eq!(
            added_link.content_repository_id(),
            mounted.id,
            "An added link's content is a revision of the repository it mounts",
        );

        let removed_link = NodeChange {
            action: change::FileAction::Delete,
            from: link_side(&unmounted),
            to: file_side(),
            flags: change::Flags::None,
            path: RelativePath::default(),
            from_path: None,
        };
        assert_eq!(
            removed_link.content_repository_id(),
            unmounted.id,
            "A delete resolves against the from side, which holds the removed link",
        );

        let file_change = NodeChange {
            action: change::FileAction::Keep,
            from: file_side(),
            to: file_side(),
            flags: change::Flags::None,
            path: RelativePath::default(),
            from_path: None,
        };
        assert_eq!(
            file_change.content_repository_id(),
            parent.id,
            "A non-link change's content lives in the repository it was walked from",
        );
    }
}
