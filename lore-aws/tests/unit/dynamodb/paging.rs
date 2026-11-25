// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Asking again for what one response did not deliver: keys the service left unprocessed, and pages
//! left behind a cursor.

use std::sync::Arc;
use std::time::Duration;

use lore_aws::aws::item::AttributeValue;
use lore_aws::aws::item::ByteStr;
use lore_aws::aws::item::Item;
use lore_aws::aws_error::AwsError;
use lore_aws::dynamodb::*;
use lore_revision::util::time::RetryPolicy;

use super::dynamodb_for as store_for;
use super::dynamodb_resending_with;
use crate::aws::test_server::StubResponse;
use crate::aws::test_server::StubServer;

fn key() -> Item {
    key_of(1)
}

fn key_of(hash: u8) -> Item {
    let mut key = Item::new();
    key.insert(ByteStr::from_static("hash"), AttributeValue::blob([hash]));
    key
}

/// A table that never drains is asked again until the resends are spent, then reported as keys
/// left unprocessed rather than asked again for ever.
///
/// The stub leaves the same key outstanding on every call, so nothing but the resend limit ends the
/// loop.
#[tokio::test(start_paused = true)]
async fn a_batch_that_never_drains_gives_up_after_its_resends() {
    const RESENDS: usize = 3;

    let server = StubServer::start(vec![StubResponse::ok(
        r#"{"Responses":{"t":[]},
            "UnprocessedKeys":{"t":{"Keys":[{"hash":{"B":"AQ=="}}],"ConsistentRead":true}}}"#,
    )])
    .await;
    let resends = RetryPolicy::builder()
        .with_initial_backoff_millis(50)
        .with_max_backoff_millis(1_000)
        .with_limit(RESENDS)
        .build();

    let error = dynamodb_resending_with(&server, resends)
        .batch_get_item(&Arc::from("t"), vec![key()], true)
        .await
        .expect_err("a read that never completed is not a result");

    assert!(
        matches!(error, AwsError::UnprocessedKeys { remaining: 1 }),
        "unexpected failure: {error:?}"
    );
    assert_eq!(
        server.request_count(),
        RESENDS + 1,
        "the first ask and every resend reach the service"
    );
}

/// A batch that drains on the second ask returns everything, across both replies.
#[tokio::test(start_paused = true)]
async fn a_batch_that_drains_returns_every_item() {
    let server = StubServer::start(vec![
        StubResponse::ok(
            r#"{"Responses":{"t":[{"hash":{"B":"AQ=="}}]},
                "UnprocessedKeys":{"t":{"Keys":[{"hash":{"B":"Ag=="}}],"ConsistentRead":true}}}"#,
        ),
        StubResponse::ok(r#"{"Responses":{"t":[{"hash":{"B":"Ag=="}}]}}"#),
    ])
    .await;

    let items = store_for(&server)
        .batch_get_item(&Arc::from("t"), vec![key_of(1), key_of(2)], true)
        .await
        .expect("the second ask completes the batch");

    assert_eq!(items.len(), 2, "both replies contribute their items");
    assert_eq!(server.request_count(), 2);
}

/// A query whose cursor names the page just asked for.
struct EverythingQuery;

impl DynamoDbQuery for EverythingQuery {
    fn key_condition_expression(&self) -> &str {
        "#pk = :pk"
    }

    fn expression_attribute_names(&self) -> ExprNames {
        ExprNames::from_slice(&[("#pk", "hash")])
    }

    fn expression_attribute_values(&self) -> ExprValues {
        smallvec::smallvec![(":pk", AttributeValue::blob([1u8]))]
    }
}

/// A cursor that has stopped moving is not sent again.
///
/// The loop asks for the next page because the last one named it. A response naming the page it
/// was just asked for names no next page, so repeating the request is a hot loop against an
/// endpoint that has already answered it. Asking again cannot help, so the failure is terminal.
#[tokio::test]
async fn a_query_whose_cursor_stops_advancing_is_abandoned() {
    const PAGE: &str = r#"{"Items":[{"hash":{"B":"AQ=="}}],"Count":1,"ScannedCount":1,
                           "LastEvaluatedKey":{"hash":{"B":"AQ=="}}}"#;

    let server = StubServer::start(vec![StubResponse::ok(PAGE), StubResponse::ok(PAGE)]).await;

    // Bounded, so a cursor that is followed indefinitely fails the test rather than hanging it.
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        store_for(&server).query_paginated(&Arc::from("t"), EverythingQuery),
    )
    .await
    .expect("a query whose cursor stops moving must not be repeated for ever")
    .expect_err("a cursor that does not move is not another page");

    assert!(
        matches!(error, AwsError::StalledCursor),
        "unexpected failure: {error:?}"
    );
    assert_eq!(
        server.request_count(),
        2,
        "the second answer settles it, so there is no third request"
    );
}
