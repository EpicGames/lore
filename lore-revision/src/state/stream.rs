// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use std::num::NonZeroUsize;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::change::NodeChange;
use crate::state::StateError;

/// Where a diff walk emits the changes it finds.
///
/// Cloned into each subtree a walk spawns, so a subtree emits to the caller directly rather than
/// collecting for a parent to fold in. Carries changes alone: what a walk could not answer is
/// its own failure, reported where it ends rather than in place of a change.
pub type ChangeSender = mpsc::Sender<NodeChange>;

/// Emits the change `build` makes to the caller reading them, once the channel has room for it.
///
/// `build` runs after the wait for room, so a walk waiting to emit holds what it builds the
/// change from rather than the change. A closed channel is a caller that has stopped listening,
/// which the walk learns of here: the error unwinds it, and a caller that closed deliberately
/// already has its answer and discards that verdict.
#[lore_macro::test_pub]
pub(crate) async fn emit(
    changes: &ChangeSender,
    build: impl FnOnce() -> NodeChange,
) -> Result<(), StateError> {
    let permit = changes
        .reserve()
        .await
        .map_err(|_closed| StateError::internal("Diff receiver dropped"))?;
    permit.send(build());
    Ok(())
}

/// How many changes a diff may run ahead of the caller reading them, for a caller with no depth
/// of its own in mind.
///
/// A walk that outruns its reader is holding change records nobody has looked at, which is what
/// taking them one at a time is for; a walk held to one at a time spends its parallelism waiting.
const CHANGE_LOOKAHEAD: NonZeroUsize = NonZeroUsize::new(1000).expect("a nonzero literal");

/// How many changes [`ChangeStream::collect`] gathers per chunk. A power of two, so the first
/// chunk, grown by doubling, ends at exactly this capacity.
const COLLECT_CHUNK: usize = 1 << 16;

/// The changes a diff finds, as it finds them, and what the walk reports once it ends.
///
/// Owns the walk as well as its output. Dropping this closes the channel, so the walk unwinds at
/// its next emit rather than running on for a caller that has gone — which is how a caller
/// probing for one kind of change stops the walk once it has found one.
///
/// Ways to read it, and which one a caller wants is what it means to have this:
///
/// - [`collect`](Self::collect) for a caller that transforms the change set as a whole
/// - [`next`](Self::next) then [`finish`](Self::finish) for one reading each change once
/// - [`any`](Self::any) for one asking whether a change of some kind is there at all
/// - [`abandon`](Self::abandon) for one that has seen enough and must not outrun the walk
/// - dropping it for one that has seen enough and has no reason to wait
///
/// [`finish`](Self::finish) is not ceremony: a walk that marks as it goes leaves those marks as
/// its real answer, and only waiting for it tells a caller they are complete.
#[must_use = "dropping a change stream stops the walk it owns"]
pub struct ChangeStream<Summary> {
    changes: mpsc::Receiver<NodeChange>,
    /// The walk producing the changes, or `None` where there is nothing to walk.
    walk: Option<JoinHandle<Result<Summary, StateError>>>,
}

