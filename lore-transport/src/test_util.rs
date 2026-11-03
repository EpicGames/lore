// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Stub peers for tests that drive a [`StorageSession`] without a server behind it.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::error::SlowDown;
use lore_base::types::*;

use crate::connection::Connection;
use crate::error::ProtocolError;
use crate::session::StorageSession;
use crate::traits::Storage;

/// A peer that answers its first `slow_downs` copy requests with `SlowDown`, and later ones with
/// `refusal`, or success if no refusal is set. It accepts every upload, and serves no reads,
/// queries or verifications.
pub struct CopyingPeer {
    slow_downs: usize,
    refusal: Option<ProtocolError>,
    copies: AtomicUsize,
    puts: AtomicUsize,
}

impl CopyingPeer {
    pub fn new(slow_downs: usize, refusal: Option<ProtocolError>) -> Arc<Self> {
        Arc::new(Self {
            slow_downs,
            refusal,
            copies: AtomicUsize::new(0),
            puts: AtomicUsize::new(0),
        })
    }

    /// A session on `partition` whose requests all reach this peer.
    pub fn session(self: &Arc<Self>, partition: Partition) -> StorageSession {
        StorageSession::resolved(
            self.clone(),
            Connection::detached(Vec::new()),
            1,
            partition,
            Arc::from("copying-peer"),
        )
    }

    /// Copy requests received so far, including those answered with `SlowDown`.
    pub fn copies(&self) -> usize {
        self.copies.load(Ordering::SeqCst)
    }

    /// Uploads received so far.
    pub fn puts(&self) -> usize {
        self.puts.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Storage for CopyingPeer {
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
        Err(ProtocolError::internal("the copying peer serves no reads"))
    }

    async fn put(
        &self,
        _session_id: u32,
        _address: Address,
        _fragment: Fragment,
        _payload: Option<Bytes>,
    ) -> Result<(), ProtocolError> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn query(&self, _session_id: u32, _address: &[Address]) -> Result<Bytes, ProtocolError> {
        Err(ProtocolError::internal(
            "the copying peer answers no queries",
        ))
    }

    async fn verify(
        &self,
        _session_id: u32,
        _address: &Address,
        _heal: bool,
    ) -> Result<VerifyResult, ProtocolError> {
        Err(ProtocolError::internal("the copying peer verifies nothing"))
    }

    async fn copy(
        &self,
        _session_id: u32,
        _source_partition: Partition,
        _source_address: Address,
        _target_context: Context,
    ) -> Result<(), ProtocolError> {
        if self.copies.fetch_add(1, Ordering::SeqCst) < self.slow_downs {
            return Err(ProtocolError::from(SlowDown));
        }
        match &self.refusal {
            Some(refusal) => Err(refusal.clone()),
            None => Ok(()),
        }
    }
}
