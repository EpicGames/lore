// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

mod client;

pub fn initialize() {
    let service = Arc::new(client::NotificationService);
    lore_revision::notification::register_notification_service("https", service.clone());
    lore_revision::notification::register_notification_service("spacesync", service.clone());
    lore_revision::notification::register_notification_service("spacesyncs", service.clone());
    lore_revision::notification::register_notification_service("lore", service.clone());
    lore_revision::notification::register_notification_service("lores", service.clone());
    // Legacy protocol schemes for backwards compatibility
    lore_revision::notification::register_notification_service("urc", service.clone());
    lore_revision::notification::register_notification_service("urcs", service.clone());
    lore_revision::notification::register_notification_service("grpc", service.clone());
    lore_revision::notification::register_notification_service("grpcs", service);
}

pub use lore_proto::lore::notification::notification_service_server::NotificationService;
pub use lore_proto::lore::notification::notification_service_server::NotificationServiceServer;
