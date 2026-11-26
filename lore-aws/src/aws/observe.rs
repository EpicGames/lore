// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The observe callback for operations on this client.
//!
//! Each operation is labelled `success`, `aws_code` and `slow`. `aws_code` is the service's error
//! code when a service answered, and the kind of failure when none did.
use std::time::Duration;

use lore_telemetry::LabelArray;
use lore_telemetry::observe::observe_result;
use opentelemetry::KeyValue;
use opentelemetry::StringValue;
use tracing::debug;
use tracing::warn;

use crate::aws::error::AwsCallError;
use crate::aws::error::RequestId;
use crate::aws::error::ServiceErrorCode;
use crate::aws::error::TransportError;

/// The result shape every operation on this client returns.
pub type AwsResult<T, E> = Result<T, AwsCallError<E>>;

const REQUEST_ID_FIELD: &str = "request_id";
const ELAPSED_MS_FIELD: &str = "elapsed_ms";
const AWS_ERROR_CODE_LABEL_KEY: &str = "aws_code";

/// Time an operation, labelling it with its outcome, its AWS error code and whether it was slow.
pub fn observe_aws_operation_callback<T, E>(
    slow_threshold: Duration,
) -> impl Fn(&AwsResult<T, E>, &Duration, &mut LabelArray) + Copy
where
    T: RequestId,
    E: ServiceErrorCode,
{
    move |result: &AwsResult<T, E>, elapsed: &Duration, labels: &mut LabelArray| {
        observe_result(result, elapsed, labels);

        if let Err(error) = &result {
            let code = match error {
                // A code this build knows is a literal, so the label borrows it. Only one kept
                // verbatim from the wire owns a copy, and that one already paid for a copy at parse
                // time.
                AwsCallError::Service(service) => match service.code.as_static_str() {
                    Some(known) => StringValue::from(known),
                    None => StringValue::from(service.code.as_str().to_owned()),
                },
                AwsCallError::Timeout => unanswered(error, elapsed, "Timeout"),
                AwsCallError::Credentials {
                    transient: true, ..
                } => unanswered(error, elapsed, "CredentialsTransient"),
                AwsCallError::Credentials { .. } => unanswered(error, elapsed, "Credentials"),
                AwsCallError::Transport(TransportError::Io(_)) => {
                    unanswered(error, elapsed, "TransportIo")
                }
                AwsCallError::Transport(TransportError::Body(_)) => {
                    unanswered(error, elapsed, "TransportBody")
                }
                AwsCallError::Transport(TransportError::Build(_)) => {
                    unanswered(error, elapsed, "TransportBuild")
                }
                AwsCallError::Transport(TransportError::Protocol(_)) => {
                    unanswered(error, elapsed, "TransportProtocol")
                }
                AwsCallError::Transport(TransportError::ConnectionLimit) => unsent(elapsed),
                AwsCallError::Transport(TransportError::ConnectionLimitTimeout) => {
                    unanswered(error, elapsed, "ConnectionLimitTimeout")
                }
            };

            labels.push(KeyValue::new(AWS_ERROR_CODE_LABEL_KEY, code));
        }

        let is_slow = if *elapsed > slow_threshold {
            let request_id = match &result {
                Ok(output) => output.request_id(),
                Err(error) => error.request_id(),
            }
            .unwrap_or("<unknown>");

            warn!(
                {ELAPSED_MS_FIELD} = elapsed.as_millis(),
                slow_threshold = ?slow_threshold,
                {REQUEST_ID_FIELD} = request_id,
                "Operation execution time exceeded slow operation threshold"
            );

            true
        } else {
            false
        };
        labels.push(KeyValue::new("slow", is_slow));
    }
}

/// Log a failure no service answered, returning `kind` as its label.
///
/// There is no service code to label these with, and they are the failures a TAM has to be shown.
fn unanswered<E: ServiceErrorCode>(
    error: &AwsCallError<E>,
    elapsed: &Duration,
    kind: &'static str,
) -> StringValue {
    warn!(
        {ELAPSED_MS_FIELD} = elapsed.as_millis(),
        error = %error,
        "AWS call failed before the service answered"
    );

    StringValue::from(kind)
}

/// Log a call this client never sent, because every connection its `max_connections` setting
/// allows stayed in use for as long as the call could wait for one, returning its label.
///
/// No service took part, so the message names the client's own limit rather than AWS. It is logged
/// at debug, as a shed store call is: under load it is ordinary, and the label already counts it.
fn unsent(elapsed: &Duration) -> StringValue {
    debug!(
        { ELAPSED_MS_FIELD } = elapsed.as_millis(),
        "AWS call not sent: no connection was released in time, and max_connections allows no \
         more"
    );

    StringValue::from("ConnectionLimit")
}
