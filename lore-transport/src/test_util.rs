// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Stub peers for tests that drive a [`StorageSession`] without a server behind it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::error::NotFound;
use lore_base::error::SlowDown;
use lore_base::types::*;

use crate::connection::Connection;
use crate::error::ProtocolError;
use crate::session::StorageSession;
use crate::traits::Storage;

/// A peer that holds the fragments uploaded to it, or given to it to begin with, without their
/// payloads. It answers a metadata request for one of them with the fragment it holds, and for any
/// other address with `NotFound`, or every metadata request with the failure
/// [`with_metadata_failure`](Self::with_metadata_failure) sets. It answers copy requests as
/// [`with_copy_answers`](Self::with_copy_answers) sets, by default with success, and a copy that
/// succeeds of a fragment it holds makes it hold the fragment under the target context too. It
/// takes every upload, with or without a key to publish, and every mutable store, and serves no
/// payloads, queries or verifications. It counts the metadata requests, the uploads and the copy
/// requests.
#[derive(Default)]
pub struct StubPeer {
    held: Mutex<Vec<(Address, Fragment)>>,
    metadata_failure: Option<ProtocolError>,
    copy_slow_downs: usize,
    copy_refusal: Option<ProtocolError>,
    metadata_requests: AtomicUsize,
    puts: AtomicUsize,
    copies: AtomicUsize,
}

impl StubPeer {
    /// Holds `held` to begin with.
    pub fn with_held(mut self, held: Vec<(Address, Fragment)>) -> Self {
        self.held = Mutex::new(held);
        self
    }

    /// Answers every metadata request with `failure`.
    pub fn with_metadata_failure(mut self, failure: ProtocolError) -> Self {
        self.metadata_failure = Some(failure);
        self
    }

    /// Answers the first `slow_downs` copy requests with `SlowDown`, and later ones with `refusal`,
    /// or success if no refusal is set.
    pub fn with_copy_answers(mut self, slow_downs: usize, refusal: Option<ProtocolError>) -> Self {
        self.copy_slow_downs = slow_downs;
        self.copy_refusal = refusal;
        self
    }

    /// A session on `partition` whose requests all reach this peer.
    pub fn session(self: &Arc<Self>, partition: Partition) -> StorageSession {
        StorageSession::resolved(
            self.clone(),
            Connection::detached(Vec::new()),
            1,
            partition,
            Arc::from("stub-peer"),
        )
    }

    /// Metadata requests received so far.
    pub fn metadata_requests(&self) -> usize {
        self.metadata_requests.load(Ordering::SeqCst)
    }

    /// Uploads received so far.
    pub fn puts(&self) -> usize {
        self.puts.load(Ordering::SeqCst)
    }

    /// Copy requests received so far, including those answered with `SlowDown`.
    pub fn copies(&self) -> usize {
        self.copies.load(Ordering::SeqCst)
    }

    /// The fragments held, even where a panicking test thread poisoned the lock.
    fn held(&self) -> MutexGuard<'_, Vec<(Address, Fragment)>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the upload of `fragment` at `address`, counting it.
    fn take(&self, address: Address, fragment: Fragment) {
        self.puts.fetch_add(1, Ordering::SeqCst);
        self.held().push((address, fragment));
    }
}

#[async_trait]
impl Storage for StubPeer {
    async fn session_start(
        &self,
        _partition: Partition,
        _correlation_id: &str,
    ) -> Result<u32, ProtocolError> {
        Ok(1)
    }

    async fn session_stop(&self, _session_id: u32) -> Result<(), ProtocolError> {
        Ok(())
    }

    async fn get(
        &self,
        _session_id: u32,
        _address: &Address,
    ) -> Result<(Fragment, Bytes), ProtocolError> {
        Err(ProtocolError::internal("the stub peer serves no payloads"))
    }

