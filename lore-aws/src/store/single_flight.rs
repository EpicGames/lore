// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::hash::Hash;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use dashmap::DashMap;
use dashmap::Entry;
use tokio::sync::watch;

/// The part a caller played in a single flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    /// Issued the read itself.
    Leader,
    /// Took the result of a read another caller already had in flight.
    Follower,
}

/// Collapses concurrent reads of one key onto a single request.
///
/// A read is shared only with callers that arrive while it is in flight. Every write to a key must
/// go through [`SingleFlight::write`], so that a caller arriving after the write reads afresh rather
/// than joining a read that began before it.
pub(crate) struct SingleFlight<K, V> {
    flights: DashMap<K, Flight<V>>,
    next_id: AtomicU64,
}

struct Flight<V> {
    /// Tells this flight apart from a later one under the same key, so that a leader only ever
    /// ends its own.
    id: u64,
    /// Created by the first follower, so a leader reading alone allocates nothing.
    sender: Option<watch::Sender<Option<V>>>,
}

type Channel<V> = (watch::Sender<Option<V>>, watch::Receiver<Option<V>>);

impl<V> Flight<V> {
    /// Subscribes to this flight's result, installing `channel` if the flight has no channel yet.
    /// Returns `None` if neither the flight nor `channel` has a channel.
    fn subscribe(
        &mut self,
        channel: &mut Option<Channel<V>>,
    ) -> Option<watch::Receiver<Option<V>>> {
        if let Some(sender) = &self.sender {
            return Some(sender.subscribe());
        }
        let (sender, receiver) = channel.take()?;
        self.sender = Some(sender);
        Some(receiver)
    }
}

enum Joined<'a, K: Eq + Hash + Clone, V> {
    Leader(Lead<'a, K, V>),
    Follower(watch::Receiver<Option<V>>),
}

/// Held by the leader of a flight. Ends the flight on drop whether or not a result was published,
/// so followers of a leader that is cancelled join again rather than wait forever.
struct Lead<'a, K: Eq + Hash + Clone, V> {
    flights: &'a SingleFlight<K, V>,
    key: Option<K>,
    id: u64,
}

impl<K: Eq + Hash + Clone, V: Clone> Lead<'_, K, V> {
    /// Ends the flight and publishes `value` to its followers, cloned only where one is waiting.
    fn complete(mut self, value: &V) {
        if let Some(sender) = self.end() {
            sender.send_replace(Some(value.clone()));
        }
    }
}

impl<K: Eq + Hash + Clone, V> Lead<'_, K, V> {
    /// Removes this leader's flight unless an invalidation has already removed it. Returns its
    /// channel if a follower is still listening. Removing the entry before sending prevents new
    /// followers from joining, so every follower that joined holds a receiver for the sent value.
    fn end(&mut self) -> Option<watch::Sender<Option<V>>> {
        let key = self.key.take()?;
        let flight = self.flights.end(&key, self.id)?;
        flight.sender.filter(|sender| sender.receiver_count() > 0)
    }
}

impl<K: Eq + Hash + Clone, V> Drop for Lead<'_, K, V> {
    fn drop(&mut self) {
        self.end();
    }
}

/// Ends the flight for `key` when dropped.
struct Invalidate<'a, K: Eq + Hash + Clone, V> {
    flights: &'a SingleFlight<K, V>,
    key: K,
}

impl<K: Eq + Hash + Clone, V> Drop for Invalidate<'_, K, V> {
    fn drop(&mut self) {
        self.flights.invalidate(&self.key);
    }
}

impl<K: Eq + Hash + Clone, V> Default for SingleFlight<K, V> {
    fn default() -> Self {
        Self {
            flights: DashMap::new(),
            next_id: AtomicU64::new(0),
        }
    }
}

impl<K: Eq + Hash + Clone, V> SingleFlight<K, V> {
    /// Ends any flight for `key`. Its followers join again, so their reads begin no earlier than
    /// this call.
    fn invalidate(&self, key: &K) {
        self.flights.remove(key);
    }

    /// Removes the flight for `key` if it is the one numbered `id`, leaving a newer flight under
    /// the same key in place.
    fn end(&self, key: &K, id: u64) -> Option<Flight<V>> {
        self.flights
            .remove_if(key, |_, flight| flight.id == id)
            .map(|(_, flight)| flight)
    }
}

