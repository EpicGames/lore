// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod batch_get_chunks {
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    use aws_sdk_dynamodb::error::SdkError;
    use aws_sdk_dynamodb::operation::batch_get_item::BatchGetItemError;
    use aws_sdk_dynamodb::operation::batch_get_item::BatchGetItemOutput;
    use aws_sdk_dynamodb::types::AttributeValue;
    use aws_sdk_dynamodb::types::KeysAndAttributes;
    use lore_aws::aws_error::AwsError;
    use lore_aws::dynamodb::BATCH_GET_ITEM_MAX_COUNT;
    use lore_aws::dynamodb::BatchGetTelemetry;
    use lore_aws::dynamodb::batch_get_chunks;
    use lore_revision::util::time::RetryPolicy;
    use lore_telemetry::LabelArray;
    use lore_telemetry::drop_record::DropRecord;
    use tokio::time::Instant;

    const TABLE: &str = "table";
    const KEY_ATTRIBUTE: &str = "id";

    const BACKOFF_START_MS: u64 = 10;
    const BACKOFF_MAX_MS: u64 = 40;
    const RESENDS: usize = 4;
    /// The waits before each resend under [`backoff`], which has no jitter.
    const WAITS_MS: [u64; RESENDS] = [10, 20, 40, 40];

    fn backoff() -> RetryPolicy {
        RetryPolicy::builder()
            .with_initial_backoff_millis(BACKOFF_START_MS)
            .with_max_backoff_millis(BACKOFF_MAX_MS)
            .with_limit(RESENDS)
            .build_with_jitter(0.0)
    }

    type Item = HashMap<String, AttributeValue>;
    type SendResult = Result<BatchGetItemOutput, SdkError<BatchGetItemError>>;

    /// A `BatchGetItem` backend in which every requested key exists. A key named in `throttles`
    /// comes back unprocessed as many times as its count before it is returned.
    #[derive(Clone, Default)]
    struct Backend {
        throttles: Arc<Mutex<HashMap<String, usize>>>,
        /// When each request arrived and how many keys it named, in the order they arrived.
        requests: Arc<Mutex<Vec<(Instant, usize)>>>,
        fail: bool,
    }

    impl Backend {
        fn throttling(throttles: &[(&str, usize)]) -> Self {
            Self {
                throttles: Arc::new(Mutex::new(
                    throttles
                        .iter()
                        .map(|(name, count)| ((*name).to_owned(), *count))
                        .collect(),
                )),
                ..Self::default()
            }
        }

        fn failing() -> Self {
            Self {
                fail: true,
                ..Self::default()
            }
        }

        fn requests(&self) -> Vec<usize> {
            self.requests
                .lock()
                .expect("requests lock")
                .iter()
                .map(|(_, keys)| *keys)
                .collect()
        }

        fn request_times(&self) -> Vec<Instant> {
            self.requests
                .lock()
                .expect("requests lock")
                .iter()
                .map(|(time, _)| *time)
                .collect()
        }

        fn send(
            &self,
        ) -> impl Fn(HashMap<String, KeysAndAttributes>) -> std::future::Ready<SendResult>
        + Clone
        + Send
        + 'static {
            let backend = self.clone();
            move |request| std::future::ready(backend.respond(request))
        }

        // The stub answers with the SDK's own result type, whatever its error's size.
        #[allow(clippy::result_large_err)]
        fn respond(&self, mut request: HashMap<String, KeysAndAttributes>) -> SendResult {
            if self.fail {
                return Err(SdkError::construction_failure("stub failure"));
            }

            let keys = request.remove(TABLE).expect("request reads the table").keys;
            assert!(request.is_empty(), "request reads only the table");
            self.requests
                .lock()
                .expect("requests lock")
                .push((Instant::now(), keys.len()));

            let mut throttles = self.throttles.lock().expect("throttles lock");
            let (unprocessed, found): (Vec<Item>, Vec<Item>) =
                keys.into_iter()
                    .partition(|key| match throttles.get_mut(&key_name(key)) {
                        Some(count) if *count > 0 => {
                            *count -= 1;
                            true
                        }
                        _ => false,
                    });

            let mut output = BatchGetItemOutput::builder().responses(TABLE, found);
            if !unprocessed.is_empty() {
                output = output.unprocessed_keys(
                    TABLE,
                    KeysAndAttributes::builder()
                        .set_keys(Some(unprocessed))
                        .build()
                        .expect("unprocessed keys"),
                );
            }
            Ok(output.build())
        }
    }

    fn key(name: &str) -> Item {
        HashMap::from([(KEY_ATTRIBUTE.to_owned(), AttributeValue::S(name.to_owned()))])
    }

    fn key_name(item: &Item) -> String {
        item[KEY_ATTRIBUTE]
            .as_s()
            .expect("key attribute is a string")
            .clone()
    }

    fn keys(count: usize) -> Vec<Item> {
        (0..count).map(|index| key(&format!("k{index}"))).collect()
    }

    fn names(items: &[Item]) -> HashSet<String> {
        items.iter().map(key_name).collect()
    }

    async fn read(
        backend: &Backend,
        keys: Vec<Item>,
    ) -> (
        Result<Vec<Item>, AwsError<SdkError<BatchGetItemError>>>,
        u64,
    ) {
        let meter = opentelemetry::global::meter("lore-aws-unit");
        let latency = meter.f64_histogram("latency").build();
        let labels = LabelArray::new();
        let mut telemetry = BatchGetTelemetry {
            latency: &latency,
            labels: &labels,
            slow_operation_duration: Duration::from_secs(1),
            resends: DropRecord::new(meter.u64_histogram("resends").build(), &[]),
        };
        let result =
            batch_get_chunks(TABLE, keys, true, backend.send(), backoff(), &mut telemetry).await;
        (result, telemetry.resends.get())
    }

    #[tokio::test(start_paused = true)]
    async fn returns_every_key_in_chunks_of_the_batch_limit() {
        let backend = Backend::default();
        let requested = keys(2 * BATCH_GET_ITEM_MAX_COUNT + 50);

        let (result, resends) = read(&backend, requested.clone()).await;

        let items = result.expect("read succeeds");
        assert_eq!(names(&items), names(&requested));
        assert_eq!(items.len(), requested.len());
        let mut requests = backend.requests();
        requests.sort_unstable();
        assert_eq!(
            requests,
            [50, BATCH_GET_ITEM_MAX_COUNT, BATCH_GET_ITEM_MAX_COUNT]
        );
        assert_eq!(resends, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn returns_keys_left_unprocessed_for_every_resend_but_the_last() {
        let backend = Backend::throttling(&[("k3", RESENDS)]);
        let requested = keys(10);

        let (result, resends) = read(&backend, requested.clone()).await;

        let items = result.expect("read succeeds once the key is processed");
        assert_eq!(names(&items), names(&requested));
        assert_eq!(resends, RESENDS as u64);
        assert_eq!(backend.requests(), [vec![10], vec![1; RESENDS]].concat());
    }

    #[tokio::test(start_paused = true)]
    async fn fails_once_a_chunk_has_spent_its_resends() {
        let backend = Backend::throttling(&[("k3", RESENDS + 1), ("k4", RESENDS + 1)]);

        let (result, resends) = read(&backend, keys(10)).await;

        let error = result.expect_err("a partial result is never returned");
        assert!(
            matches!(error, AwsError::UnprocessedKeys { remaining: 2 }),
            "expected 2 unprocessed keys, got {error:?}"
        );
        assert_eq!(resends, RESENDS as u64);
    }

    #[tokio::test(start_paused = true)]
    async fn chunks_spend_resends_independently() {
        let first_chunk_key = "k0";
        let last_chunk_key = format!("k{}", 2 * BATCH_GET_ITEM_MAX_COUNT - 1);
        let backend = Backend::throttling(&[
            (first_chunk_key, RESENDS),
            (last_chunk_key.as_str(), RESENDS),
        ]);
        let requested = keys(2 * BATCH_GET_ITEM_MAX_COUNT);

        let (result, resends) = read(&backend, requested.clone()).await;

        let items = result.expect("each chunk stays within its own resends");
        assert_eq!(names(&items), names(&requested));
        assert_eq!(resends, 2 * RESENDS as u64);
    }

    #[tokio::test(start_paused = true)]
    async fn resent_keys_keep_their_chunk_backoff_when_some_keys_are_processed() {
        let backend = Backend::throttling(&[("k1", 1), ("k2", RESENDS + 1)]);

        let (result, _) = read(&backend, keys(10)).await;

        let error = result.expect_err("processing some keys leaves the resends spent so far spent");
        assert!(
            matches!(error, AwsError::UnprocessedKeys { remaining: 1 }),
            "expected 1 unprocessed keys, got {error:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn waits_with_doubling_capped_backoff_before_each_resend() {
        let backend = Backend::throttling(&[("k0", RESENDS)]);

        let (result, _) = read(&backend, keys(1)).await;

        result.expect("read succeeds once the key is processed");
        let times = backend.request_times();
        let waits: Vec<Duration> = times
            .windows(2)
            .map(|pair| pair[1].duration_since(pair[0]))
            .collect();
        assert_eq!(waits, WAITS_MS.map(Duration::from_millis));
    }

    #[tokio::test(start_paused = true)]
    async fn fails_with_the_sdk_error_of_a_failed_request() {
        let backend = Backend::failing();

        let (result, _) = read(&backend, keys(10)).await;

        let error = result.expect_err("a failed request fails the read");
        let AwsError::AwsSdkError(sdk_error) = error else {
            panic!("expected an SDK error, got {error:?}");
        };
        assert!(
            matches!(*sdk_error, SdkError::ConstructionFailure(_)),
            "expected the stub's construction failure, got {sdk_error:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn returns_nothing_for_no_keys() {
        let backend = Backend::default();

        let (result, _) = read(&backend, Vec::new()).await;

        assert_eq!(result.expect("read succeeds"), Vec::<Item>::new());
        assert!(backend.requests().is_empty());
    }
}
