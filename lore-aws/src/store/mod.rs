// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod immutable_store;
pub mod lock_store;
pub mod mutable_store;
pub mod object_metadata;
#[cfg(not(feature = "test-util"))]
mod single_flight;
#[cfg(feature = "test-util")]
pub mod single_flight;

use lore_base::error::SlowDown;
use lore_storage::StoreError;
use tracing::debug;
use tracing::warn;

use crate::aws::error::AwsCallError;
use crate::aws::error::ServiceErrorCode;
use crate::aws::error::is_overloaded;
use crate::aws_error::AwsError;

/// Whether a failed call went unanswered because the service shed it, rather than failing.
///
/// An [`AwsError::Aws`] that [`is_overloaded`] accepts is shed: a spent budget, a transport failure
/// it counts as transient, a request the client's connection limit held back for as long as it
/// could wait or kept waiting until it ran out of time, an HTTP 429 or 5xx, a code the service
/// marks as retryable, or a credential provider that could not be reached. So is
/// [`AwsError::UnprocessedKeys`], a batch read whose table kept shedding part of it. Everything
/// else is a failure, including a service error naming a misconfiguration such as `AccessDenied`
/// and every other [`AwsError`] variant.
///
/// The stores report a shed call as [`SlowDown`], which their retry ladders reconsider, and any
/// other failure as internal, which they do not. Neither is reported as not-found: a failed read
/// says nothing about whether the content exists.
pub(crate) fn is_store_overloaded<C: ServiceErrorCode>(error: &AwsError<AwsCallError<C>>) -> bool {
    match error {
        AwsError::Aws(call_error) => is_overloaded(call_error),
        AwsError::UnprocessedKeys { .. } => true,
        AwsError::StalledCursor | AwsError::JoinError => false,
    }
}

/// The store error for a failed call: [`SlowDown`] when it was shed, internal otherwise.
///
/// A shed call is logged at debug, since shedding is ordinary under load and every call's latency
/// histogram already counts it by error code. Any other failure is logged at warn, under `context`.
#[track_caller]
pub(crate) fn store_failure<C>(
    error: AwsError<AwsCallError<C>>,
    context: &'static str,
) -> StoreError
where
    C: ServiceErrorCode + Send + Sync + 'static,
{
    if is_store_overloaded(&error) {
        debug!(%error, context, "A store call was shed, backing off");
        return StoreError::from(SlowDown);
    }

    warn!(%error, context, "AWS store call failed");
    StoreError::internal_with_context(error, context)
}

/// Run `operation` again while it fails with a back-off and the store retry ladder has a wait left.
///
/// One wait per store operation rather than per HTTP request: the client does not retry a service
/// that answered, so this is the only ladder a shed call meets.
pub(crate) async fn with_backoff<T, E, F, Fut>(
    is_slow_down: fn(&E) -> bool,
    mut operation: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let mut backoff = lore_storage::store_retry();

    loop {
        match operation().await {
            Err(error) if is_slow_down(&error) && backoff.wait().await => {}
            outcome => return outcome,
        }
    }
}
