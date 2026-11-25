// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use opentelemetry::KeyValue;
use opentelemetry_sdk::resource::Resource;
use opentelemetry_sdk::resource::ResourceDetector;
use opentelemetry_semantic_conventions::resource::CLOUD_PROVIDER;
use opentelemetry_semantic_conventions::resource::CLOUD_REGION;
use tokio::runtime::Handle;
use tokio::runtime::RuntimeFlavor;

use crate::aws::imds::ImdsProvider;
use crate::aws::resolve_region;
use crate::aws::resolve_region_with_imds;

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
    fn region(&self) -> Option<String> {
        if let Some(region) = resolve_region(None) {
            return Some(region);
        }

        // `block_in_place` panics on a current-thread runtime, so there the detector reports only
        // what the environment names.
        if ImdsProvider::disabled() || self.handle.runtime_flavor() == RuntimeFlavor::CurrentThread
        {
            return None;
        }

        // `ResourceDetector::detect` is a synchronous trait method. It runs once while telemetry
        // is built, before the server serves anything, so no other core is waiting on this one.
        #[allow(clippy::disallowed_methods)]
        tokio::task::block_in_place(|| self.handle.block_on(resolve_region_with_imds(None)))
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
