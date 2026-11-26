// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What a failed AWS call is, and how a response becomes one.
//!
//! Every store above this layer sorts AWS failures into two piles: back off and retry, or a bug to
//! surface. That decision is made from a service's error *code*, so the code is a typed enum here
//! rather than a string to match on, and the two payloads a caller actually reads — the item behind
//! a failed condition, and a cancelled transaction's reasons — travel with it.
use std::fmt;

use http::StatusCode;

use crate::aws::item::ByteStr;
use crate::aws::item::Item;

/// A call that never produced a service response.
#[derive(Debug)]
pub enum TransportError {
    /// The request could not be built, so nothing was sent.
    Build(String),
    /// Connecting, writing or reading failed.
    Io(String),
    /// A response arrived but its body could not be read to the end.
    Body(String),
    /// The response was not the shape the protocol requires.
    Protocol(String),
    /// Every connection the client may have in use stayed in use for as long as the request could
    /// wait — the client's acquire timeout or the operation's deadline, whichever came first — so
    /// the request was never sent.
    ConnectionLimit,
    /// Waiting for a connection cut the request's timeout short — its whole timeout fit before the
    /// operation's deadline when it began to wait, and no longer did once it had a connection — and
    /// it was not answered in full in what remained. It may have been sent, so it may have been
    /// applied.
    ConnectionLimitTimeout,
}

impl TransportError {
    /// Whether sending the same request again could reach the service.
    ///
    /// A request that could not be built will not build on the next attempt either, and a body the
    /// client refuses to hold will be the same size when it arrives again. Both are properties of
    /// the exchange rather than of the moment it was attempted in, so waiting them out only delays
    /// the report. A request the connection limit held back is shed rather than sent again at once,
    /// since the connections it waited on are still busy and the caller's back-off paces the next
    /// try. One that queued for a connection and then ran out of what was left has no budget to be
    /// sent in.
    pub fn is_worth_resending(&self) -> bool {
        matches!(self, Self::Io(_) | Self::Body(_))
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Build(why) => write!(f, "failed to build request: {why}"),
            Self::Io(why) => write!(f, "transport failure: {why}"),
            Self::Body(why) => write!(f, "failed to read response body: {why}"),
            Self::Protocol(why) => write!(f, "malformed response: {why}"),
            Self::ConnectionLimit => f.write_str(
                "no connection was released in time; every connection max_connections allows \
                 was in use",
            ),
            Self::ConnectionLimitTimeout => f.write_str(
                "the exchange did not finish within the operation's budget, after waiting for a \
                 connection under max_connections until too little of it was left",
            ),
        }
    }
}

impl std::error::Error for TransportError {}

/// Extra detail carried on the error bodies that callers read rather than just report.
///
/// The item is boxed: an [`Item`] holds four attributes inline, and an error that carries none —
/// which is almost all of them — should not pay for the space.
#[derive(Debug, Default, PartialEq)]
pub enum ErrorPayload {
    #[default]
    None,
    /// `ReturnValuesOnConditionCheckFailure=ALL_OLD` puts the losing row on the error.
    Item(Box<Item>),
    /// A cancelled transaction says why, per item, in request order.
    CancellationReasons(Vec<CancellationReason>),
}

/// Why one item in a transaction did not apply.
///
/// `DynamoDB` returns one of these per transaction item, including for items that were fine — those
/// carry code `None`, which is why callers filter before reading them.
#[derive(Debug, Default, PartialEq)]
pub struct CancellationReason {
    pub code: Option<ByteStr>,
    pub message: Option<ByteStr>,
    pub item: Option<Box<Item>>,
}

impl CancellationReason {
    pub fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    pub fn item(&self) -> Option<&Item> {
        self.item.as_deref()
    }
}

