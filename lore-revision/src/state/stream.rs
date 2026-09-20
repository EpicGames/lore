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

/// Emits one change to the caller reading them.
///
/// A closed channel is a caller that has stopped listening, which the walk learns of here: the
/// error unwinds it, and a caller that closed deliberately already has its answer and discards
/// that verdict.
pub(crate) async fn emit(changes: &ChangeSender, change: NodeChange) -> Result<(), StateError> {
    changes
        .send(change)
        .await
        .map_err(|_closed| StateError::internal("Diff receiver dropped"))
}

/// How many changes a diff may run ahead of the caller reading them, for a caller with no depth
/// of its own in mind.
///
/// A walk that outruns its reader is holding change records nobody has looked at, which is what
/// taking them one at a time is for; a walk held to one at a time spends its parallelism waiting.
const CHANGE_LOOKAHEAD: NonZeroUsize = NonZeroUsize::new(1000).expect("a nonzero literal");

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
            walk: Some(lore_spawn!(walk(sender))),
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
    pub async fn collect(self) -> Result<Vec<NodeChange>, StateError> {
        let ChangeStream { mut changes, walk } = self;
        let mut collected = Vec::new();
        while let Some(change) = changes.recv().await {
            collected.push(change);
        }
        joined(walk).await?;
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
        while let Some(change) = self.next().await {
            if wanted(&change) {
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use lore_base::lore_spawn;
    use lore_base::runtime::LORE_CONTEXT;
    use tokio::sync::oneshot;

    use super::*;
    use crate::change::FileAction;
    use crate::change::Flags;
    use crate::change::NodeChangeState;
    use crate::fs::filesystem_provider::tests::setup_test_execution;
    use crate::fs::filesystem_provider::tests::test_store_create;
    use crate::lore::Address;
    use crate::node::NodeFlags;
    use crate::repository::RepositoryContext;
    use crate::repository::test_helpers::default_repository_creation_args;
    use crate::state::NodeMapping;
    use crate::state::State;

    /// Records that the walk holding it has unwound. Stands for what a real walk captures and
    /// holds until it ends, such as the repository and the filesystem operation it reads through.
    struct Guard(Arc<AtomicBool>);

    impl Guard {
        /// A guard, and the flag that reads whether it has dropped.
        fn new() -> (Guard, Arc<AtomicBool>) {
            let released = Arc::new(AtomicBool::new(false));
            (Guard(released.clone()), released)
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    /// A change for a walk to emit. What it holds does not matter: the predicates below answer
    /// without reading it.
    async fn a_change() -> NodeChange {
        let (immutable_store, mutable_store, _execution) =
            test_store_create().await.expect("making test stores");
        let repository = Arc::new(RepositoryContext::new(default_repository_creation_args(
            immutable_store,
            mutable_store,
        )));
        let side = NodeChangeState {
            mapping: NodeMapping::root(repository, Arc::new(State::new())),
            observed: None,
            flags: NodeFlags::NoFlags,
            address: Address::default(),
            mode: 0,
        };
        NodeChange {
            action: FileAction::Keep,
            flags: Flags::None,
            from: side.clone(),
            to: side,
        }
    }

    /// `any` answers at the first change the caller accepts and cuts the walk short there. The
    /// walk holds what it captured until its next emit reaches the closed channel, so an answer
    /// ahead of that lets the caller tear down what the walk is still reading through.
    ///
    /// The walk emits the change `any` accepts, parks on the closed channel, which is `any`
    /// having decided, and unwinds at the emit that finds the channel closed, which is the
    /// report `any` discards rather than answers with.
    #[tokio::test]
    async fn any_answers_once_the_walk_it_cut_short_has_unwound() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let change = a_change().await;
                let (guard, released) = Guard::new();
                let (decided, decided_by_any) = oneshot::channel();
                let (release, released_by_test) = oneshot::channel();

                let stream = ChangeStream::spawn(async move |changes| {
                    let _guard = guard;
                    emit(&changes, change.clone()).await?;
                    changes.closed().await;
                    decided.send(()).expect("the test reads the decision");
                    released_by_test.await.expect("the test releases the walk");
                    emit(&changes, change).await?;
                    Ok(())
                });

                let answered = {
                    let released = released.clone();
                    lore_spawn!(async move {
                        let found = stream.any(|_change| true).await;
                        (found, released.load(Ordering::Acquire))
                    })
                };

                tokio::time::timeout(Duration::from_secs(5), decided_by_any)
                    .await
                    .expect("any decides rather than reading on")
                    .expect("the walk reads the closed channel");
                assert!(
                    !released.load(Ordering::Acquire),
                    "the walk holds its guard until it unwinds"
                );
                release.send(()).expect("the walk waits to be released");

                let (found, released_when_answered) =
                    answered.await.expect("the task reading the walk");
                assert!(found.expect("a walk cut short reports no failure"));
                assert!(
                    released_when_answered,
                    "any answered before the walk it cut short had unwound"
                );
            })
            .await;
    }

    /// A walk offering nothing the caller accepts is read to its end rather than cut short, so
    /// `any` answers on what the walk reported and the walk has unwound by then either way.
    #[tokio::test]
    async fn any_reads_to_the_end_of_a_walk_it_accepts_nothing_from() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let change = a_change().await;
                let (guard, released) = Guard::new();

                let stream = ChangeStream::spawn(async move |changes| {
                    let _guard = guard;
                    emit(&changes, change).await?;
                    Ok(())
                });

                let found = stream
                    .any(|_change| false)
                    .await
                    .expect("a walk that ran to its end reports no failure");

                assert!(!found);
                assert!(
                    released.load(Ordering::Acquire),
                    "any answered before the walk had unwound"
                );
            })
            .await;
    }
}
