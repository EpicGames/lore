// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use lore_base::lore_spawn_net;
use lore_base::types::Hash;
use lore_error_set::prelude::*;
use lore_proto::lore::notification;
use lore_proto::lore::notification::event::Event::BranchCreated;
use lore_proto::lore::notification::event::Event::BranchDeleted;
use lore_proto::lore::notification::event::Event::BranchPushed;
use lore_proto::lore::notification::event::Event::ResourceLocked;
use lore_proto::lore::notification::event::Event::ResourceUnlocked;
use lore_proto::lore::notification::notification_service_client;
use lore_proto::lore::notification::notification_service_client::NotificationServiceClient;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreEvent;
use lore_revision::interface::LoreString;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::lore::execution_context;
use lore_revision::lore_debug;
use lore_revision::notification::LoreNotificationBranchCreatedEventData;
use lore_revision::notification::LoreNotificationBranchDeletedEventData;
use lore_revision::notification::LoreNotificationBranchPushedEventData;
use lore_revision::notification::LoreNotificationResourceLockedEventData;
use lore_revision::notification::LoreNotificationResourceUnlockedEventData;
use lore_revision::notification::LoreNotificationSubscribedEventData;
use lore_revision::notification::LoreNotificationUnsubscribedEventData;
use lore_revision::notification::NotificationError;
use lore_revision::notification::NotificationSubscription;
use lore_revision::util;
use lore_transport::Connection;
use lore_transport::grpc;
use lore_transport::grpc::AuthzInterceptor;
use lore_transport::grpc::Channel;
use tokio_util::sync::CancellationToken;
use tonic::Streaming;
use tonic::codegen::InterceptedService;

pub(crate) struct NotificationService;

#[async_trait]
impl lore_revision::notification::NotificationService for NotificationService {
    async fn create_client(
        &self,
        remote: Arc<Connection>,
        endpoint: &str,
    ) -> Result<Arc<dyn lore_revision::notification::NotificationClient>, NotificationError> {
        Ok(Arc::new(NotificationClient::new(
            remote,
            endpoint.to_string(),
        )))
    }
}

struct NotificationClient {
    remote: Arc<Connection>,
    endpoint: String,
}

impl NotificationClient {
    fn new(remote: Arc<Connection>, endpoint: String) -> Self {
        Self { remote, endpoint }
    }
}

const RETRY_START_DURATION: u64 = 100;
const RETRY_MAX_DURATION: u64 = 1_000;
const RETRY_MAX_ATTEMPTS: usize = 10;

impl NotificationClient {
    async fn connect(
        &self,
        repository: RepositoryId,
    ) -> Result<
        NotificationServiceClient<InterceptedService<Channel, AuthzInterceptor>>,
        NotificationError,
    > {
        let mut retry_attempt = 1;
        let mut retry =
            util::time::retry(RETRY_START_DURATION, RETRY_MAX_DURATION, RETRY_MAX_ATTEMPTS);

        let endpoint = self.endpoint.as_str();

        let auth_url = self.remote.auth_url.as_str();
        let identity = execution_context().user_id().await;

        loop {
            lore_debug!(
                "Connecting to notification endpoint {endpoint}{}",
                if retry_attempt > 1 {
                    format!(" (attempt {retry_attempt})")
                } else {
                    String::default()
                }
            );
            match grpc::connect(Arc::downgrade(&self.remote), endpoint, true).await {
                Ok(connection) => {
                    let auth = connection
                        .repository_authz(auth_url, &identity, repository)
                        .await;
                    let client =
                        notification_service_client::NotificationServiceClient::with_interceptor(
                            connection.channel(),
                            AuthzInterceptor { repository, auth },
                        );
                    return Ok(client);
                }
                Err(err) => {
                    if !retry.wait().await {
                        return Err(err).internal("connecting to notification service")?;
                    }
                    retry_attempt += 1;
                }
            }
        }
    }

