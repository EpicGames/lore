// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_aws::telemetry::AWSResourceDetector;
use opentelemetry::Key;
use opentelemetry::Value;
use opentelemetry_sdk::resource::ResourceDetector;
use opentelemetry_semantic_conventions::resource::CLOUD_PROVIDER;
use opentelemetry_semantic_conventions::resource::CLOUD_REGION;
use tokio::runtime::Handle;

/// The environment's region is reported, with AWS as the provider.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn test_aws_resource_detector_with_env_vars() {
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

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn test_aws_resource_detector_with_missing_env_vars() {
    // Unset every region variable, and switch IMDS off so this says the same thing on a
    // developer's machine as on an EC2 build host, where the metadata service would answer.
    temp_env::with_vars(
        [
            ("AWS_REGION", None),
            ("AWS_DEFAULT_REGION", None),
            ("AWS_EC2_METADATA_DISABLED", Some("true")),
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