/// The reason codes a transaction's cancellation reasons use.
///
/// `DynamoDB` shares this vocabulary between `BatchStatement` results and transaction
/// cancellations, which is why the name is the service's rather than the transaction's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchStatementErrorCode {
    ConditionalCheckFailed,
    ItemCollectionSizeLimitExceeded,
    TransactionConflict,
    ProvisionedThroughputExceeded,
    ThrottlingError,
    ValidationError,
    RequestLimitExceeded,
    InternalServerError,
    DuplicateItem,
    AccessDenied,
    /// A code this build does not know. Treated as unexpected, never as retryable.
    Unknown,
}

impl BatchStatementErrorCode {
    /// Whether this reason says the entry failed for load rather than for its contents.
    ///
    /// A cancelled transaction names one reason per entry, and the codes mix two unrelated answers:
    /// a decision about the data — the condition did not hold, the item was named twice — and the
    /// service declining to do the work right now. The second kind is the same backpressure a whole
    /// throttled transaction reports, and it reaches the caller here instead, so it has to classify
    /// the same way or a shed entry becomes a terminal failure.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::ThrottlingError
                | Self::ProvisionedThroughputExceeded
                | Self::RequestLimitExceeded
                | Self::InternalServerError
                // Another transaction held the item. Nothing about this one is wrong, and the retry
                // is how it gets its turn.
                | Self::TransactionConflict
        )
    }
}

impl From<&str> for BatchStatementErrorCode {
    fn from(code: &str) -> Self {
        match code {
            "ConditionalCheckFailed" => Self::ConditionalCheckFailed,
            "ItemCollectionSizeLimitExceeded" => Self::ItemCollectionSizeLimitExceeded,
            "TransactionConflict" => Self::TransactionConflict,
            "ProvisionedThroughputExceeded" => Self::ProvisionedThroughputExceeded,
            "ThrottlingError" => Self::ThrottlingError,
            "ValidationError" => Self::ValidationError,
            "RequestLimitExceeded" => Self::RequestLimitExceeded,
            "InternalServerError" => Self::InternalServerError,
            "DuplicateItem" => Self::DuplicateItem,
            "AccessDenied" => Self::AccessDenied,
            _ => Self::Unknown,
        }
    }
}

/// How much of any text the service chose is kept.
///
/// A message, a request id, and a code while it still carries its decorations all reach a `warn!`
/// line and an error type that is cloned up through the stores. Real ones are a sentence at most;
/// anything longer means the endpoint is not what we think it is, and a body-sized log line is its
/// own outage. Generous enough that none of them is ever cut.
#[lore_macro::test_pub]
const MAX_SERVICE_TEXT: usize = 512;

/// How much of an exception *name* is kept.
///
/// Tighter than [`MAX_SERVICE_TEXT`] because the name goes further than the rest: it is the
/// `aws_code` metric label. The longest name either service sends is in the forties, so no real
/// code is cut.
///
/// A name only, never the decorated form it arrives in. A `__type` carries a Smithy namespace
/// before the name and sometimes a URL after it, and together those run well past this — cutting to
/// it before the name has been found would take the name with the decorations, and turn a code the
/// match would have recognised into one it would not. So this is applied after the known spellings
/// have been matched, where a cut can shorten a code that was already unrecognised, and therefore
/// already `Retryable::No`, and nothing else.
#[lore_macro::test_pub]
const MAX_ERROR_CODE: usize = 64;

/// A name is part of the text it arrives in, so its bound cannot be the looser of the two. Checked
/// at build time rather than in a test, because it is a property of the constants rather than of
/// any behaviour.
const _: () = assert!(MAX_ERROR_CODE < MAX_SERVICE_TEXT);

/// `text`, cut to at most `max` bytes on a character boundary.
///
/// The boundary is what keeps the result text — truncating mid-character would make a hostile
/// response able to produce an invalid `ByteStr`.
fn bounded(text: &str, max: usize) -> ByteStr {
    if text.len() <= max {
        return ByteStr::copy_from_str(text);
    }

    let end = (0..=max)
        .rev()
        .find(|index| text.is_char_boundary(*index))
        .unwrap_or(0);

    ByteStr::copy_from_str(&text[..end])
}

