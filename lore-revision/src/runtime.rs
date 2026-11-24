// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::atomic::fence;

use lore_base::error::Cancelled;
pub use lore_base::runtime::LORE_CONTEXT;
pub use lore_base::runtime::runtime;
use tokio_util::sync::CancellationToken;
use tokio_util::sync::WaitForCancellationFutureOwned;

use crate::interface::ExecutionContext;

/// Get the current `ExecutionContext` from the task-local. Panics if not set.
pub fn execution_context() -> std::sync::Arc<ExecutionContext> {
    lore_base::runtime::lore_context()
        .downcast::<ExecutionContext>()
        .expect("ExecutionContext not set on LORE_CONTEXT")
}

/// Get the current `ExecutionContext` if set, or `None`.
pub fn try_execution_context() -> Option<std::sync::Arc<ExecutionContext>> {
    lore_base::runtime::try_lore_context()?
        .downcast::<ExecutionContext>()
        .ok()
}

/// Fails once the current call has been cancelled.
///
/// Call it only where returning leaves no partial write behind: before an operation's first
/// write, or between steps of read-only work. Never call it after a write that later steps must
/// follow up, such as storing a branch's latest revision, which a notification has to report. A
/// task inherits its spawner's context, so this also holds for code a spawned task runs.
pub fn check_cancelled() -> Result<(), Cancelled> {
    let cancelled = LORE_CONTEXT
        .try_with(|context| {
            context
                .downcast_ref::<ExecutionContext>()
                .and_then(ExecutionContext::cancellation)
                .is_some_and(Cancellation::is_cancelled)
        })
        .unwrap_or(false);
    if cancelled { Err(Cancelled) } else { Ok(()) }
}

/// The current call's cancellation, looked up once for a loop that checks it at every step.
#[derive(Clone, Debug, Default)]
pub struct CancellationCheck(Option<Cancellation>);

impl CancellationCheck {
    /// The cancellation of the call the current task serves. Nothing cancels it outside a call.
    pub fn current() -> Self {
        Self(current_cancellation())
    }

    /// Fails once the call has been cancelled, under the same rules as [`check_cancelled`].
    pub fn check(&self) -> Result<(), Cancelled> {
        if self.0.as_ref().is_some_and(Cancellation::is_cancelled) {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }
}

/// The current call's cancellation, if anything can cancel it.
pub fn current_cancellation() -> Option<Cancellation> {
    LORE_CONTEXT
        .try_with(|context| {
            context
                .downcast_ref::<ExecutionContext>()
                .and_then(ExecutionContext::cancellation)
                .cloned()
        })
        .ok()
        .flatten()
}

/// Whether a call has been cancelled, and a way to wait until it is.
///
/// A check reads one atomic flag per level: the call's own and those of the cancellations it was
/// made a child of, and takes no lock. Code that waits for cancellation rather than polling for it
/// waits on a token, made by the first wait, as most calls never wait. A clone shares the same
/// cancellation.
#[derive(Clone, Debug, Default)]
pub struct Cancellation(Arc<CancellationNode>);

#[derive(Debug, Default)]
struct CancellationNode {
    cancelled: AtomicBool,
    parent: Option<Arc<CancellationNode>>,
    /// Cancelled along with `cancelled` once it exists. A child's is a child of its parent's.
    token: OnceLock<CancellationToken>,
}

impl CancellationNode {
    fn token(&self) -> &CancellationToken {
        let token = self.token.get_or_init(|| match &self.parent {
            Some(parent) => parent.token().child_token(),
            None => CancellationToken::new(),
        });
        // A cancel that ran before the token existed found no token to cancel. The fence pairs
        // with the one in `cancel`, so one of the two sees the other's write.
        fence(Ordering::SeqCst);
        if self.cancelled.load(Ordering::Relaxed) {
            token.cancel();
        }
        token
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
            || self
                .parent
                .as_deref()
                .is_some_and(CancellationNode::is_cancelled)
    }
}

impl Cancellation {
    pub fn new() -> Self {
        Self::default()
    }

    /// A cancellation that is cancelled along with this one, and can also be cancelled alone.
    pub fn child(&self) -> Self {
        Self(Arc::new(CancellationNode {
            cancelled: AtomicBool::new(false),
            parent: Some(self.0.clone()),
            token: OnceLock::new(),
        }))
    }

    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Relaxed);
        fence(Ordering::SeqCst);
        if let Some(token) = self.0.token.get() {
            token.cancel();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    /// Whether a cancellation this was made a child of has been cancelled, as opposed to this
    /// one alone.
    pub fn is_parent_cancelled(&self) -> bool {
        self.0
            .parent
            .as_deref()
            .is_some_and(CancellationNode::is_cancelled)
    }

    /// Resolves once this is cancelled.
    pub fn cancelled(&self) -> WaitForCancellationFutureOwned {
        self.0.token().clone().cancelled_owned()
    }

    /// Cancels this when the returned guard is dropped.
    pub fn cancel_on_drop(self) -> CancelOnDrop {
        CancelOnDrop(self)
    }
}

/// Cancels its [`Cancellation`] when dropped.
#[derive(Debug)]
pub struct CancelOnDrop(Cancellation);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
