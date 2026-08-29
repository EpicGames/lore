// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::atomic::AtomicUsize;

use bytes::Bytes;
pub use lore_base::types::store_types::KeyType;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::Stream;

use crate::Context;
use crate::Fragment;
use crate::Hash;
use crate::Partition;
use crate::immutable_store::StoreError;

/// Progressive match hierarchy for store lookups.
///
/// When querying the store, callers specify a minimum match level.
/// The store returns the best match found up to the requested level.
///
/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C)]
#[derive(
    Debug, Copy, Clone, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
pub enum StoreMatch {
    #[default]
    MatchNone = 0,
    MatchHash = 1,
    MatchPartition = 2,
    MatchFull = 3,
}

impl StoreMatch {
    pub fn next(&self) -> Option<Self> {
        match self {
            StoreMatch::MatchNone => Some(StoreMatch::MatchHash),
            StoreMatch::MatchHash => Some(StoreMatch::MatchPartition),
            StoreMatch::MatchPartition => Some(StoreMatch::MatchFull),
            StoreMatch::MatchFull => None,
        }
    }

    pub fn prev(&self) -> Option<Self> {
        match self {
            StoreMatch::MatchNone => None,
            StoreMatch::MatchHash => Some(StoreMatch::MatchNone),
            StoreMatch::MatchPartition => Some(StoreMatch::MatchHash),
            StoreMatch::MatchFull => Some(StoreMatch::MatchPartition),
        }
    }

    pub fn is_partial(&self) -> bool {
        self != &StoreMatch::MatchFull
    }
}

impl From<StoreMatch> for u8 {
    fn from(value: StoreMatch) -> Self {
        match value {
            StoreMatch::MatchNone => 0,
            StoreMatch::MatchHash => 1,
            StoreMatch::MatchPartition => 2,
            StoreMatch::MatchFull => 3,
        }
    }
}

impl TryFrom<u8> for StoreMatch {
    type Error = String;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(StoreMatch::MatchNone),
            1 => Ok(StoreMatch::MatchHash),
            2 => Ok(StoreMatch::MatchPartition),
            3 => Ok(StoreMatch::MatchFull),
            unknown => Err(format!("Unknown store match '{unknown}'")),
        }
    }
}

/// What a store found for one address: the fragment describing the payload, the level it was found
/// at, and the payload itself when the caller asked for one.
///
/// [`ImmutableStore::get`] and [`ImmutableStore::get_metadata`] both answer with this, because they
/// are one lookup that differs only in whether the bytes are fetched. A level below
/// [`StoreMatch::MatchFull`] means the content was reached under another context or partition -
/// the same bytes under the same hash, but an association the caller does not hold. That is what
/// lets a caller loading content decide it can duplicate the association rather than store it
/// again.
///
/// [`ImmutableStore::get`]: crate::immutable_store::ImmutableStore::get
/// [`ImmutableStore::get_metadata`]: crate::immutable_store::ImmutableStore::get_metadata
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StoreGetData {
    /// The representation of the stored payload: its compression and its sizes. It describes the
    /// bytes as they are stored rather than the content they decode to, so `size_payload` is what a
    /// reader transfers and `size_content` what it holds once decoded. Zeroed when nothing matched.
    pub fragment: Fragment,
    /// The level the lookup resolved at, gated by the store's
    /// [`read_scope`](crate::immutable_store::ImmutableStore::read_scope). A store that isolates
    /// partitions serves only the association it was asked for, so it answers
    /// [`StoreMatch::MatchFull`] or nothing at all; one that does not may answer weaker, meaning the
    /// same bytes under the same hash reached under a context or partition the caller did not name.
    ///
    /// Absence arrives one of two ways and a store may pick either: [`StoreMatch::MatchNone`] with
    /// nothing described, or a not-found error. `get` always takes the second. So a caller decides
    /// whether it has a match by comparing against `MatchNone`, never by requiring `MatchFull` -
    /// requiring the strongest level turns a usable representation into a miss.
    pub match_made: StoreMatch,
    /// The partition the content was found in. The one asked about whenever it holds the hash;
    /// another only on [`StoreMatch::MatchHash`], which only a store that reads across partitions
    /// can report. Zero when nothing matched.
    pub partition: Partition,
    /// The bytes, when they were asked for. `get_metadata` never carries them; a `get` that matched
    /// always does.
    pub payload: Option<Bytes>,
}

