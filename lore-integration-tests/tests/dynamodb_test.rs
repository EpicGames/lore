// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(all(test, feature = "integration_tests"))]
mod dynamo_tests {
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::error::Error;
    use std::sync::Arc;

    use aws_sdk_dynamodb::types::AttributeValue;
    use aws_sdk_dynamodb::types::Put;
    use aws_sdk_dynamodb::types::ReturnValuesOnConditionCheckFailure;
    use aws_sdk_dynamodb::types::Select;
    use aws_sdk_dynamodb::types::TransactWriteItem;
    use aws_sdk_s3::primitives::Blob;
    use bytes::Bytes;
    use lore_aws::dynamodb::DynamoDbQuery;
    use lore_aws::store::immutable_store::FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE;
    use lore_aws::store::immutable_store::FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE;
    use lore_aws::store::lock_store::HASH_KEY;
    use lore_aws::store::lock_store::LockEntry;
    use lore_aws::store::lock_store::REPO_BRANCH_KEY;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_revision::lock::LockError;
    use lore_revision::lore::RepositoryId;
    use rand::random;
    use tracing::warn;
    use zerocopy::IntoBytes;

    use crate::common::aws_common::FRAGMENTS_TABLE_NAME;
    use crate::common::aws_common::LOCKS_TABLE_NAME;
    use crate::common::aws_common::dynamodb_client;
    use crate::setup_execution;

    type TestResult = Result<(), Box<dyn Error>>;

    #[tokio::test]
    async fn test_dynamo_batch_get_item() -> TestResult {
        let execution = setup_execution("test".to_string());
        let dynamo = LORE_CONTEXT
            .scope(execution.clone(), async move {
                dynamodb_client(
                    "http://127.0.0.1:9090".to_string(),
                    vec![FRAGMENTS_TABLE_NAME],
                )
                .await
            })
            .await?;

        let repository = random::<Context>();

        let mut addresses = vec![];
        let count = 834;
        for _ in 0..count {
            let address = random::<Address>();

            let mut repository_context = [0u8; size_of::<Context>() * 2];
            repository_context[..size_of::<Context>()].copy_from_slice(repository.data());
            repository_context[size_of::<Context>()..].copy_from_slice(address.context.data());

            let item = HashMap::from([
                (
                    FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE.to_string(),
                    AttributeValue::B(Blob::from(address.hash.as_bytes())),
                ),
                (
                    FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE.to_string(),
                    AttributeValue::B(Blob::from(repository_context.to_vec())),
                ),
            ]);

            dynamo
                .put_item(&Arc::from(FRAGMENTS_TABLE_NAME), item.clone())
                .await?;
            addresses.push(item);
        }

        let result = dynamo
            .batch_get_item(
                &Arc::from(FRAGMENTS_TABLE_NAME),
                addresses.clone(),
                true, /* consistent */
            )
            .await?;

        assert_eq!(count, result.len());

        for item in result {
            if let Some(pos) = addresses.iter().position(|i| i == &item) {
                addresses.remove(pos);
            }
        }

        assert!(addresses.is_empty());

        Ok(())
    }

    struct FragmentsCountQuery(Hash);

    impl DynamoDbQuery for FragmentsCountQuery {
        fn key_condition_expression(&self) -> &str {
            "#pk = :hash"
        }

        fn expression_attribute_names(&self) -> HashMap<String, String> {
            HashMap::from([(
                "#pk".to_string(),
                FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE.to_string(),
            )])
        }

        fn expression_attribute_values(&self) -> HashMap<String, AttributeValue> {
            HashMap::from([(
                ":hash".to_string(),
                AttributeValue::B(Blob::new(self.0.data())),
            )])
        }

        fn select(&self) -> Option<Select> {
            Some(Select::Count)
        }
    }