/// Text the service chose, bounded.
#[lore_macro::test_pub]
pub(crate) fn service_text(text: &str) -> ByteStr {
    bounded(text, MAX_SERVICE_TEXT)
}

/// An exception name, bounded.
///
/// An unrecognised code survives verbatim so a new exception names itself in the metric rather than
/// collapsing into "unknown", which is exactly why its length cannot be the service's to choose.
#[lore_macro::test_pub]
pub(crate) fn error_code(text: &str) -> ByteStr {
    bounded(text, MAX_ERROR_CODE)
}

/// A service that answered with an error.
#[derive(Debug)]
pub struct ServiceError<E> {
    pub status: StatusCode,
    pub code: E,
    pub message: Option<ByteStr>,
    pub request_id: Option<ByteStr>,
    pub payload: ErrorPayload,
}

impl<E: ServiceErrorCode> ServiceError<E> {
    pub fn code_str(&self) -> &str {
        self.code.as_str()
    }

    /// The item a failed conditional write returned, if it asked for one.
    pub fn item(&self) -> Option<&Item> {
        match &self.payload {
            ErrorPayload::Item(item) => Some(item),
            _ => None,
        }
    }

    /// The reasons a cancelled transaction gave. Empty for any other error.
    pub fn cancellation_reasons(&self) -> &[CancellationReason] {
        match &self.payload {
            ErrorPayload::CancellationReasons(reasons) => reasons,
            _ => &[],
        }
    }
}

impl<E: ServiceErrorCode> fmt::Display for ServiceError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.code.as_str(), self.status.as_u16())?;
        if let Some(message) = &self.message {
            write!(f, ": {}", message.as_str())?;
        }
        Ok(())
    }
}

impl<E> PartialEq for ServiceError<E>
where
    E: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.status == other.status
            && self.code == other.code
            && self.message == other.message
            && self.payload == other.payload
    }
}

/// A service error code: an enum over what we branch on, with room for what we do not know.
pub trait ServiceErrorCode: fmt::Debug + Sized {
    /// The wire spelling. Kept as the service sent it, including for unknown codes, so a log or a
    /// metric label names the real thing.
    fn as_str(&self) -> &str;

    /// The same spelling as a `'static` literal, when the code is one this build knows.
    ///
    /// `None` for a code kept verbatim from the wire. A metric label built from a known code
    /// borrows the literal where it would otherwise own a copy — a cost every failed call pays, and
    /// the throttle is the failure this client sees most. An unknown code owns its label, which is
    /// the rare case and the one already paying for a copy at parse time.
    fn as_static_str(&self) -> Option<&'static str> {
        None
    }

    /// Parse from the wire, given the status for the bodyless case.
    fn from_wire(code: &str, status: StatusCode) -> Self;

    /// Whether re-sending the identical request could succeed.
    fn retryable(&self) -> Retryable;

    /// Whether the service rejected the credentials as expired.
    ///
    /// Deliberately not a [`Retryable`] variant: re-sending the same signature cannot help, so the
    /// answer to `retryable` stays `No`. What *can* help is discarding the cached credentials and
    /// signing again with fresh ones, and only the credential cache can do that — it has no other
    /// way to learn that its own view of an expiry disagreed with the service's.
    fn is_expired_credentials(&self) -> bool {
        false
    }
}

/// Whether, and how, a failure can be re-sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retryable {
    /// Re-sending cannot help. The caller sees this failure.
    No,
    /// Re-sending after a wait can help.
    Yes,
    /// The signature was rejected for its time. Re-sign against the service's clock and retry once.
    ClockSkew,
}

