// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(all(test, feature = "integration_tests"))]
pub(crate) mod net_common {
    /// How many numbers [`bind_matched_pair`] tries before giving up. Each attempt is a fresh port
    /// from the OS, so this only runs out if UDP is congested across the whole ephemeral range.
    pub(crate) const PORT_PAIR_ATTEMPTS: usize = 100;

    /// A TCP listener and a UDP socket on the same port, both held exclusively.
    ///
    /// A `lore://` server serves gRPC on TCP and QUIC on UDP at one number, and no bind can reserve
    /// a number for the other protocol. So the port is not chosen and then bound twice — it is
    /// taken from the OS on TCP, matched on UDP, and both sockets are handed to the servers already
    /// bound. Nothing can take either between choosing and serving, because there is no such gap.
    ///
    /// Neither socket sets a reuse option, so losing the UDP half is an error rather than a silent
    /// share of somebody else's port; the pair is released and the OS asked for a different number.
    pub(crate) fn bind_matched_pair() -> (std::net::TcpListener, std::net::UdpSocket) {
        for _ in 0..PORT_PAIR_ATTEMPTS {
            let tcp = std::net::TcpListener::bind("127.0.0.1:0").expect("bind tcp");
            let port = tcp.local_addr().expect("tcp local addr").port();
            match std::net::UdpSocket::bind(("127.0.0.1", port)) {
                Ok(udp) => return (tcp, udp),
                // Free on TCP, taken on UDP. Drop the listener too: keeping it would only make the
                // OS hand out a different number next time while this one stayed half-held.
                Err(_) => drop(tcp),
            }
        }
        panic!("no port free on both TCP and UDP after {PORT_PAIR_ATTEMPTS} attempts");
    }
}

#[cfg(all(test, feature = "integration_tests"))]
pub(crate) mod aws_common {
    use std::error::Error;
    use std::sync::Arc;

    use aws_sdk_dynamodb::operation::create_table::CreateTableError;
    use aws_sdk_dynamodb::types::AttributeDefinition;
    use aws_sdk_dynamodb::types::GlobalSecondaryIndex;
    use aws_sdk_dynamodb::types::KeySchemaElement;
    use aws_sdk_dynamodb::types::KeyType;
    use aws_sdk_dynamodb::types::Projection;
    use aws_sdk_dynamodb::types::ProjectionType;
    use aws_sdk_dynamodb::types::ProvisionedThroughput;
    use aws_sdk_dynamodb::types::ScalarAttributeType;
    use aws_sdk_s3::operation::create_bucket::CreateBucketError;
    use lore_aws::clients::AwsClientBuilder;
    use lore_aws::clients::HttpClientSettings;
    use lore_aws::dynamodb::DynamoDb;
    use lore_aws::s3::S3;
    use lore_aws::store::immutable_store::FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE;
    use lore_aws::store::immutable_store::FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE;
    use lore_aws::store::lock_store::*;
    use lore_aws::store::mutable_store::MUTABLE_STORE_DYNAMO_PARTITION_KEY_ATTRIBUTE;
    use lore_aws::store::mutable_store::MUTABLE_STORE_DYNAMO_SORT_KEY_ATTRIBUTE;
    use tracing::info;
    use tracing::warn;

    pub const LOCKS_TABLE_NAME: &str = "locks-local";
    pub const STORE_BUCKET_NAME: &str = "lore-immutable-store-local";
    pub const MUTABLE_STORE_TABLE_NAME: &str = "lore-mutable-store-local";
    pub const FRAGMENTS_TABLE_NAME: &str = "lore-fragments-local";
    pub const FRAGMENT_STATE_TABLE_NAME: &str = "lore-fragment-state-local";
    pub const FRAGMENT_METADATA_TABLE_NAME: &str = "lore-fragment-metadata-local";

    // NOTE: these credentials are just hardcoded in lore-integration-tests/compose.yaml
    const AWS_ACCESS_KEY_ID: &str = "lorelocal";
    const AWS_SECRET_ACCESS_KEY: &str = "lorelocal";

    pub async fn setup(
        tables: Vec<&str>,
    ) -> Result<(S3, DynamoDb, DynamoDb), Box<dyn Error + 'static>> {
        let _ = tracing_subscriber::fmt::try_init();