    async fn subscribe(
        &self,
        client: NotificationServiceClient<InterceptedService<Channel, AuthzInterceptor>>,
        repository: RepositoryId,
    ) -> Result<Streaming<notification::Event>, NotificationError> {
        let mut retry_attempt = 1;
        let mut retry =
            util::time::retry(RETRY_START_DURATION, RETRY_MAX_DURATION, RETRY_MAX_ATTEMPTS);

        loop {
            lore_debug!("Attempt {retry_attempt} to subscribe to repository stream {repository}",);
            let request = notification::SubscribeRequest {
                repository: repository.into(),
            };

            let mut client = client.clone();
            let result = lore_spawn_net!(async move { client.subscribe(request).await })
                .await
                .internal("subscribing to notification stream")?;
            match result {
                Ok(response) => {
                    lore_debug!("Subscription to stream successful");
                    return Ok(response.into_inner());
                }
                Err(err) => {
                    lore_debug!("Subscription to stream failure: {err:?}");
                    if err.code() == tonic::Code::Unauthenticated {
                        return Err(err).internal("not authorized for notifications")?;
                    }
                    if !retry.wait().await {
                        return Err(NotificationError::internal("subscribing to stream"));
                    }
                    retry_attempt += 1;
                }
            }
        }
    }
}

#[async_trait]
impl lore_revision::notification::NotificationClient for NotificationClient {
    async fn subscribe_repository(
        self: Arc<Self>,
        repository: RepositoryId,
    ) -> Result<NotificationSubscription, NotificationError> {
        let client = self.connect(repository).await?;

        let stream = self.subscribe(client.clone(), repository).await?;

        let cancellation_token = CancellationToken::new();

        let stop = cancellation_token.clone();
        let client_ref = client;
        let event_sender = execution_context().dispatcher.sender();
        let task = lore_spawn_net!(async move {
            LoreEvent::NotificationSubscribed(LoreNotificationSubscribedEventData { repository })
                .send();

            event_loop(repository, stream, stop).await;

            LoreEvent::NotificationUnsubscribed(LoreNotificationUnsubscribedEventData {
                repository,
            })
            .send();

            drop(event_sender);
            drop(client_ref);
        });

        Ok(NotificationSubscription::new(task, cancellation_token))
    }
}

async fn event_loop(
    repository: RepositoryId,
    stream: Streaming<notification::Event>,
    stop: CancellationToken,
) {
    lore_debug!("Entering notification event loop for {repository}");

    let mut stream = stream;
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            message = stream.message() => {
                match message {
                    Ok(Some(event)) => {
                        lore_debug!("Processing notification event {event:?}");
                        let _ = handle_event(&event);
                    }
                    Ok(None) => {
                        lore_debug!("Notification stream closed for {repository}");
                        break;
                    }
                    Err(status) => {
                        lore_debug!("Failed to receive notification event: {status:?}");
                        break;
                    }
                }
            }
        }
    }

    lore_debug!("Exiting notification event loop for {repository}");
}

fn handle_event(event: &notification::Event) -> Result<(), NotificationError> {
    match &event.event {
        Some(BranchCreated(data)) => {
            LoreEvent::NotificationBranchCreated(LoreNotificationBranchCreatedEventData {
                branch: BranchId::from(&data.branch),
            })
            .send();
        }
        Some(BranchDeleted(data)) => {
            LoreEvent::NotificationBranchDeleted(LoreNotificationBranchDeletedEventData {
                branch: BranchId::from(&data.branch),
            })
            .send();
        }
        Some(BranchPushed(data)) => {
            let revision = Hash::from(data.revision.clone());
            let branch = BranchId::from(data.branch.clone());
            LoreEvent::NotificationBranchPushed(LoreNotificationBranchPushedEventData {
                revision,
                revision_number: data.revision_number,
                branch,
                user_id: LoreString::from(&data.user_id),
            })
            .send();
        }
        Some(ResourceLocked(data)) => {
            let branch = branch_from_resources(&data.resources);
            let paths = paths_from_resources(&data.resources);
            LoreEvent::NotificationResourceLocked(LoreNotificationResourceLockedEventData {
                user_id: LoreString::from(&data.user_id),
                branch,
                paths,
            })
            .send();
        }
        Some(ResourceUnlocked(data)) => {
            let branch = branch_from_resources(&data.resources);
            let paths = paths_from_resources(&data.resources);
            LoreEvent::NotificationResourceUnlocked(LoreNotificationResourceUnlockedEventData {
                user_id: LoreString::from(&data.user_id),
                branch,
                paths,
            })
            .send();
        }
        _ => {}
    }

    Ok(())
}

fn branch_from_resources(resources: &[lore_proto::lock::Resource]) -> BranchId {
    if resources.is_empty() {
        BranchId::default()
    } else {
        BranchId::from(resources[0].branch.clone())
    }
}

fn paths_from_resources(resources: &[lore_proto::lock::Resource]) -> LoreArray<LoreString> {
    let mut paths = vec![];
    for resource in resources {
        paths.push(LoreString::from(&resource.description));
    }
    LoreArray::from_vec(paths)
}