/// A failed call, whatever stage it failed at.
///
/// One type covering service, transport, credential and timeout failures, so a store sorts every
/// failure of a call in one place: [`is_overloaded`] says which of them mean back off and which are
/// real failures.
///
/// The service error is boxed so this type stays a machine word or two — it is the error half of
/// every operation's `Result`, so its size is on every call's return path, failing or not.
#[derive(Debug)]
pub enum AwsCallError<E> {
    Service(Box<ServiceError<E>>),
    Transport(TransportError),
    /// No credentials could be resolved to sign with. `transient` marks a provider that was shed or
    /// unreachable rather than one that answered, so the caller backs off instead of reporting a
    /// failure.
    Credentials {
        detail: String,
        transient: bool,
    },
    /// The operation's whole budget was spent across its attempts.
    Timeout,
}

impl<E: ServiceErrorCode> AwsCallError<E> {
    /// The service's error code, when a service answered.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Service(err) => Some(err.code.as_str()),
            _ => None,
        }
    }

    pub fn request_id(&self) -> Option<&str> {
        match self {
            Self::Service(err) => err.request_id.as_deref(),
            _ => None,
        }
    }

    /// The service error, for a caller that needs to read its code or payload.
    pub fn as_service_error(&self) -> Option<&ServiceError<E>> {
        match self {
            Self::Service(err) => Some(err),
            _ => None,
        }
    }

    /// Wrap a service error, so callers need not name the box.
    pub fn service(error: ServiceError<E>) -> Self {
        Self::Service(Box::new(error))
    }
}

/// Whether a status on its own says the request was shed, whatever code came with it.
pub fn shed_by_status(status: StatusCode) -> bool {
    status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS
}

/// Whether a failure means the service shed the request, so the caller should back off rather than
/// treat it as a bug.
///
/// The attempt loop waits out only a request that never reached the service; a service that answers
/// with congestion answers the caller, whose own ladder is the one that paces the retries.
///
/// [`Retryable::Yes`] rather than "not [`Retryable::No`]": a rejected signature is also retryable,
/// but after one clock correction it is a misconfiguration, and calling it backpressure would ask a
/// caller to keep retrying something that cannot succeed.
///
/// The status decides on its own as well, because a code is only recognised if this build has seen
/// its spelling. An intermediary in front of the service, or a store that answers the same
/// protocol, names a shed request whatever it likes; the status is the part of that answer every
/// one of them agrees on.
///
/// A transport failure counts when it is worth resending — a connection or a read that failed — or
/// when the client's connection limit held the request back, or kept it waiting until it ran out of
/// time, since the limit sheds load as a service does. A request that could not be built, or a
/// response this client will not read, fails the same way on every attempt, so backing off from it
/// only delays the report.
///
/// A credential provider that could not be asked counts too: the signing material is a call away,
/// and that call was shed or never arrived.
pub fn is_overloaded<E: ServiceErrorCode>(error: &AwsCallError<E>) -> bool {
    match error {
        AwsCallError::Service(service) => {
            service.code.retryable() == Retryable::Yes || shed_by_status(service.status)
        }
        AwsCallError::Transport(
            TransportError::ConnectionLimit | TransportError::ConnectionLimitTimeout,
        )
        | AwsCallError::Timeout => true,
        AwsCallError::Transport(transport) => transport.is_worth_resending(),
        AwsCallError::Credentials { transient, .. } => *transient,
    }
}

impl<E> From<TransportError> for AwsCallError<E> {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

impl<E: ServiceErrorCode> fmt::Display for AwsCallError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Service(err) => write!(f, "{err}"),
            Self::Transport(err) => write!(f, "{err}"),
            Self::Credentials { detail, .. } => write!(f, "no AWS credentials: {detail}"),
            Self::Timeout => write!(f, "operation timed out"),
        }
    }
}

impl<E: ServiceErrorCode> std::error::Error for AwsCallError<E> {}

impl<E: PartialEq> PartialEq for AwsCallError<E> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Service(a), Self::Service(b)) => a == b,
            // Transport and credential failures carry a message from the layer below, which is not
            // something to assert on; that they are the same kind of failure is.
            (Self::Transport(_), Self::Transport(_)) | (Self::Timeout, Self::Timeout) => true,
            (Self::Credentials { transient: a, .. }, Self::Credentials { transient: b, .. }) => {
                a == b
            }
            _ => false,
        }
    }
}

