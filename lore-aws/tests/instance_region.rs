// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Client builds that resolve their region through the process's instance metadata lookup, as a
//! binary of their own.
//!
//! The lookup keeps its first answer that is not a transient failure for the life of the process,
//! so a test that reaches it with no region named fixes the region every later test in the same
//! process would see.

#[path = "unit/aws/test_server.rs"]
#[allow(
    dead_code,
    reason = "the unit binary's stub service, of which this binary uses a part"
)]
mod test_server;

use std::sync::Arc;

use lore_aws::aws::creds::Credentials;
use lore_aws::aws::dynamodb::DynamoClient;
use lore_aws::aws::error::AwsCallError;
use lore_aws::aws::error::TransportError;
use lore_aws::aws::s3::S3Client;
use lore_aws::aws_error::AwsError;
use lore_aws::clients::AwsClientBuilder;
use lore_aws::clients::AwsClientError;
use lore_aws::clients::HttpClientSettings;
use lore_aws::clients::WantsService;
use lore_aws::dynamodb::DynamoDb;
use lore_aws::s3::S3;
use parking_lot::Mutex;
use test_server::StubResponse;
use test_server::StubServer;

/// A builder that names no region and no endpoint, and signs with static keys.
fn without_region() -> AwsClientBuilder<WantsService> {
    AwsClientBuilder::builder()
        .with_http_settings(&HttpClientSettings::default())
        .with_static_credentials(Credentials::new("AKIDTEST", "secret-for-tests", None, None))
        .build_config()
}

/// No region and no service endpoint in the environment, and the metadata service at `imds`.
fn no_region_variables(imds: Option<&str>) -> [(&'static str, Option<&str>); 7] {
    [
        ("AWS_REGION", None),
        ("AWS_DEFAULT_REGION", None),
        ("AWS_ENDPOINT_URL", None),
        ("AWS_ENDPOINT_URL_S3", None),
        ("AWS_ENDPOINT_URL_DYNAMODB", None),
        ("AWS_EC2_METADATA_DISABLED", None),
        ("AWS_EC2_METADATA_SERVICE_ENDPOINT", imds),
    ]
}

/// Clients that name no region sign for, and send to, the region of the instance they run on,
/// and every client in the process shares one metadata lookup.
#[tokio::test]
#[serial_test::serial]
async fn clients_with_no_region_take_the_instance_region() {
    let server = StubServer::start(vec![
        StubResponse::ok("imds-token"),
        StubResponse::ok("eu-west-3"),
    ])
    .await;
    let imds = server.endpoint().as_str().trim_end_matches('/').to_owned();

    temp_env::async_with_vars(no_region_variables(Some(&imds)), async {
        let s3_built = Arc::new(Mutex::new(None::<S3Client>));
        let s3_new = S3::new_context();
        s3_new.expect().returning({
            let s3_built = Arc::clone(&s3_built);
            move |client, _| {
                *s3_built.lock() = Some(client);
                S3::default()
            }
        });
        let dynamo_built = Arc::new(Mutex::new(None::<DynamoClient>));
        let dynamo_new = DynamoDb::new_context();
        dynamo_new.expect().returning({
            let dynamo_built = Arc::clone(&dynamo_built);
            move |client, _, _, _| {
                *dynamo_built.lock() = Some(client);
                DynamoDb::default()
            }
        });

        without_region()
            .s3()
            .build()
            .await
            .expect("the S3 client builds");
        without_region()
            .dynamodb()
            .build()
            .await
            .expect("the DynamoDB client builds");

        let s3 = s3_built
            .lock()
            .take()
            .expect("the builder made an S3 client");
        let dynamo = dynamo_built
            .lock()
            .take()
            .expect("the builder made a DynamoDB client");
        assert_eq!(
            s3.core().endpoint().as_str(),
            "https://s3.eu-west-3.amazonaws.com/"
        );
        assert_eq!(
            dynamo.core().endpoint().as_str(),
            "https://dynamodb.eu-west-3.amazonaws.com/"
        );
    })
    .await;

    assert_eq!(server.request_count(), 2);
}

/// With no region named and the metadata service switched off, a client fails to build.
#[tokio::test]
#[serial_test::serial]
async fn a_client_with_no_region_to_find_fails_to_build() {
    let mut variables = no_region_variables(None);
    variables[5] = ("AWS_EC2_METADATA_DISABLED", Some("true"));

    temp_env::async_with_vars(variables, async {
        let Err(error) = without_region().s3().build().await else {
            panic!("a client with no region built");
        };

        assert!(
            matches!(
                error,
                AwsClientError::Aws(AwsError::Aws(AwsCallError::Transport(
                    TransportError::Build(_)
                )))
            ),
            "got: {error:?}"
        );
    })
    .await;
}
