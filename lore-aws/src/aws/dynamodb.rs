// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The eight `DynamoDB` operations Lore makes.
//!
//! All of them are `POST /` with `x-amz-target: DynamoDB_20120810.<Op>` and a JSON body
//! (<https://docs.aws.amazon.com/amazondynamodb/latest/APIReference/API_Operations.html>). Each
//! request is written straight into one buffer by [`BodyWriter`], and each response is read
//! straight out of the response buffer by [`json`]'s seeds — no intermediate map, no `String` per
//! attribute.
use std::cell::RefCell;
use std::sync::Arc;

use bytes::Bytes;
use http::HeaderValue;
use http::Method;
use serde::de::DeserializeSeed as _;
use serde::de::IgnoredAny;
use serde::de::MapAccess;
use serde::de::Visitor;

use crate::aws::ClientCore;
use crate::aws::ServiceProtocol;
use crate::aws::error::AwsCallError;
use crate::aws::error::CancellationReason;
use crate::aws::error::DynamoErrorCode;
use crate::aws::error::ErrorPayload;
use crate::aws::error::ServiceError;
use crate::aws::error::ServiceErrorCode as _;
use crate::aws::error::carries_request_id;
use crate::aws::error::service_text;
use crate::aws::http::HttpRequest;
use crate::aws::http::HttpResponse;
use crate::aws::item::AttributeValue;
use crate::aws::item::ByteStr;
use crate::aws::item::Item;
use crate::aws::json::Arena;
use crate::aws::json::BodyWriter;
use crate::aws::json::ItemSeed;
use crate::aws::json::ParseError;
use crate::aws::json::SeqSeed;
use crate::aws::json::TextSeed;
use crate::dynamodb::TRANSACT_WRITE_ITEMS_CHUNKS;

/// The protocol's content type. `DynamoDB` rejects anything else.
const CONTENT_TYPE: HeaderValue = HeaderValue::from_static("application/x-amz-json-1.0");

/// A convenient alias: every operation fails the same way.
pub type DynamoResult<T> = Result<T, AwsCallError<DynamoErrorCode>>;

/// `DynamoDB`'s error protocol: a JSON body naming the exception in `__type`.
pub struct DynamoProtocol;

impl ServiceProtocol for DynamoProtocol {
    type Code = DynamoErrorCode;

    const SERVICE: &'static str = "dynamodb";
    const REQUEST_ID_HEADER: &'static str = "x-amzn-requestid";

    fn parse_error(response: &HttpResponse) -> ServiceError<Self::Code> {
        // A failed call always keeps its request id: it is what AWS support asks for, and an error
        // is rare enough that the allocation does not matter.
        let request_id = response
            .header(Self::REQUEST_ID_HEADER)
            .map(ByteStr::copy_from_str);

        let parsed = parse_error_body(response.body.clone()).unwrap_or_default();

        // `__type` is what DynamoDB itself sends, so it is consulted first. The other two are the
        // rest of what the AWS JSON protocol permits, and they are what an intermediary answers
        // with — a gateway or proxy in front of the endpoint. Without them such a response degrades
        // to an unknown code, which is never retryable, so a throttle arriving through a proxy
        // would surface as a hard failure.
        let code = parsed
            .code()
            .or_else(|| response.header(ERROR_TYPE_HEADER))
            .map_or_default(error_code_name);

        ServiceError {
            status: response.status,
            code: DynamoErrorCode::from_wire(code, response.status),
            message: parsed.message,
            request_id,
            payload: parsed.payload,
        }
    }
}

/// Where the AWS JSON protocol allows an error code outside the body.
#[lore_macro::test_pub]
const ERROR_TYPE_HEADER: &str = "x-amzn-errortype";

/// The exception name inside an error code, however the service spelled it.
///
/// A code arrives decorated two ways: a Smithy namespace before a `#`
/// (`com.amazonaws.dynamodb.v20120810#ConditionalCheckFailedException`), and, in what some
/// endpoints answer with, a URL after a `:`. The name is what sits between the two.
#[lore_macro::test_pub]
fn error_code_name(code: &str) -> &str {
    let after_namespace = code.rsplit('#').next().unwrap_or(code);
    let before_url = after_namespace.split(':').next().unwrap_or(after_namespace);

    before_url.trim()
}

/// What a `DynamoDB` error body carries.
#[lore_macro::test_pub]
#[derive(Default)]
struct ErrorBody {
    /// The `__type` field, which is what `DynamoDB` sends.
    type_field: Option<ByteStr>,
    /// The `code` field, the protocol's other spelling. Kept apart from `__type` so field order in
    /// the body cannot decide which one wins.
    code_field: Option<ByteStr>,
    message: Option<ByteStr>,
    payload: ErrorPayload,
}

impl ErrorBody {
    #[lore_macro::test_pub]
    fn code(&self) -> Option<&str> {
        self.type_field.as_deref().or(self.code_field.as_deref())
    }
}

