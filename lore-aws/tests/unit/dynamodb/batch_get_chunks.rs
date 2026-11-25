// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Reading a batch in chunks, each resending the keys `DynamoDB` leaves unprocessed after a wait of
//! its own.

use std::sync::Arc;
use std::time::Duration;

use lore_aws::aws::dynamodb::BatchGetItemOutput;
use lore_aws::aws::dynamodb::DynamoResult;
use lore_aws::aws::error::AwsCallError;
use lore_aws::aws::item::AttributeValue;
use lore_aws::aws::item::ByteStr;
use lore_aws::aws::item::Item;
use lore_aws::aws_error::AwsError;
use lore_aws::dynamodb::BATCH_GET_ITEM_MAX_COUNT;
use lore_aws::dynamodb::BatchGetRequest;
use lore_aws::dynamodb::BatchRead;
use lore_aws::dynamodb::batch_get_chunks;
use lore_revision::util::time::RetryPolicy;
use parking_lot::Mutex;
use tokio::time::Instant;

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

/// A `BatchGetItem` backend in which every requested key exists. A key named in `throttles` comes
/// back unprocessed as many times as its count before it is returned.
#[derive(Clone, Default)]
struct Backend {
    throttles: Arc<Mutex<Vec<(Item, usize)>>>,
    /// When each request arrived and how many keys it named, in the order they arrived.
    requests: Arc<Mutex<Vec<(Instant, usize)>>>,
    fail: bool,
}

impl Backend {
    fn throttling(throttles: &[(usize, usize)]) -> Self {
        Self {
            throttles: Arc::new(Mutex::new(
                throttles
                    .iter()
                    .map(|(index, count)| (key(*index), *count))
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
        self.requests.lock().iter().map(|(_, keys)| *keys).collect()
    }

    fn request_times(&self) -> Vec<Instant> {
        self.requests.lock().iter().map(|(time, _)| *time).collect()
    }

    /// Take one of `key`'s throttles, if it has one left.
    fn throttled(&self, key: &Item) -> bool {
        let mut throttles = self.throttles.lock();
        match throttles.iter_mut().find(|(throttled, _)| throttled == key) {
            Some((_, count)) if *count > 0 => {
                *count -= 1;
                true
            }
            _ => false,
        }
    }
}

impl BatchGetRequest for Backend {
    async fn send(&self, keys: &[Item]) -> DynamoResult<BatchGetItemOutput> {
        if self.fail {
            return Err(AwsCallError::Timeout);
        }

        self.requests.lock().push((Instant::now(), keys.len()));

        let (unprocessed_keys, items) = keys.iter().cloned().partition(|key| self.throttled(key));
        Ok(BatchGetItemOutput {
            items,
            unprocessed_keys,
            request_id: None,
        })
    }
}

fn key(index: usize) -> Item {
    let mut key = Item::new();
    key.insert(
        ByteStr::from_static("id"),
        AttributeValue::blob(index.to_be_bytes()),
    );
    key
}

fn keys(count: usize) -> Vec<Item> {
    (0..count).map(key).collect()
}

/// Whether `items` holds each of `requested` exactly once, in any order.
fn holds_exactly(items: &[Item], requested: &[Item]) -> bool {
    items.len() == requested.len() && requested.iter().all(|key| items.contains(key))
}

async fn read(backend: &Backend, keys: Vec<Item>) -> BatchRead {
    batch_get_chunks(backend.clone(), keys, backoff()).await
}

#[tokio::test(start_paused = true)]
async fn returns_every_key_in_chunks_of_the_batch_limit() {
    let backend = Backend::default();
    let requested = keys(2 * BATCH_GET_ITEM_MAX_COUNT + 50);

    let read = read(&backend, requested.clone()).await;

    let items = read.items.expect("read succeeds");
    assert!(holds_exactly(&items, &requested));
    let mut requests = backend.requests();
    requests.sort_unstable();
    assert_eq!(
        requests,
        [50, BATCH_GET_ITEM_MAX_COUNT, BATCH_GET_ITEM_MAX_COUNT]
    );
    assert_eq!(read.resends, 0);
}

#[tokio::test(start_paused = true)]
async fn returns_keys_left_unprocessed_for_every_resend_but_the_last() {
    let backend = Backend::throttling(&[(3, RESENDS)]);
    let requested = keys(10);

    let read = read(&backend, requested.clone()).await;

    let items = read.items.expect("read succeeds once the key is processed");
    assert!(holds_exactly(&items, &requested));
    assert_eq!(read.resends, RESENDS as u64);
    assert_eq!(backend.requests(), [vec![10], vec![1; RESENDS]].concat());
}

#[tokio::test(start_paused = true)]
async fn fails_once_a_chunk_has_spent_its_resends() {
    let backend = Backend::throttling(&[(3, RESENDS + 1), (4, RESENDS + 1)]);

    let read = read(&backend, keys(10)).await;

    let error = read.items.expect_err("a partial result is never returned");
    assert!(
        matches!(error, AwsError::UnprocessedKeys { remaining: 2 }),
        "expected 2 unprocessed keys, got {error:?}"
    );
    assert_eq!(read.resends, RESENDS as u64);
}

#[tokio::test(start_paused = true)]
async fn chunks_spend_resends_independently() {
    let backend = Backend::throttling(&[(0, RESENDS), (2 * BATCH_GET_ITEM_MAX_COUNT - 1, RESENDS)]);
    let requested = keys(2 * BATCH_GET_ITEM_MAX_COUNT);

    let read = read(&backend, requested.clone()).await;

    let items = read.items.expect("each chunk stays within its own resends");
    assert!(holds_exactly(&items, &requested));
    assert_eq!(read.resends, 2 * RESENDS as u64);
}

#[tokio::test(start_paused = true)]
async fn resent_keys_keep_their_chunk_backoff_when_some_keys_are_processed() {
    let backend = Backend::throttling(&[(1, 1), (2, RESENDS + 1)]);

    let read = read(&backend, keys(10)).await;

    let error = read
        .items
        .expect_err("processing some keys leaves the resends spent so far spent");
    assert!(
        matches!(error, AwsError::UnprocessedKeys { remaining: 1 }),
        "expected 1 unprocessed key, got {error:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn waits_with_doubling_capped_backoff_before_each_resend() {
    let backend = Backend::throttling(&[(0, RESENDS)]);

    let read = read(&backend, keys(1)).await;

    read.items.expect("read succeeds once the key is processed");
    let times = backend.request_times();
    let waits: Vec<Duration> = times
        .windows(2)
        .map(|pair| pair[1].duration_since(pair[0]))
        .collect();
    assert_eq!(waits, WAITS_MS.map(Duration::from_millis));
}

#[tokio::test(start_paused = true)]
async fn fails_with_the_error_of_a_failed_request() {
    let backend = Backend::failing();

    let read = read(&backend, keys(10)).await;

    let error = read.items.expect_err("a failed request fails the read");
    assert!(
        matches!(error, AwsError::Aws(AwsCallError::Timeout)),
        "expected the stub's timeout, got {error:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn returns_nothing_for_no_keys() {
    let backend = Backend::default();

    let read = read(&backend, Vec::new()).await;

    assert!(read.items.expect("read succeeds").is_empty());
    assert!(backend.requests().is_empty());
}