impl StoreGetData {
    /// The representation alone, for a store answering `get_metadata`.
    ///
    /// A level of [`StoreMatch::MatchNone`] collapses to a miss whatever the caller passed
    /// alongside it: nothing matched, so there is no representation to describe and no partition
    /// to name as its source. Stores that answer a miss by forwarding a peer's empty response
    /// would otherwise hand back the partition they were asked about.
    pub fn metadata(fragment: Fragment, match_made: StoreMatch, partition: Partition) -> Self {
        if match_made == StoreMatch::MatchNone {
            return Self::default();
        }

        Self {
            fragment,
            match_made,
            partition,
            payload: None,
        }
    }

    /// Split into the pair a caller that came for the bytes wants. A `get` that reports a match
    /// carries a payload; anything else is a store that did not keep its side of the contract.
    pub fn into_payload(self) -> Result<(Fragment, Bytes), StoreError> {
        match self.payload {
            Some(payload) => Ok((self.fragment, payload)),
            None => Err(StoreError::internal(
                "store reported a match but served no payload",
            )),
        }
    }
}

/// What a store holds for one address.
///
/// No fragment: the representation is what [`crate::immutable_store::ImmutableStore::get_metadata`]
/// answers, and fetching it can cost a round trip that resolution deliberately avoids. The two
/// booleans are the only part of the fragment that callers of the old `query` ever read — where the
/// payload is, which is the store's answer and not something the caller can know.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreMatchResult {
    /// The best level the store established. A lower bound: a store may report less than the truth
    /// when establishing more costs more than it is worth, so a weak level means "no shortcut
    /// available", never "not there".
    pub match_made: StoreMatch,
    /// The partition the content was found in. The one asked about whenever it holds the hash;
    /// another only on [`StoreMatch::MatchHash`], which only a store that reads across partitions
    /// can report. Zero when nothing matched.
    ///
    /// This is what lets a caller holding content already stored elsewhere name a source to copy
    /// from rather than transfer the payload again.
    pub partition: Partition,
    /// The context the content was found under, completing the source address the partition begins.
    /// Optional where the partition is not: zero means unnamed, which
    /// [`ImmutableStore::copy`](crate::immutable_store::ImmutableStore::copy) takes as any
    /// association the source partition holds.
    pub context: Context,
    /// Whether the payload is held locally.
    pub stored_local: bool,
    /// Whether the payload is durable.
    pub stored_durable: bool,
}

impl StoreMatchResult {
    /// The source address a copy reads from: `hash` under the context this match named, if any.
    pub fn source_address(&self, hash: Hash) -> crate::Address {
        crate::Address {
            hash,
            context: self.context,
        }
    }
}

#[repr(C)]
#[derive(Debug, Default)]
pub struct StoreObliterateStats {
    pub num_fragments: AtomicUsize,
    pub num_payloads: AtomicUsize,
}

pub struct KeyValueStream {
    channel: UnboundedReceiver<(Hash, Hash)>,
}

impl KeyValueStream {
    pub fn new() -> (Self, UnboundedSender<(Hash, Hash)>) {
        // Unbounded to ensure not blocking while holding group bucket read lock
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self { channel: rx }, tx)
    }

    pub fn channel(self) -> UnboundedReceiver<(Hash, Hash)> {
        self.channel
    }
}

impl Stream for KeyValueStream
where
    Self: Unpin,
{
    type Item = (Hash, Hash);

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.get_mut().channel.poll_recv(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}