    #[tokio::test]
    async fn test_query_select() -> TestResult {
        let execution = setup_execution("test".to_string());
        let dynamo = LORE_CONTEXT
            .scope(execution.clone(), async move {
                dynamodb_client(
                    "http://127.0.0.1:9090".to_string(),
                    vec![FRAGMENTS_TABLE_NAME],
                )
                .await
            })
            .await?;

        let repository = random::<Context>();

        let address = random::<Address>();
        let count = 834;
        for _ in 0..count {
            let mut address = address;
            address.context = random::<Context>();

            let mut repository_context = [0u8; size_of::<Context>() * 2];
            repository_context[..size_of::<Context>()].copy_from_slice(repository.data());
            repository_context[size_of::<Context>()..].copy_from_slice(address.context.data());

            let item = HashMap::from([
                (
                    FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE.to_string(),
                    AttributeValue::B(Blob::from(address.hash.as_bytes())),
                ),
                (
                    FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE.to_string(),
                    AttributeValue::B(Blob::from(repository_context.to_vec())),
                ),
            ]);

            dynamo
                .put_item(&Arc::from(FRAGMENTS_TABLE_NAME), item.clone())
                .await?;
        }

        let result = dynamo
            .query_single(
                &Arc::from(FRAGMENTS_TABLE_NAME),
                FragmentsCountQuery(address.hash),
            )
            .await?;

        assert_eq!(count, result.count);

        Ok(())
    }

    struct LimitedFragmentsCountQuery(Hash);

    impl DynamoDbQuery for LimitedFragmentsCountQuery {
        fn key_condition_expression(&self) -> &str {
            "#pk = :hash"
        }

        fn expression_attribute_names(&self) -> HashMap<String, String> {
            HashMap::from([(
                "#pk".to_string(),
                FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE.to_string(),
            )])
        }

        fn expression_attribute_values(&self) -> HashMap<String, AttributeValue> {
            HashMap::from([(
                ":hash".to_string(),
                AttributeValue::B(Blob::new(self.0.data())),
            )])
        }

        fn select(&self) -> Option<Select> {
            Some(Select::Count)
        }

        fn limit(&self) -> Option<i32> {
            Some(1)
        }
    }

    #[tokio::test]
    async fn test_limited_query_select() -> TestResult {
        let execution = setup_execution("test".to_string());
        let dynamo = LORE_CONTEXT
            .scope(execution.clone(), async move {
                dynamodb_client(
                    "http://127.0.0.1:9090".to_string(),
                    vec![FRAGMENTS_TABLE_NAME],
                )
                .await
            })
            .await?;

        let repository = random::<Context>();
        let address = random::<Address>();
        let count = 10;
        for _ in 0..count {
            let mut address = address;
            address.context = random::<Context>();

            let mut repository_context = [0u8; size_of::<Context>() * 2];
            repository_context[..size_of::<Context>()].copy_from_slice(repository.data());
            repository_context[size_of::<Context>()..].copy_from_slice(address.context.data());

            let item = HashMap::from([
                (
                    FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE.to_string(),
                    AttributeValue::B(Blob::from(address.hash.as_bytes())),
                ),
                (
                    FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE.to_string(),
                    AttributeValue::B(Blob::from(repository_context.to_vec())),
                ),
            ]);

            dynamo
                .put_item(&Arc::from(FRAGMENTS_TABLE_NAME), item.clone())
                .await?;
        }

        let result = dynamo
            .query_paginated(
                &Arc::from(FRAGMENTS_TABLE_NAME),
                LimitedFragmentsCountQuery(address.hash),
            )
            .await?;

        assert_eq!(count, result.count);

        Ok(())
    }

    #[derive(Copy, Clone)]
    struct LimitedFragmentsQuery(Hash);

    impl DynamoDbQuery for LimitedFragmentsQuery {
        fn key_condition_expression(&self) -> &str {
            "#pk = :hash"
        }

        fn expression_attribute_names(&self) -> HashMap<String, String> {
            HashMap::from([(
                "#pk".to_string(),
                FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE.to_string(),
            )])
        }

        fn expression_attribute_values(&self) -> HashMap<String, AttributeValue> {
            HashMap::from([(
                ":hash".to_string(),
                AttributeValue::B(Blob::new(self.0.data())),
            )])
        }

        fn limit(&self) -> Option<i32> {
            Some(5)
        }
    }

