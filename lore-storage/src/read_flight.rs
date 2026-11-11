// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::hash::BuildHasherDefault;
use std::hash::Hasher;
use std::sync::OnceLock;

use dashmap::DashMap;
use dashmap::Entry;
use tokio::sync::watch;

use crate::error::StorageError;
use crate::options::ReadOptions;
use crate::read::Keep;
use crate::read::Loaded;
use crate::types::Address;
use crate::types::Hash;
use crate::types::Partition;
use crate::types::PassThroughHasher;
use crate::write::FlightStore;
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
/// read of the same content under any context, and into any store, joins the flight. The partition
/// is part of the key only where the store isolates partitions; whether the remote is trusted, and
/// whether the read stores nothing locally, always are.
static READ_IN_FLIGHT: OnceLock<
    DashMap<ReadFlightKey, ReadFlightEntry, BuildHasherDefault<PassThroughHasher>>,
> = OnceLock::new();

/// The content a read single-flight fetches: its hash, the partition where the store isolates
/// partitions, whether the remote it is fetched from is trusted, and whether the read stores
/// nothing it fetches locally ([`ReadOptions::no_local_write`]).
#[derive(Clone, Copy, Eq, PartialEq)]
struct ReadFlightKey {
    hash: Hash,
    partition: Option<Partition>,
    trusted: bool,
    no_local_write: bool,
}

/// Writes one word, which [`PassThroughHasher`] takes as the hash: the first eight bytes of the
/// content hash, which is uniform already, folded with the partition's where the key carries one,
/// and with whether the remote is trusted and whether the read stores nothing locally, a bit each.
impl std::hash::Hash for ReadFlightKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let partition = self
            .partition
            .map_or(0, |partition| leading_word(partition.as_ref()));
        let flags = u64::from(self.trusted) | (u64::from(self.no_local_write) << 1);
        state.write_u64(leading_word(self.hash.as_ref()) ^ partition ^ flags);
    }
}

fn leading_word(bytes: &[u8]) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(word)
}

/// A caller joining a read single-flight: the store it reads into with its partition and address,
/// and what it keeps there.
struct Joining {
    key: StoreInFlightKey,
    keep: Option<Keep>,
}

/// A read in flight: the leader's store, partition and address, what is kept of the fetch there,
/// whether the leader took it, and the channel its outcome goes out on, which the first follower
/// creates.
struct ReadFlightEntry {
    leader: StoreInFlightKey,
    keep: Option<Keep>,
    taken: bool,
    sender: Option<watch::Sender<ReadFlightResult>>,
}

