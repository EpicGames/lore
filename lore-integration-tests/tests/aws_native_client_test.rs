// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The lore-native AWS client against real services.
//!
//! `lore-aws`'s own tests cover the client against a scripted stub, which can assert what was
//! *sent* but has to be told what a service would say back. These assert the other half: that
//! `MinIO`, which verifies `SigV4` signatures, accepts what this client signs, and that every
//! operation's wire format is what `MinIO` and `DynamoDB` Local expect. `DynamoDB` Local holds no
//! secret to check a signature against, so its tests cover the wire format alone.
//!
//! Written against the client directly rather than through `lore-aws`'s facades.
#[cfg(all(test, feature = "integration_tests"))]
mod native_client_tests {
    use std::collections::HashSet;
    use std::error::Error;
    use std::sync::Arc;

    use bytes::Bytes;
    use lore_aws::aws::dynamodb::Condition;
    use lore_aws::aws::dynamodb::DynamoClient;
    use lore_aws::aws::dynamodb::QueryRequest;
    use lore_aws::aws::dynamodb::TransactPut;
    use lore_aws::aws::dynamodb::TransactWriteItem;
    use lore_aws::aws::item::AttributeValue;
    use lore_aws::aws::item::ByteStr;
    use lore_aws::aws::item::Item;
    use lore_aws::aws::s3::ObjectMetadata;
    use lore_aws::aws::s3::ObjectVersion;
    use lore_aws::aws::s3::S3Client;
    use lore_aws::store::immutable_store::FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE;
    use lore_aws::store::immutable_store::FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE;
    use lore_base::runtime::LORE_CONTEXT;
    use rand::random;

    use crate::common::aws_common::DYNAMODB_ENDPOINT;
    use crate::common::aws_common::FRAGMENTS_TABLE_NAME;
    use crate::common::aws_common::S3_ENDPOINT;
    use crate::common::aws_common::STORE_BUCKET_NAME;
    use crate::common::aws_common::VERSIONED_BUCKET_NAME;
    use crate::common::aws_common::client_core;
    use crate::common::aws_common::dynamodb_client;
    use crate::common::aws_common::fragments_item;
    use crate::common::aws_common::s3_bucket;
    use crate::common::aws_common::versioned_bucket;
    use crate::setup_execution;

    type TestResult = Result<(), Box<dyn Error>>;

    /// A client for the fragments table, created by the shared fixture if needed.
    async fn dynamo() -> Result<DynamoClient, Box<dyn Error>> {
        dynamodb_client(DYNAMODB_ENDPOINT.to_string(), vec![FRAGMENTS_TABLE_NAME]).await?;

        Ok(DynamoClient::new(client_core(DYNAMODB_ENDPOINT, false)?))
    }

    /// A client that keeps request ids and forces path-style addressing, so the bucket goes in the
    /// path whatever the endpoint.
    fn s3_client() -> Result<S3Client, Box<dyn Error>> {
        Ok(S3Client::new(client_core(S3_ENDPOINT, true)?, true))
    }

    /// [`s3_client`], once the store bucket exists.
    async fn s3() -> Result<S3Client, Box<dyn Error>> {
        s3_bucket(S3_ENDPOINT.to_string()).await?;

        s3_client()
    }