        Ok((
            s3_client("http://127.0.0.1:9000".to_string()).await?,
            dynamodb_client("http://127.0.0.1:9090".to_string(), tables.clone()).await?,
            dynamodb_client("http://127.0.0.1:9090".to_string(), tables).await?,
        ))
    }

    async fn create_store_bucket(client: &aws_sdk_s3::Client) -> Result<(), Box<dyn Error>> {
        match client
            .create_bucket()
            .bucket(STORE_BUCKET_NAME)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                let err = e.as_service_error().unwrap();
                if let CreateBucketError::BucketAlreadyOwnedByYou(_) = err {
                    // Since tests run in parallel there can be a race condition trying to create
                    // the bucket, if it turns out the bucket exists, just ignore the failure.
                    return Ok(());
                }

                Err(e.into())
            }
        }
    }

    async fn s3_client(endpoint_url: String) -> Result<S3, Box<dyn Error + 'static>> {
        let http_settings = HttpClientSettings::default();

        // Set up AWS client.
        let creds = aws_sdk_s3::config::Credentials::new(
            AWS_ACCESS_KEY_ID,
            AWS_SECRET_ACCESS_KEY,
            None,
            None,
            "test",
        );

        let client = AwsClientBuilder::builder()
            .with_http_settings(&http_settings)
            .with_credentials_provider(creds)
            .region("us-east-1")
            .endpoint(endpoint_url)
            .build_config()
            .await
            .s3()
            .build()
            .await?;

        match client.bucket_exists(STORE_BUCKET_NAME.to_string()).await {
            Ok(exists) => {
                if !exists {
                    info!("Bucket {STORE_BUCKET_NAME} does not exist, creating...");
                    create_store_bucket(client.sdk_client()).await?;
                }

                Ok(client)
            }
            Err(e) => {
                warn!("Failed to check if bucket exists: {e:?}");
                Err(e.into())
            }
        }
    }

    async fn create_locks_table(client: &aws_sdk_dynamodb::Client) -> Result<(), Box<dyn Error>> {
        let result = client
            .create_table()
            .table_name(LOCKS_TABLE_NAME)
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name(HASH_KEY)
                    .set_attribute_type(Some(ScalarAttributeType::B))
                    .build()?,
            )
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name(REPO_KEY)
                    .set_attribute_type(Some(ScalarAttributeType::B))
                    .build()?,
            )
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name(BRANCH_KEY)
                    .set_attribute_type(Some(ScalarAttributeType::B))
                    .build()?,
            )
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name(REPO_BRANCH_KEY)
                    .set_attribute_type(Some(ScalarAttributeType::B))
                    .build()?,
            )
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name(OWNER_KEY)
                    .set_attribute_type(Some(ScalarAttributeType::S))
                    .build()?,
            )
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name(DESC_KEY)
                    .set_attribute_type(Some(ScalarAttributeType::S))
                    .build()?,
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name(HASH_KEY)
                    .set_key_type(Some(KeyType::Hash))
                    .build()?,
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name(REPO_BRANCH_KEY)
                    .set_key_type(Some(KeyType::Range))
                    .build()?,
            )
            .global_secondary_indexes(
                GlobalSecondaryIndex::builder()
                    .index_name(OWNER_REPO_BRANCH_GSI)
                    .key_schema(
                        KeySchemaElement::builder()
                            .attribute_name(OWNER_KEY)
                            .set_key_type(Some(KeyType::Hash))
                            .build()?,
                    )
                    .key_schema(
                        KeySchemaElement::builder()
                            .attribute_name(REPO_BRANCH_KEY)
                            .set_key_type(Some(KeyType::Range))
                            .build()?,
                    )
                    .projection(
                        Projection::builder()
                            .projection_type(ProjectionType::All)
                            .build(),
                    )
                    .provisioned_throughput(
                        ProvisionedThroughput::builder()
                            .set_read_capacity_units(Some(5000))
                            .set_write_capacity_units(Some(5000))
                            .build()?,
                    )
                    .build()?,
            )
            .global_secondary_indexes(
                GlobalSecondaryIndex::builder()
                    .index_name(REPO_BRANCH_GSI)
                    .key_schema(
                        KeySchemaElement::builder()
                            .attribute_name(REPO_KEY)
                            .set_key_type(Some(KeyType::Hash))
                            .build()?,
                    )
                    .key_schema(
                        KeySchemaElement::builder()
                            .attribute_name(BRANCH_KEY)
                            .set_key_type(Some(KeyType::Range))
                            .build()?,
                    )
                    .projection(
                        Projection::builder()
                            .projection_type(ProjectionType::All)
                            .build(),
                    )
                    .provisioned_throughput(
                        ProvisionedThroughput::builder()
                            .set_read_capacity_units(Some(5000))
                            .set_write_capacity_units(Some(5000))
                            .build()?,
                    )
                    .build()?,
            )
            .global_secondary_indexes(
                GlobalSecondaryIndex::builder()
                    .index_name(REPO_BRANCH_DESC_GSI)
                    .key_schema(
                        KeySchemaElement::builder()
                            .attribute_name(REPO_BRANCH_KEY)
                            .set_key_type(Some(KeyType::Hash))
                            .build()?,
                    )
                    .key_schema(
                        KeySchemaElement::builder()
                            .attribute_name(DESC_KEY)
                            .set_key_type(Some(KeyType::Range))
                            .build()?,
                    )
                    .projection(
                        Projection::builder()
                            .projection_type(ProjectionType::All)
                            .build(),
                    )
                    .provisioned_throughput(
                        ProvisionedThroughput::builder()
                            .set_read_capacity_units(Some(5000))
                            .set_write_capacity_units(Some(5000))
                            .build()?,
                    )
                    .build()?,
            )
            .provisioned_throughput(
                ProvisionedThroughput::builder()
                    .set_read_capacity_units(Some(5000))
                    .set_write_capacity_units(Some(5000))
                    .build()?,
            )
            .send()
            .await;

        match result {
            Err(e) => {
                let err = e.as_service_error().unwrap();
                if let CreateTableError::ResourceInUseException(_) = err {
                    // Since tests run in parallel there can be a race condition trying to create
                    // the table, if it turns out the table exists, just ignore the failure.
                    return Ok(());
                }

                Err(e.into())
            }
            _ => Ok(()),
        }
    }

    async fn create_store_table(client: &aws_sdk_dynamodb::Client) -> Result<(), Box<dyn Error>> {
        let result = client
            .create_table()
            .table_name(MUTABLE_STORE_TABLE_NAME)
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name(MUTABLE_STORE_DYNAMO_PARTITION_KEY_ATTRIBUTE)
                    .set_attribute_type(Some(ScalarAttributeType::B))
                    .build()?,
            )
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name(MUTABLE_STORE_DYNAMO_SORT_KEY_ATTRIBUTE)
                    .set_attribute_type(Some(ScalarAttributeType::B))
                    .build()?,
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name(MUTABLE_STORE_DYNAMO_PARTITION_KEY_ATTRIBUTE)
                    .set_key_type(Some(KeyType::Hash))
                    .build()?,
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name(MUTABLE_STORE_DYNAMO_SORT_KEY_ATTRIBUTE)
                    .set_key_type(Some(KeyType::Range))
                    .build()?,
            )
            .provisioned_throughput(
                ProvisionedThroughput::builder()
                    .set_read_capacity_units(Some(5000))
                    .set_write_capacity_units(Some(5000))
                    .build()?,
            )
            .send()
            .await;

        match result {
            Err(e) => {
                let err = e.as_service_error().unwrap();
                if let CreateTableError::ResourceInUseException(_) = err {
                    // Since tests run in parallel there can be a race condition trying to create
                    // the table, if it turns out the table exists, just ignore the failure.
                    return Ok(());
                }

                Err(e.into())
            }
            _ => Ok(()),
        }
    }

    async fn create_fragments_table(
        client: &aws_sdk_dynamodb::Client,
    ) -> Result<(), Box<dyn Error>> {
        let result = client
            .create_table()
            .table_name(FRAGMENTS_TABLE_NAME)
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name(FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE)
                    .set_attribute_type(Some(ScalarAttributeType::B))
                    .build()?,
            )
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name(FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE)
                    .set_attribute_type(Some(ScalarAttributeType::B))
                    .build()?,
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name(FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE)
                    .set_key_type(Some(KeyType::Hash))
                    .build()?,
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name(FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE)
                    .set_key_type(Some(KeyType::Range))
                    .build()?,
            )
            .provisioned_throughput(
                ProvisionedThroughput::builder()
                    .set_read_capacity_units(Some(5000))
                    .set_write_capacity_units(Some(5000))
                    .build()?,
            )
            .send()
            .await;

        match result {
            Err(e) => {
                let err = e.as_service_error().unwrap();
                if let CreateTableError::ResourceInUseException(_) = err {
                    // Since tests run in parallel there can be a race condition trying to create
                    // the table, if it turns out the table exists, just ignore the failure.
                    return Ok(());
                }

                Err(e.into())
            }
            _ => Ok(()),
        }
    }

    async fn create_fragment_state_table(
        client: &aws_sdk_dynamodb::Client,
    ) -> Result<(), Box<dyn Error>> {
        let result = client
            .create_table()
            .table_name(FRAGMENT_STATE_TABLE_NAME)
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name("hash")
                    .set_attribute_type(Some(ScalarAttributeType::B))
                    .build()?,
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name("hash")
                    .set_key_type(Some(KeyType::Hash))
                    .build()?,
            )
            .provisioned_throughput(
                ProvisionedThroughput::builder()
                    .set_read_capacity_units(Some(5000))
                    .set_write_capacity_units(Some(5000))
                    .build()?,
            )
            .send()
            .await;

        match result {
            Err(e) => {
                let err = e.as_service_error().unwrap();
                if let CreateTableError::ResourceInUseException(_) = err {
                    return Ok(());
                }

                Err(e.into())
            }
            _ => Ok(()),
        }
    }

    async fn create_fragment_metadata_table(
        client: &aws_sdk_dynamodb::Client,
    ) -> Result<(), Box<dyn Error>> {
        let result = client
            .create_table()
            .table_name(FRAGMENT_METADATA_TABLE_NAME)
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name("hash")
                    .set_attribute_type(Some(ScalarAttributeType::B))
                    .build()?,
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name("hash")
                    .set_key_type(Some(KeyType::Hash))
                    .build()?,
            )
            .provisioned_throughput(
                ProvisionedThroughput::builder()
                    .set_read_capacity_units(Some(5000))
                    .set_write_capacity_units(Some(5000))
                    .build()?,
            )
            .send()
            .await;

        match result {
            Err(e) => {
                let err = e.as_service_error().unwrap();
                if let CreateTableError::ResourceInUseException(_) = err {
                    // Since tests run in parallel there can be a race condition trying to create
                    // the table, if it turns out the table exists, just ignore the failure.
                    return Ok(());
                }

                Err(e.into())
            }
            _ => Ok(()),
        }
    }

    pub(crate) async fn dynamodb_client(
        endpoint_url: String,
        tables: Vec<&str>,
    ) -> Result<DynamoDb, Box<dyn Error + 'static>> {
        let http_settings = HttpClientSettings::default();

        let creds = aws_sdk_dynamodb::config::Credentials::new(
            AWS_ACCESS_KEY_ID,
            AWS_SECRET_ACCESS_KEY,
            None,
            None,
            "test",
        );

        let client = AwsClientBuilder::builder()
            .with_http_settings(&http_settings)
            .with_credentials_provider(creds)
            .region("us-east-2")
            .endpoint(endpoint_url)
            .build_config()
            .await
            .dynamodb()
            .build()
            .await?;

        for table_name in tables {
            match client.table_exists(&Arc::from(table_name)).await {
                Ok(exists) => {
                    if !exists {
                        match table_name {
                            MUTABLE_STORE_TABLE_NAME => {
                                create_store_table(client.sdk_client()).await?;
                            }
                            FRAGMENTS_TABLE_NAME => {
                                create_fragments_table(client.sdk_client()).await?;
                            }
                            FRAGMENT_STATE_TABLE_NAME => {
                                create_fragment_state_table(client.sdk_client()).await?;
                            }
                            FRAGMENT_METADATA_TABLE_NAME => {
                                create_fragment_metadata_table(client.sdk_client()).await?;
                            }
                            LOCKS_TABLE_NAME => create_locks_table(client.sdk_client()).await?,
                            _ => {
                                return Err(
                                    anyhow::anyhow!("Invalid table name: {table_name}").into()
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to check if table exists: {e:?}");
                    return Err(e.into());
                }
            }
        }

        Ok(client)
    }
}
