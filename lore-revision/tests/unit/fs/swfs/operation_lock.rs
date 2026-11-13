// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use std::sync::Arc;
use std::time::Duration;

use lore_base::error::NotSupported;
use lore_base::lore_spawn;
use lore_revision::fs::swfs::operation_lock::OperationLock;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// How long a command that has reached the operation lock is watched, to see that it goes on
/// waiting. Every passing run waits it out, so it is kept short.
const STILL_WAITING: Duration = Duration::from_millis(10);

/// Spawns `acquire`, and returns once it has begun acquiring the operation. The runtime must run
/// one thread, and `acquire` must reach the lock without first awaiting anything else: then the
/// spawned task's first poll, which sends the signal and reaches the lock, ends before the signal
/// is received, so the task has reached the lock when this returns.
async fn spawn_acquiring<T: Send + 'static>(
    acquire: impl Future<Output = T> + Send + 'static,
) -> JoinHandle<T> {
    assert_eq!(
        tokio::runtime::Handle::current().runtime_flavor(),
        tokio::runtime::RuntimeFlavor::CurrentThread,
        "the signal shows the task reached the lock only on a one-thread runtime"
    );
    let (acquiring, started) = oneshot::channel();
    let task = lore_spawn!(async move {
        acquiring
            .send(())
            .expect("the test is waiting for the signal");
        acquire.await
    });
    started.await.expect("the task signals before it acquires");
    task
}

/// Asserts that `task`, which has reached the lock, is still waiting there.
async fn assert_still_waiting<T: std::fmt::Debug>(task: &mut JoinHandle<T>, why: &str) {
    let finished = tokio::time::timeout(STILL_WAITING, task).await;
    assert!(finished.is_err(), "{why}: {finished:?}");
}

#[tokio::test]
async fn a_second_command_waits_until_the_first_has_finished() {
    let lock = Arc::new(OperationLock::default());
    let first = lock.acquire_as(Some(1)).await.expect("the first command");

    let mut second = spawn_acquiring({
        let lock = lock.clone();
        async move { lock.acquire_as(Some(2)).await.map(drop) }
    })
    .await;
    assert_still_waiting(
        &mut second,
        "the second command must not start while the first holds the operation",
    )
    .await;

    drop(first);
    second
        .await
        .expect("joins")
        .expect("the second command, once the first has finished");
}

#[tokio::test]
async fn an_operation_inside_the_holders_own_is_refused() {
    let lock = OperationLock::default();
    let _outer = lock.acquire_as(Some(1)).await.expect("the outer operation");

    let inner = lock.acquire_as(Some(1)).await;

    assert!(
        matches!(inner, Err(NotSupported { .. })),
        "waiting for itself would never end: {inner:?}"
    );
}

#[tokio::test]
async fn the_operation_can_be_held_again_once_it_is_dropped() {
    let lock = OperationLock::default();
    drop(lock.acquire_as(Some(1)).await.expect("the first operation"));

    lock.acquire_as(Some(1))
        .await
        .expect("the same command, after its operation ended");
}

#[tokio::test]
async fn a_command_without_a_context_waits_rather_than_being_refused() {
    let lock = Arc::new(OperationLock::default());
    let first = lock.acquire_as(None).await.expect("the first");

    let mut second = spawn_acquiring({
        let lock = lock.clone();
        async move { lock.acquire_as(None).await.map(drop) }
    })
    .await;
    assert_still_waiting(&mut second, "a command without a context must wait").await;

    drop(first);
    second
        .await
        .expect("joins")
        .expect("acquired after the first");
}