/// Where an operation's AWS request id is, for the slow-operation warning that reports it.
///
/// A successful output carries one only when its client was configured to capture request ids.
pub trait RequestId {
    fn request_id(&self) -> Option<&str>;
}

/// Implement [`RequestId`] for outputs that carry one in a `request_id` field.
macro_rules! carries_request_id {
    ($($output:ty),+ $(,)?) => {
        $(
            impl $crate::aws::error::RequestId for $output {
                fn request_id(&self) -> Option<&str> {
                    self.request_id.as_deref()
                }
            }
        )+
    };
}

pub(crate) use carries_request_id;

/// A boolean answer carries no request id. `table_exists` reduces a whole response to one, and it
/// runs once at startup, so there is nothing to report and nothing worth plumbing to report it.
impl RequestId for bool {
    fn request_id(&self) -> Option<&str> {
        None
    }
}

impl RequestId for () {
    fn request_id(&self) -> Option<&str> {
        None
    }
}

/// `DynamoDB`'s error codes, as far as anything branches on them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DynamoErrorCode {
    ConditionalCheckFailedException,
    TransactionCanceledException,
    TransactionInProgressException,
    TransactionConflictException,
    ResourceNotFoundException,
    ProvisionedThroughputExceededException,
    ThrottlingException,
    RequestLimitExceeded,
    InternalServerError,
    ServiceUnavailable,
    ValidationException,
    AccessDeniedException,
    ItemCollectionSizeLimitExceededException,
    IdempotentParameterMismatchException,
    RequestTimeTooSkewed,
    InvalidSignatureException,
    ExpiredTokenException,
    /// A code this build does not know, kept verbatim.
    Other(ByteStr),
}

impl ServiceErrorCode for DynamoErrorCode {
    fn as_str(&self) -> &str {
        match self {
            Self::Other(code) => code.as_str(),
            known => known.as_static_str().unwrap_or_default(),
        }
    }