impl<K: Eq + Hash + Clone, V: Clone> SingleFlight<K, V> {
    /// Runs `read` for `key`, unless a read of `key` is already in flight, in which case this
    /// takes that read's result instead.
    pub(crate) async fn run<F, Fut>(&self, key: K, read: F) -> (V, Role)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = V>,
    {
        // Unbounded on purpose: each pass waits on a flight rather than spinning, and holds one
        // receiver at a time. A flight that ends without a result leaves the key free, so one of
        // the callers joining again becomes the leader.
        let lead = loop {
            match self.join(&key) {
                Joined::Leader(lead) => break lead,
                Joined::Follower(receiver) => {
                    if let Some(value) = follow(receiver).await {
                        return (value, Role::Follower);
                    }
                }
            }
        };
        let value = read().await;
        lead.complete(&value);
        (value, Role::Leader)
    }

    /// Calls `write` to create a write to `key`, runs it, then ends any flight for `key`. Also ends
    /// the flight if the write is dropped before completion, because a write can take effect
    /// before its caller receives the response. Creating the write here ensures that only the
    /// await holds its future.
    pub(crate) async fn write<F, Fut>(&self, key: K, write: F) -> Fut::Output
    where
        F: FnOnce() -> Fut,
        Fut: Future,
    {
        let _invalidate = Invalidate { flights: self, key };
        write().await
    }

    /// Joins the flight for `key`, or starts one if none exists. Each attempt holds one shard lock
    /// only to insert or subscribe. No channel is created while the lock is held: the first
    /// follower creates it outside the lock, then acquires the lock again.
    fn join(&self, key: &K) -> Joined<'_, K, V> {
        let mut channel = None;
        loop {
            // `DashMap::entry` is safe here as it is not held across any awaits and no other locks
            // are acquired while it is held.
            #[allow(clippy::disallowed_methods)]
            let receiver = match self.flights.entry(key.clone()) {
                Entry::Vacant(vacant) => {
                    let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                    vacant.insert(Flight { id, sender: None });
                    return Joined::Leader(Lead {
                        flights: self,
                        key: Some(key.clone()),
                        id,
                    });
                }
                Entry::Occupied(mut occupied) => occupied.get_mut().subscribe(&mut channel),
            };
            if let Some(receiver) = receiver {
                return Joined::Follower(receiver);
            }
            channel = Some(watch::channel(None));
        }
    }
}

