// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::lore_spawn;
use lore_error_set::internal::SupportsInternalError;
use lore_revision::util::collect_stream::*;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// Error type for these tests. The collector needs one that can describe a
/// producer task that did not finish, which is what `SupportsInternalError`
/// asks for and what a bare `&str` cannot provide.
#[derive(Debug, PartialEq, Eq)]
struct TestError(String);

impl SupportsInternalError for TestError {
    fn internal(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }

    fn internal_with_context(
        source: impl std::error::Error + Send + Sync + 'static,
        context: &str,
    ) -> Self {
        Self(format!("{context}: {source}"))
    }
}

/// Shorthand, so a test reads as `err("boom")` rather than the constructor.
fn err(msg: &str) -> TestError {
    TestError::internal(msg)
}

#[tokio::test]
async fn happy_path_emits_in_order() {
    let result: Result<((), Vec<u32>), TestError> = collect_stream_with_summary(|tx| async move {
        for n in 0..10 {
            tx.send(Ok(n)).await.unwrap();
        }
        Ok(())
    })
    .await;
    assert_eq!(result.unwrap().1, (0..10).collect::<Vec<u32>>());
}

#[tokio::test]
async fn empty_producer_yields_empty_vec() {
    let result: Result<((), Vec<u32>), TestError> =
        collect_stream_with_summary(|_tx| async move { Ok(()) }).await;
    assert_eq!(result.unwrap().1, Vec::<u32>::new());
}

/// An item error is reported, and the producer reaches its own end first.
///
/// The error goes first and is followed by more items than the channel holds,
/// so the producer is genuinely suspended in `send` when the receiver takes the
/// error. Returning there would leave it unawaited mid-send, and its last statement
/// would not have run by the time the caller looks, which is what the flag detects.
/// Fewer items than the channel's capacity would prove nothing: the producer would
/// finish on its first poll, before the receiver had looked at anything.
#[tokio::test]
async fn an_item_error_lets_the_producer_finish() {
    let finished = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&finished);
    let result: Result<((), Vec<usize>), TestError> =
        collect_stream_with_summary(move |tx| async move {
            tx.send(Err(err("nope"))).await.unwrap();
            for n in 0..=CAPACITY {
                // Refused once the error is taken, which this producer does not mind.
                let _ = tx.send(Ok(n)).await;
            }
            flag.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;
    assert_eq!(result, Err(err("nope")));
    assert!(
        finished.load(Ordering::SeqCst),
        "the producer ran to its own end rather than being dropped at the error"
    );
}

/// **An item error stops the producer at its next send.** The channel closes at the
/// first failure, so a producer with more to send is told nobody will read it,
/// rather than going on to produce all of it.
///
/// More items follow the error than the channel holds, so however far the producer
/// runs ahead, at least one of its sends is still to come when the channel closes.
#[tokio::test]
async fn an_item_error_stops_the_producer_at_its_next_send() {
    let refused = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&refused);
    let result: Result<((), Vec<usize>), TestError> =
        collect_stream_with_summary(move |tx| async move {
            tx.send(Err(err("nope"))).await.unwrap();
            for n in 0..=CAPACITY {
                if tx.send(Ok(n)).await.is_err() {
                    flag.store(true, Ordering::SeqCst);
                    break;
                }
            }
            Ok(())
        })
        .await;
    assert_eq!(result, Err(err("nope")));
    assert!(
        refused.load(Ordering::SeqCst),
        "the producer's send after the error was refused"
    );
}

/// **Cancelling the caller does not cancel the producer.** Dropping this
/// function's future releases the producer's handle rather than aborting it, so
/// a producer already running finishes and the tasks it spawned are not torn
/// down underneath it. A producer driven inline would go with its caller.
///
/// The gates make it deterministic: the producer proves it started before the
/// caller is aborted, and is only released afterwards, so reaching the end is
/// evidence it survived the abort rather than evidence it beat it.
#[tokio::test]
async fn cancelling_the_caller_leaves_the_producer_running() {
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let (done_tx, done_rx) = oneshot::channel();

    let caller = lore_spawn!(async move {
        let _: Result<((), Vec<usize>), TestError> =
            collect_stream_with_summary(move |_tx| async move {
                let _ = started_tx.send(());
                let _ = release_rx.await;
                let _ = done_tx.send(());
                Ok(())
            })
            .await;
    });

    started_rx.await.expect("the producer started");
    caller.abort();
    // Awaited, not just aborted: `abort` only schedules the cancellation, and
    // releasing the producer before the caller has actually been dropped would
    // let it finish for the wrong reason.
    let _ = caller.await;
    let _ = release_tx.send(());

    tokio::time::timeout(Duration::from_secs(5), done_rx)
        .await
        .expect("the producer was cancelled along with its caller")
        .expect("the producer reached its end");
}

/// A producer that panics instead of returning, for the test below.
fn explode() -> Result<(), TestError> {
    panic!("producer exploded");
}

