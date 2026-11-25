// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use opentelemetry::KeyValue;
use opentelemetry_sdk::resource::Resource;
use opentelemetry_sdk::resource::ResourceDetector;
use opentelemetry_semantic_conventions::resource::CLOUD_PROVIDER;
use opentelemetry_semantic_conventions::resource::CLOUD_REGION;
use tokio::runtime::Handle;

use crate::aws::http::HttpClient;
use crate::aws::imds::ImdsProvider;
use crate::aws::resolve_region;

/// Resource detector for AWS cloud environment.
///
/// Reports the cloud provider and region, reading the region from the environment variables and,
/// when they name none, from the instance metadata service.
pub struct AWSResourceDetector {
    handle: Handle,
}

impl AWSResourceDetector {
    /// Creates a new AWS resource detector with the given tokio runtime handle.
    ///
    /// The handle is used to execute the async IMDS lookup within the synchronous `detect()` method,
    /// which is the fallback when the environment names no region.
    pub fn new(handle: Handle) -> Self {
        Self { handle }
    }

    /// The region, from the environment or — on EC2, where the environment often says nothing — from
    /// the instance metadata service.
    ///
    /// IMDS is worth the round trip here and nowhere else in this client: this runs once while
    /// telemetry is built, and without it an EC2 deployment that configures no region loses its
    /// `cloud.region` attribute entirely.
    fn region(&self) -> Option<String> {
        if let Some(region) = resolve_region(None) {
            return Some(region);
        }

        let http = HttpClient::for_metadata().ok()?;

        // `ResourceDetector::detect` is a synchronous trait method. It runs once while telemetry
        // is built, before the server serves anything, so no other core is waiting on this one.
        #[allow(clippy::disallowed_methods)]
        tokio::task::block_in_place(|| {
            self.handle
                .block_on(async { ImdsProvider::new(http).region().await })
        })
    }
}

impl ResourceDetector for AWSResourceDetector {
    fn detect(&self) -> Resource {
        let mut attributes = vec![];

        if let Some(region) = self.region() {
            attributes.push(KeyValue::new(CLOUD_PROVIDER, "aws"));
            attributes.push(KeyValue::new(CLOUD_REGION, region));
        }

        Resource::builder_empty()
            .with_attributes(attributes)
            .build()
    }
}