/// Read an error body: `__type`, a message under either spelling, and whichever payload the
/// exception carries.
#[lore_macro::test_pub]
fn parse_error_body(body: Bytes) -> Result<ErrorBody, ParseError> {
    crate::aws::json::parse_response(body, |deserializer, arena| {
        ErrorBodySeed(arena).deserialize(deserializer)
    })
}

struct ErrorBodySeed<'a>(&'a Arena);

impl<'de> serde::de::DeserializeSeed<'de> for ErrorBodySeed<'_> {
    type Value = ErrorBody;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for ErrorBodySeed<'_> {
    type Value = ErrorBody;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "a DynamoDB error body")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut body = ErrorBody::default();

        while let Some(field) = map.next_key::<&str>()? {
            match field {
                // Both are kept as sent, decorations and all: `error_code_name` is what strips a
                // namespace or a trailing URL, and it runs at the call site so it happens in one
                // place for the header spelling too. Bounded to what a decorated code can be rather
                // than to what a name can be, because the name is still inside this.
                "__type" => {
                    let full = map.next_value_seed(TextSeed(self.0))?;
                    body.type_field = Some(service_text(&full));
                }
                "code" => {
                    let full = map.next_value_seed(TextSeed(self.0))?;
                    body.code_field = Some(service_text(&full));
                }
                // Both spellings appear in the wild, depending on the operation.
                "message" | "Message" => {
                    let message = map.next_value_seed(TextSeed(self.0))?;
                    body.message = Some(service_text(&message));
                }
                // The row a conditional write lost against.
                "Item" => {
                    let item = map.next_value_seed(ItemSeed::new(self.0))?;
                    body.payload = ErrorPayload::Item(Box::new(item));
                }
                // One reason per item of the transaction, which holds at most that many.
                "CancellationReasons" => {
                    let reasons = map.next_value_seed(
                        SeqSeed::new(ReasonSeed(self.0)).at_most(TRANSACT_WRITE_ITEMS_CHUNKS),
                    )?;
                    body.payload = ErrorPayload::CancellationReasons(reasons);
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }

        Ok(body)
    }
}

/// One transaction item's cancellation reason.
#[derive(Clone, Copy)]
struct ReasonSeed<'a>(&'a Arena);

impl<'de> serde::de::DeserializeSeed<'de> for ReasonSeed<'_> {
    type Value = CancellationReason;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for ReasonSeed<'_> {
    type Value = CancellationReason;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "a cancellation reason")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut reason = CancellationReason::default();

        while let Some(field) = map.next_key::<&str>()? {
            match field {
                "Code" => reason.code = Some(map.next_value_seed(TextSeed(self.0))?),
                "Message" => reason.message = Some(map.next_value_seed(TextSeed(self.0))?),
                "Item" => {
                    reason.item = Some(Box::new(map.next_value_seed(ItemSeed::new(self.0))?));
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }

        Ok(reason)
    }
}

/// Expression attribute names: placeholder to real attribute name.
///
/// `&'static str` on both sides because every name in every query and condition Lore builds is a
/// literal or a `const`. That is what removes two `HashMap`s and a `String` per attribute from each
/// query.
pub type ExprNames = smallvec::SmallVec<[(&'static str, &'static str); 4]>;
/// Expression attribute values: placeholder to value.
pub type ExprValues = smallvec::SmallVec<[(&'static str, AttributeValue); 4]>;

/// What a `Query` should return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Select {
    AllAttributes,
    AllProjectedAttributes,
    SpecificAttributes,
    Count,
}

impl Select {
    #[lore_macro::test_pub]
    fn as_str(self) -> &'static str {
        match self {
            Self::AllAttributes => "ALL_ATTRIBUTES",
            Self::AllProjectedAttributes => "ALL_PROJECTED_ATTRIBUTES",
            Self::SpecificAttributes => "SPECIFIC_ATTRIBUTES",
            Self::Count => "COUNT",
        }
    }
}

/// What a write should hand back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnValue {
    None,
    AllOld,
}

impl ReturnValue {
    #[lore_macro::test_pub]
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "NONE",
            Self::AllOld => "ALL_OLD",
        }
    }
}

/// A `Query`, with everything the callers actually set.
pub struct QueryRequest<'a> {
    pub table: &'a str,
    pub index: Option<&'a str>,
    pub key_condition: &'a str,
    pub names: &'a [(&'static str, &'static str)],
    pub values: &'a [(&'static str, AttributeValue)],
    pub filter: Option<&'a str>,
    pub limit: Option<i32>,
    pub select: Option<Select>,
    pub consistent: bool,
    pub exclusive_start_key: Option<&'a Item>,
}

/// A `Scan` of one segment of a table.
pub struct ScanRequest<'a> {
    pub table: &'a str,
    pub limit: Option<i32>,
    pub segment: Option<i32>,
    pub total_segments: Option<i32>,
    pub exclusive_start_key: Option<&'a Item>,
}