    async fn get_metadata(
        &self,
        _session_id: u32,
        address: &Address,
    ) -> Result<Fragment, ProtocolError> {
        self.metadata_requests.fetch_add(1, Ordering::SeqCst);
        if let Some(failure) = &self.metadata_failure {
            return Err(failure.clone());
        }
        self.held()
            .iter()
            .find(|(held, _)| held == address)
            .map(|(_, fragment)| *fragment)
            .ok_or_else(|| ProtocolError::from(NotFound))
    }

    async fn put(
        &self,
        _session_id: u32,
        address: Address,
        fragment: Fragment,
        _payload: Option<Bytes>,
    ) -> Result<(), ProtocolError> {
        self.take(address, fragment);
        Ok(())
    }

    async fn put_resolved(
        &self,
        _session_id: u32,
        _key: &Hash,
        address: Address,
        fragment: Fragment,
        _payload: Option<Bytes>,
    ) -> Result<(), ProtocolError> {
        self.take(address, fragment);
        Ok(())
    }

    async fn mutable_store(
        &self,
        _session_id: u32,
        _key: Hash,
        _value: Hash,
        _key_type: KeyType,
    ) -> Result<(), ProtocolError> {
        Ok(())
    }

    async fn query(&self, _session_id: u32, _address: &[Address]) -> Result<Bytes, ProtocolError> {
        Err(ProtocolError::internal("the stub peer answers no queries"))
    }

    async fn verify(
        &self,
        _session_id: u32,
        _address: &Address,
        _heal: bool,
    ) -> Result<VerifyResult, ProtocolError> {
        Err(ProtocolError::internal("the stub peer verifies nothing"))
    }

    async fn copy(
        &self,
        _session_id: u32,
        _source_partition: Partition,
        source_address: Address,
        target_context: Context,
    ) -> Result<(), ProtocolError> {
        if self.copies.fetch_add(1, Ordering::SeqCst) < self.copy_slow_downs {
            return Err(ProtocolError::from(SlowDown));
        }
        if let Some(refusal) = &self.copy_refusal {
            return Err(refusal.clone());
        }
        let mut held = self.held();
        if let Some(fragment) = held
            .iter()
            .find(|(address, _)| *address == source_address)
            .map(|(_, fragment)| *fragment)
        {
            held.push((
                Address {
                    hash: source_address.hash,
                    context: target_context,
                },
                fragment,
            ));
        }
        Ok(())
    }
}

/// A peer that holds what it is sent: it keeps each put's fragment and payload by address, serves
/// gets of them, and answers a get of anything else `NotFound`, as a server does, which a read
/// reports as the address not found. It answers a query with one byte per address, zero where it
/// holds the address, and refuses copies and verifications. A peer built with
/// [`refusing_puts`](Self::refusing_puts), or told to with [`refuse_puts`](Self::refuse_puts),
/// answers every put with its refusal and keeps nothing, and one built with
/// [`answering_queries_short`](Self::answering_queries_short) leaves the last address of each query
/// unanswered.
pub struct MemoryPeer {
    held: Mutex<BTreeMap<Address, (Fragment, Option<Bytes>)>>,
    put_refusal: Mutex<Option<ProtocolError>>,
    short_queries: bool,
    puts: AtomicUsize,
    gets: AtomicUsize,
    queries: AtomicUsize,
}

impl MemoryPeer {
    /// A peer that holds nothing yet and keeps every put.
    pub fn new() -> Arc<Self> {
        Self::built(None, false)
    }

    /// A peer that answers every put with `refusal`.
    pub fn refusing_puts(refusal: ProtocolError) -> Arc<Self> {
        Self::built(Some(refusal), false)
    }

    /// A peer that answers each query with one status fewer than the addresses it names.
    pub fn answering_queries_short() -> Arc<Self> {
        Self::built(None, true)
    }

    fn built(put_refusal: Option<ProtocolError>, short_queries: bool) -> Arc<Self> {
        Arc::new(Self {
            held: Mutex::default(),
            put_refusal: Mutex::new(put_refusal),
            short_queries,
            puts: AtomicUsize::new(0),
            gets: AtomicUsize::new(0),
            queries: AtomicUsize::new(0),
        })
    }