    fn as_static_str(&self) -> Option<&'static str> {
        Some(match self {
            Self::ConditionalCheckFailedException => "ConditionalCheckFailedException",
            Self::TransactionCanceledException => "TransactionCanceledException",
            Self::TransactionInProgressException => "TransactionInProgressException",
            Self::TransactionConflictException => "TransactionConflictException",
            Self::ResourceNotFoundException => "ResourceNotFoundException",
            Self::ProvisionedThroughputExceededException => {
                "ProvisionedThroughputExceededException"
            }
            Self::ThrottlingException => "ThrottlingException",
            Self::RequestLimitExceeded => "RequestLimitExceeded",
            Self::InternalServerError => "InternalServerError",
            Self::ServiceUnavailable => "ServiceUnavailable",
            Self::ValidationException => "ValidationException",
            Self::AccessDeniedException => "AccessDeniedException",
            Self::ItemCollectionSizeLimitExceededException => {
                "ItemCollectionSizeLimitExceededException"
            }
            Self::IdempotentParameterMismatchException => "IdempotentParameterMismatchException",
            Self::RequestTimeTooSkewed => "RequestTimeTooSkewed",
            Self::InvalidSignatureException => "InvalidSignatureException",
            Self::ExpiredTokenException => "ExpiredTokenException",
            Self::Other(_) => return None,
        })
    }

    fn from_wire(code: &str, status: StatusCode) -> Self {
        match code {
            "ConditionalCheckFailedException" => Self::ConditionalCheckFailedException,
            "TransactionCanceledException" => Self::TransactionCanceledException,
            "TransactionInProgressException" => Self::TransactionInProgressException,
            "TransactionConflictException" => Self::TransactionConflictException,
            "ResourceNotFoundException" => Self::ResourceNotFoundException,
            "ProvisionedThroughputExceededException" => {
                Self::ProvisionedThroughputExceededException
            }
            // `TooManyRequests` is not one of DynamoDB's own spellings; it is what an intermediary
            // in front of the endpoint answers a shed request with, and it means the same thing.
            "ThrottlingException" | "ThrottledException" | "Throttling" | "TooManyRequests" => {
                Self::ThrottlingException
            }
            "RequestLimitExceeded" => Self::RequestLimitExceeded,
            "InternalServerError" | "InternalFailure" => Self::InternalServerError,
            "ServiceUnavailable" | "ServiceUnavailableException" => Self::ServiceUnavailable,
            "ValidationException" => Self::ValidationException,
            "AccessDeniedException" | "AccessDenied" => Self::AccessDeniedException,
            "ItemCollectionSizeLimitExceededException" => {
                Self::ItemCollectionSizeLimitExceededException
            }
            "IdempotentParameterMismatchException" => Self::IdempotentParameterMismatchException,
            "RequestTimeTooSkewed" | "RequestExpired" => Self::RequestTimeTooSkewed,
            "InvalidSignatureException" | "SignatureDoesNotMatch" => {
                Self::InvalidSignatureException
            }
            "ExpiredTokenException" | "ExpiredToken" => Self::ExpiredTokenException,
            // A 5xx with no code we recognise is still a server-side failure, and the status is the
            // only thing that says so.
            "" if status.is_server_error() => Self::InternalServerError,
            // The one 4xx that means "come back later" rather than "this request was wrong", so it
            // cannot fall through to the unrecognised-code arm below, which is not retried.
            "" if status == StatusCode::TOO_MANY_REQUESTS => Self::ThrottlingException,
            // Any other answer without a code is named by its status, as S3's is, so a log line or a
            // metric label never carries an empty code.
            "" => Self::Other(ByteStr::copy_from_str(status.as_str())),
            other => Self::Other(error_code(other)),
        }
    }

    fn retryable(&self) -> Retryable {
        match self {
            Self::ProvisionedThroughputExceededException
            | Self::ThrottlingException
            | Self::RequestLimitExceeded
            | Self::InternalServerError
            | Self::ServiceUnavailable
            // A transaction already in flight under this request token will finish; asking again is
            // how the caller finds out that it did.
            | Self::TransactionInProgressException
            // A single-item write onto an item some other transaction holds. The hold is released
            // when that transaction settles, so the same request succeeds once it has.
            | Self::TransactionConflictException => Retryable::Yes,
            Self::RequestTimeTooSkewed | Self::InvalidSignatureException => Retryable::ClockSkew,
            // Deliberately not retried: a cancelled transaction is a decision, not a hiccup, and
            // its reasons are what the caller reads. A throttled *whole* transaction arrives as
            // ThrottlingException instead, which is retried above.
            Self::TransactionCanceledException
            | Self::ConditionalCheckFailedException
            | Self::ResourceNotFoundException
            | Self::ValidationException
            | Self::AccessDeniedException
            | Self::ItemCollectionSizeLimitExceededException
            | Self::IdempotentParameterMismatchException
            | Self::ExpiredTokenException
            | Self::Other(_) => Retryable::No,
        }
    }

    fn is_expired_credentials(&self) -> bool {
        matches!(self, Self::ExpiredTokenException)
    }
}

impl DynamoErrorCode {
    pub fn is_conditional_check_failed_exception(&self) -> bool {
        matches!(self, Self::ConditionalCheckFailedException)
    }

    pub fn is_resource_not_found_exception(&self) -> bool {
        matches!(self, Self::ResourceNotFoundException)
    }

    pub fn is_transaction_canceled_exception(&self) -> bool {
        matches!(self, Self::TransactionCanceledException)
    }
}