/// Waits for the flight's result. Returns `None` if the flight ended without a result.
async fn follow<V: Clone>(mut receiver: watch::Receiver<Option<V>>) -> Option<V> {
    receiver.changed().await.ok()?;
    receiver.borrow_and_update().clone()
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::Poll;

    use tokio::sync::oneshot;

    use super::*;
    use crate::store::test_util::poll_once;

    type Run<'a> = Pin<Box<dyn Future<Output = (u32, Role)> + 'a>>;

    /// Builds a run of `key` without polling it. If it becomes the leader, its read waits for a
    /// value from the returned sender.
    fn held(flights: &SingleFlight<u32, u32>, key: u32) -> (oneshot::Sender<u32>, Run<'_>) {
        let (release, released) = oneshot::channel();
        let run = flights.run(key, || async { released.await.expect("released") });
        (release, Box::pin(run))
    }

    /// Builds a run of `key` without polling it. If it becomes the leader, its read returns `value`
    /// immediately.
    fn ready(flights: &SingleFlight<u32, u32>, key: u32, value: u32) -> Run<'_> {
        Box::pin(flights.run(key, move || async move { value }))
    }

    mod run {
        use super::*;

        #[tokio::test]
        async fn a_caller_arriving_during_a_read_takes_its_result() {
            let flights = SingleFlight::default();
            let (release, mut leader) = held(&flights, 1);
            let mut follower = ready(&flights, 1, 0);
            assert!(poll_once(leader.as_mut()).await.is_pending());
            assert!(poll_once(follower.as_mut()).await.is_pending());

            release.send(7).expect("leader waiting");
            assert_eq!(leader.await, (7, Role::Leader));
            assert_eq!(follower.await, (7, Role::Follower));
        }

        #[tokio::test]
        async fn reads_of_different_keys_are_not_shared() {
            let flights = SingleFlight::default();
            let (_release, mut first) = held(&flights, 1);
            assert!(poll_once(first.as_mut()).await.is_pending());

            assert_eq!(ready(&flights, 2, 3).await, (3, Role::Leader));
        }

        #[tokio::test]
        async fn a_caller_arriving_after_a_read_completes_reads_again() {
            let flights = SingleFlight::default();

            assert_eq!(ready(&flights, 1, 1).await, (1, Role::Leader));
            assert_eq!(ready(&flights, 1, 2).await, (2, Role::Leader));
        }

        #[tokio::test]
        async fn followers_of_a_cancelled_leader_read_again() {
            let flights = SingleFlight::default();
            let (_release, mut leader) = held(&flights, 1);
            let mut follower = ready(&flights, 1, 9);
            assert!(poll_once(leader.as_mut()).await.is_pending());
            assert!(poll_once(follower.as_mut()).await.is_pending());

            drop(leader);
            assert_eq!(follower.await, (9, Role::Leader));
        }

        #[tokio::test]
        async fn a_follower_keeps_sharing_while_flights_end_without_a_result() {
            let flights = SingleFlight::default();
            let mut leaders = Vec::new();
            let mut follower = ready(&flights, 1, 0);

            for flight in 0..8 {
                if flight > 0 {
                    flights.invalidate(&1);
                }
                let (release, mut leader) = held(&flights, 1);
                assert!(poll_once(leader.as_mut()).await.is_pending());
                assert!(poll_once(follower.as_mut()).await.is_pending());
                leaders.push((release, leader));
            }

            let (release, leader) = leaders.pop().expect("a leader in flight");
            release.send(6).expect("leader waiting");
            assert_eq!(leader.await, (6, Role::Leader));
            assert_eq!(follower.await, (6, Role::Follower));
        }
    }

    mod write {
        use super::*;

        #[tokio::test]
        async fn a_caller_arriving_after_a_write_starts_a_new_flight() {
            let flights = SingleFlight::default();
            let (_release, mut stale) = held(&flights, 1);
            assert!(poll_once(stale.as_mut()).await.is_pending());

            flights.write(1, || async {}).await;
            assert_eq!(
                poll_once(ready(&flights, 1, 4).as_mut()).await,
                Poll::Ready((4, Role::Leader))
            );
        }

        #[tokio::test]
        async fn a_write_dropped_part_way_still_ends_the_flight() {
            let flights = SingleFlight::default();
            let (_release, mut stale) = held(&flights, 1);
            assert!(poll_once(stale.as_mut()).await.is_pending());

            let (_landed, landing) = oneshot::channel::<()>();
            let mut write = Box::pin(flights.write(1, || landing));
            assert!(poll_once(write.as_mut()).await.is_pending());
            drop(write);

            assert_eq!(
                poll_once(ready(&flights, 1, 4).as_mut()).await,
                Poll::Ready((4, Role::Leader))
            );
        }

        #[tokio::test]
        async fn a_write_in_progress_leaves_a_new_flight_open() {
            let flights = SingleFlight::default();
            let (_landed, landing) = oneshot::channel::<()>();
            let mut write = Box::pin(flights.write(1, || landing));
            assert!(poll_once(write.as_mut()).await.is_pending());

            let (release, mut leader) = held(&flights, 1);
            let mut follower = ready(&flights, 1, 0);
            assert!(poll_once(leader.as_mut()).await.is_pending());
            assert!(poll_once(follower.as_mut()).await.is_pending());

            release.send(3).expect("leader waiting");
            assert_eq!(leader.await, (3, Role::Leader));
            assert_eq!(follower.await, (3, Role::Follower));
        }
    }

    mod invalidate {
        use super::*;

        #[tokio::test]
        async fn followers_already_waiting_read_again() {
            let flights = SingleFlight::default();
            let (release, mut leader) = held(&flights, 1);
            let mut follower = ready(&flights, 1, 2);
            assert!(poll_once(leader.as_mut()).await.is_pending());
            assert!(poll_once(follower.as_mut()).await.is_pending());

            flights.invalidate(&1);
            release.send(1).expect("leader waiting");
            assert_eq!(leader.await, (1, Role::Leader));
            assert_eq!(follower.await, (2, Role::Leader));
        }

        #[tokio::test]
        async fn a_caller_arriving_after_it_starts_a_new_flight() {
            let flights = SingleFlight::default();
            let (_release, mut stale) = held(&flights, 1);
            assert!(poll_once(stale.as_mut()).await.is_pending());

            flights.invalidate(&1);
            assert_eq!(
                poll_once(ready(&flights, 1, 4).as_mut()).await,
                Poll::Ready((4, Role::Leader))
            );
        }

        #[tokio::test]
        async fn a_leader_ending_after_it_leaves_the_newer_flight_in_place() {
            let flights = SingleFlight::default();
            let (stale_release, mut stale) = held(&flights, 1);
            assert!(poll_once(stale.as_mut()).await.is_pending());

            flights.invalidate(&1);
            let (fresh_release, mut fresh) = held(&flights, 1);
            assert!(poll_once(fresh.as_mut()).await.is_pending());

            stale_release.send(1).expect("stale leader waiting");
            assert_eq!(stale.await, (1, Role::Leader));

            let mut follower = ready(&flights, 1, 0);
            assert!(poll_once(follower.as_mut()).await.is_pending());

            fresh_release.send(2).expect("fresh leader waiting");
            assert_eq!(fresh.await, (2, Role::Leader));
            assert_eq!(follower.await, (2, Role::Follower));
        }
    }
}
