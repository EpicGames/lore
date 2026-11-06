// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::hash::BuildHasherDefault;
use std::hash::Hasher;
use std::sync::OnceLock;

use dashmap::DashMap;
use dashmap::Entry;
use tokio::sync::watch;

use crate::error::StorageError;
use crate::read::Loaded;
use crate::types::Address;
use crate::types::Hash;
use crate::types::Partition;
use crate::types::PassThroughHasher;
use crate::write::StoreInFlightKey;

/// Outcome the read single-flight leader publishes to its followers: `None`
/// while the read is still running, `Some(result)` once it finishes.
#[lore_macro::test_pub]
type ReadFlightResult = Option<Result<Loaded, StorageError>>;

type Channel = (
    watch::Sender<ReadFlightResult>,
    watch::Receiver<ReadFlightResult>,
);

/// Reads in flight, keyed by content hash: an address's context does not change its content, so a
/// read of the same content under any context joins the flight. The partition is part of the key
/// only where the store isolates partitions, and whether the remote is trusted always is.
static READ_IN_FLIGHT: OnceLock<
    DashMap<ReadFlightKey, ReadFlightEntry, BuildHasherDefault<PassThroughHasher>>,
> = OnceLock::new();

/// The content a read single-flight fetches: its hash, the partition where the store isolates
/// partitions, and whether the remote it is fetched from is trusted.
#[derive(Clone, Copy, Eq, PartialEq)]
struct ReadFlightKey {
    hash: Hash,
    partition: Option<Partition>,
    trusted: bool,
}

/// Writes one word, which [`PassThroughHasher`] takes as the hash: the first eight bytes of the
/// content hash, which is uniform already, folded with the partition's where the key carries one
/// and with its trust.
impl std::hash::Hash for ReadFlightKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let partition = self
            .partition
            .map_or(0, |partition| leading_word(partition.as_ref()));
        state.write_u64(leading_word(self.hash.as_ref()) ^ partition ^ u64::from(self.trusted));
    }
}

fn leading_word(bytes: &[u8]) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(word)
}

/// A read in flight: the leader's partition and address, and the channel its outcome goes out on,
/// which the first follower creates.
struct ReadFlightEntry {
    leader: StoreInFlightKey,
    sender: Option<watch::Sender<ReadFlightResult>>,
}

impl ReadFlightEntry {
    /// Subscribes to this flight's outcome, installing `channel` where the flight has no channel
    /// yet; `None` where it has none and `channel` holds none.
    fn subscribe(&mut self, channel: &mut Option<Channel>) -> Option<ReadFlightFollower> {
        let receiver = if let Some(sender) = &self.sender {
            sender.subscribe()
        } else {
            let (sender, receiver) = channel.take()?;
            self.sender = Some(sender);
            receiver
        };
        Some(ReadFlightFollower {
            leader: self.leader.clone(),
            receiver,
        })
    }
}

/// Outcome of joining the read single-flight for an address's content.
#[lore_macro::test_pub]
pub(crate) enum ReadFlight {
    /// No read of this content is in flight; the caller must perform it and call `complete`.
    Leader(ReadFlightGuard),
    /// Another caller is already reading this content; await its outcome via `await_flight`.
    Follower(ReadFlightFollower),
}

/// A follower's view of a read single-flight: the leader's partition and address, and the
/// receiver its outcome arrives on.
#[lore_macro::test_pub]
pub(crate) struct ReadFlightFollower {
    leader: StoreInFlightKey,
    receiver: watch::Receiver<ReadFlightResult>,
}

impl ReadFlightFollower {
    /// Whether the leader reads `address` in `partition`, so its outcome is this caller's own.
    #[lore_macro::test_pub]
    pub(crate) fn led_by(&self, partition: Partition, address: Address) -> bool {
        self.leader.partition == partition && self.leader.address == address
    }
}

/// RAII guard held by the leader of a read single-flight. Removes the in-flight
/// map entry on drop whether or not `complete` was called, so a leader whose task
/// is cancelled before finishing does not wedge followers forever.
#[lore_macro::test_pub]
pub(crate) struct ReadFlightGuard {
    key: Option<ReadFlightKey>,
}