/// S3's error codes, as far as anything branches on them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum S3ErrorCode {
    NoSuchKey,
    NoSuchBucket,
    NoSuchVersion,
    NotFound,
    AccessDenied,
    /// A 403 whose answer named no code, as every HEAD does. Distinct from [`Self::AccessDenied`]
    /// because the store did not say why it refused, and an expired session is one of the reasons.
    Forbidden,
    SlowDown,
    RequestTimeout,
    OperationAborted,
    InternalError,
    ServiceUnavailable,
    InvalidRequest,
    RequestTimeTooSkewed,
    SignatureDoesNotMatch,
    ExpiredToken,
    PreconditionFailed,
    /// The checksum the request declared, `Content-MD5` or `x-amz-checksum-*`, does not match the
    /// body S3 received.
    BadDigest,
    /// The `x-amz-content-sha256` the request signed does not match the body S3 received.
    XAmzContentSha256Mismatch,
    /// An `x-amz-checksum-*` that does not match the body received, as `MinIO` names it; S3 names
    /// it `BadDigest`.
    XAmzContentChecksumMismatch,
    /// Fewer bytes arrived than `Content-Length` declared.
    IncompleteBody,
    Other(ByteStr),
}

impl ServiceErrorCode for S3ErrorCode {
    fn as_str(&self) -> &str {
        match self {
            Self::Other(code) => code.as_str(),
            known => known.as_static_str().unwrap_or_default(),
        }
    }

    fn as_static_str(&self) -> Option<&'static str> {
        Some(match self {
            Self::NoSuchKey => "NoSuchKey",
            Self::NoSuchBucket => "NoSuchBucket",
            Self::NoSuchVersion => "NoSuchVersion",
            Self::NotFound => "NotFound",
            Self::AccessDenied => "AccessDenied",
            Self::Forbidden => "Forbidden",
            Self::SlowDown => "SlowDown",
            Self::RequestTimeout => "RequestTimeout",
            Self::OperationAborted => "OperationAborted",
            Self::InternalError => "InternalError",
            Self::ServiceUnavailable => "ServiceUnavailable",
            Self::InvalidRequest => "InvalidRequest",
            Self::RequestTimeTooSkewed => "RequestTimeTooSkewed",
            Self::SignatureDoesNotMatch => "SignatureDoesNotMatch",
            Self::ExpiredToken => "ExpiredToken",
            Self::PreconditionFailed => "PreconditionFailed",
            Self::BadDigest => "BadDigest",
            Self::XAmzContentSha256Mismatch => "XAmzContentSHA256Mismatch",
            Self::XAmzContentChecksumMismatch => "XAmzContentChecksumMismatch",
            Self::IncompleteBody => "IncompleteBody",
            Self::Other(_) => return None,
        })
    }

    fn from_wire(code: &str, status: StatusCode) -> Self {
        match code {
            "NoSuchKey" => Self::NoSuchKey,
            "NoSuchBucket" => Self::NoSuchBucket,
            "NoSuchVersion" => Self::NoSuchVersion,
            "NotFound" => Self::NotFound,
            "AccessDenied" | "AllAccessDisabled" => Self::AccessDenied,
            // Only `SlowDown` is a code AWS itself sends; the rest are what S3-compatible stores
            // answer a shed request with, and a store telling us to slow down means the same thing
            // whichever word it uses.
            "SlowDown" | "TooManyRequests" | "RequestThrottled" | "ThrottlingException" => {
                Self::SlowDown
            }
            "RequestTimeout" | "RequestTimeoutException" => Self::RequestTimeout,
            "OperationAborted" => Self::OperationAborted,
            "InternalError" => Self::InternalError,
            "ServiceUnavailable" => Self::ServiceUnavailable,
            "InvalidRequest" => Self::InvalidRequest,
            "RequestTimeTooSkewed" => Self::RequestTimeTooSkewed,
            "SignatureDoesNotMatch" => Self::SignatureDoesNotMatch,
            "ExpiredToken" | "TokenRefreshRequired" => Self::ExpiredToken,
            "PreconditionFailed" => Self::PreconditionFailed,
            "BadDigest" => Self::BadDigest,
            "XAmzContentSHA256Mismatch" => Self::XAmzContentSha256Mismatch,
            "XAmzContentChecksumMismatch" => Self::XAmzContentChecksumMismatch,
            "IncompleteBody" => Self::IncompleteBody,
            // HEAD carries no body, so the status is the whole answer. A 404 on HeadObject is what
            // `head_fragment` reads as "no such hash", and it must not be mistaken for a bucket
            // that is missing.
            "" => match status {
                StatusCode::NOT_FOUND => Self::NotFound,
                StatusCode::FORBIDDEN => Self::Forbidden,
                StatusCode::SERVICE_UNAVAILABLE => Self::ServiceUnavailable,
                StatusCode::REQUEST_TIMEOUT => Self::RequestTimeout,
                // The one 4xx that means "come back later" rather than "this request was wrong", so
                // it cannot fall through to the unrecognised-code arm below, which is not retried.
                StatusCode::TOO_MANY_REQUESTS => Self::SlowDown,
                status if status.is_server_error() => Self::InternalError,
                status => Self::Other(ByteStr::copy_from_str(status.as_str())),
            },
            other => Self::Other(error_code(other)),
        }
    }

    fn retryable(&self) -> Retryable {
        match self {
            Self::SlowDown
            | Self::RequestTimeout
            | Self::InternalError
            | Self::ServiceUnavailable
            // A conditional operation on the same resource is still running. It settles on its own,
            // and the request that lost the race succeeds once it has.
            | Self::OperationAborted
            // The body S3 received is not the body that was sent. The checksum and the payload hash
            // are computed from the bytes in hand, so a mismatch is damage on the way, and sending
            // the identical request again is the fix rather than a repeat of the same rejection.
            | Self::BadDigest
            | Self::XAmzContentSha256Mismatch
            | Self::XAmzContentChecksumMismatch
            | Self::IncompleteBody => Retryable::Yes,
            Self::RequestTimeTooSkewed | Self::SignatureDoesNotMatch => Retryable::ClockSkew,
            Self::NoSuchKey
            | Self::NoSuchBucket
            | Self::NoSuchVersion
            | Self::NotFound
            | Self::AccessDenied
            | Self::Forbidden
            | Self::InvalidRequest
            | Self::ExpiredToken
            | Self::PreconditionFailed
            | Self::Other(_) => Retryable::No,
        }
    }

    /// Only a named `ExpiredToken`. A HEAD's bare 403 can be an expired session token too, but it is
    /// as often a denied key, or a missing one read without `s3:ListBucket`, and refreshing on each
    /// of those would put a credential-provider call behind every refusal.
    fn is_expired_credentials(&self) -> bool {
        matches!(self, Self::ExpiredToken)
    }
}