    /// A session on `partition` whose requests all reach this peer.
    pub fn session(self: &Arc<Self>, partition: Partition) -> StorageSession {
        StorageSession::resolved(
            self.clone(),
            Connection::detached(Vec::new()),
            1,
            partition,
            Arc::from("memory-peer"),
        )
    }

    /// The fragment and payload the peer holds at `address`.
    pub fn held(&self, address: &Address) -> Option<(Fragment, Option<Bytes>)> {
        self.stored().get(address).cloned()
    }

    /// Every address the peer holds, in address order.
    pub fn addresses(&self) -> Vec<Address> {
        self.stored().keys().copied().collect()
    }

    /// What the peer holds, even where a panicking test thread poisoned the lock.
    fn stored(&self) -> MutexGuard<'_, BTreeMap<Address, (Fragment, Option<Bytes>)>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Answers every later put with `refusal`, so that a test can set the peer up with puts first.
    pub fn refuse_puts(&self, refusal: ProtocolError) {
        *self
            .put_refusal
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(refusal);
    }

    /// Puts received so far, refused ones included.
    pub fn puts(&self) -> usize {
        self.puts.load(Ordering::SeqCst)
    }

    /// Gets received so far, of payloads and of fragments alone.
    pub fn gets(&self) -> usize {
        self.gets.load(Ordering::SeqCst)
    }

    /// Queries received so far.
    pub fn queries(&self) -> usize {
        self.queries.load(Ordering::SeqCst)
    }

    fn fragment_at(&self, address: &Address) -> Result<(Fragment, Option<Bytes>), ProtocolError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        self.held(address)
            .ok_or_else(|| ProtocolError::from(NotFound))
    }
}

#[async_trait]
impl Storage for MemoryPeer {
    async fn session_start(
        &self,
        _partition: Partition,
        _correlation_id: &str,
    ) -> Result<u32, ProtocolError> {
        Ok(1)
    }

    async fn session_stop(&self, _session_id: u32) -> Result<(), ProtocolError> {
        Ok(())
    }

    async fn get(
        &self,
        _session_id: u32,
        address: &Address,
    ) -> Result<(Fragment, Bytes), ProtocolError> {
        match self.fragment_at(address)? {
            (fragment, Some(payload)) => Ok((fragment, payload)),
            (_, None) => Err(ProtocolError::internal(
                "the memory peer holds no payload at the address",
            )),
        }
    }

    async fn get_metadata(
        &self,
        _session_id: u32,
        address: &Address,
    ) -> Result<Fragment, ProtocolError> {
        Ok(self.fragment_at(address)?.0)
    }

    async fn put(
        &self,
        _session_id: u32,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
    ) -> Result<(), ProtocolError> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        if let Some(refusal) = &*self
            .put_refusal
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            return Err(refusal.clone());
        }
        self.stored().insert(address, (fragment, payload));
        Ok(())
    }

    async fn query(&self, _session_id: u32, addresses: &[Address]) -> Result<Bytes, ProtocolError> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        let answered = addresses
            .len()
            .saturating_sub(usize::from(self.short_queries));
        let held = self.stored();
        Ok(addresses[..answered]
            .iter()
            .map(|address| u8::from(!held.contains_key(address)))
            .collect())
    }

    async fn verify(
        &self,
        _session_id: u32,
        _address: &Address,
        _heal: bool,
    ) -> Result<VerifyResult, ProtocolError> {
        Err(ProtocolError::internal("the memory peer verifies nothing"))
    }

    async fn copy(
        &self,
        _session_id: u32,
        _source_partition: Partition,
        _source_address: Address,
        _target_context: Context,
    ) -> Result<(), ProtocolError> {
        Err(ProtocolError::internal("the memory peer takes no copies"))
    }
}