/// A conditional write's condition.
pub struct Condition<'a> {
    pub expression: &'a str,
    pub names: &'a [(&'static str, &'static str)],
    pub values: &'a [(&'static str, AttributeValue)],
    /// Whether to ask for the losing row when the condition fails.
    pub return_values_on_failure: bool,
}

/// One item in a transaction: exactly one kind of write.
#[derive(Debug, Clone, PartialEq)]
pub enum TransactWriteItem {
    Put(TransactPut),
    Delete(TransactDelete),
}

/// A conditional put inside a transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct TransactPut {
    pub table_name: Arc<str>,
    pub item: Item,
    pub condition_expression: Option<&'static str>,
    pub names: ExprNames,
    pub values: ExprValues,
    pub return_values_on_failure: bool,
}

impl TransactPut {
    pub fn new(table_name: Arc<str>, item: Item) -> Self {
        Self {
            table_name,
            item,
            condition_expression: None,
            names: ExprNames::new(),
            values: ExprValues::new(),
            return_values_on_failure: false,
        }
    }

    pub fn condition(mut self, expression: &'static str) -> Self {
        self.condition_expression = Some(expression);
        self
    }

    pub fn name(mut self, placeholder: &'static str, attribute: &'static str) -> Self {
        self.names.push((placeholder, attribute));
        self
    }

    pub fn value(mut self, placeholder: &'static str, value: AttributeValue) -> Self {
        self.values.push((placeholder, value));
        self
    }

    /// Ask for the row the condition lost against, which is how a lock's owner is identified.
    pub fn returning_old_on_failure(mut self) -> Self {
        self.return_values_on_failure = true;
        self
    }
}

/// A conditional delete inside a transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct TransactDelete {
    pub table_name: Arc<str>,
    pub key: Item,
    pub condition_expression: Option<&'static str>,
    pub names: ExprNames,
    pub values: ExprValues,
    pub return_values_on_failure: bool,
}

impl TransactDelete {
    pub fn new(table_name: Arc<str>, key: Item) -> Self {
        Self {
            table_name,
            key,
            condition_expression: None,
            names: ExprNames::new(),
            values: ExprValues::new(),
            return_values_on_failure: false,
        }
    }

    pub fn condition(mut self, expression: &'static str) -> Self {
        self.condition_expression = Some(expression);
        self
    }

    pub fn value(mut self, placeholder: &'static str, value: AttributeValue) -> Self {
        self.values.push((placeholder, value));
        self
    }

    pub fn returning_old_on_failure(mut self) -> Self {
        self.return_values_on_failure = true;
        self
    }
}

/// What `GetItem` answers.
#[derive(Debug, Default)]
pub struct GetItemOutput {
    pub item: Option<Item>,
    pub request_id: Option<ByteStr>,
}

/// What `Query` answers.
#[derive(Debug, Default)]
pub struct QueryOutput {
    pub items: Option<Vec<Item>>,
    pub count: i32,
    pub scanned_count: i32,
    pub last_evaluated_key: Option<Item>,
    pub request_id: Option<ByteStr>,
}

impl QueryOutput {
    pub fn count(&self) -> i32 {
        self.count
    }

    pub fn scanned_count(&self) -> i32 {
        self.scanned_count
    }
}

/// What `Scan` answers.
#[derive(Debug, Default)]
pub struct ScanOutput {
    pub items: Vec<Item>,
    pub last_evaluated_key: Option<Item>,
    pub request_id: Option<ByteStr>,
}

/// What `BatchGetItem` answers for the one table it was asked about.
///
/// The single-table shape rather than the service's map-of-tables: this client always asks about
/// one table, so the caller has no map to unwrap and no table name to look itself up by.
#[derive(Debug, Default)]
pub struct BatchGetItemOutput {
    pub items: Vec<Item>,
    /// Keys the service declined to serve this time. A 200 with these set is how `DynamoDB` sheds
    /// load, so an empty list is the only "fully served" answer.
    pub unprocessed_keys: Vec<Item>,
    pub request_id: Option<ByteStr>,
}

/// What a write answers: nothing but its request id.
#[derive(Debug, Default)]
pub struct WriteOutput {
    pub request_id: Option<ByteStr>,
}

carries_request_id!(
    GetItemOutput,
    QueryOutput,
    ScanOutput,
    BatchGetItemOutput,
    WriteOutput
);

thread_local! {
    /// The buffer request bodies are written into, one per worker thread.
    ///
    /// A body is frozen and handed to the transport, but the writer keeps the rest of the chunk it
    /// was carved from for the next body.
    ///
    /// Borrowed only across the synchronous closure in [`write_body`]; see the signing scratch in
    /// [`crate::aws`] for what makes a thread-local sound here.
    static BODY_WRITER: RefCell<BodyWriter> = RefCell::new(BodyWriter::new());
}