    #[tokio::test]
    async fn test_limited_query() -> TestResult {
        let execution = setup_execution("test".to_string());
        let dynamo = LORE_CONTEXT
            .scope(execution.clone(), async move {
                dynamodb_client(
                    "http://127.0.0.1:9090".to_string(),
                    vec![FRAGMENTS_TABLE_NAME],
                )
                .await
            })
            .await?;

        let repository = random::<Context>();
        let address = random::<Address>();

        let query = LimitedFragmentsQuery(address.hash);

        let count = query.limit().unwrap() * 10;

        // Write 10x limit rows to DDB
        for _ in 0..count {
            let mut address = address;
            address.context = random::<Context>();

            let mut repository_context = [0u8; size_of::<Context>() * 2];
            repository_context[..size_of::<Context>()].copy_from_slice(repository.data());
            repository_context[size_of::<Context>()..].copy_from_slice(address.context.data());

            let item = HashMap::from([
                (
                    FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE.to_string(),
                    AttributeValue::B(Blob::from(address.hash.as_bytes())),
                ),
                (
                    FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE.to_string(),
                    AttributeValue::B(Blob::from(repository_context.to_vec())),
                ),
            ]);

            dynamo
                .put_item(&Arc::from(FRAGMENTS_TABLE_NAME), item.clone())
                .await?;
        }

        let result = dynamo
            .query_single(&Arc::from(FRAGMENTS_TABLE_NAME), query)
            .await?;

        // We should only have gotten the number of rows we requested.
        assert_eq!(query.limit().unwrap(), result.count);

        Ok(())
    }

    #[tokio::test]
    async fn test_transact_write_items() -> TestResult {
        let execution = setup_execution("test".to_string());
        let dynamo = LORE_CONTEXT
            .scope(execution.clone(), async move {
                dynamodb_client("http://127.0.0.1:9090".to_string(), vec![LOCKS_TABLE_NAME]).await
            })
            .await?;

        // Chunk size is 100, so this should generate multiple chunks
        let count = 250;
        let mut items = Vec::with_capacity(count);
        let mut hashes = Vec::with_capacity(count);

        let branch = random::<Context>();
        let branch_bytes: Bytes = branch.into();
        let repository = random::<RepositoryId>();
        let repository_bytes: Bytes = repository.into();

        let repo_branch =
            Bytes::from_owner([repository_bytes.clone(), branch_bytes.clone()].concat());

        for i in 0..count {
            let hash = random::<Hash>();
            let hash_bytes: Bytes = hash.into();
            hashes.push((hash_bytes.clone(), repo_branch.clone()));

            let lock_entry = LockEntry {
                description: format!("resource-{i}"),
                branch,
                hash,
                owner_id: "some-owner".to_owned(),
                repository,
                repository_branch: repo_branch.clone(),
                timestamp: "some-timestamp".to_owned(),
            };

            let item = serde_dynamo::to_item(&lock_entry).map_err(|e| {
                warn!("Error converting LockEntry to DDB item: {e} - {lock_entry:?}");
                LockError::internal("failed to convert lock entry to DDB item")
            })?;

            let put_item = Put::builder()
                .table_name(LOCKS_TABLE_NAME)
                .set_item(Some(item))
                // Since PutItem will pull up an existing PK + SK combo to begin with,
                // only checking for existence of the PK attr is necessary
                .condition_expression("attribute_not_exists(#pk)")
                .expression_attribute_names("#pk", HASH_KEY)
                .return_values_on_condition_check_failure(
                    ReturnValuesOnConditionCheckFailure::AllOld,
                )
                .build()
                .map_err(|e| {
                    warn!("Error creating Put request: {e} - {lock_entry:?}");
                    LockError::internal("failed to create put request")
                })?;

            items.push(TransactWriteItem::builder().put(put_item).build());
        }

        dynamo.transact_write_items(items).await?;

        // Try and fetch all of the items back to ensure they were written
        let keys: Vec<HashMap<String, AttributeValue>> = hashes
            .iter()
            .map(|h| {
                let mut avmap = HashMap::new();
                avmap.insert(
                    HASH_KEY.to_owned(),
                    AttributeValue::B(Blob::new(h.0.clone())),
                );
                avmap.insert(
                    REPO_BRANCH_KEY.to_owned(),
                    AttributeValue::B(Blob::new(h.1.clone())),
                );
                avmap
            })
            .collect();

        let result = dynamo
            .batch_get_item(&Arc::from(LOCKS_TABLE_NAME), keys, false)
            .await?
            .iter()
            .map(|av| {
                av.get(HASH_KEY)
                    .unwrap()
                    .as_b()
                    .unwrap()
                    .clone()
                    .into_inner()
                    .as_slice()
                    .into()
            })
            .collect::<HashSet<Hash>>();

        let expected = hashes
            .iter()
            .map(|h| h.0.clone().into())
            .collect::<HashSet<Hash>>();

        assert_eq!(result, expected);

        Ok(())
    }

