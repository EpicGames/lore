// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::OnceLock;

use async_trait::async_trait;
use dashmap::DashMap;
use lore_base::types::LockResource;
use lore_error_set::prelude::*;
use lore_transport::Connection;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::interface::LoreArray;
use crate::interface::LoreString;
use crate::lore::Address;
use crate::lore::BranchId;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore_debug;
use crate::repository::RepositoryContext;

#[error_set]
pub enum NotificationError {}

impl crate::event::EventError for NotificationError {}

/// Data for a notification that a branch received a new revision.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreNotificationBranchPushedEventData {
    /// Hash of the pushed revision.
    pub revision: Hash,
    /// Sequence number of the pushed revision.
    pub revision_number: u64,
    /// Identifier of the branch that received the revision.
    pub branch: BranchId,
    /// Identifier of the user that pushed the revision.
    pub user_id: LoreString,
}

/// Data for a notification that a branch was created.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreNotificationBranchCreatedEventData {
    /// Identifier of the created branch.
    pub branch: BranchId,
}

/// Data for a notification that a branch was deleted.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreNotificationBranchDeletedEventData {
    /// Identifier of the deleted branch.
    pub branch: BranchId,
}

/// Data for a notification that resources were locked.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreNotificationResourceLockedEventData {
    /// Identifier of the user that locked the resources.
    pub user_id: LoreString,
    /// Identifier of the branch the resources belong to.
    pub branch: BranchId,
    /// Paths of the locked resources.
    pub paths: LoreArray<LoreString>,
}

/// Data for a notification that resources were unlocked.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreNotificationResourceUnlockedEventData {
    /// Identifier of the user that unlocked the resources.
    pub user_id: LoreString,
    /// Identifier of the branch the resources belong to.
    pub branch: BranchId,
    /// Paths of the unlocked resources.
    pub paths: LoreArray<LoreString>,
}

/// Data for a notification carrying a text message.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreNotificationTextEventData {
    /// Text content of the notification.
    pub text: LoreString,
}

/// Data for a notification carrying binary content.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreNotificationBinaryDataEventData {
    /// Binary content of the notification.
    pub data: LoreArray<u8>,
}

/// Data for a notification that a subscription to a repository was established.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreNotificationSubscribedEventData {
    /// Identifier of the subscribed repository.
    pub repository: RepositoryId,
}

/// Data for a notification that a subscription to a repository was removed.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreNotificationUnsubscribedEventData {
    /// Identifier of the unsubscribed repository.
    pub repository: RepositoryId,
}

#[async_trait]
pub trait NotificationService: Send + Sync {
    async fn create_client(
        &self,
        remote: Arc<Connection>,
        endpoint: &str,
    ) -> Result<Arc<dyn NotificationClient>, NotificationError>;
}

#[async_trait]
pub trait NotificationClient {
    async fn subscribe_repository(
        self: Arc<Self>,
        repository: RepositoryId,
    ) -> Result<NotificationSubscription, NotificationError>;
}

pub struct NotificationSubscription {
    task: JoinHandle<()>,
    cancellation_token: CancellationToken,
}

impl NotificationSubscription {
    pub fn new(task: JoinHandle<()>, cancellation_token: CancellationToken) -> Self {
        Self {
            task,
            cancellation_token,
        }
    }
}

static NOTIFICATION_SUBSCRIBERS: OnceLock<DashMap<RepositoryId, NotificationSubscription>> =
    OnceLock::new();

fn notification_subscribers() -> &'static DashMap<RepositoryId, NotificationSubscription> {
    NOTIFICATION_SUBSCRIBERS.get_or_init(DashMap::new)
}

/// Subscribe to notifications for the given repository
pub async fn subscribe(repository: Arc<RepositoryContext>) -> Result<(), NotificationError> {
    if notification_subscribers().contains_key(&repository.id) {
        return Ok(());
    }

    let Ok(remote) = repository.remote().await else {
        return Err(NotificationError::internal(
            "notifications not available when offline",
        ));
    };

    let remote_url = remote.remote_url.to_string();
    let endpoint = remote.environment.notification_url(&remote_url).to_string();

    lore_debug!("Creating notification client");
    let client = create_client(remote, &endpoint).await?;

    lore_debug!(
        "Subscribe to repository notifications for {}",
        repository.id
    );
    let subscriber = client.subscribe_repository(repository.id).await?;
    let _previous = notification_subscribers().insert(repository.id, subscriber);
    lore_debug!(
        "Subscribed to repository notifications for {}",
        repository.id
    );

    Ok(())
}

/// Unsubscribe from notifications for the given repository
pub async fn unsubscribe(repository: Arc<RepositoryContext>) -> Result<(), NotificationError> {
    let Some((_, subscriber)) = notification_subscribers().remove(&repository.id) else {
        return Err(NotificationError::internal(
            "notifications not available when offline",
        ));
    };

    lore_debug!("Unsubscribing notification client from {}", repository.id);

    subscriber.cancellation_token.cancel();
    let _ = subscriber.task.await;

    lore_debug!("Unsubscribed notification client from {}", repository.id);

    Ok(())
}

async fn create_client(
    remote: Arc<Connection>,
    endpoint: &str,
) -> Result<Arc<dyn NotificationClient>, NotificationError> {
    lore_debug!("Creating notification client for endpoint: {}", endpoint);
    let service_name = endpoint.split("://").next().unwrap_or("lores");
    let Some(service) = notification_service_registry().get(service_name) else {
        return Err(NotificationError::internal(
            "notification service type not supported",
        ));
    };

    service.value().create_client(remote, endpoint).await
}

static NOTIFICATION_SERVICE: OnceLock<DashMap<String, Arc<dyn NotificationService>>> =
    OnceLock::new();

fn notification_service_registry() -> &'static DashMap<String, Arc<dyn NotificationService>> {
    NOTIFICATION_SERVICE.get_or_init(DashMap::new)
}

pub fn register_notification_service(id: &str, service: Arc<dyn NotificationService>) {
    notification_service_registry().insert(id.to_string(), service);
}

#[async_trait]
pub trait NotificationSender
where
    Self: Send + Sync,
{
    async fn branch_created(&self, repository: RepositoryId, branch: BranchId);

    async fn branch_pushed(
        &self,
        repository: RepositoryId,
        branch: BranchId,
        user_id: &str,
        revision: Hash,
        revision_number: u64,
    );

    async fn branch_deleted(&self, repository: RepositoryId, branch: BranchId);

    async fn resource_locked(
        &self,
        repository: RepositoryId,
        branch: BranchId,
        user_id: &str,
        resources: &[LockResource],
    );

    async fn resource_unlocked(
        &self,
        repository: RepositoryId,
        branch: BranchId,
        user_id: &str,
        resources: &[LockResource],
    );

    async fn obliterate(
        &self,
        repository: RepositoryId,
        address: Address,
    ) -> Result<(), NotificationError>;

    #[allow(clippy::too_many_arguments)]
    async fn compliance_check(
        &self,
        stream_name: &str,
        repository: RepositoryId,
        branch: BranchId,
        user_id: &str,
        revision: Hash,
        revision_number: u64,
        ip_addr: Option<String>,
    );
}
