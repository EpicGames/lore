// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::cell::Cell;
use std::future::Future;
use std::hash::BuildHasherDefault;
use std::hash::Hasher;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use dashmap::DashMap;
use dashmap::Entry;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_base::types::Partition;
use tokio::sync::watch;

/// The part a caller played in a single flight.
#[lore_macro::test_pub]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    /// Issued the read itself.
    Leader,
    /// Took the result of a read another caller already had in flight.
    Follower,
}

/// A key that reads are shared under.
#[lore_macro::test_pub]
pub(crate) trait FlightKey: Eq + Clone {
    /// The key's hash in the flight map: a word of its content hash, which is uniform already,
    /// folded with the rest of the key, so the map never hashes the key's bytes.
    fn word(&self) -> u64;
}

impl FlightKey for Hash {
    fn word(&self) -> u64 {
        leading_word(self.as_ref())
    }
}

impl FlightKey for (Partition, Address) {
    fn word(&self) -> u64 {
        let (partition, address) = self;
        leading_word(address.hash.as_ref())
            ^ leading_word(address.context.as_ref())
            ^ leading_word(partition.as_ref())
    }
}

fn leading_word(bytes: &[u8]) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(word)
}

/// A key as the flight map holds it, hashed to its [`FlightKey::word`].
#[lore_macro::test_pub]
#[derive(Clone, PartialEq, Eq)]
struct MapKey<K>(K);

impl<K: FlightKey> std::hash::Hash for MapKey<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.0.word());
    }
}

/// Takes a [`MapKey`]'s word as its hash. The key writes that word alone, so nothing writes bytes.
#[lore_macro::test_pub]
#[derive(Default)]
struct WordHasher(u64);

impl Hasher for WordHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, _bytes: &[u8]) {
        unreachable!("MapKey hashes itself with write_u64");
    }

    fn write_u64(&mut self, word: u64) {
        self.0 = word;
    }
}

/// Numbers a new flight uniquely among every flight this process starts. Each thread numbers its
/// flights from a block of numbers it takes from a shared counter, so starting a flight writes no
/// memory that another thread writes.
fn next_id() -> u64 {
    const BLOCK: u64 = 1 << 20;
    static BLOCKS: AtomicU64 = AtomicU64::new(0);
    thread_local! {
        static NEXT: Cell<u64> = const { Cell::new(0) };
    }
    NEXT.with(|next| {
        let mut id = next.get();
        if id % BLOCK == 0 {
            id = BLOCKS.fetch_add(BLOCK, Ordering::Relaxed);
        }
        next.set(id + 1);
        id
    })
}

/// Collapses concurrent reads of one key onto a single request.
///
/// A read is shared only with callers that arrive while it is in flight. Every write to a key must
/// go through [`SingleFlight::write`], so that a caller arriving after the write reads afresh rather
/// than joining a read that began before it.
#[lore_macro::test_pub]
pub(crate) struct SingleFlight<K, V> {
    flights: DashMap<MapKey<K>, Flight<V>, BuildHasherDefault<WordHasher>>,
}

#[lore_macro::test_pub]
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

enum Joined<'a, K: FlightKey, V> {
    Leader(Lead<'a, K, V>),
    Follower(watch::Receiver<Option<V>>),
}

/// Held by the leader of a flight. Ends the flight on drop whether or not a result was published,
/// so followers of a leader that is cancelled join again rather than wait forever.
struct Lead<'a, K: FlightKey, V> {
    flights: &'a SingleFlight<K, V>,
    key: Option<&'a MapKey<K>>,
    id: u64,
}

impl<K: FlightKey, V: Clone> Lead<'_, K, V> {
    /// Ends the flight and publishes `value` to its followers, cloned only where one is waiting.
    fn complete(mut self, value: &V) {
        if let Some(sender) = self.end() {
            sender.send_replace(Some(value.clone()));
        }
    }
}

impl<K: FlightKey, V> Lead<'_, K, V> {
    /// Removes this leader's flight unless an invalidation has already removed it. Returns its
    /// channel if a follower is still listening. Removing the entry before sending prevents new
    /// followers from joining, so every follower that joined holds a receiver for the sent value.
    fn end(&mut self) -> Option<watch::Sender<Option<V>>> {
        let key = self.key.take()?;
        let flight = self.flights.end(key, self.id)?;
        flight.sender.filter(|sender| sender.receiver_count() > 0)
    }
}

impl<K: FlightKey, V> Drop for Lead<'_, K, V> {
    fn drop(&mut self) {
        self.end();
    }
}

/// Ends the flight for `key` when dropped.
struct Invalidate<'a, K: FlightKey, V> {
    flights: &'a SingleFlight<K, V>,
    key: MapKey<K>,
}

impl<K: FlightKey, V> Drop for Invalidate<'_, K, V> {
    fn drop(&mut self) {
        self.flights.flights.remove(&self.key);
    }
}

impl<K: FlightKey, V> Default for SingleFlight<K, V> {
    fn default() -> Self {
        Self {
            flights: DashMap::default(),
        }
    }
}

impl<K: FlightKey, V> SingleFlight<K, V> {
    /// Removes the flight for `key` if it is the one numbered `id`, leaving a newer flight under
    /// the same key in place.
    fn end(&self, key: &MapKey<K>, id: u64) -> Option<Flight<V>> {
        self.flights
            .remove_if(key, |_, flight| flight.id == id)
            .map(|(_, flight)| flight)
    }
}

impl<K: FlightKey, V: Clone> SingleFlight<K, V> {
    /// Runs `read` for `key`, unless a read of `key` is already in flight, in which case this
    /// takes that read's result instead.
    ///
    /// A follower whose flight ends without a result joins again, without bound: each pass waits
    /// on a flight rather than spinning and holds one receiver at a time, and the key is free once
    /// the flight ends, so one of the callers joining again becomes the leader.
    #[lore_macro::test_pub]
    pub(crate) async fn run<F, Fut>(&self, key: K, read: F) -> (V, Role)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = V>,
    {
        let key = MapKey(key);
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

    /// Calls `write` to create a write to `key`, runs it, then ends any flight for `key`, so its
    /// followers join again and their reads begin no earlier than the write completed. Also ends
    /// the flight if the write is dropped before completion, because a write can take effect
    /// before its caller receives the response. Creating the write here ensures that only the
    /// await holds its future.
    #[lore_macro::test_pub]
    pub(crate) async fn write<F, Fut>(&self, key: K, write: F) -> Fut::Output
    where
        F: FnOnce() -> Fut,
        Fut: Future,
    {
        let _invalidate = Invalidate {
            flights: self,
            key: MapKey(key),
        };
        write().await
    }

    /// Joins the flight for `key`, or starts one if none exists. Each attempt holds one shard lock
    /// only to insert or subscribe. No channel is created while the lock is held: the first
    /// follower creates it outside the lock, then acquires the lock again.
    fn join<'a>(&'a self, key: &'a MapKey<K>) -> Joined<'a, K, V> {
        let mut channel = None;
        loop {
            // `DashMap::entry` is safe here as it is not held across any awaits and no other locks
            // are acquired while it is held.
            #[allow(clippy::disallowed_methods)]
            let receiver = match self.flights.entry(key.clone()) {
                Entry::Vacant(vacant) => {
                    let id = next_id();
                    vacant.insert(Flight { id, sender: None });
                    return Joined::Leader(Lead {
                        flights: self,
                        key: Some(key),
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