/// Write a request body into this thread's buffer and take it.
///
/// `capacity` estimates the finished size; overshooting spends a little of the chunk, undershooting
/// costs one growth.
fn write_body(capacity: usize, write: impl FnOnce(&mut BodyWriter)) -> Bytes {
    BODY_WRITER.with_borrow_mut(|body| {
        body.reserve_for(capacity);
        write(body);
        body.split()
    })
}

/// A client for one `DynamoDB` endpoint.
#[derive(Debug, Clone)]
pub struct DynamoClient {
    core: Arc<ClientCore>,
}

impl DynamoClient {
    pub fn new(core: Arc<ClientCore>) -> Self {
        Self { core }
    }

    pub fn core(&self) -> &Arc<ClientCore> {
        &self.core
    }

    /// The response's request id, when this client was built to capture one.
    ///
    /// Off unless a slow-operation threshold is configured: only that warning reads it, and
    /// `lore-server` leaves the threshold at `u64::MAX` by default, so capturing one per response
    /// would be an allocation for nothing.
    fn request_id(&self, response: &HttpResponse) -> Option<ByteStr> {
        if !self.core.capture_request_id() {
            return None;
        }

        response
            .header(DynamoProtocol::REQUEST_ID_HEADER)
            .map(ByteStr::copy_from_str)
    }

