// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use tokio::sync::mpsc;

use crate::change::NodeChange;
use crate::state::StateError;

/// Output destination for change records produced by the diff engine.
///
/// Two variants:
/// - `ChangeSink::Vec` — pushes each record onto an in-memory `Vec`,
///   matching the legacy buffered-diff behaviour used by filesystem
///   diff, merge, and capi consumers.
/// - `ChangeSink::Channel` — forwards each record through an `mpsc::Sender`
///   so the revision-diff path can stream records end-to-end without
///   materialising the full diff in memory.
///
/// `emit` is the synchronous-style per-change push. `task_sink` returns
/// an owned sink suitable for handing to a spawned `JoinSet` task; in the
/// `Channel` case it clones the sender, in the `Vec` case it produces a
/// per-task `Vec` that the caller finalizes back in via `finalize_task_sink`
/// after `JoinSet::join_next`.
pub enum ChangeSink<'a> {
    Vec(&'a mut Vec<NodeChange>),
    Channel(&'a mpsc::Sender<Result<NodeChange, StateError>>),
}

/// Owned per-task sink handed to a spawned `JoinSet` task. The streaming
/// variant clones the parent's sender; the buffered variant owns a fresh
/// `Vec` whose contents merge back into the parent when the task joins.
pub enum OwnedChangeSink {
    Vec(Vec<NodeChange>),
    Channel(mpsc::Sender<Result<NodeChange, StateError>>),
}

impl OwnedChangeSink {
    pub async fn emit(&mut self, change: NodeChange) -> Result<(), StateError> {
        match self {
            Self::Vec(v) => {
                v.push(change);
                Ok(())
            }
            Self::Channel(tx) => tx
                .send(Ok(change))
                .await
                .map_err(|_send_err| StateError::internal("Diff receiver dropped")),
        }
    }

    pub fn as_sink(&mut self) -> ChangeSink<'_> {
        match self {
            Self::Vec(v) => ChangeSink::Vec(v),
            Self::Channel(tx) => ChangeSink::Channel(tx),
        }
    }
}

impl<'a> ChangeSink<'a> {
    pub async fn emit(&mut self, change: NodeChange) -> Result<(), StateError> {
        match self {
            ChangeSink::Vec(v) => {
                v.push(change);
                Ok(())
            }
            ChangeSink::Channel(tx) => tx
                .send(Ok(change))
                .await
                .map_err(|_send_err| StateError::internal("Diff receiver dropped")),
        }
    }

    /// Produce an owned sink suitable for handing to a spawned task.
    pub fn task_sink(&self) -> OwnedChangeSink {
        match self {
            ChangeSink::Vec(_) => OwnedChangeSink::Vec(Vec::new()),
            ChangeSink::Channel(tx) => OwnedChangeSink::Channel((*tx).clone()),
        }
    }

    /// Reconcile a finished task's sink into this one. Buffered variant
    /// extends the parent `Vec` with the task's items. Streaming variant
    /// is a no-op — the task already forwarded its items through the
    /// cloned sender.
    pub fn finalize_task_sink(&mut self, task_sink: OwnedChangeSink) {
        if let (ChangeSink::Vec(v), OwnedChangeSink::Vec(task_v)) = (self, task_sink) {
            v.extend(task_v);
        }
        // Channel variant: task already streamed via its cloned sender.
        // Mixed variants are not produced by `task_sink()` and are silently
        // ignored if they ever occur.
    }
}
