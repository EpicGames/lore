// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Requests larger than one call may carry, sent as as many calls as they need.

use std::collections::HashSet;
use std::sync::Arc;

use lore_aws::aws::item::AttributeValue;
use lore_aws::aws::item::ByteStr;
use lore_aws::aws::item::Item;
use lore_aws::dynamodb::BATCH_GET_ITEM_MAX_COUNT;
use lore_aws::dynamodb::TRANSACT_WRITE_ITEMS_CHUNKS;
use lore_aws::dynamodb::TransactPut;
use lore_aws::dynamodb::TransactWriteItem;
use lore_aws::dynamodb::chunk_token;
use lore_base::runtime::LORE_CONTEXT;
use serde_json::Value;
use uuid::Uuid;

use super::dynamodb_for;
use crate::aws::test_server::RecordedRequest;
use crate::aws::test_server::StubResponse;
use crate::aws::test_server::StubServer;

/// A row numbered `n`, so a request body says which rows it carried and in what order.
fn numbered(n: usize) -> Item {
    let mut item = Item::new();
    item.insert(ByteStr::from_static("n"), AttributeValue::number(n));
    item
}

/// The body of `request`, parsed.
fn body(request: &RecordedRequest) -> Value {
    serde_json::from_slice(&request.body).expect("a request body is JSON")
}

/// The number of a row as a request body writes it.
fn number_of(row: &Value) -> u64 {
    row["n"]["N"]
        .as_str()
        .and_then(|number| number.parse().ok())
        .expect("every row carries its number")
}

/// A transaction past the per-call limit is sent as consecutive chunks, each in order and each
/// with a token of its own.
#[tokio::test]
async fn a_transaction_past_the_limit_is_sent_in_ordered_chunks() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let total = TRANSACT_WRITE_ITEMS_CHUNKS + TRANSACT_WRITE_ITEMS_CHUNKS / 2;
            let server = StubServer::start(vec![StubResponse::ok("{}")]).await;
            let items = (0..total)
                .map(|n| TransactWriteItem::Put(TransactPut::new(Arc::from("t"), numbered(n))))
                .collect();

            let outputs = dynamodb_for(&server)
                .transact_write_items(&Arc::from("t"), items, Uuid::new_v4())
                .await
                .expect("every chunk is applied");

            assert_eq!(outputs.len(), 2, "one output per chunk");

            let mut chunks: Vec<Vec<u64>> = Vec::new();
            let mut tokens = HashSet::new();
            for request in server.requests() {
                let sent = body(&request);
                let token = sent["ClientRequestToken"]
                    .as_str()
                    .expect("every chunk carries a token");
                assert_eq!(token.len(), 36, "a token is a hyphenated UUID");
                tokens.insert(token.to_owned());

                chunks.push(
                    sent["TransactItems"]
                        .as_array()
                        .expect("a transaction lists its items")
                        .iter()
                        .map(|item| number_of(&item["Put"]["Item"]))
                        .collect(),
                );
            }
            chunks.sort();

            assert_eq!(tokens.len(), 2, "each chunk has a token of its own");
            assert_eq!(
                chunks,
                vec![
                    (0..TRANSACT_WRITE_ITEMS_CHUNKS as u64).collect::<Vec<_>>(),
                    (TRANSACT_WRITE_ITEMS_CHUNKS as u64..total as u64).collect::<Vec<_>>(),
                ],
                "the chunks are consecutive runs of the items, in order"
            );
        })
        .await;
}

/// A transaction sent again under its token sends every chunk under the token it first carried, the
/// first chunk under the caller's own, so the service can answer a chunk it already applied.
#[tokio::test]
async fn a_transaction_sent_again_under_its_token_reuses_every_chunk_token() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let total = TRANSACT_WRITE_ITEMS_CHUNKS + 1;
            let server = StubServer::start(vec![StubResponse::ok("{}")]).await;
            let dynamodb = dynamodb_for(&server);
            let items: Vec<TransactWriteItem> = (0..total)
                .map(|n| TransactWriteItem::Put(TransactPut::new(Arc::from("t"), numbered(n))))
                .collect();
            let token = Uuid::new_v4();

            for _ in 0..2 {
                dynamodb
                    .transact_write_items(&Arc::from("t"), items.clone(), token)
                    .await
                    .expect("every chunk is applied");
            }

            let token_of_chunk = |starting_row: u64| {
                let mut tokens = server
                    .requests()
                    .iter()
                    .map(body)
                    .filter(|sent| {
                        number_of(&sent["TransactItems"][0]["Put"]["Item"]) == starting_row
                    })
                    .map(|sent| sent["ClientRequestToken"].as_str().map(str::to_owned))
                    .collect::<Vec<_>>();
                tokens.dedup();
                tokens
            };

            assert_eq!(token_of_chunk(0), vec![Some(token.to_string())]);
            let second = token_of_chunk(TRANSACT_WRITE_ITEMS_CHUNKS as u64);
            assert_eq!(second.len(), 1, "both sends of a chunk carry one token");
            assert_ne!(second[0], Some(token.to_string()));
        })
        .await;
}

/// Chunk tokens are stable for an index, distinct across indices, and stay version 4 UUIDs.
#[test]
fn chunk_tokens_are_stable_distinct_and_well_formed() {
    let token = Uuid::new_v4();

    assert_eq!(chunk_token(token, 0), token);

    let tokens: HashSet<Uuid> = (0..1_000).map(|index| chunk_token(token, index)).collect();
    assert_eq!(tokens.len(), 1_000);

    for index in [1, 255, 256, 999] {
        let derived = chunk_token(token, index);
        assert_eq!(derived, chunk_token(token, index));
        assert_eq!(derived.get_version_num(), 4);
        assert_eq!(derived.get_variant(), uuid::Variant::RFC4122);
    }
}

/// A batch read past the per-call limit is sent as several calls naming every key once, and the
/// rows every call returns are gathered into one answer.
#[tokio::test]
async fn a_batch_read_past_the_limit_is_split_and_gathered() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let total = BATCH_GET_ITEM_MAX_COUNT + BATCH_GET_ITEM_MAX_COUNT / 2;
            let server = StubServer::start(vec![
                StubResponse::ok(r#"{"Responses":{"t":[{"n":{"N":"1"}}]}}"#),
                StubResponse::ok(r#"{"Responses":{"t":[{"n":{"N":"2"}}]}}"#),
            ])
            .await;

            let rows = dynamodb_for(&server)
                .batch_get_item(&Arc::from("t"), (0..total).map(numbered).collect(), true)
                .await
                .expect("every call answers");

            assert_eq!(rows.len(), 2, "every call's rows are in the answer");

            let mut sizes = Vec::new();
            let mut named = HashSet::new();
            for request in server.requests() {
                let keys = body(&request)["RequestItems"]["t"]["Keys"]
                    .as_array()
                    .expect("a batch read lists its keys")
                    .iter()
                    .map(number_of)
                    .collect::<Vec<_>>();
                sizes.push(keys.len());
                named.extend(keys);
            }
            sizes.sort_unstable();

            assert_eq!(
                sizes,
                vec![total - BATCH_GET_ITEM_MAX_COUNT, BATCH_GET_ITEM_MAX_COUNT]
            );
            assert_eq!(named, (0..total as u64).collect::<HashSet<_>>());
        })
        .await;
}