impl ReadFlightGuard {
    /// Whether a follower is subscribed to this flight's outcome.
    #[cfg(feature = "test-util")]
    #[lore_macro::test_pub]
    pub(crate) fn has_followers(&self) -> bool {
        self.key
            .and_then(|key| {
                READ_IN_FLIGHT.get()?.get(&key).map(|flight| {
                    flight
                        .sender
                        .as_ref()
                        .is_some_and(|sender| sender.receiver_count() > 0)
                })
            })
            .unwrap_or(false)
    }

    /// End the flight and publish `result` to the followers subscribed to it, cloned only where
    /// one is.
    #[lore_macro::test_pub]
    pub(crate) fn complete(mut self, result: &Result<Loaded, StorageError>) {
        if let Some(sender) = self.key.take().and_then(close) {
            sender.send_replace(Some(result.clone()));
        }
    }
}

impl Drop for ReadFlightGuard {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            close(key);
        }
    }
}

/// Removes the flight for `key`, and answers its channel where a follower still listens on it.
/// Removing the entry first closes the flight to further followers, so every follower that joined
/// holds a receiver of what is sent. A flight ended without a result drops its channel, which tells
/// its followers to join again.
fn close(key: ReadFlightKey) -> Option<watch::Sender<ReadFlightResult>> {
    let (_, flight) = READ_IN_FLIGHT.get()?.remove(&key)?;
    flight.sender.filter(|sender| sender.receiver_count() > 0)
}

/// Join the read single-flight for `address`'s content: its hash, `partition` where
/// `isolate_partitions` says the store serves each partition only what it holds, and whether the
/// remote is `trusted`. The first caller becomes the leader and must perform the read, then call
/// `ReadFlightGuard::complete`. Later callers become followers, whatever their context, and should
/// pass the follower to `await_flight`.
///
/// One shard lock serves each join, held only to insert the flight or subscribe to it, and nothing
/// is allocated under it. A leader allocates nothing. The first follower of a flight creates its
/// channel, outside the lock, and takes the lock again to install it.
#[lore_macro::test_pub]
pub(crate) fn join_read_flight(
    partition: Partition,
    address: Address,
    isolate_partitions: bool,
    trusted: bool,
) -> ReadFlight {
    let key = ReadFlightKey {
        hash: address.hash,
        partition: isolate_partitions.then_some(partition),
        trusted,
    };
    let in_flight =
        READ_IN_FLIGHT.get_or_init(|| DashMap::with_hasher(BuildHasherDefault::default()));
    let mut channel = None;
    loop {
        // `DashMap::entry` is safe here as it is not held across any awaits and no other locks are acquired while held
        #[allow(clippy::disallowed_methods)]
        let follower = match in_flight.entry(key) {
            Entry::Vacant(vacant) => {
                vacant.insert(ReadFlightEntry {
                    leader: StoreInFlightKey { partition, address },
                    sender: None,
                });
                return ReadFlight::Leader(ReadFlightGuard { key: Some(key) });
            }
            Entry::Occupied(mut occupied) => occupied.get_mut().subscribe(&mut channel),
        };
        if let Some(follower) = follower {
            #[cfg(feature = "test-util")]
            crate::test_util::count_follower();
            return ReadFlight::Follower(follower);
        }
        channel = Some(watch::channel(None));
    }
}

/// Wait for the read single-flight leader to finish. Returns `None` if the leader
/// vanished without producing a result (e.g. its task was cancelled), so the caller
/// can promote itself to leader via `join_read_flight` and retry.
#[lore_macro::test_pub]
pub(crate) async fn await_flight(follower: ReadFlightFollower) -> ReadFlightResult {
    let mut receiver = follower.receiver;
    if let Some(result) = receiver.borrow().clone() {
        return Some(result);
    }
    match receiver.changed().await {
        Ok(()) => receiver.borrow().clone(),
        Err(_) => None,
    }
}
