// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use ::http::StatusCode;
use bytes::Bytes;
use http::HeaderValue;
use lore_aws::aws::ServiceProtocol;
use lore_aws::aws::dynamodb::*;
use lore_aws::aws::error::AwsCallError;
use lore_aws::aws::error::DynamoErrorCode;
use lore_aws::aws::error::ErrorPayload;
use lore_aws::aws::error::Retryable;
use lore_aws::aws::error::ServiceErrorCode;
use lore_aws::aws::error::TransportError;
use lore_aws::aws::http::HttpResponse;
use lore_aws::aws::item::AttributeValue;
use lore_aws::aws::item::ByteStr;
use lore_aws::aws::item::Item;
use lore_aws::aws::json::BodyWriter;
use lore_aws::dynamodb::TRANSACT_WRITE_ITEMS_CHUNKS;
use serde::de::DeserializeSeed;

fn body_of<F: FnOnce(&mut BodyWriter)>(write: F) -> String {
    let mut body = BodyWriter::new();
    write(&mut body);
    String::from_utf8(body.split().to_vec()).unwrap()
}

fn text(body: Bytes) -> String {
    String::from_utf8(body.to_vec()).unwrap()
}

fn key_item() -> Item {
    let mut item = Item::new();
    item.insert(ByteStr::from_static("hash"), AttributeValue::blob([1u8, 2]));
    item
}

fn other_key_item() -> Item {
    let mut item = Item::new();
    item.insert(ByteStr::from_static("hash"), AttributeValue::blob([3u8]));
    item
}

/// Each target is a literal, so the API version and the operation's name sit in one string per
/// operation, where a typo would silently send the wrong operation.
#[test]
fn every_target_names_the_api_version_and_its_operation() {
    for (operation, name) in [
        (Operation::BatchGetItem, "BatchGetItem"),
        (Operation::DeleteItem, "DeleteItem"),
        (Operation::DescribeTable, "DescribeTable"),
        (Operation::GetItem, "GetItem"),
        (Operation::PutItem, "PutItem"),
        (Operation::Query, "Query"),
        (Operation::Scan, "Scan"),
        (Operation::TransactWriteItems, "TransactWriteItems"),
    ] {
        assert_eq!(
            operation.target().to_str().unwrap(),
            format!("DynamoDB_20120810.{name}"),
            "the target for {name} disagrees with its name"
        );
    }
}

#[test]
fn absent_expression_maps_write_no_field() {
    // DynamoDB rejects an empty ExpressionAttributeNames object, so absence has to mean absence.
    assert_eq!(body_of(|body| write_names(body, &[])), "");
    assert_eq!(body_of(|body| write_values(body, &[])), "");
}

#[test]
fn expression_names_and_values_write_as_objects() {
    assert_eq!(
        body_of(|body| write_names(body, &[("#pk", "hash"), ("#sk", "context")])),
        r##","ExpressionAttributeNames":{"#pk":"hash","#sk":"context"}"##
    );
    assert_eq!(
        body_of(|body| write_values(
            body,
            &[
                (":hash", AttributeValue::blob([0xab])),
                (":n", AttributeValue::number(3u32))
            ]
        )),
        r#","ExpressionAttributeValues":{":hash":{"B":"qw=="},":n":{"N":"3"}}"#
    );
}