impl<Summary: Default + Send + 'static> ChangeStream<Summary> {
    /// Spawns `walk` behind a channel, and answers with the changes it emits.
    ///
    /// `walk` is handed the sender to emit through, so how it walks and what it reports stay its
    /// own. A caller that holds what it reads somewhere else as well wants the walk to run less
    /// far ahead of it, and says so with
    /// [`spawn_with_lookahead`](Self::spawn_with_lookahead).
    pub fn spawn<Walk, Walking>(walk: Walk) -> Self
    where
        Walk: FnOnce(ChangeSender) -> Walking,
        Walking: Future<Output = Result<Summary, StateError>> + Send + 'static,
    {
        Self::spawn_with_lookahead(CHANGE_LOOKAHEAD, walk)
    }

    /// [`spawn`](Self::spawn) with the walk held to `lookahead` changes ahead of its reader.
    ///
    /// Nonzero because a channel of no depth is one no change fits through, which the channel
    /// itself refuses rather than deadlocks on.
    pub fn spawn_with_lookahead<Walk, Walking>(lookahead: NonZeroUsize, walk: Walk) -> Self
    where
        Walk: FnOnce(ChangeSender) -> Walking,
        Walking: Future<Output = Result<Summary, StateError>> + Send + 'static,
    {
        let (sender, changes) = mpsc::channel(lookahead.get());
        ChangeStream {
            changes,
            walk: Some(lore_spawn!(crate::subtask!(walk(sender)))),
        }
    }

    /// A walk with nothing to report, for a path the filter leaves out entirely.
    pub fn nothing() -> Self {
        let (sender, changes) = mpsc::channel(1);
        drop(sender);
        ChangeStream {
            changes,
            walk: None,
        }
    }

    /// The next change, or `None` once the walk has no more.
    ///
    /// A walk that failed reports it from [`finish`](Self::finish), so `None` is the end of the
    /// changes and not yet the verdict on them.
    pub async fn next(&mut self) -> Option<NodeChange> {
        self.changes.recv().await
    }

    /// Every change the walk finds.
    ///
    /// Drains as the walk produces, so the lookahead never holds a walk against a caller that is
    /// collecting. A caller that wants what the walk reported reads the changes with
    /// [`next`](Self::next) and then asks [`finish`](Self::finish).
    ///
    /// The changes are gathered in chunks of [`COLLECT_CHUNK`] and moved into one list of exactly
    /// their number once the walk ends. A list grown by doubling holds up to twice the changes'
    /// size, and three times it while it moves to its last allocation; a tree of millions of
    /// files makes that gigabytes.
    pub async fn collect(self) -> Result<Vec<NodeChange>, StateError> {
        let ChangeStream { mut changes, walk } = self;
        let mut full_chunks: Vec<Vec<NodeChange>> = Vec::new();
        let mut chunk = Vec::new();
        while let Some(change) = changes.recv().await {
            if chunk.len() == COLLECT_CHUNK {
                full_chunks.push(std::mem::replace(
                    &mut chunk,
                    Vec::with_capacity(COLLECT_CHUNK),
                ));
            }
            chunk.push(change);
        }
        joined(walk).await?;
        if full_chunks.is_empty() {
            return Ok(chunk);
        }
        let mut collected = Vec::with_capacity(full_chunks.len() * COLLECT_CHUNK + chunk.len());
        for full_chunk in full_chunks {
            collected.extend(full_chunk);
        }
        collected.extend(chunk);
        Ok(collected)
    }

    /// Whether the walk finds a change `wanted` accepts, answered at the first one that does.
    ///
    /// Stops the walk at that change by [abandoning](Self::abandon) it rather than reading the
    /// rest: one change is the whole of the answer, and a walk still looking for a second is work
    /// nobody asked for. What the walk would have reported goes unread, which is why this is for
    /// a walk whose only output is the changes it was cut short of finding.
    ///
    /// Either way the walk has unwound when this answers, so what it captured is released before
    /// the caller acts on the answer.
    pub async fn any(mut self, wanted: impl Fn(&NodeChange) -> bool) -> Result<bool, StateError> {
        while let Some(found) = self.next().await.map(|change| wanted(&change)) {
            if found {
                self.abandon().await;
                return Ok(true);
            }
        }
        joined(self.walk).await?;
        Ok(false)
    }

    /// Ends the walk where it stands, and waits for it to stop.
    ///
    /// Closes the channel so the walk unwinds at its next emit, then joins it, so what the walk
    /// captured is released by the time this answers. What it reports is then the closed channel
    /// rather than what it found, so nothing comes back. A caller with no reason to wait drops
    /// the stream instead.
    pub async fn abandon(self) {
        let ChangeStream { changes, walk } = self;
        drop(changes);
        if let Some(walk) = walk {
            let _stopped = walk.await;
        }
    }

    /// What the walk reported, for a caller that has read the changes it came for.
    ///
    /// Lets the walk finish rather than cutting it short, so a marking walk's flags are complete
    /// when this answers. A caller that wants it stopped drops the stream instead.
    pub async fn finish(self) -> Result<Summary, StateError> {
        let ChangeStream { mut changes, walk } = self;
        while changes.recv().await.is_some() {}
        joined(walk).await
    }
}

/// What a joined walk reported, and nothing where there was no walk.
async fn joined<Summary: Default>(
    walk: Option<JoinHandle<Result<Summary, StateError>>>,
) -> Result<Summary, StateError> {
    match walk {
        Some(walk) => walk
            .await
            .internal("Diff task failed")
            .map_err(StateError::from)?,
        None => Ok(Summary::default()),
    }
}