impl ReadFlightEntry {
    /// Subscribes `joining` to this flight's outcome, installing `channel` where the flight has
    /// no channel yet; `None` where it has none and `channel` holds none. A follower reading the
    /// leader's partition and address in the leader's store, the association the leader's put
    /// stores, merges what it keeps into what the leader keeps there until the leader takes it, and
    /// once taken, is kept where what was taken covers what it keeps.
    fn subscribe(
        &mut self,
        joining: &Joining,
        channel: &mut Option<Channel>,
    ) -> Option<ReadFlightFollower> {
        let receiver = if let Some(sender) = &self.sender {
            sender.subscribe()
        } else {
            let (sender, receiver) = channel.take()?;
            self.sender = Some(sender);
            receiver
        };
        let same_address = joining.key.partition == self.leader.partition
            && joining.key.address == self.leader.address;
        let here = same_address && joining.key.store == self.leader.store;
        let merged = joining
            .keep
            .filter(|&keep| here && (!self.taken || self.keep >= Some(keep)));
        if let Some(keep) = merged
            && !self.taken
        {
            self.keep = Some(self.keep.map_or(keep, |kept| kept.max(keep)));
        }
        Some(ReadFlightFollower {
            same_address,
            kept: joining.keep.is_none() || merged.is_some(),
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

/// A follower's view of a read single-flight: what [`Self::same_address`] and [`Self::kept`]
/// answer, and the receiver its outcome arrives on.
#[lore_macro::test_pub]
pub(crate) struct ReadFlightFollower {
    same_address: bool,
    kept: bool,
    receiver: watch::Receiver<ReadFlightResult>,
}

impl ReadFlightFollower {
    /// Whether the leader reads the follower's partition and address, so its outcome is the
    /// follower's own.
    #[lore_macro::test_pub]
    pub(crate) fn same_address(&self) -> bool {
        self.same_address
    }

    /// Whether the follower has nothing left to keep of the fetch: it keeps nothing, or the leader
    /// keeps what it keeps, in its store before the flight ends, as [`ReadFlightEntry::subscribe`]
    /// merges it. A follower with something left to keep stores the fetch itself.
    #[lore_macro::test_pub]
    pub(crate) fn kept(&self) -> bool {
        self.kept
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

    /// What the flight keeps of the fetch in the leader's store, its own and its followers' there,
    /// which the leader must store before it completes the flight. A follower joining afterwards
    /// keeps its own unless what was taken covers it ([`ReadFlightFollower::kept`]). One shard lock
    /// serves it.
    #[lore_macro::test_pub]
    pub(crate) fn take_keep(&self) -> Option<Keep> {
        let mut flight = self
            .key
            .and_then(|key| READ_IN_FLIGHT.get()?.get_mut(&key))?;
        flight.taken = true;
        flight.keep
    }

    /// End the flight and publish `result` to the followers subscribed to it, cloned only where
    /// one is.
    #[lore_macro::test_pub]
    pub(crate) fn complete(mut self, result: &Result<Loaded, StorageError>) {
        let Some(flight) = self.key.take().and_then(close) else {
            return;
        };
        debug_assert!(
            result.is_err() || flight.taken || flight.keep.is_none(),
            "a leader completed with a fetch without taking what its flight keeps"
        );
        if let Some(sender) = flight.sender.filter(|sender| sender.receiver_count() > 0) {
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

/// Removes the flight for `key`, and answers its entry, whose channel a follower may still listen
/// on. Removing the entry first closes the flight to further followers, so every follower that
/// joined holds a receiver of what is sent. A flight ended without a result drops its channel,
/// which tells its followers to join again.
fn close(key: ReadFlightKey) -> Option<ReadFlightEntry> {
    READ_IN_FLIGHT.get()?.remove(&key).map(|(_, flight)| flight)
}

/// Join the read single-flight for `address`'s content: its hash, `partition` where
/// `isolate_partitions` says the store serves each association only what it holds for it rather
/// than whatever it holds for the hash ([`crate::immutable_store::ImmutableStore::read_scope`]),
/// and whether `options` trust the remote ([`ReadOptions::trust_remote`]) and store nothing the
/// read fetches locally ([`ReadOptions::no_local_write`]). The first caller becomes the leader and
/// must perform the read, store what [`ReadFlightGuard::take_keep`] answers where the read
/// succeeded, then call `ReadFlightGuard::complete`. Later callers with the same key become
/// followers, whatever their context and store, and should pass the follower to `await_flight`.
/// `store` is the store the caller reads into, and `keep` what it keeps there of the fetch; the
/// leader keeps a follower's as [`ReadFlightEntry::subscribe`] describes. The caller must hold
/// `store` while it leads, so no other store takes its address while the flight is open.
///
/// One shard lock serves each join, held only to insert the flight or subscribe to it, and nothing
/// is allocated under it. A leader allocates nothing. The first follower of a flight creates its
/// channel, outside the lock, and takes the lock again to install it.
#[lore_macro::test_pub]
pub(crate) fn join_read_flight(
    store: FlightStore,
    keep: Option<Keep>,
    partition: Partition,
    address: Address,
    isolate_partitions: bool,
    options: ReadOptions,
) -> ReadFlight {
    let key = ReadFlightKey {
        hash: address.hash,
        partition: isolate_partitions.then_some(partition),
        trusted: options.trust_remote,
        no_local_write: options.no_local_write,
    };
    let in_flight =
        READ_IN_FLIGHT.get_or_init(|| DashMap::with_hasher(BuildHasherDefault::default()));
    let joining = Joining {
        key: StoreInFlightKey {
            store,
            partition,
            address,
        },
        keep,
    };
    let mut channel = None;
    loop {
        // `DashMap::entry` is safe here as it is not held across any awaits and no other locks are acquired while held
        #[allow(clippy::disallowed_methods)]
        let follower = match in_flight.entry(key) {
            Entry::Vacant(vacant) => {
                vacant.insert(ReadFlightEntry {
                    leader: joining.key,
                    keep,
                    taken: false,
                    sender: None,
                });
                return ReadFlight::Leader(ReadFlightGuard { key: Some(key) });
            }
            Entry::Occupied(mut occupied) => occupied.get_mut().subscribe(&joining, &mut channel),
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