/// **A producer's panic is the caller's panic**, as it would be if the producer ran
/// in the caller's own task, rather than an error value this function makes up.
#[tokio::test]
#[should_panic(expected = "producer exploded")]
async fn a_producer_panic_reaches_the_caller() {
    let _: Result<((), Vec<u32>), TestError> =
        collect_stream_with_summary(|_tx| async move { explode() }).await;
}

/// The first error is the one reported, so a later one cannot displace the
/// failure a caller is told about.
#[tokio::test]
async fn the_first_item_error_wins() {
    let result: Result<((), Vec<u32>), TestError> = collect_stream_with_summary(|tx| async move {
        tx.send(Err(err("first"))).await.unwrap();
        tx.send(Err(err("second"))).await.unwrap();
        Ok(())
    })
    .await;
    assert_eq!(result, Err(err("first")));
}

/// An item error outranks the producer's own result, which may be `Ok` even
/// though an item failed.
#[tokio::test]
async fn an_item_error_outranks_a_later_producer_error() {
    let result: Result<((), Vec<u32>), TestError> = collect_stream_with_summary(|tx| async move {
        tx.send(Err(err("item"))).await.unwrap();
        Err(err("producer"))
    })
    .await;
    assert_eq!(result, Err(err("item")));
}

#[tokio::test]
async fn producer_error_propagates() {
    let result: Result<((), Vec<u32>), TestError> = collect_stream_with_summary(|tx| async move {
        tx.send(Ok(1)).await.unwrap();
        Err(err("boom"))
    })
    .await;
    assert_eq!(result, Err(err("boom")));
}

#[tokio::test]
async fn drains_after_producer_finishes() {
    // Producer fills the channel and returns; the drain must collect
    // the remaining buffered items rather than dropping them.
    let result: Result<((), Vec<u32>), TestError> = collect_stream_with_summary(|tx| async move {
        for n in 0..50 {
            tx.send(Ok(n)).await.unwrap();
        }
        Ok(())
    })
    .await;
    assert_eq!(result.unwrap().1, (0..50).collect::<Vec<u32>>());
}

#[tokio::test]
async fn large_burst_exercises_channel_backpressure() {
    // Producer emits more items than `CAPACITY`, so `tx.send` must await the
    // receiver between batches. The producer runs as a task of its own, so this
    // proves the receive loop and the producer make progress together without
    // deadlock.
    let result: Result<((), Vec<u32>), TestError> = collect_stream_with_summary(|tx| async move {
        for n in 0..1000 {
            tx.send(Ok(n)).await.unwrap();
        }
        Ok(())
    })
    .await;
    assert_eq!(result.unwrap().1, (0..1000).collect::<Vec<u32>>());
}

#[tokio::test]
async fn producer_error_after_dropping_tx_still_yields_error() {
    // Producer drops the sender, yields once so the receive loop observes
    // the channel close, then returns an error. The driver's error must
    // win even though `rx.recv()` returned `None` first.
    let result: Result<((), Vec<u32>), TestError> = collect_stream_with_summary(|tx| async move {
        drop(tx);
        tokio::task::yield_now().await;
        Err(err("boom"))
    })
    .await;
    assert_eq!(result, Err(err("boom")));
}

#[tokio::test]
async fn producer_error_after_items_still_yields_error() {
    // Producer emits a few items, then errors. The returned `Err`
    // takes priority over partial item accumulation; the caller does
    // not see a partial `Vec`.
    let result: Result<((), Vec<u32>), TestError> = collect_stream_with_summary(|tx| async move {
        tx.send(Ok(1)).await.unwrap();
        tx.send(Ok(2)).await.unwrap();
        Err(err("boom"))
    })
    .await;
    assert_eq!(result, Err(err("boom")));
}

#[tokio::test]
async fn with_summary_returns_summary_and_items() {
    let result: Result<(&'static str, Vec<u32>), TestError> =
        collect_stream_with_summary(|tx| async move {
            for n in 0..5 {
                tx.send(Ok(n)).await.unwrap();
            }
            Ok("done")
        })
        .await;
    let (summary, items) = result.unwrap();
    assert_eq!(summary, "done");
    assert_eq!(items, vec![0, 1, 2, 3, 4]);
}

/// The drain holds no received item beside its producer, while it receives or while it awaits
/// a producer that outlives the channel.
#[test]
fn the_drain_holds_no_item_beside_its_producer() {
    async fn produce(tx: mpsc::Sender<Result<[u8; 1024], TestError>>) -> Result<(), TestError> {
        tx.send(Ok([0; 1024]))
            .await
            .map_err(|_closed| err("closed"))
    }

    let (tx, _rx) = mpsc::channel(1);
    let producer = size_of_val(&produce(tx));
    let drain = size_of_val(&collect_stream_with_summary(produce));

    assert!(
        drain < producer + size_of::<[u8; 1024]>(),
        "the drain holds {drain} bytes over a producer of {producer}"
    );
}

#[tokio::test]
async fn with_summary_producer_error_propagates() {
    let result: Result<(&'static str, Vec<u32>), TestError> =
        collect_stream_with_summary(|tx| async move {
            tx.send(Ok(1)).await.unwrap();
            Err(err("boom"))
        })
        .await;
    assert_eq!(result, Err(err("boom")));
}