    /// A hex key of the shape the immutable store uses: the payload's content hash.
    fn object_key() -> String {
        random::<[u8; 32]>()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// A fragment-association row with a random hash and repository context.
    fn random_fragments_item() -> Item {
        fragments_item(&random::<[u8; 32]>(), &random::<[u8; 32]>())
    }

    #[tokio::test]
    async fn a_real_dynamodb_accepts_a_write_and_returns_it() -> TestResult {
        LORE_CONTEXT
            .scope(setup_execution("test".to_string()), async {
                let client = dynamo().await?;
                let key = random_fragments_item();

                client.put_item(FRAGMENTS_TABLE_NAME, &key, None).await?;

                let output = client
                    .get_item(FRAGMENTS_TABLE_NAME, &key, true)
                    .await?
                    .item
                    .expect("the row just written is there");

                assert_eq!(
                    output.get(FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE),
                    key.get(FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE),
                    "a binary key must survive the round trip byte for byte"
                );

                client.delete_item(FRAGMENTS_TABLE_NAME, &key, None).await?;
                assert!(
                    client
                        .get_item(FRAGMENTS_TABLE_NAME, &key, true)
                        .await?
                        .item
                        .is_none()
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn a_real_dynamodb_reports_a_table_by_name() -> TestResult {
        LORE_CONTEXT
            .scope(setup_execution("test".to_string()), async {
                let client = dynamo().await?;

                assert!(client.table_exists(FRAGMENTS_TABLE_NAME).await?);
                assert!(
                    !client.table_exists("no-such-table-2b8f1c").await?,
                    "a missing table is an answer, not a failure"
                );

                Ok(())
            })
            .await
    }

    /// The condition, the placeholders and the returned row all have to be understood by the
    /// service — this is the path a lock acquisition takes.
    #[tokio::test]
    async fn a_real_dynamodb_enforces_a_condition_and_returns_the_losing_row() -> TestResult {
        LORE_CONTEXT
            .scope(setup_execution("test".to_string()), async {
                let client = dynamo().await?;
                let key = random_fragments_item();

                client.put_item(FRAGMENTS_TABLE_NAME, &key, None).await?;

                let condition = Condition {
                    expression: "attribute_not_exists(#pk)",
                    names: &[("#pk", FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE)],
                    values: &[],
                    return_values_on_failure: true,
                };

                let error = client
                    .put_item(FRAGMENTS_TABLE_NAME, &key, Some(&condition))
                    .await
                    .expect_err("the row exists, so the condition must fail");

                let service = error.as_service_error().expect("a service error");
                assert!(service.code.is_conditional_check_failed_exception());
                assert!(
                    service
                        .item()
                        .and_then(|item| item.get(FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE))
                        .is_some(),
                    "the losing row is how a lock's current owner is identified"
                );

                client.delete_item(FRAGMENTS_TABLE_NAME, &key, None).await?;

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn a_real_dynamodb_queries_and_batches() -> TestResult {
        LORE_CONTEXT
            .scope(setup_execution("test".to_string()), async {
                let client = dynamo().await?;
                let partition = random::<[u8; 32]>();

                let addresses: Vec<[u8; 32]> = (0..5).map(|_| random()).collect();
                let keys: Vec<Item> = addresses
                    .iter()
                    .map(|address| fragments_item(&partition, address))
                    .collect();
                for key in &keys {
                    client.put_item(FRAGMENTS_TABLE_NAME, key, None).await?;
                }

                let query = client
                    .query(&QueryRequest {
                        table: FRAGMENTS_TABLE_NAME,
                        index: None,
                        key_condition: "#pk = :pk",
                        names: &[("#pk", FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE)],
                        values: &[(":pk", AttributeValue::blob(partition))],
                        filter: None,
                        limit: None,
                        select: None,
                        consistent: true,
                        exclusive_start_key: None,
                    })
                    .await?;

                assert_eq!(query.count(), 5, "every row in the partition");

                let batch = client
                    .batch_get_item(FRAGMENTS_TABLE_NAME, &keys, true)
                    .await?;

                assert!(
                    batch.unprocessed_keys.is_empty(),
                    "five small rows fit in one answer: {:?}",
                    batch.unprocessed_keys
                );
                assert_eq!(batch.items.len(), 5, "one row per key");

                let served: HashSet<Vec<u8>> = batch
                    .items
                    .iter()
                    .map(|item| {
                        item.get(FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE)
                            .and_then(AttributeValue::as_blob)
                            .expect("every row carries its sort key")
                            .to_vec()
                    })
                    .collect();
                let written: HashSet<Vec<u8>> =
                    addresses.iter().map(|address| address.to_vec()).collect();
                assert_eq!(served, written, "the rows served are the rows asked for");

                for key in &keys {
                    client.delete_item(FRAGMENTS_TABLE_NAME, key, None).await?;
                }

                Ok(())
            })
            .await
    }

    /// A transaction is the one write that carries an idempotency token, and the service validates
    /// it.
    #[tokio::test]
    async fn a_real_dynamodb_applies_a_transaction() -> TestResult {
        LORE_CONTEXT
            .scope(setup_execution("test".to_string()), async {
                let client = dynamo().await?;
                let partition = random::<[u8; 32]>();
                let table: Arc<str> = Arc::from(FRAGMENTS_TABLE_NAME);

                let items: Vec<TransactWriteItem> = (0..3)
                    .map(|_| {
                        TransactWriteItem::Put(TransactPut::new(
                            table.clone(),
                            fragments_item(&partition, &random::<[u8; 32]>()),
                        ))
                    })
                    .collect();
                let token = format!("{:032x}", random::<u128>());

                client.transact_write_items(&items, &token).await?;

                let query = client
                    .query(&QueryRequest {
                        table: FRAGMENTS_TABLE_NAME,
                        index: None,
                        key_condition: "#pk = :pk",
                        names: &[("#pk", FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE)],
                        values: &[(":pk", AttributeValue::blob(partition))],
                        filter: None,
                        limit: None,
                        select: None,
                        consistent: true,
                        exclusive_start_key: None,
                    })
                    .await?;

                assert_eq!(query.count(), 3, "all three writes applied together");

                for item in query.items.into_iter().flatten() {
                    client
                        .delete_item(FRAGMENTS_TABLE_NAME, &item, None)
                        .await?;
                }

                Ok(())
            })
            .await
    }

    /// The headline round trip: an object and the metadata describing it, written and read back
    /// through a service that validates the signature over both.
    #[tokio::test]
    async fn a_real_s3_round_trips_an_object_and_its_metadata() -> TestResult {
        LORE_CONTEXT
            .scope(setup_execution("test".to_string()), async {
                let client = s3().await?;
                let key = object_key();
                let payload = Bytes::from_static(b"a fragment payload, stored whole");

                let metadata = ObjectMetadata::from([(
                    ByteStr::from_static("lore-fragment"),
                    ByteStr::from_static("8:32:64"),
                )]);

                let written = client
                    .put_object(STORE_BUCKET_NAME, &key, payload.clone(), Some(&metadata))
                    .await?;
                assert!(
                    written.request_id.is_some(),
                    "a client that keeps request ids reads one off a successful write"
                );

                let head = client.head_object(STORE_BUCKET_NAME, &key).await?;
                assert_eq!(
                    head.metadata().get("lore-fragment"),
                    Some("8:32:64"),
                    "object metadata must survive the round trip, or a fragment loses its description"
                );
                assert_eq!(head.content_length(), Some(payload.len() as u64));

                let mut got = client.get_object(STORE_BUCKET_NAME, &key, None).await?;
                assert_eq!(got.metadata().get("lore-fragment"), Some("8:32:64"));

                let mut body = Vec::new();
                while let Some(chunk) = got.body.next().await {
                    body.extend_from_slice(&chunk?);
                }
                assert_eq!(body, payload, "the payload must come back unchanged");

                client.delete_object(STORE_BUCKET_NAME, &key, None).await?;

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn a_real_s3_serves_a_byte_range() -> TestResult {
        LORE_CONTEXT
            .scope(setup_execution("test".to_string()), async {
                let client = s3().await?;
                let key = object_key();
                let payload = Bytes::from_static(b"0123456789abcdef");

                client
                    .put_object(STORE_BUCKET_NAME, &key, payload, None)
                    .await?;

                let got = client
                    .get_object(STORE_BUCKET_NAME, &key, Some(4..8))
                    .await?;

                assert_eq!(
                    got.content_length(),
                    Some(4),
                    "a ranged read transfers the range and nothing else"
                );
                let body = got.body.collect().await?;
                assert_eq!(&body[..], b"4567");

                client.delete_object(STORE_BUCKET_NAME, &key, None).await?;

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn a_real_s3_reports_a_missing_object_as_not_found() -> TestResult {
        LORE_CONTEXT
            .scope(setup_execution("test".to_string()), async {
                let client = s3().await?;

                let error = client
                    .head_object(STORE_BUCKET_NAME, &object_key())
                    .await
                    .expect_err("a key that was never written cannot be found");

                let service = error.as_service_error().expect("a service error");
                assert!(
                    service.code.is_not_found(),
                    "the store reads this as \"no such hash\": {:?}",
                    service.code
                );

                let error = client
                    .get_object(STORE_BUCKET_NAME, &object_key(), None)
                    .await
                    .expect_err("nor can it be read");
                assert!(
                    error
                        .as_service_error()
                        .is_some_and(|service| service.code.is_no_such_key())
                );

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn a_real_s3_reports_buckets_by_name() -> TestResult {
        LORE_CONTEXT
            .scope(setup_execution("test".to_string()), async {
                let client = s3().await?;

                assert!(client.bucket_exists(STORE_BUCKET_NAME).await?);
                assert!(
                    !client.bucket_exists("no-such-bucket-2b8f1c").await?,
                    "a missing bucket is an answer, not a failure"
                );

                Ok(())
            })
            .await
    }

    /// Obliteration deletes every version of a key, so the listing has to name each version and
    /// each delete marker. Following the markers across pages is covered against the scripted
    /// service, which can produce a truncated page on demand.
    #[tokio::test]
    async fn a_real_s3_lists_the_versions_and_delete_markers_of_a_key() -> TestResult {
        LORE_CONTEXT
            .scope(setup_execution("test".to_string()), async {
                versioned_bucket().await?;
                let client = s3_client()?;
                let key = object_key();

                for generation in 0..3u8 {
                    client
                        .put_object(
                            VERSIONED_BUCKET_NAME,
                            &key,
                            Bytes::copy_from_slice(&[generation; 4]),
                            None,
                        )
                        .await?;
                }

                let listing = client
                    .list_object_versions(VERSIONED_BUCKET_NAME, &key)
                    .await?;
                assert!(!listing.truncated, "a handful of versions is one page");

                let written: Vec<String> = listing
                    .versions_of(&key)
                    .map(|version| {
                        version
                            .version_id
                            .as_deref()
                            .expect("a versioned bucket names every version")
                            .to_owned()
                    })
                    .collect();
                assert_eq!(
                    written.len(),
                    3,
                    "each write is a version of its own: {:?}",
                    listing.versions
                );
                assert!(
                    listing
                        .versions_of(&key)
                        .all(|version| !version.is_delete_marker)
                );

                client
                    .delete_object(VERSIONED_BUCKET_NAME, &key, Some(written[0].as_str()))
                    .await?;
                client
                    .delete_object(VERSIONED_BUCKET_NAME, &key, None)
                    .await?;

                let listing = client
                    .list_object_versions(VERSIONED_BUCKET_NAME, &key)
                    .await?;
                let remaining: Vec<&ObjectVersion> = listing.versions_of(&key).collect();

                assert_eq!(
                    remaining.len(),
                    3,
                    "two versions and one delete marker: {remaining:?}"
                );
                assert_eq!(
                    remaining
                        .iter()
                        .filter(|version| version.is_delete_marker)
                        .count(),
                    1,
                    "a delete naming no version writes a delete marker: {remaining:?}"
                );
                assert!(
                    remaining
                        .iter()
                        .all(|version| version.version_id.as_deref() != Some(written[0].as_str())),
                    "a delete naming a version removes it: {remaining:?}"
                );

                for version in &remaining {
                    client
                        .delete_object(VERSIONED_BUCKET_NAME, &key, version.version_id.as_deref())
                        .await?;
                }

                let listing = client
                    .list_object_versions(VERSIONED_BUCKET_NAME, &key)
                    .await?;
                assert_eq!(
                    listing.versions_of(&key).count(),
                    0,
                    "deleting every listed version by id leaves none: {:?}",
                    listing.versions
                );

                Ok(())
            })
            .await
    }

    /// A key with characters that have to be encoded, to prove what is signed is what is sent.
    /// Lore's own keys are hex, but the encoding rule is the part that is easy to get wrong.
    #[tokio::test]
    async fn a_real_s3_handles_a_key_that_needs_encoding() -> TestResult {
        LORE_CONTEXT
            .scope(setup_execution("test".to_string()), async {
                let client = s3().await?;
                let key = format!("{}/needs encoding+and~more", object_key());
                let payload = Bytes::from_static(b"encoded");

                client
                    .put_object(STORE_BUCKET_NAME, &key, payload.clone(), None)
                    .await?;

                let got = client.get_object(STORE_BUCKET_NAME, &key, None).await?;
                assert_eq!(got.body.collect().await?, payload);

                client.delete_object(STORE_BUCKET_NAME, &key, None).await?;

                Ok(())
            })
            .await
    }
}
