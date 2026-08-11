// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::SystemTime;

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;
use dashmap::Entry;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::LockResource;
use lore_proto::lock;
use lore_proto::lore::notification;
use lore_proto::lore::notification::BranchCreated;
use lore_proto::lore::notification::BranchDeleted;
use lore_proto::lore::notification::BranchPushed;
use lore_proto::lore::notification::ResourceLocked;
use lore_proto::lore::notification::ResourceUnlocked;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::notification::NotificationError;
use uuid::Uuid;

type Sender = tokio::sync::broadcast::Sender<notification::Event>;
type Receiver = tokio::sync::broadcast::Receiver<notification::Event>;

#[derive(Default)]
pub struct NotificationSender {
    sender: DashMap<RepositoryId, Sender>,
}

/// Default maximum number of events in broadcast channel buffer
static DEFAULT_CAPACITY: usize = 200;

impl NotificationSender {
    /// Subscribe to this repository's event stream, creating the channel on
    /// first use.
    ///
    /// The `entry` shard write lock is confined to the `match` below: both arms
    /// only construct or subscribe to a broadcast channel, neither awaits nor
    /// touches `sender` again, and the guard is dropped before this function
    /// returns — so no caller can hold it across a suspension point.
    #[allow(clippy::disallowed_methods)]
    pub fn register(&self, repository: RepositoryId) -> Receiver {
        match self.sender.entry(repository) {
            Entry::Occupied(sender) => sender.get().subscribe(),
            Entry::Vacant(entry) => {
                let (tx, rx) = tokio::sync::broadcast::channel(DEFAULT_CAPACITY);
                entry.insert(tx);
                rx
            }
        }
    }

    fn send_event(&self, repository: RepositoryId, event: notification::event::Event) {
        if let Some(sender) = self.sender.get(&repository) {
            let _ = sender.send(notification::Event {
                id: Uuid::new_v4().to_string(),
                time: Some(prost_types::Timestamp::from(SystemTime::now())),
                repository: Bytes::from_owner(Context::from(repository)),
                event: Some(event.clone()),
            });
        }
    }
}

#[async_trait]
impl lore_revision::notification::NotificationSender for NotificationSender {
    async fn branch_created(&self, repository: RepositoryId, branch: BranchId) {
        self.send_event(
            repository,
            notification::event::Event::BranchCreated(BranchCreated {
                branch: Bytes::from_owner(branch),
            }),
        );
    }

    async fn branch_pushed(
        &self,
        repository: RepositoryId,
        branch: BranchId,
        user_id: &str,
        revision: Hash,
        revision_number: u64,
    ) {
        self.send_event(
            repository,
            notification::event::Event::BranchPushed(BranchPushed {
                revision: Bytes::from_owner(revision),
                revision_number,
                branch: Bytes::from_owner(branch),
                user_id: user_id.to_string(),
            }),
        );
    }

    async fn branch_deleted(&self, repository: RepositoryId, branch: BranchId) {
        self.send_event(
            repository,
            notification::event::Event::BranchDeleted(BranchDeleted {
                branch: Bytes::from_owner(branch),
            }),
        );
    }

    async fn resource_locked(
        &self,
        repository: RepositoryId,
        _branch: BranchId,
        user_id: &str,
        resources: &[LockResource],
    ) {
        self.send_event(
            repository,
            notification::event::Event::ResourceLocked(ResourceLocked {
                user_id: user_id.to_string(),
                resources: resources
                    .iter()
                    .map(|res| lock::Resource {
                        branch: Bytes::from(res.branch),
                        hash: Bytes::from(res.hash),
                        description: res.description.clone(),
                    })
                    .collect(),
            }),
        );
    }

    async fn resource_unlocked(
        &self,
        repository: RepositoryId,
        _branch: BranchId,
        user_id: &str,
        resources: &[LockResource],
    ) {
        self.send_event(
            repository,
            notification::event::Event::ResourceUnlocked(ResourceUnlocked {
                user_id: user_id.to_string(),
                resources: resources
                    .iter()
                    .map(|res| lock::Resource {
                        branch: Bytes::from(res.branch),
                        hash: Bytes::from(res.hash),
                        description: res.description.clone(),
                    })
                    .collect(),
            }),
        );
    }

    async fn obliterate(
        &self,
        _repository: RepositoryId,
        _address: Address,
    ) -> Result<(), NotificationError> {
        // No-op for local notifications
        Ok(())
    }

    async fn compliance_check(
        &self,
        _stream_name: &str,
        _repository: RepositoryId,
        _branch: BranchId,
        _user_id: &str,
        _revision: Hash,
        _revision_number: u64,
        _ip_addr: Option<String>,
    ) {
        // No-op for local notifications
    }
}