impl S3ErrorCode {
    /// Whether the request named something that does not exist.
    ///
    /// `HeadObject` answers a missing key with a bare 404 and no code, while `GetObject` names
    /// `NoSuchKey`, so both spellings mean the same thing to a caller.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound | Self::NoSuchKey | Self::NoSuchBucket)
    }

    pub fn is_no_such_key(&self) -> bool {
        matches!(self, Self::NoSuchKey | Self::NotFound)
    }
}

/// STS error codes. Narrow on purpose: credential acquisition either works or is reported as-is.
///
/// A bodyless answer is named by its status. Only codes are classified here; whether a status says
/// the request was shed is [`shed_by_status`]'s question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StsErrorCode(pub ByteStr);

impl ServiceErrorCode for StsErrorCode {
    fn as_str(&self) -> &str {
        self.0.as_str()
    }

    fn from_wire(code: &str, status: StatusCode) -> Self {
        if code.is_empty() {
            Self(ByteStr::copy_from_str(status.as_str()))
        } else {
            Self(error_code(code))
        }
    }

    fn retryable(&self) -> Retryable {
        match self.0.as_str() {
            "Throttling" | "ThrottlingException" | "ServiceUnavailable" | "InternalFailure" => {
                Retryable::Yes
            }
            _ => Retryable::No,
        }
    }
}