    /// Whether a table exists, which is all `ensure_table` needs at startup.
    pub async fn table_exists(&self, table: &str) -> DynamoResult<bool> {
        let body = write_body(64, |body| {
            body.raw("{");
            body.string_field("TableName", table);
            body.raw("}");
        });

        match self.call(Operation::DescribeTable, body).await {
            Ok(_) => Ok(true),
            Err(AwsCallError::Service(error)) if error.code.is_resource_not_found_exception() => {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    pub async fn get_item(
        &self,
        table: &str,
        key: &Item,
        consistent: bool,
    ) -> DynamoResult<GetItemOutput> {
        let response = self
            .call(Operation::GetItem, get_item_body(table, key, consistent))
            .await?;
        let request_id = self.request_id(&response);

        let mut output: GetItemOutput = parse(response.body, |deserializer, arena| {
            GetItemSeed { arena }.deserialize(deserializer)
        })?;
        output.request_id = request_id;

        Ok(output)
    }

    /// Whether `key` names an item in `table`, without decoding the item.
    ///
    /// The request is the one `get_item` sends; only the response handling differs. A caller that
    /// wants presence and nothing else would otherwise pay for the item it throws away: every
    /// binary attribute base64-decoded into the response arena, and a map built to hold them. On
    /// the store's hottest read that is an allocation per lookup for a `bool`.
    pub async fn item_exists(
        &self,
        table: &str,
        key: &Item,
        consistent: bool,
    ) -> DynamoResult<bool> {
        let response = self
            .call(Operation::GetItem, get_item_body(table, key, consistent))
            .await?;

        let present = parse(response.body, |deserializer, arena| {
            ItemPresenceSeed { arena }.deserialize(deserializer)
        })?;

        Ok(present)
    }

    /// Read up to a hundred keys from one table.
    ///
    /// The caller chunks; this sends exactly what it is given. `unprocessed_keys` on the output is
    /// a 200 with work left over, not an error.
    pub async fn batch_get_item(
        &self,
        table: &str,
        keys: &[Item],
        consistent: bool,
    ) -> DynamoResult<BatchGetItemOutput> {
        let response = self
            .call(
                Operation::BatchGetItem,
                batch_get_item_body(table, keys, consistent),
            )
            .await?;
        let request_id = self.request_id(&response);

        let mut output: BatchGetItemOutput = parse(response.body, |deserializer, arena| {
            BatchGetItemSeed {
                arena,
                table,
                capacity: keys.len(),
            }
            .deserialize(deserializer)
        })?;
        output.request_id = request_id;

        Ok(output)
    }

    pub async fn put_item(
        &self,
        table: &str,
        item: &Item,
        condition: Option<&Condition<'_>>,
    ) -> DynamoResult<WriteOutput> {
        let response = self
            .call(Operation::PutItem, put_item_body(table, item, condition))
            .await?;

        Ok(WriteOutput {
            request_id: self.request_id(&response),
        })
    }

    pub async fn delete_item(
        &self,
        table: &str,
        key: &Item,
        condition: Option<&Condition<'_>>,
    ) -> DynamoResult<WriteOutput> {
        let response = self
            .call(
                Operation::DeleteItem,
                delete_item_body(table, key, condition),
            )
            .await?;

        Ok(WriteOutput {
            request_id: self.request_id(&response),
        })
    }

    pub async fn query(&self, request: &QueryRequest<'_>) -> DynamoResult<QueryOutput> {
        let response = self.call(Operation::Query, query_body(request)).await?;
        let request_id = self.request_id(&response);
        let capacity = request
            .limit
            .map_or(0, |limit| usize::try_from(limit).unwrap_or(0));

        let mut output: QueryOutput = parse(response.body, |deserializer, arena| {
            QuerySeed { arena, capacity }.deserialize(deserializer)
        })?;
        output.request_id = request_id;

        Ok(output)
    }

    pub async fn scan(&self, request: &ScanRequest<'_>) -> DynamoResult<ScanOutput> {
        let response = self.call(Operation::Scan, scan_body(request)).await?;
        let request_id = self.request_id(&response);

        let mut output: ScanOutput = parse(response.body, |deserializer, arena| {
            ScanSeed { arena }.deserialize(deserializer)
        })?;
        output.request_id = request_id;

        Ok(output)
    }

    /// Apply up to a hundred writes atomically.
    ///
    /// `client_request_token` makes the call idempotent for ten minutes, which is what allows the
    /// attempt loop to re-send it. Without one, a retry after a lost response could apply the
    /// transaction twice — and for a lock acquisition the second attempt would come back as a
    /// conditional-check failure against a lock the caller already holds.
    pub async fn transact_write_items(
        &self,
        items: &[TransactWriteItem],
        client_request_token: &str,
    ) -> DynamoResult<WriteOutput> {
        let response = self
            .call(
                Operation::TransactWriteItems,
                transact_write_items_body(items, client_request_token),
            )
            .await?;

        Ok(WriteOutput {
            request_id: self.request_id(&response),
        })
    }

    /// Send one operation.
    ///
    /// Both header values are shared bytes and the body is a `Bytes`, so building a request per
    /// attempt is one header map and three reference counts.
    async fn call(&self, operation: Operation, body: Bytes) -> DynamoResult<HttpResponse> {
        let target = operation.target();

        self.core
            .send::<DynamoProtocol>(|| {
                let mut request = HttpRequest::new(Method::POST, self.core.url("/", None))
                    .body(body.clone())
                    .response_cap(operation.response_cap());

                request
                    .headers
                    .insert(http::header::CONTENT_TYPE, CONTENT_TYPE);
                request.headers.insert("x-amz-target", target.clone());

                request
            })
            .await
    }
}

/// An operation this client sends.
#[lore_macro::test_pub]
#[derive(Clone, Copy)]
enum Operation {
    BatchGetItem,
    DeleteItem,
    DescribeTable,
    GetItem,
    PutItem,
    Query,
    Scan,
    TransactWriteItems,
}

impl Operation {
    /// The `x-amz-target` value: the API version and the operation's name.
    ///
    /// Each is built at compile time from a literal, so it is shared storage already and cloning it
    /// into a request on every attempt is a reference count rather than a copy of its bytes.
    #[lore_macro::test_pub]
    fn target(self) -> HeaderValue {
        match self {
            Self::BatchGetItem => {
                const { HeaderValue::from_static("DynamoDB_20120810.BatchGetItem") }
            }
            Self::DeleteItem => const { HeaderValue::from_static("DynamoDB_20120810.DeleteItem") },
            Self::DescribeTable => {
                const { HeaderValue::from_static("DynamoDB_20120810.DescribeTable") }
            }
            Self::GetItem => const { HeaderValue::from_static("DynamoDB_20120810.GetItem") },
            Self::PutItem => const { HeaderValue::from_static("DynamoDB_20120810.PutItem") },
            Self::Query => const { HeaderValue::from_static("DynamoDB_20120810.Query") },
            Self::Scan => const { HeaderValue::from_static("DynamoDB_20120810.Scan") },
            Self::TransactWriteItems => {
                const { HeaderValue::from_static("DynamoDB_20120810.TransactWriteItems") }
            }
        }
    }

    /// The largest response body this operation accepts.
    ///
    /// `BatchGetItem` answers with up to sixteen megabytes of rows and `TransactWriteItems` can
    /// return an old item per cancelled write, so both keep the client's own cap. Every other
    /// operation answers with at most one `Query` or `Scan` page, a megabyte of data, or one item
    /// of at most 400 KB, which [`SINGLE_RESPONSE_CAP`] holds with room to spare.
    fn response_cap(self) -> u64 {
        match self {
            Self::BatchGetItem | Self::TransactWriteItems => u64::MAX,
            Self::DeleteItem
            | Self::DescribeTable
            | Self::GetItem
            | Self::PutItem
            | Self::Query
            | Self::Scan => SINGLE_RESPONSE_CAP,
        }
    }
}

/// The largest response an operation answering with at most a page or an item accepts.
///
/// Four times the megabyte of data a `Query` or `Scan` page carries, which leaves room for the JSON
/// around it, while bounding what a misdirected endpoint can make one response decode into.
#[lore_macro::test_pub]
const SINGLE_RESPONSE_CAP: u64 = 4 * 1024 * 1024;

/// The body `GetItem` sends, which is also what `item_exists` asks with.
#[lore_macro::test_pub]
fn get_item_body(table: &str, key: &Item, consistent: bool) -> Bytes {
    write_body(128, |body| {
        body.raw("{");
        body.string_field("TableName", table);
        body.raw(",");
        body.key("Key");
        body.item(key);
        body.raw(",");
        body.bool_field("ConsistentRead", consistent);
        body.raw("}");
    })
}

/// The body `BatchGetItem` sends: `keys` under `RequestItems`, keyed by `table`.
#[lore_macro::test_pub]
fn batch_get_item_body(table: &str, keys: &[Item], consistent: bool) -> Bytes {
    write_body(64 + keys.len() * 96, |body| {
        body.raw("{");
        body.key("RequestItems");
        body.raw("{");
        body.key(table);
        body.raw("{");
        body.key("Keys");
        body.raw("[");
        for (index, key) in keys.iter().enumerate() {
            if index > 0 {
                body.raw(",");
            }
            body.item(key);
        }
        body.raw("],");
        body.bool_field("ConsistentRead", consistent);
        body.raw("}}}");
    })
}

/// The body `PutItem` sends.
#[lore_macro::test_pub]
fn put_item_body(table: &str, item: &Item, condition: Option<&Condition<'_>>) -> Bytes {
    write_body(192, |body| {
        body.raw("{");
        body.string_field("TableName", table);
        body.raw(",");
        body.key("Item");
        body.item(item);
        write_condition(body, condition);
        body.raw("}");
    })
}

/// The body `DeleteItem` sends, asking for nothing back but, with a condition that asks for it,
/// the item the condition refused.
#[lore_macro::test_pub]
fn delete_item_body(table: &str, key: &Item, condition: Option<&Condition<'_>>) -> Bytes {
    write_body(128, |body| {
        body.raw("{");
        body.string_field("TableName", table);
        body.raw(",");
        body.key("Key");
        body.item(key);
        body.raw(",");
        body.string_field("ReturnValues", ReturnValue::None.as_str());
        write_condition(body, condition);
        body.raw("}");
    })
}

/// The body `Query` sends, with only the optional fields the request sets.
#[lore_macro::test_pub]
fn query_body(request: &QueryRequest<'_>) -> Bytes {
    write_body(256, |body| {
        body.raw("{");
        body.string_field("TableName", request.table);
        body.raw(",");
        body.string_field("KeyConditionExpression", request.key_condition);
        body.raw(",");
        body.bool_field("ConsistentRead", request.consistent);

        if let Some(index) = request.index {
            body.raw(",");
            body.string_field("IndexName", index);
        }
        if let Some(filter) = request.filter {
            body.raw(",");
            body.string_field("FilterExpression", filter);
        }
        if let Some(limit) = request.limit {
            body.raw(",");
            body.number_field("Limit", i64::from(limit));
        }
        if let Some(select) = request.select {
            body.raw(",");
            body.string_field("Select", select.as_str());
        }
        write_names(body, request.names);
        write_values(body, request.values);
        if let Some(key) = request.exclusive_start_key {
            body.raw(",");
            body.key("ExclusiveStartKey");
            body.item(key);
        }
        body.raw("}");
    })
}

/// The body `Scan` sends, with only the optional fields the request sets.
#[lore_macro::test_pub]
fn scan_body(request: &ScanRequest<'_>) -> Bytes {
    write_body(192, |body| {
        body.raw("{");
        body.string_field("TableName", request.table);

        if let Some(limit) = request.limit {
            body.raw(",");
            body.number_field("Limit", i64::from(limit));
        }
        if let Some(segment) = request.segment {
            body.raw(",");
            body.number_field("Segment", i64::from(segment));
        }
        if let Some(total) = request.total_segments {
            body.raw(",");
            body.number_field("TotalSegments", i64::from(total));
        }
        if let Some(key) = request.exclusive_start_key {
            body.raw(",");
            body.key("ExclusiveStartKey");
            body.item(key);
        }
        body.raw("}");
    })
}

/// The body `TransactWriteItems` sends: each item as a `Put` or a `Delete`, with its condition
/// inside.
#[lore_macro::test_pub]
fn transact_write_items_body(items: &[TransactWriteItem], client_request_token: &str) -> Bytes {
    write_body(128 + items.len() * 160, |body| {
        body.raw("{");
        body.string_field("ClientRequestToken", client_request_token);
        body.raw(",");
        body.key("TransactItems");
        body.raw("[");

        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                body.raw(",");
            }
            body.raw("{");

            match item {
                TransactWriteItem::Put(put) => {
                    body.key("Put");
                    body.raw("{");
                    body.string_field("TableName", &put.table_name);
                    body.raw(",");
                    body.key("Item");
                    body.item(&put.item);
                    write_condition_fields(
                        body,
                        put.condition_expression,
                        &put.names,
                        &put.values,
                        put.return_values_on_failure,
                    );
                    body.raw("}");
                }
                TransactWriteItem::Delete(delete) => {
                    body.key("Delete");
                    body.raw("{");
                    body.string_field("TableName", &delete.table_name);
                    body.raw(",");
                    body.key("Key");
                    body.item(&delete.key);
                    write_condition_fields(
                        body,
                        delete.condition_expression,
                        &delete.names,
                        &delete.values,
                        delete.return_values_on_failure,
                    );
                    body.raw("}");
                }
            }

            body.raw("}");
        }

        body.raw("]}");
    })
}

/// A conditional write's condition fields, or nothing without a condition.
#[lore_macro::test_pub]
fn write_condition(body: &mut BodyWriter, condition: Option<&Condition<'_>>) {
    if let Some(condition) = condition {
        write_condition_fields(
            body,
            Some(condition.expression),
            condition.names,
            condition.values,
            condition.return_values_on_failure,
        );
    }
}

/// `,"ConditionExpression":…`, `,"ReturnValuesOnConditionCheckFailure":"ALL_OLD"` and the
/// expression maps, each written only when there is something to write.
#[lore_macro::test_pub]
fn write_condition_fields(
    body: &mut BodyWriter,
    expression: Option<&str>,
    names: &[(&'static str, &'static str)],
    values: &[(&'static str, AttributeValue)],
    return_values_on_failure: bool,
) {
    if let Some(expression) = expression {
        body.raw(",");
        body.string_field("ConditionExpression", expression);
    }
    if return_values_on_failure {
        body.raw(",");
        body.string_field(
            "ReturnValuesOnConditionCheckFailure",
            ReturnValue::AllOld.as_str(),
        );
    }
    write_names(body, names);
    write_values(body, values);
}

/// `,"ExpressionAttributeNames":{…}`, or nothing when there are none.
///
/// An empty object is not the same as an absent field to `DynamoDB`: it rejects the former.
#[lore_macro::test_pub]
fn write_names(body: &mut BodyWriter, names: &[(&'static str, &'static str)]) {
    if names.is_empty() {
        return;
    }

    body.raw(",");
    body.key("ExpressionAttributeNames");
    body.raw("{");
    for (index, (placeholder, attribute)) in names.iter().enumerate() {
        if index > 0 {
            body.raw(",");
        }
        body.string_field(placeholder, attribute);
    }
    body.raw("}");
}

#[lore_macro::test_pub]
fn write_values(body: &mut BodyWriter, values: &[(&'static str, AttributeValue)]) {
    if values.is_empty() {
        return;
    }

    body.raw(",");
    body.key("ExpressionAttributeValues");
    body.raw("{");
    for (index, (placeholder, value)) in values.iter().enumerate() {
        if index > 0 {
            body.raw(",");
        }
        body.key(placeholder);
        body.attribute_value(value);
    }
    body.raw("}");
}

/// Parse a successful response body, mapping a malformed one onto a transport failure.
///
/// A response that does not parse is not a service error — the service answered fine — so it
/// reports as a protocol failure, which is retryable in case it was truncated in flight.
#[lore_macro::test_pub]
fn parse<T, F>(body: Bytes, parser: F) -> DynamoResult<T>
where
    F: for<'de> FnOnce(
        &mut serde_json::Deserializer<serde_json::de::SliceRead<'de>>,
        &Arena,
    ) -> Result<T, serde_json::Error>,
{
    crate::aws::json::parse_response(body, parser).map_err(|error| {
        AwsCallError::Transport(crate::aws::error::TransportError::Protocol(
            error.to_string(),
        ))
    })
}

/// A response seed over the top-level object: `$seed` holds the arena and any listed fields, and
/// `$body` reads `$map` into `$out` with the arena bound to `$arena` and each field to its name.
macro_rules! response_seed {
    (
        $(#[$attribute:meta])*
        $seed:ident { $($field:ident: $field_type:ty),* },
        $output:ty,
        $expecting:literal,
        |$map:ident, $arena:ident, $out:ident| $body:block
    ) => {
        $(#[$attribute])*
        #[lore_macro::test_pub]
        struct $seed<'a> {
            arena: &'a Arena,
            $($field: $field_type,)*
        }

        impl<'de> serde::de::DeserializeSeed<'de> for $seed<'_> {
            type Value = $output;

            fn deserialize<D: serde::Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> Result<Self::Value, D::Error> {
                deserializer.deserialize_map(self)
            }
        }

        impl<'de> Visitor<'de> for $seed<'_> {
            type Value = $output;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(formatter, $expecting)
            }

            fn visit_map<A: MapAccess<'de>>(self, mut $map: A) -> Result<Self::Value, A::Error> {
                let $seed { arena: $arena $(, $field)* } = self;
                let mut $out = <$output>::default();
                $body
                Ok($out)
            }
        }
    };
}

response_seed!(
    GetItemSeed {},
    GetItemOutput,
    "a GetItem response",
    |map, arena, out| {
        while let Some(field) = map.next_key::<&str>()? {
            match field {
                "Item" => out.item = Some(map.next_value_seed(ItemSeed::new(arena))?),
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
    }
);

response_seed!(
    /// Only whether the item was there; its value is stepped over rather than decoded.
    ItemPresenceSeed {},
    bool,
    "a GetItem response",
    |map, _arena, out| {
        while let Some(field) = map.next_key::<&str>()? {
            if field == "Item" {
                out = true;
            }
            map.next_value::<IgnoredAny>()?;
        }
    }
);

response_seed!(
    /// `capacity` is how many items the query asked for at most, which sizes the list up front.
    QuerySeed { capacity: usize },
    QueryOutput,
    "a Query response",
    |map, arena, out| {
        while let Some(field) = map.next_key::<&str>()? {
            match field {
                "Items" => {
                    out.items =
                        Some(map.next_value_seed(SeqSeed::with_capacity(
                            ItemSeed::new(arena),
                            capacity,
                        ))?);
                }
                "Count" => out.count = map.next_value()?,
                "ScannedCount" => out.scanned_count = map.next_value()?,
                "LastEvaluatedKey" => {
                    out.last_evaluated_key = Some(map.next_value_seed(ItemSeed::new(arena))?);
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
    }
);

response_seed!(
    ScanSeed {},
    ScanOutput,
    "a Scan response",
    |map, arena, out| {
        while let Some(field) = map.next_key::<&str>()? {
            match field {
                "Items" => out.items = map.next_value_seed(SeqSeed::new(ItemSeed::new(arena)))?,
                "LastEvaluatedKey" => {
                    out.last_evaluated_key = Some(map.next_value_seed(ItemSeed::new(arena))?);
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
    }
);

/// `BatchGetItem`'s response, reduced to the one table that was asked about.
///
/// `capacity` is how many keys were asked for, which sizes the list of items up front and bounds
/// the answer: every key comes back as a row, as an unprocessed key, or not at all, so a response
/// naming more rows and keys between them than were asked for is refused.
#[lore_macro::test_pub]
struct BatchGetItemSeed<'a> {
    arena: &'a Arena,
    table: &'a str,
    capacity: usize,
}

impl<'de> serde::de::DeserializeSeed<'de> for BatchGetItemSeed<'_> {
    type Value = BatchGetItemOutput;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for BatchGetItemSeed<'_> {
    type Value = BatchGetItemOutput;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "a BatchGetItem response")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut output = BatchGetItemOutput::default();

        while let Some(field) = map.next_key::<&str>()? {
            match field {
                "Responses" => {
                    output.items = map.next_value_seed(TableSeed {
                        table: self.table,
                        inner: SeqSeed::with_capacity(ItemSeed::new(self.arena), self.capacity)
                            .at_most(self.capacity),
                    })?;
                }
                "UnprocessedKeys" => {
                    output.unprocessed_keys = map.next_value_seed(TableSeed {
                        table: self.table,
                        inner: KeysAndAttributesSeed {
                            arena: self.arena,
                            limit: self.capacity,
                        },
                    })?;
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }

        if output.items.len() + output.unprocessed_keys.len() > self.capacity {
            return Err(serde::de::Error::custom(format_args!(
                "{} rows and {} unprocessed keys answer only {} keys",
                output.items.len(),
                output.unprocessed_keys.len(),
                self.capacity
            )));
        }

        Ok(output)
    }
}

/// A map of table name to `inner`'s shape, taking only this client's table.
struct TableSeed<'a, S> {
    table: &'a str,
    inner: S,
}

impl<'de, S> serde::de::DeserializeSeed<'de> for TableSeed<'_, S>
where
    S: serde::de::DeserializeSeed<'de> + Clone,
    S::Value: Default,
{
    type Value = S::Value;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de, S> Visitor<'de> for TableSeed<'_, S>
where
    S: serde::de::DeserializeSeed<'de> + Clone,
    S::Value: Default,
{
    type Value = S::Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "a map of table name to entries")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut value = S::Value::default();

        while let Some(name) = map.next_key::<&str>()? {
            if name == self.table {
                value = map.next_value_seed(self.inner.clone())?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }

        Ok(value)
    }
}

/// `{"Keys": [items], …}`, taking only the keys, of which there may be at most `limit`.
#[derive(Clone, Copy)]
struct KeysAndAttributesSeed<'a> {
    arena: &'a Arena,
    limit: usize,
}

impl<'de> serde::de::DeserializeSeed<'de> for KeysAndAttributesSeed<'_> {
    type Value = Vec<Item>;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for KeysAndAttributesSeed<'_> {
    type Value = Vec<Item>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "a KeysAndAttributes")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut keys = Vec::new();

        while let Some(field) = map.next_key::<&str>()? {
            match field {
                "Keys" => {
                    keys = map.next_value_seed(
                        SeqSeed::new(ItemSeed::new(self.arena)).at_most(self.limit),
                    )?;
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }

        Ok(keys)
    }
}
