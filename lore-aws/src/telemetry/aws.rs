// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use aws_config::BehaviorVersion;
use aws_smithy_http_client::Builder as HttpClientBuilder;
use aws_smithy_http_client::tls;
use aws_smithy_http_client::tls::rustls_provider::CryptoMode;
use opentelemetry::KeyValue;
use opentelemetry_sdk::resource::Resource;
use opentelemetry_sdk::resource::ResourceDetector;
use opentelemetry_semantic_conventions::resource::CLOUD_PROVIDER;
use opentelemetry_semantic_conventions::resource::CLOUD_REGION;
use tokio::runtime::Handle;

use crate::net_http_client::NetHttpClient;

/// Resource detector for AWS cloud environment.
///
/// Detects AWS-specific resource attributes like cloud provider and region
/// by loading AWS configuration from the environment.
pub struct AWSResourceDetector {
    handle: Handle,
}

impl AWSResourceDetector {
    /// Creates a new AWS resource detector with the given tokio runtime handle.
    ///
    /// The handle is used to execute async AWS SDK calls within the synchronous
    /// `detect()` method.
    pub fn new(handle: Handle) -> Self {
        Self { handle }
    }
}

impl ResourceDetector for AWSResourceDetector {
    fn detect(&self) -> Resource {
        let http_client = HttpClientBuilder::new()
            .tls_provider(tls::Provider::Rustls(CryptoMode::Ring))
            .build_https();
        // `ResourceDetector::detect` is a synchronous trait method. It runs once while telemetry
        // is built, before the server serves anything, so no other core is waiting on this one.
        #[allow(clippy::disallowed_methods)]
        let config = tokio::task::block_in_place(|| {
            self.handle.block_on(
                aws_config::defaults(BehaviorVersion::latest())
                    .http_client(NetHttpClient::new(http_client))
                    .load(),
            )
        });

        let mut attributes = vec![];

        if let Some(region) = config.region() {
            attributes.push(KeyValue::new(CLOUD_PROVIDER, "aws"));
            attributes.push(KeyValue::new(CLOUD_REGION, region.to_string()));
        }

        Resource::builder_empty()
            .with_attributes(attributes)
            .build()
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry::Key;
    use opentelemetry::Value;
    use serial_test::serial;

    use super::*;

    #[serial]
    fn test_aws_resource_detector_with_env_vars() {
        temp_env::with_vars([("AWS_REGION", Some("us-east-2"))], || {
            let detector = AWSResourceDetector::new(Handle::current());
            let resource = detector.detect();

            assert_eq!(
                resource.get(&Key::from_static_str(CLOUD_PROVIDER)),
                Some(Value::from("aws"))
            );
            assert_eq!(
                resource.get(&Key::from_static_str(CLOUD_REGION)),
                Some(Value::from("us-east-2"))
            );
        });
    }

    #[serial]
    fn test_aws_resource_detector_with_missing_env_vars() {
        // Unset all AWS-related env vars and point config files to non-existent paths
        temp_env::with_vars(
            [
                ("AWS_REGION", None),
                ("AWS_DEFAULT_REGION", None),
                ("AWS_CONFIG_FILE", Some("/dev/null")),
                ("AWS_SHARED_CREDENTIALS_FILE", Some("/dev/null")),
            ],
            || {
                let detector = AWSResourceDetector::new(Handle::current());
                let resource = detector.detect();

                assert!(
                    resource.is_empty(),
                    "AWS resource is not empty: {resource:?}"
                );
            },
        );
    }
}
