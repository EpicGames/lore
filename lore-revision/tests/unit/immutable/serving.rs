// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A remote standing in for a server, serving what a repository stores, altered where a test asks.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use lore_base::error::NotFound;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::HealResult;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_base::types::VerifyResult;
use lore_revision::repository::RemoteState;
use lore_revision::repository::RepositoryContext;
use lore_revision::test_util::context_with_state;
use lore_storage::StoreError;
use lore_transport::Connection;
use lore_transport::ProtocolError;
use lore_transport::StorageSession;

use super::session_tests::under_execution_context;

/// How a [`ServingStorage`] departs from serving what it holds: the address it answers with its
/// payload altered, a byte short where it is `shortened` and in its first byte otherwise, until a
/// heal where it `heals`, and after that heal too where it `heal_fails`, though it reports the heal
/// made; the address whose first get it answers `SlowDown`; whether it answers every get
/// `NotSupported`, as a remote that does not support the request; whether it fails every get on
/// the first session it started, as a server that lost its sessions; the gate it holds each get at
/// until as many as the gate takes are in; the content whose first get it holds for a
/// notification; and the refusal of the credentials it answers every get, verify and mutable load
/// with.
#[derive(Default)]
pub(crate) struct Serving {
    pub(super) tampered: Option<Address>,
    pub(super) shortened: bool,
    pub(super) heals: bool,
    pub(super) heal_fails: bool,
    pub(super) slow_down: Option<Address>,
    pub(super) unsupported: bool,
    pub(super) forgets_first_session: bool,
    pub(super) gate: Option<Arc<tokio::sync::Barrier>>,
    pub(super) hold: Option<(Hash, Arc<tokio::sync::Notify>)>,
    pub(super) refusal: Option<ProtocolError>,
}

/// A remote serving what `source` stores as `serving` says, recording each address asked of it, the
/// sessions started on it, numbered from 1, the sessions stopped, the verifies asked of it and the
/// most gets it held at once.
pub(crate) struct ServingStorage {
    source: Arc<RepositoryContext>,
    serving: Serving,
    asked: std::sync::Mutex<Vec<Address>>,
    starts: AtomicU32,
    stops: AtomicU32,
    verifies: AtomicUsize,
    healed: AtomicBool,
    slowed: AtomicBool,
    in_flight: AtomicUsize,
    peak: AtomicUsize,
}

#[async_trait::async_trait]
impl lore_transport::Storage for ServingStorage {
    async fn session_start(
        &self,
        _partition: Partition,
        _correlation_id: &str,
    ) -> Result<u32, ProtocolError> {
        Ok(self.starts.fetch_add(1, Ordering::SeqCst) + 1)
    }

    async fn session_stop(&self, _session_id: u32) -> Result<(), ProtocolError> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn get(
        &self,
        session_id: u32,
        address: &Address,
    ) -> Result<(Fragment, Bytes), ProtocolError> {
        if let Some(refusal) = &self.serving.refusal {
            return Err(refusal.clone());
        }
        if self.serving.forgets_first_session && session_id == 1 {
            return Err(ProtocolError::internal(
                "the serving remote no longer knows session 1",
            ));
        }
        let first = {
            let mut asked = self.asked.lock().expect("asked");
            asked.push(*address);
            asked
                .iter()
                .filter(|asked| asked.hash == address.hash)
                .count()
                == 1
        };
        if first
            && let Some((hash, hold)) = &self.serving.hold
            && *hash == address.hash
        {
            hold.notified().await;
        }
        if self.serving.slow_down == Some(*address) && !self.slowed.swap(true, Ordering::SeqCst) {
            return Err(ProtocolError::from(lore_base::error::SlowDown));
        }
        if self.serving.unsupported {
            return Err(ProtocolError::from(lore_base::error::NotSupported {
                operation: "the serving remote supports no get".to_string(),
            }));
        }
        let in_flight = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(in_flight, Ordering::SeqCst);
        if let Some(gate) = &self.serving.gate {
            gate.wait().await;
        }
        let served = self.serve(*address).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        served
    }

    async fn put(
        &self,
        _session_id: u32,
        _address: Address,
        _fragment: Fragment,
        _payload: Option<Bytes>,
    ) -> Result<(), ProtocolError> {
        Err(ProtocolError::internal("the serving remote takes no puts"))
    }

    async fn query(&self, _session_id: u32, _address: &[Address]) -> Result<Bytes, ProtocolError> {
        Err(ProtocolError::internal(
            "the serving remote takes no queries",
        ))
    }

    /// Answers `NotFound` for an address `source` stores nothing at, and heals only the tampered
    /// address, where the remote `heals`.
    async fn verify(
        &self,
        _session_id: u32,
        address: &Address,
        heal: bool,
    ) -> Result<VerifyResult, ProtocolError> {
        self.verifies.fetch_add(1, Ordering::SeqCst);
        if let Some(refusal) = &self.serving.refusal {
            return Err(refusal.clone());
        }
        if let Err(err @ ProtocolError::NotFound(_)) = self.serve(*address).await {
            return Err(err);
        }
        if !(heal && self.serving.heals && self.serving.tampered == Some(*address)) {
            return Err(ProtocolError::internal("the serving remote heals nothing"));
        }
        if !self.serving.heal_fails {
            self.healed.store(true, Ordering::SeqCst);
        }
        Ok(VerifyResult {
            corrupted: true,
            healed: HealResult::Healed,
        })
    }

