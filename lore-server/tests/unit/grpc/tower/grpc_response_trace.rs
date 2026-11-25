// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::convert::Infallible;
use std::future::Ready;
use std::task::Context;
use std::task::Poll;

use http::Request;
use http::Response;
use lore_server::grpc::tower::grpc_response_trace::*;
use tonic::Code;
use tonic::Status;
use tower::Layer;
use tower::Service;
use tracing::Level;

use crate::log_capture::CapturedLogs;

/// Answers its one request with the response it holds.
struct Inner(Option<Response<()>>);

impl Service<Request<()>> for Inner {
    type Response = Response<()>;
    type Error = Infallible;
    type Future = Ready<Result<Response<()>, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _request: Request<()>) -> Self::Future {
        std::future::ready(Ok(self.0.take().expect("one request")))
    }
}

/// A trailers-only response, with tonic's own encoding of the status in its headers.
fn status_in_headers(code: Code, message: &str) -> Response<()> {
    Status::new(code, message.to_owned()).into_http()
}

async fn serve(response: Response<()>) {
    let mut service = GrpcResponseTraceLayer.layer(Inner(Some(response)));
    service
        .call(Request::new(()))
        .await
        .expect("the inner service is infallible");
}

/// The one line logging the request's outcome, with what it says checked against `level` and
/// `message`.
fn outcome(logs: &CapturedLogs, level: &str, message: &str) -> serde_json::Value {
    const OUTCOMES: [&str; 3] = [
        "Lore success response",
        "Lore user error response",
        "Lore server error response",
    ];
    let mut lines: Vec<_> = logs
        .lines()
        .into_iter()
        .filter(|line| {
            OUTCOMES
                .iter()
                .any(|outcome| line["fields"]["message"] == *outcome)
        })
        .collect();
    assert_eq!(lines.len(), 1, "outcome lines: {lines:?}");
    let line = lines.remove(0);
    assert_eq!(line["fields"]["message"], message, "{line:?}");
    assert_eq!(line["level"], level, "{line:?}");
    line
}

mod call {
    use super::*;

    /// The message is percent-encoded on the wire; the log carries it decoded.
    #[tokio::test]
    async fn logs_the_decoded_message_of_a_server_error() {
        let (logs, _guard) = CapturedLogs::start_at(Level::DEBUG);

        serve(status_in_headers(
            Code::Internal,
            "Unexpected EOF decoding stream.",
        ))
        .await;

        let line = outcome(&logs, "WARN", "Lore server error response");
        assert_eq!(line["fields"]["rpc_status_code"], "Internal");
        assert_eq!(
            line["fields"]["rpc_status_message"],
            "Unexpected EOF decoding stream."
        );
    }

    #[tokio::test]
    async fn logs_a_message_holding_percent_signs_and_multibyte_text() {
        let (logs, _guard) = CapturedLogs::start_at(Level::DEBUG);

        serve(status_in_headers(
            Code::Unavailable,
            "store 100% busy: café",
        ))
        .await;

        let line = outcome(&logs, "WARN", "Lore server error response");
        assert_eq!(line["fields"]["rpc_status_code"], "Unavailable");
        assert_eq!(
            line["fields"]["rpc_status_message"],
            "store 100% busy: café"
        );
    }

    /// Status details that are not base64 leave the logged message intact.
    #[tokio::test]
    async fn logs_the_message_beside_details_that_are_not_base64() {
        let (logs, _guard) = CapturedLogs::start_at(Level::DEBUG);

        let mut response = status_in_headers(Code::Internal, "store failed");
        response.headers_mut().insert(
            "grpc-status-details-bin",
            http::HeaderValue::from_static("not base64!"),
        );
        serve(response).await;

        let line = outcome(&logs, "WARN", "Lore server error response");
        assert_eq!(line["fields"]["rpc_status_message"], "store failed");
    }

    #[tokio::test]
    async fn logs_a_server_error_without_a_message_without_the_message_field() {
        let (logs, _guard) = CapturedLogs::start_at(Level::DEBUG);

        let mut response = status_in_headers(Code::Internal, "");
        response.headers_mut().remove("grpc-message");
        serve(response).await;

        let line = outcome(&logs, "WARN", "Lore server error response");
        assert!(
            line["fields"].get("rpc_status_message").is_none(),
            "{line:?}"
        );
    }

    #[tokio::test]
    async fn logs_a_caller_error_as_a_user_error() {
        let (logs, _guard) = CapturedLogs::start_at(Level::DEBUG);

        serve(status_in_headers(Code::InvalidArgument, "bad request")).await;

        let line = outcome(&logs, "DEBUG", "Lore user error response");
        assert_eq!(line["fields"]["rpc_status_code"], "InvalidArgument");
    }

    #[tokio::test]
    async fn logs_a_response_without_a_status_as_success() {
        let (logs, _guard) = CapturedLogs::start_at(Level::DEBUG);

        serve(Response::new(())).await;

        let line = outcome(&logs, "DEBUG", "Lore success response");
        assert_eq!(line["fields"]["rpc_status_code"], "Ok");
    }
}
