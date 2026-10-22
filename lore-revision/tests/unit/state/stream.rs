// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_revision::change::FileAction;
use lore_revision::change::Flags;
use lore_revision::change::NodeChange;
use lore_revision::change::NodeChangeState;
use lore_revision::node::NodeFlags;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::NodeMapping;
use lore_revision::state::State;
use lore_revision::state::stream::*;
use tokio::sync::oneshot;

use crate::fs::filesystem_provider::setup_test_execution;
use crate::fs::filesystem_provider::test_store_create;
use crate::helper::default_repository_creation_args;

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
        mapping: NodeMapping::root(repository, State::new()),
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
                emit(&changes, || change.clone()).await?;
                changes.closed().await;
                decided.send(()).expect("the test reads the decision");
                released_by_test.await.expect("the test releases the walk");
                emit(&changes, || change).await?;
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

/// `any` reduces each change to its verdict as it arrives, so it holds no change while the walk
/// it cut short unwinds.
#[test]
fn any_holds_no_change_while_the_walk_unwinds() {
    let answer = ChangeStream::<()>::nothing().any(|_change| true);

    assert!(
        size_of_val(&answer) < size_of::<NodeChange>(),
        "any holds {} bytes",
        size_of_val(&answer)
    );
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
                emit(&changes, || change).await?;
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