#[test]
fn a_condition_asks_for_the_losing_row_only_when_told_to() {
    let condition = Condition {
        expression: "attribute_not_exists(#pk)",
        names: &[("#pk", "hash")],
        values: &[],
        return_values_on_failure: true,
    };

    let written = body_of(|body| write_condition(body, Some(&condition)));

    assert!(written.contains(r#""ConditionExpression":"attribute_not_exists(#pk)""#));
    assert!(written.contains(r#""ReturnValuesOnConditionCheckFailure":"ALL_OLD""#));

    let quiet = Condition {
        return_values_on_failure: false,
        ..condition
    };
    assert!(
        !body_of(|body| write_condition(body, Some(&quiet)))
            .contains("ReturnValuesOnConditionCheckFailure")
    );
    assert_eq!(body_of(|body| write_condition(body, None)), "");
}

#[test]
fn parses_a_get_item_response() {
    let body = Bytes::from_static(br#"{"Item":{"hash":{"B":"AQI="},"state":{"N":"1"}}}"#);

    let output: GetItemOutput = parse(body, |deserializer, arena| {
        GetItemSeed { arena }.deserialize(deserializer)
    })
    .unwrap();

    let item = output.item.expect("an item");
    assert_eq!(item.get("hash"), Some(&AttributeValue::blob([1u8, 2])));
    assert_eq!(item.get("state"), Some(&AttributeValue::number(1u32)));
}

/// A miss is an empty object, not an error and not an empty item.
#[test]
fn a_get_item_miss_reads_as_no_item() {
    let output: GetItemOutput = parse(Bytes::from_static(b"{}"), |deserializer, arena| {
        GetItemSeed { arena }.deserialize(deserializer)
    })
    .unwrap();

    assert!(output.item.is_none());
}

/// A successful read always answers with a JSON object. An empty body is one that did not arrive,
/// and reading it as a miss would report a stored item as absent.
#[test]
fn an_empty_read_body_is_a_protocol_failure() {
    let present = parse(Bytes::new(), |deserializer, arena| {
        ItemPresenceSeed { arena }.deserialize(deserializer)
    });
    assert!(
        matches!(
            present,
            Err(AwsCallError::Transport(TransportError::Protocol(_)))
        ),
        "{present:?}"
    );

    let output = parse(Bytes::new(), |deserializer, arena| {
        GetItemSeed { arena }.deserialize(deserializer)
    });
    assert!(matches!(
        output,
        Err(AwsCallError::Transport(TransportError::Protocol(_)))
    ));
}

#[test]
fn parses_a_query_response_with_its_counts_and_cursor() {
    let body = Bytes::from_static(
        br#"{"Items":[{"a":{"N":"1"}}],"Count":1,"ScannedCount":7,
             "LastEvaluatedKey":{"a":{"N":"1"}}}"#,
    );

    let output: QueryOutput = parse(body, |deserializer, arena| {
        QuerySeed { arena, capacity: 1 }.deserialize(deserializer)
    })
    .unwrap();

    assert_eq!(output.count(), 1);
    assert_eq!(output.scanned_count(), 7);
    assert_eq!(output.items.as_ref().map(Vec::len), Some(1));
    assert!(output.last_evaluated_key.is_some());
}

/// The end of a scan is an absent `LastEvaluatedKey`, which is what stops the migrator's loop.
#[test]
fn a_scan_without_a_cursor_has_reached_the_end() {
    let output: ScanOutput = parse(
        Bytes::from_static(br#"{"Items":[]}"#),
        |deserializer, arena| ScanSeed { arena }.deserialize(deserializer),
    )
    .unwrap();

    assert!(output.items.is_empty());
    assert!(output.last_evaluated_key.is_none());
}

#[test]
fn parses_a_batch_get_response_for_its_own_table() {
    let body = Bytes::from_static(
        br#"{"Responses":{"fragments":[{"a":{"N":"1"}}],"other":[{"b":{"N":"2"}}]},
             "UnprocessedKeys":{"fragments":{"Keys":[{"a":{"N":"3"}}],"ConsistentRead":true}}}"#,
    );

    let output: BatchGetItemOutput = parse(body, |deserializer, arena| {
        BatchGetItemSeed {
            arena,
            table: "fragments",
            capacity: 2,
        }
        .deserialize(deserializer)
    })
    .unwrap();

    assert_eq!(output.items.len(), 1, "another table's rows are not ours");
    assert_eq!(
        output.items[0].get("a"),
        Some(&AttributeValue::number(1u32))
    );
    assert_eq!(output.unprocessed_keys.len(), 1);
    assert_eq!(
        output.unprocessed_keys[0].get("a"),
        Some(&AttributeValue::number(3u32))
    );
}

/// Every key comes back as a row, as an unprocessed key, or not at all, so an answer naming more
/// than were asked for is not this request's answer and is refused rather than decoded.
#[test]
fn a_batch_answer_naming_more_than_its_keys_is_refused() {
    let parse_batch = |body: &'static [u8]| {
        parse(Bytes::from_static(body), |deserializer, arena| {
            BatchGetItemSeed {
                arena,
                table: "t",
                capacity: 1,
            }
            .deserialize(deserializer)
        })
    };

    assert!(parse_batch(br#"{"Responses":{"t":[{},{}]}}"#).is_err());
    assert!(parse_batch(br#"{"UnprocessedKeys":{"t":{"Keys":[{},{}]}}}"#).is_err());
    assert!(
        parse_batch(br#"{"Responses":{"t":[{}]},"UnprocessedKeys":{"t":{"Keys":[{}]}}}"#).is_err()
    );
    assert!(parse_batch(br#"{"Responses":{"t":[{}]}}"#).is_ok());
}

/// A transaction cancels with one reason per item, and holds at most a hundred items.
#[test]
fn more_cancellation_reasons_than_a_transaction_holds_are_refused() {
    let reasons = |count: usize| {
        let mut body = String::from(
            r#"{"__type":"com.amazonaws.dynamodb.v20120810#TransactionCanceledException","CancellationReasons":["#,
        );
        for index in 0..count {
            if index > 0 {
                body.push(',');
            }
            body.push_str(r#"{"Code":"None"}"#);
        }
        body.push_str("]}");
        Bytes::from(body)
    };

    assert!(parse_error_body(reasons(TRANSACT_WRITE_ITEMS_CHUNKS)).is_ok());
    assert!(parse_error_body(reasons(TRANSACT_WRITE_ITEMS_CHUNKS + 1)).is_err());
}

/// A fully served batch has no `UnprocessedKeys` field at all.
#[test]
fn a_fully_served_batch_has_nothing_unprocessed() {
    let body = Bytes::from_static(br#"{"Responses":{"t":[]}}"#);

    let output: BatchGetItemOutput = parse(body, |deserializer, arena| {
        BatchGetItemSeed {
            arena,
            table: "t",
            capacity: 0,
        }
        .deserialize(deserializer)
    })
    .unwrap();

    assert!(output.unprocessed_keys.is_empty());
}

#[test]
fn reads_the_exception_name_out_of_the_type_field() {
    let body = Bytes::from_static(
        br#"{"__type":"com.amazonaws.dynamodb.v20120810#ConditionalCheckFailedException",
             "message":"The conditional request failed"}"#,
    );

    let parsed = parse_error_body(body).unwrap();

    assert_eq!(
        parsed.code().map(error_code_name),
        Some("ConditionalCheckFailedException")
    );
    assert_eq!(
        parsed.message.as_deref(),
        Some("The conditional request failed")
    );
}

/// A conditional write that asked for the old row gets it on the error, and that row is how
/// `publish_state` tells "already stored" from "an obliteration holds this".
#[test]
fn reads_the_item_off_a_conditional_check_failure() {
    let body = Bytes::from_static(
        br#"{"__type":"com.amazonaws.dynamodb.v20120810#ConditionalCheckFailedException",
             "Item":{"hash":{"B":"AQ=="},"state":{"N":"2"}}}"#,
    );

    let parsed = parse_error_body(body).unwrap();

    let ErrorPayload::Item(item) = parsed.payload else {
        panic!("expected the losing row");
    };
    assert_eq!(item.get("state"), Some(&AttributeValue::number(2u32)));
}

#[test]
fn reads_cancellation_reasons_in_request_order() {
    let body = Bytes::from_static(
        br#"{"__type":"com.amazonaws.dynamodb.v20120810#TransactionCanceledException",
             "Message":"Transaction cancelled",
             "CancellationReasons":[
               {"Code":"None"},
               {"Code":"ConditionalCheckFailed","Message":"failed",
                "Item":{"ownerId":{"S":"someone"}}}
             ]}"#,
    );

    let parsed = parse_error_body(body).unwrap();

    let ErrorPayload::CancellationReasons(reasons) = parsed.payload else {
        panic!("expected cancellation reasons");
    };
    assert_eq!(reasons.len(), 2);
    assert_eq!(reasons[0].code(), Some("None"), "order is request order");
    assert_eq!(reasons[1].code(), Some("ConditionalCheckFailed"));
    assert_eq!(
        reasons[1].item().and_then(|item| item.get("ownerId")),
        Some(&AttributeValue::string("someone"))
    );
}

/// JSON may escape any character, and a message quoting the expression it rejects carries quotes.
/// An escaped string has to read like a plain one, not fail the whole error body.
#[test]
fn escaped_error_text_reads_as_its_unescaped_text() {
    let parsed = parse_error_body(Bytes::from_static(
        br##"{"__type":"com.amazonaws.dynamodb.v20120810#ValidationException",
              "message":"Invalid token: \"#v\""}"##,
    ))
    .unwrap();

    assert_eq!(
        parsed.code().map(error_code_name),
        Some("ValidationException")
    );
    assert_eq!(parsed.message.as_deref(), Some(r##"Invalid token: "#v""##));
}

#[test]
fn an_escaped_cancellation_reason_reads_as_its_unescaped_text() {
    let parsed = parse_error_body(Bytes::from_static(
        br#"{"__type":"com.amazonaws.dynamodb.v20120810#TransactionCanceledException",
             "CancellationReasons":[
               {"Code":"ConditionalCheckFailed",
                "Message":"The conditional request failed for \"lock\""}
             ]}"#,
    ))
    .unwrap();

    let ErrorPayload::CancellationReasons(reasons) = parsed.payload else {
        panic!("expected cancellation reasons");
    };
    assert_eq!(reasons[0].code(), Some("ConditionalCheckFailed"));
    assert_eq!(
        reasons[0].message(),
        Some(r#"The conditional request failed for "lock""#)
    );
}

#[test]
fn a_message_under_either_spelling_is_read() {
    for body in [
        &br#"{"message":"lower"}"#[..],
        &br#"{"Message":"upper"}"#[..],
    ] {
        let parsed = parse_error_body(Bytes::copy_from_slice(body)).unwrap();
        assert!(parsed.message.is_some(), "{body:?}");
    }
}

/// An error body that is not JSON at all — a proxy's HTML page, an empty 503 — must still yield an
/// error, taken from the status.
#[test]
fn an_unparsable_error_body_falls_back_to_the_status() {
    let response = HttpResponse {
        status: StatusCode::SERVICE_UNAVAILABLE,
        headers: http::HeaderMap::new(),
        body: Bytes::from_static(b"<html>503</html>"),
    };

    let error = DynamoProtocol::parse_error(&response);

    assert_eq!(error.code, DynamoErrorCode::InternalServerError);
    assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
}

/// The protocol's other body spelling. An intermediary that answers with `code` instead of
/// `__type` would otherwise degrade to an unknown code, and an unknown code is never retryable —
/// so a throttle arriving through a proxy would surface as a hard failure.
#[test]
fn the_code_field_names_the_exception_when_type_is_absent() {
    let response = HttpResponse {
        status: StatusCode::BAD_REQUEST,
        headers: http::HeaderMap::new(),
        body: Bytes::from_static(br#"{"code":"ProvisionedThroughputExceededException"}"#),
    };

    let error = DynamoProtocol::parse_error(&response);

    assert_eq!(
        error.code,
        DynamoErrorCode::ProvisionedThroughputExceededException
    );
    assert_eq!(error.code.retryable(), Retryable::Yes);
}

/// `__type` is what `DynamoDB` itself sends, so it decides — whichever order the two arrive in.
#[test]
fn the_type_field_wins_over_the_code_field() {
    for body in [
        &br#"{"__type":"x#ValidationException","code":"ThrottlingException"}"#[..],
        &br#"{"code":"ThrottlingException","__type":"x#ValidationException"}"#[..],
    ] {
        let response = HttpResponse {
            status: StatusCode::BAD_REQUEST,
            headers: http::HeaderMap::new(),
            body: Bytes::copy_from_slice(body),
        };

        let error = DynamoProtocol::parse_error(&response);

        assert_eq!(error.code, DynamoErrorCode::ValidationException);
    }
}

/// The AWS JSON protocol also allows the code in a header, which is where it lands when the body
/// is something else entirely.
#[test]
fn the_error_type_header_names_the_exception_when_the_body_does_not() {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        ERROR_TYPE_HEADER,
        HeaderValue::from_static("ThrottlingException"),
    );

    let error = DynamoProtocol::parse_error(&HttpResponse {
        status: StatusCode::BAD_REQUEST,
        headers,
        body: Bytes::from_static(b"<html>rejected by a proxy</html>"),
    });

    assert_eq!(error.code, DynamoErrorCode::ThrottlingException);
    assert_eq!(error.code.retryable(), Retryable::Yes);
}

/// A shed request must be retried here too, whether the code names the throttle or only the
/// status does.
///
/// `DynamoDB` itself throttles with a named 400, but an intermediary in front of the endpoint
/// answers with a 429, and an unrecognised code is never retried.
#[test]
fn a_shed_request_is_a_throttle_however_it_is_spelled() {
    for body in [
        &br#"{"__type":"TooManyRequests"}"#[..],
        &br#"{"message":"Please reduce your request rate"}"#[..],
    ] {
        let response = HttpResponse {
            status: StatusCode::TOO_MANY_REQUESTS,
            headers: http::HeaderMap::new(),
            body: Bytes::copy_from_slice(body),
        };

        let error = DynamoProtocol::parse_error(&response);

        assert_eq!(error.code, DynamoErrorCode::ThrottlingException);
        assert_eq!(error.code.retryable(), Retryable::Yes);
    }
}

/// The code is bounded, and the bound is on the *name*. A decoration long enough to push the whole
/// thing past that bound must not take the name with it.
///
/// `com.amazonaws.dynamodb.v20120810#ProvisionedThroughputExceededException` is already seventy
/// characters. Cutting a decorated code to the name's own bound would leave a prefix the match does
/// not recognise, and an unrecognised code is never retried — so a throttle would surface as a hard
/// failure.
#[test]
fn a_long_decoration_does_not_cut_the_exception_name() {
    for decorated in [
        "com.amazonaws.dynamodb.v20120810#ProvisionedThroughputExceededException".to_owned(),
        format!(
            "{}#ThrottlingException:http://internal.amazon.com/coral/com.amazon.coral.validate/",
            "com.amazonaws.a.deeply.nested.namespace.v20120810".repeat(3)
        ),
    ] {
        let body = format!(r#"{{"__type":"{decorated}"}}"#);
        let response = HttpResponse {
            status: StatusCode::BAD_REQUEST,
            headers: http::HeaderMap::new(),
            body: Bytes::from(body),
        };

        let error = DynamoProtocol::parse_error(&response);

        assert_eq!(
            error.code.retryable(),
            Retryable::Yes,
            "{decorated} names a throttle"
        );
    }
}

/// A code arrives decorated with a Smithy namespace, a trailing URL, or both.
#[test]
fn an_exception_name_is_read_out_of_every_decoration() {
    for decorated in [
        "ThrottlingException",
        "com.amazonaws.dynamodb.v20120810#ThrottlingException",
        "ThrottlingException:http://internal.amazon.com/coral/com.amazon.coral.validate/",
        "com.amazon.coral.validate#ThrottlingException:http://internal.amazon.com/",
        "  ThrottlingException  ",
    ] {
        assert_eq!(
            error_code_name(decorated),
            "ThrottlingException",
            "{decorated}"
        );
    }
}

#[test]
fn an_error_keeps_its_request_id_for_support() {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        "x-amzn-requestid",
        HeaderValue::from_static("ABC123REQUESTID"),
    );

    let error = DynamoProtocol::parse_error(&HttpResponse {
        status: StatusCode::BAD_REQUEST,
        headers,
        body: Bytes::from_static(br#"{"__type":"x#ValidationException"}"#),
    });

    assert_eq!(error.request_id.as_deref(), Some("ABC123REQUESTID"));
    assert_eq!(error.code, DynamoErrorCode::ValidationException);
}

#[test]
fn a_transact_put_writes_its_condition_and_return_values() {
    let put = TransactPut::new(Arc::from("locks"), key_item())
        .condition("attribute_not_exists(#pk)")
        .name("#pk", "hash")
        .returning_old_on_failure();

    let written = body_of(|body| {
        write_condition_fields(
            body,
            put.condition_expression,
            &put.names,
            &put.values,
            put.return_values_on_failure,
        );
    });

    assert!(written.contains(r#""ConditionExpression":"attribute_not_exists(#pk)""#));
    assert!(written.contains(r#""ReturnValuesOnConditionCheckFailure":"ALL_OLD""#));
    assert!(written.contains(r##""ExpressionAttributeNames":{"#pk":"hash"}"##));
}

#[test]
fn a_transact_delete_without_a_condition_writes_none() {
    let delete = TransactDelete::new(Arc::from("locks"), key_item());

    assert_eq!(
        body_of(|body| write_condition_fields(
            body,
            delete.condition_expression,
            &delete.names,
            &delete.values,
            delete.return_values_on_failure
        )),
        ""
    );
}

#[test]
fn select_and_return_values_use_the_wire_spellings() {
    assert_eq!(Select::Count.as_str(), "COUNT");
    assert_eq!(Select::AllAttributes.as_str(), "ALL_ATTRIBUTES");
    assert_eq!(ReturnValue::None.as_str(), "NONE");
    assert_eq!(ReturnValue::AllOld.as_str(), "ALL_OLD");
}

#[test]
fn a_get_item_body_names_its_table_key_and_read_consistency() {
    assert_eq!(
        text(get_item_body("fragments", &key_item(), true)),
        r##"{"TableName":"fragments","Key":{"hash":{"B":"AQI="}},"ConsistentRead":true}"##
    );
}

/// Every optional field a query sets is written, in a fixed order, and the start key is the cursor
/// the previous page returned.
#[test]
fn a_query_body_writes_every_field_it_sets() {
    let start = key_item();
    let request = QueryRequest {
        table: "locks",
        index: Some("by-owner"),
        key_condition: "#pk = :pk",
        names: &[("#pk", "ownerId")],
        values: &[(":pk", AttributeValue::string("someone"))],
        filter: Some("attribute_exists(#pk)"),
        limit: Some(25),
        select: Some(Select::Count),
        consistent: false,
        exclusive_start_key: Some(&start),
    };

    assert_eq!(
        text(query_body(&request)),
        concat!(
            r##"{"TableName":"locks","KeyConditionExpression":"#pk = :pk","ConsistentRead":false,"##,
            r##""IndexName":"by-owner","FilterExpression":"attribute_exists(#pk)","Limit":25,"##,
            r##""Select":"COUNT","ExpressionAttributeNames":{"#pk":"ownerId"},"##,
            r##""ExpressionAttributeValues":{":pk":{"S":"someone"}},"##,
            r##""ExclusiveStartKey":{"hash":{"B":"AQI="}}}"##,
        )
    );
}

#[test]
fn a_query_body_leaves_out_every_field_it_does_not_set() {
    let request = QueryRequest {
        table: "fragments",
        index: None,
        key_condition: "hash = :hash",
        names: &[],
        values: &[],
        filter: None,
        limit: None,
        select: None,
        consistent: true,
        exclusive_start_key: None,
    };

    assert_eq!(
        text(query_body(&request)),
        r##"{"TableName":"fragments","KeyConditionExpression":"hash = :hash","ConsistentRead":true}"##
    );
}

#[test]
fn a_scan_body_writes_its_segment_and_cursor() {
    let start = key_item();
    let request = ScanRequest {
        table: "fragments",
        limit: Some(100),
        segment: Some(3),
        total_segments: Some(8),
        exclusive_start_key: Some(&start),
    };

    assert_eq!(
        text(scan_body(&request)),
        concat!(
            r##"{"TableName":"fragments","Limit":100,"Segment":3,"TotalSegments":8,"##,
            r##""ExclusiveStartKey":{"hash":{"B":"AQI="}}}"##,
        )
    );

    let whole = ScanRequest {
        table: "fragments",
        limit: None,
        segment: None,
        total_segments: None,
        exclusive_start_key: None,
    };
    assert_eq!(text(scan_body(&whole)), r##"{"TableName":"fragments"}"##);
}

/// The keys sit under `RequestItems`, then the table's name, then `Keys`, which is the nesting the
/// service reads; the consistency flag belongs to the table, beside its keys.
#[test]
fn a_batch_get_body_nests_its_keys_under_the_table() {
    assert_eq!(
        text(batch_get_item_body(
            "fragments",
            &[key_item(), other_key_item()],
            true
        )),
        concat!(
            r##"{"RequestItems":{"fragments":{"Keys":[{"hash":{"B":"AQI="}},{"hash":{"B":"Aw=="}}],"##,
            r##""ConsistentRead":true}}}"##,
        )
    );
}

/// Each transaction item is wrapped in its kind, with its condition, its request for the losing
/// row, and its expression maps inside that kind rather than beside it.
#[test]
fn a_transact_write_body_lays_out_each_put_and_delete_with_its_condition() {
    let items = [
        TransactWriteItem::Put(
            TransactPut::new(Arc::from("locks"), key_item())
                .condition("attribute_not_exists(#pk)")
                .name("#pk", "hash")
                .returning_old_on_failure(),
        ),
        TransactWriteItem::Delete(
            TransactDelete::new(Arc::from("locks"), other_key_item())
                .condition("ownerId = :owner")
                .value(":owner", AttributeValue::string("someone")),
        ),
    ];

    assert_eq!(
        text(transact_write_items_body(&items, "token-1")),
        concat!(
            r##"{"ClientRequestToken":"token-1","TransactItems":["##,
            r##"{"Put":{"TableName":"locks","Item":{"hash":{"B":"AQI="}},"##,
            r##""ConditionExpression":"attribute_not_exists(#pk)","##,
            r##""ReturnValuesOnConditionCheckFailure":"ALL_OLD","##,
            r##""ExpressionAttributeNames":{"#pk":"hash"}}},"##,
            r##"{"Delete":{"TableName":"locks","Key":{"hash":{"B":"Aw=="}},"##,
            r##""ConditionExpression":"ownerId = :owner","##,
            r##""ExpressionAttributeValues":{":owner":{"S":"someone"}}}}"##,
            r##"]}"##,
        )
    );
}

#[test]
fn a_put_item_body_carries_its_condition_after_the_item() {
    let condition = Condition {
        expression: "attribute_not_exists(#pk)",
        names: &[("#pk", "hash")],
        values: &[],
        return_values_on_failure: true,
    };

    assert_eq!(
        text(put_item_body("fragments", &key_item(), Some(&condition))),
        concat!(
            r##"{"TableName":"fragments","Item":{"hash":{"B":"AQI="}},"##,
            r##""ConditionExpression":"attribute_not_exists(#pk)","##,
            r##""ReturnValuesOnConditionCheckFailure":"ALL_OLD","##,
            r##""ExpressionAttributeNames":{"#pk":"hash"}}"##,
        )
    );
    assert_eq!(
        text(put_item_body("fragments", &key_item(), None)),
        r##"{"TableName":"fragments","Item":{"hash":{"B":"AQI="}}}"##
    );
}

/// A delete asks for nothing back: the stores never read the deleted row.
#[test]
fn a_delete_item_body_names_its_key_and_asks_for_nothing_back() {
    assert_eq!(
        text(delete_item_body("fragments", &key_item(), None)),
        r##"{"TableName":"fragments","Key":{"hash":{"B":"AQI="}},"ReturnValues":"NONE"}"##
    );
}

/// A conditional delete carries its condition, and its request for the item the condition refused,
/// after the key.
#[test]
fn a_delete_item_body_carries_its_condition_after_the_key() {
    let condition = Condition {
        expression: "#v = :value",
        names: &[("#v", "value")],
        values: &[(":value", AttributeValue::blob([7u8]))],
        return_values_on_failure: true,
    };

    assert_eq!(
        text(delete_item_body("fragments", &key_item(), Some(&condition))),
        concat!(
            r##"{"TableName":"fragments","Key":{"hash":{"B":"AQI="}},"ReturnValues":"NONE","##,
            r##""ConditionExpression":"#v = :value","##,
            r##""ReturnValuesOnConditionCheckFailure":"ALL_OLD","##,
            r##""ExpressionAttributeNames":{"#v":"value"},"##,
            r##""ExpressionAttributeValues":{":value":{"B":"Bw=="}}}"##,
        )
    );
}