    async fn copy(
        &self,
        _session_id: u32,
        _source_partition: Partition,
        _source_address: Address,
        _target_context: Context,
    ) -> Result<(), ProtocolError> {
        Err(ProtocolError::internal(
            "the serving remote takes no copies",
        ))
    }

    async fn mutable_load(
        &self,
        _session_id: u32,
        _key: &Hash,
        _key_type: KeyType,
    ) -> Result<Hash, ProtocolError> {
        Err(self
            .serving
            .refusal
            .clone()
            .unwrap_or_else(|| ProtocolError::internal("the serving remote holds no mutable keys")))
    }
}

impl ServingStorage {
    /// What `source` stores at `address`, altered where it is the tampered address and not healed,
    /// and `NotFound` where it stores nothing.
    async fn serve(&self, address: Address) -> Result<(Fragment, Bytes), ProtocolError> {
        let stored = self
            .source
            .immutable_store()
            .get(self.source.id, address)
            .await
            .map_err(|err| match err {
                StoreError::AddressNotFound(_) => ProtocolError::from(NotFound),
                err => ProtocolError::internal(err.to_string()),
            })?;
        let payload = stored
            .payload
            .ok_or_else(|| ProtocolError::internal("the source holds no payload"))?;
        if self.serving.tampered == Some(address) && !self.healed.load(Ordering::SeqCst) {
            let mut altered = payload.to_vec();
            if self.serving.shortened {
                altered.pop();
            } else {
                altered[0] ^= 0xff;
            }
            return Ok((stored.fragment, Bytes::from(altered)));
        }
        Ok((stored.fragment, payload))
    }

    /// The most gets the remote held at once.
    pub(super) fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    /// The distinct addresses asked of the remote.
    pub(super) fn asked(&self) -> BTreeSet<Address> {
        self.asked.lock().expect("asked").iter().copied().collect()
    }

    /// How many gets the remote was asked.
    pub(super) fn gets(&self) -> usize {
        self.asked.lock().expect("asked").len()
    }

    /// How many sessions were started on the remote.
    pub(super) fn starts(&self) -> u32 {
        self.starts.load(Ordering::SeqCst)
    }

    /// How many sessions were stopped on the remote.
    pub(super) fn stops(&self) -> u32 {
        self.stops.load(Ordering::SeqCst)
    }

    /// How many verifies the remote was asked.
    pub(super) fn verifies(&self) -> usize {
        self.verifies.load(Ordering::SeqCst)
    }

    /// How many gets the remote was asked for `address`.
    pub(super) fn gets_of(&self, address: Address) -> usize {
        self.asked
            .lock()
            .expect("asked")
            .iter()
            .filter(|asked| **asked == address)
            .count()
    }
}

/// A remote serving what `source` stores as `serving` says.
pub(crate) fn serving_remote(
    source: &Arc<RepositoryContext>,
    serving: Serving,
) -> Arc<ServingStorage> {
    Arc::new(ServingStorage {
        source: source.clone(),
        serving,
        asked: std::sync::Mutex::default(),
        starts: AtomicU32::new(0),
        stops: AtomicU32::new(0),
        verifies: AtomicUsize::new(0),
        healed: AtomicBool::new(false),
        slowed: AtomicBool::new(false),
        in_flight: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    })
}

/// A session over a remote serving what `source` stores as `serving` says ([`ServingStorage`]),
/// and that remote.
pub(super) async fn serving_session(
    source: &Arc<RepositoryContext>,
    serving: Serving,
) -> (Arc<StorageSession>, Arc<ServingStorage>) {
    let storage = serving_remote(source, serving);
    let connection = Connection::detached(vec![storage.clone()]);
    let session = under_execution_context(|| async {
        connection
            .session(source.id, "cache")
            .await
            .expect("a session over the serving remote")
    })
    .await;
    (session, storage)
}

/// `content` stored with its payload under `flags` in a repository of its own, standing in for the
/// remote's copy, and its address and fragment.
pub(crate) async fn remote_copy(
    content: &'static [u8],
    seed: u8,
    flags: u32,
) -> (Arc<RepositoryContext>, Address, Fragment) {
    let source = context_with_state(RemoteState::Offline).await;
    let address = Address {
        hash: lore_storage::hash::hash_slice(content),
        context: Context::from([seed; 16]),
    };
    let fragment = Fragment {
        flags,
        size_payload: content.len() as u32,
        size_content: content.len() as u64,
    };
    source
        .immutable_store()
        .put(
            source.id,
            address,
            fragment,
            Some(Bytes::from_static(content)),
            false,
        )
        .await
        .expect("the remote's copy is stored");
    (source, address, fragment)
}
