// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//
// This module provides metrics utilities without initialization logic.
// For meter provider initialization, use urc-server's telemetry module.

pub mod grpc_metrics;
pub mod grpc_tower_layer;
pub mod http_metrics;
pub mod http_tower_layer;
pub mod user_agent_filter;

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::OnceLock;
use std::sync::RwLock;

use opentelemetry::metrics::Meter;
use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use tracing::debug;
use tracing::error;

/// Recorded in place of a `User-Agent` the request did not carry. Interned so a metric label
/// costs a refcount bump rather than an allocation.
pub static USER_AGENT_NONE: LazyLock<Arc<str>> =
    LazyLock::new(|| Arc::from(user_agent_filter::USER_AGENT_NONE));
/// Recorded in place of a `User-Agent` no configured pattern recognises, so a caller cannot
/// choose the value. Interned so a metric label costs a refcount bump rather than an allocation.
pub static USER_AGENT_UNKNOWN: LazyLock<Arc<str>> =
    LazyLock::new(|| Arc::from(user_agent_filter::USER_AGENT_UNKNOWN));

static METER_PROVIDER: OnceLock<RwLock<Arc<SdkMeterProvider>>> = OnceLock::new();

fn meter_provider_lock() -> &'static RwLock<Arc<SdkMeterProvider>> {
    METER_PROVIDER.get_or_init(|| RwLock::new(Arc::new(SdkMeterProvider::builder().build())))
}

/// Sets the global meter provider.
///
/// This is typically called by the server's telemetry initialization code.
pub fn set_meter_provider(provider: SdkMeterProvider) {
    if let Ok(ref mut meter_provider) = meter_provider_lock().write() {
        **meter_provider = Arc::new(provider);
        debug!("Set new meter provider");
    } else {
        error!("Failed to obtain write lock to set meter provider");
    }
}

/// Gets a reference to the global meter provider.
pub fn meter_provider() -> Arc<SdkMeterProvider> {
    if let Ok(provider) = meter_provider_lock().read() {
        provider.clone()
    } else {
        error!("Failed to obtain read lock for meter provider, returning a no-op one");
        Arc::new(SdkMeterProvider::builder().build())
    }
}

/// Gets a meter with the given name from the global meter provider.
pub fn meter(name: &'static str) -> Meter {
    meter_provider().meter(name)
}
