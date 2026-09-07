use std::future::Future;

use lore_base::lore_spawn;
use lore_error_set::internal::SupportsInternalError;
use tokio::sync::mpsc;

/// Items the channel holds before a producer's `send` starts waiting.
///
/// Backpressure is the point: a producer that runs ahead of its consumer is made to
/// wait rather than allowed to buffer a whole result, which for a repository with no
/// bound on its size is the difference between a fixed cost and an unbounded one.
#[lore_macro::test_pub]
const CAPACITY: usize = 256;

/// Drain a streaming producer into a `Vec` alongside a summary value for
/// callers that do not benefit from streaming.
///
/// The producer takes an `mpsc::Sender` and emits `Ok(T)` items as it goes,
/// returning `Ok(S)` on success (where `S` is some summary or metadata
/// value — use `()` when there is none) or `Err(E)` on failure.
///
/// **The producer runs as its own task, so nothing here can cancel it.** A
/// `JoinHandle` released without being awaited leaves its task running, where a
/// future dropped where it stood takes with it every task that future had spawned
/// into a `JoinSet`. That difference is the whole reason for the spawn: it holds
/// whether this function returns early, and whether its own caller is dropped.
///
/// A producer whose consumer has gone away is stopped by the channel rather than by
/// cancellation — its next `send` fails, which is a result it can report.
///
/// Returns `Ok((S, Vec<T>))` — the summary first, then every item the
/// producer emitted.
///
/// **A failure stops the producer without cancelling it.** The first error closes
/// the channel, so the producer's next `send` fails — a result it handles like any
/// other, which ends the work it would have started next. The receive loop goes on
/// draining what was already sent and then waits for the producer, so work it had
/// already started — tasks it spawned included — runs to completion and reports,
/// and the error reaches the caller only once nothing is still in flight.
///
/// Draining to the end is also what keeps the join from waiting forever. The
/// channel is bounded, so items left in it leave the producer blocked in `send`
/// and this function blocked on the producer.
///
/// Items drained after the first error are dropped rather than accumulated: the
/// caller is given an error, never a partial `Vec`, so keeping them would grow a
/// buffer nobody reads.
///
/// An item error wins over the producer's own result. It is the earlier failure,
/// and a producer that reports its items' errors through the channel may well
/// return `Ok` regardless.
pub async fn collect_stream_with_summary<T, S, E, Fut>(
    f: impl FnOnce(mpsc::Sender<Result<T, E>>) -> Fut,
) -> Result<(S, Vec<T>), E>
where
    Fut: Future<Output = Result<S, E>> + Send + 'static,
    T: Send + 'static,
    S: Send + 'static,
    E: SupportsInternalError + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel(CAPACITY);
    let producer = lore_spawn!(f(tx));
    let mut out = Vec::new();
    let mut failure = None;
    // Drained to the end even once a failure is in hand: the producer sends into a
    // bounded channel, so items left in it would block the producer and the join
    // below with it. Closed at the first failure, so the producer's next send fails
    // and it stops producing items nobody will read.
    while let Some(item) = rx.recv().await {
        absorb(item, &mut out, &mut failure);
        if failure.is_some() {
            rx.close();
        }
    }
    let summary = match producer.await {
        Ok(summary) => summary,
        Err(join) => match join.try_into_panic() {
            // A panic in the producer stays the caller's panic, exactly as it would
            // be if the producer ran here rather than in a task of its own.
            Ok(panic) => std::panic::resume_unwind(panic),
            // Reached only if something aborts the task, which nothing here does;
            // the runtime shutting down can.
            Err(join) => Err(E::internal_with_context(
                join,
                "streaming producer task did not finish",
            )),
        },
    };
    match failure {
        Some(err) => Err(err),
        None => Ok((summary?, out)),
    }
}

/// Records one streamed item, keeping the first failure and discarding everything
/// that follows it.
fn absorb<T, E>(item: Result<T, E>, out: &mut Vec<T>, failure: &mut Option<E>) {
    match item {
        Ok(item) if failure.is_none() => out.push(item),
        Err(err) if failure.is_none() => *failure = Some(err),
        Ok(_) | Err(_) => {}
    }
}