    #[tokio::test]
    async fn scan_page_with_total_segments_one_returns_all_items() -> TestResult {
        use lore_aws::dynamodb::ScanConfig;

        let execution = setup_execution("test".to_string());
        let dynamo = LORE_CONTEXT
            .scope(execution.clone(), async {
                dynamodb_client(
                    "http://127.0.0.1:9090".to_string(),
                    vec![FRAGMENTS_TABLE_NAME],
                )
                .await
            })
            .await?;

        // Insert a known set of items
        let hashes: Vec<Hash> = (0..12).map(|_| random::<Hash>()).collect();
        for hash in &hashes {
            let repo_ctx: [u8; 64] = random();
            let item = HashMap::from([
                (
                    FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE.to_string(),
                    AttributeValue::B(Blob::from(hash.as_bytes())),
                ),
                (
                    FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE.to_string(),
                    AttributeValue::B(Blob::from(repo_ctx.to_vec())),
                ),
            ]);
            dynamo
                .put_item(&Arc::from(FRAGMENTS_TABLE_NAME), item)
                .await?;
        }

        // Scan with TotalSegments=1, segment=0 until exhausted
        let table: Arc<str> = Arc::from(FRAGMENTS_TABLE_NAME);
        let mut start_key = None;
        let mut found_hashes: HashSet<Vec<u8>> = HashSet::new();
        loop {
            let page = dynamo
                .scan_page(&table, start_key, &ScanConfig::segment(0, 1))
                .await?;
            for item in page.items {
                if let Some(AttributeValue::B(b)) =
                    item.get(FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE)
                {
                    found_hashes.insert(b.as_ref().to_vec());
                }
            }
            match page.last_evaluated_key {
                Some(k) => start_key = Some(k),
                None => break,
            }
        }

        for hash in &hashes {
            assert!(
                found_hashes.contains(hash.as_bytes()),
                "hash should appear in single-segment scan"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn scan_page_with_multiple_segments_covers_all_items_exactly_once() -> TestResult {
        use lore_aws::dynamodb::ScanConfig;

        let execution = setup_execution("test".to_string());
        let dynamo = LORE_CONTEXT
            .scope(execution.clone(), async {
                dynamodb_client(
                    "http://127.0.0.1:9090".to_string(),
                    vec![FRAGMENTS_TABLE_NAME],
                )
                .await
            })
            .await?;

        let total_segments = 4i32;
        let hashes: Vec<Hash> = (0..20).map(|_| random::<Hash>()).collect();
        for hash in &hashes {
            let repo_ctx: [u8; 64] = random();
            let item = HashMap::from([
                (
                    FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE.to_string(),
                    AttributeValue::B(Blob::from(hash.as_bytes())),
                ),
                (
                    FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE.to_string(),
                    AttributeValue::B(Blob::from(repo_ctx.to_vec())),
                ),
            ]);
            dynamo
                .put_item(&Arc::from(FRAGMENTS_TABLE_NAME), item)
                .await?;
        }

        let table: Arc<str> = Arc::from(FRAGMENTS_TABLE_NAME);
        let mut seen: HashMap<Vec<u8>, usize> = HashMap::new();

        for seg in 0..total_segments {
            let mut start_key = None;
            loop {
                let page = dynamo
                    .scan_page(&table, start_key, &ScanConfig::segment(seg, total_segments))
                    .await?;
                for item in page.items {
                    if let Some(AttributeValue::B(b)) =
                        item.get(FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE)
                    {
                        *seen.entry(b.as_ref().to_vec()).or_insert(0) += 1;
                    }
                }
                match page.last_evaluated_key {
                    Some(k) => start_key = Some(k),
                    None => break,
                }
            }
        }

        for hash in &hashes {
            let count = seen.get(hash.as_bytes()).copied().unwrap_or(0);
            assert_eq!(count, 1, "each item should appear in exactly one segment");
        }
        Ok(())
    }
}
