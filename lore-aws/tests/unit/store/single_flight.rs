// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::Pin;
use std::task::Poll;

use lore_aws::store::single_flight::FlightKey;
use lore_aws::store::single_flight::Role;
use lore_aws::store::single_flight::SingleFlight;
use lore_base::types::Hash;
use tokio::sync::oneshot;

use crate::store::poll_once;

type Run<'a> = Pin<Box<dyn Future<Output = (u32, Role)> + 'a>>;

/// The content hash every byte of which is `n`.
fn key(n: u8) -> Hash {
    Hash::from([n; 32])
}

/// Builds a run of `key` without polling it. If it becomes the leader, its read waits for a value
/// from the returned sender.
fn held<K: FlightKey>(flights: &SingleFlight<K, u32>, key: K) -> (oneshot::Sender<u32>, Run<'_>) {
    let (release, released) = oneshot::channel();
    let run = flights.run(key, || async { released.await.expect("released") });
    (release, Box::pin(run))
}

/// Builds a run of `key` without polling it. If it becomes the leader, its read returns `value`
/// immediately.
fn ready<K: FlightKey>(flights: &SingleFlight<K, u32>, key: K, value: u32) -> Run<'_> {
    Box::pin(flights.run(key, move || async move { value }))
}

mod run {
    use super::*;

    #[tokio::test]
    async fn a_caller_arriving_during_a_read_takes_its_result() {
        let flights = SingleFlight::default();
        let (release, mut leader) = held(&flights, key(1));
        let mut follower = ready(&flights, key(1), 0);
        assert!(poll_once(leader.as_mut()).await.is_pending());
        assert!(poll_once(follower.as_mut()).await.is_pending());

        release.send(7).expect("leader waiting");
        assert_eq!(leader.await, (7, Role::Leader));
        assert_eq!(follower.await, (7, Role::Follower));
    }

    #[tokio::test]
    async fn reads_of_different_keys_are_not_shared() {
        let flights = SingleFlight::default();
        let (_release, mut first) = held(&flights, key(1));
        assert!(poll_once(first.as_mut()).await.is_pending());

        assert_eq!(ready(&flights, key(2), 3).await, (3, Role::Leader));
    }

    #[tokio::test]
    async fn a_caller_arriving_after_a_read_completes_reads_again() {
        let flights = SingleFlight::default();

        assert_eq!(ready(&flights, key(1), 1).await, (1, Role::Leader));
        assert_eq!(ready(&flights, key(1), 2).await, (2, Role::Leader));
    }

    #[tokio::test]
    async fn followers_of_a_cancelled_leader_read_again() {
        let flights = SingleFlight::default();
        let (_release, mut leader) = held(&flights, key(1));
        let mut follower = ready(&flights, key(1), 9);
        assert!(poll_once(leader.as_mut()).await.is_pending());
        assert!(poll_once(follower.as_mut()).await.is_pending());

        drop(leader);
        assert_eq!(follower.await, (9, Role::Leader));
    }

    #[tokio::test]
    async fn a_follower_keeps_sharing_while_flights_end_without_a_result() {
        let flights = SingleFlight::default();
        let mut leaders = Vec::new();
        let mut follower = ready(&flights, key(1), 0);

        for flight in 0..8 {
            if flight > 0 {
                flights.write(key(1), || async {}).await;
            }
            let (release, mut leader) = held(&flights, key(1));
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
        let (_release, mut stale) = held(&flights, key(1));
        assert!(poll_once(stale.as_mut()).await.is_pending());

        flights.write(key(1), || async {}).await;
        assert_eq!(
            poll_once(ready(&flights, key(1), 4).as_mut()).await,
            Poll::Ready((4, Role::Leader))
        );
    }

    #[tokio::test]
    async fn a_write_dropped_part_way_still_ends_the_flight() {
        let flights = SingleFlight::default();
        let (_release, mut stale) = held(&flights, key(1));
        assert!(poll_once(stale.as_mut()).await.is_pending());

        let (_landed, landing) = oneshot::channel::<()>();
        let mut write = Box::pin(flights.write(key(1), || landing));
        assert!(poll_once(write.as_mut()).await.is_pending());
        drop(write);

        assert_eq!(
            poll_once(ready(&flights, key(1), 4).as_mut()).await,
            Poll::Ready((4, Role::Leader))
        );
    }

    #[tokio::test]
    async fn a_write_in_progress_leaves_a_new_flight_open() {
        let flights = SingleFlight::default();
        let (_landed, landing) = oneshot::channel::<()>();
        let mut write = Box::pin(flights.write(key(1), || landing));
        assert!(poll_once(write.as_mut()).await.is_pending());

        let (release, mut leader) = held(&flights, key(1));
        let mut follower = ready(&flights, key(1), 0);
        assert!(poll_once(leader.as_mut()).await.is_pending());
        assert!(poll_once(follower.as_mut()).await.is_pending());

        release.send(3).expect("leader waiting");
        assert_eq!(leader.await, (3, Role::Leader));
        assert_eq!(follower.await, (3, Role::Follower));
    }

    #[tokio::test]
    async fn followers_already_waiting_read_again() {
        let flights = SingleFlight::default();
        let (release, mut leader) = held(&flights, key(1));
        let mut follower = ready(&flights, key(1), 2);
        assert!(poll_once(leader.as_mut()).await.is_pending());
        assert!(poll_once(follower.as_mut()).await.is_pending());

        flights.write(key(1), || async {}).await;
        release.send(1).expect("leader waiting");
        assert_eq!(leader.await, (1, Role::Leader));
        assert_eq!(follower.await, (2, Role::Leader));
    }

    #[tokio::test]
    async fn a_leader_ending_after_a_write_leaves_the_newer_flight_in_place() {
        let flights = SingleFlight::default();
        let (stale_release, mut stale) = held(&flights, key(1));
        assert!(poll_once(stale.as_mut()).await.is_pending());

        flights.write(key(1), || async {}).await;
        let (fresh_release, mut fresh) = held(&flights, key(1));
        assert!(poll_once(fresh.as_mut()).await.is_pending());

        stale_release.send(1).expect("stale leader waiting");
        assert_eq!(stale.await, (1, Role::Leader));

        let mut follower = ready(&flights, key(1), 0);
        assert!(poll_once(follower.as_mut()).await.is_pending());

        fresh_release.send(2).expect("fresh leader waiting");
        assert_eq!(fresh.await, (2, Role::Leader));
        assert_eq!(follower.await, (2, Role::Follower));
    }
}

mod keys {
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Partition;

    use super::*;

    #[tokio::test]
    async fn association_keys_sharing_a_word_are_not_shared() {
        let flights = SingleFlight::default();
        let (one, two) = ([1; 16], [2; 16]);
        let first = (
            Partition::from(one),
            Address {
                hash: key(3),
                context: Context::from(two),
            },
        );
        let second = (
            Partition::from(two),
            Address {
                hash: key(3),
                context: Context::from(one),
            },
        );
        assert_eq!(first.word(), second.word());

        let (_release, mut leader) = held(&flights, first);
        assert!(poll_once(leader.as_mut()).await.is_pending());

        assert_eq!(ready(&flights, second, 5).await, (5, Role::Leader));
    }
}
