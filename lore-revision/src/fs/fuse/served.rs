// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What the provider serves: the filesystem State, the context it is read under (and so the view
//! filter), and the listings built from that State, keyed by node id.
//!
//! A sync changes the State in place and republishes the same State, so a reply computed while
//! it ran carries zero cache timeouts ([`Served::superseded`]). The State itself is replaced only
//! by the first publish onto the empty revision a mount starts on, and the listings only by that
//! publish and a view change, which publishes a fresh listing cache and clears the one it
//! replaces.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use lore_base::types::Hash;

use crate::fs::fuse::listing_cache::ListingCache;
use crate::repository::RepositoryContext;
use crate::state::State;

pub struct Served {
    pub state: Arc<State>,
    pub repository: Arc<RepositoryContext>,
    pub listings: Arc<ListingCache>,
    superseded: AtomicBool,
}

impl Served {
    pub fn new(
        state: Arc<State>,
        repository: Arc<RepositoryContext>,
        listings: Arc<ListingCache>,
    ) -> Self {
        Self {
            state,
            repository,
            listings,
            superseded: AtomicBool::new(false),
        }
    }

    /// Whether a later publish has replaced this one. A reply computed from a superseded one
    /// carries the shortest cache timeout rather than the usual dentry lifetime.
    pub fn superseded(&self) -> bool {
        self.superseded.load(Ordering::Acquire)
    }

    /// The revision the State stands at.
    pub fn revision(&self) -> Hash {
        self.state.revision()
    }
}

/// The current [`Served`]: `load` is the hot path every request takes once, `publish` the cold
/// path a sync takes once.
pub struct ServedCell {
    current: parking_lot::RwLock<Arc<Served>>,
}

impl ServedCell {
    pub fn new(first: Arc<Served>) -> Self {
        Self {
            current: parking_lot::RwLock::new(first),
        }
    }

    /// One atomic increment under an uncontended read lock; never allocates.
    pub fn load(&self) -> Arc<Served> {
        self.current.read().clone()
    }

    /// Installs `next`, marks the outgoing one superseded and hands it back.
    pub fn publish(&self, next: Arc<Served>) -> Arc<Served> {
        let previous = std::mem::replace(&mut *self.current.write(), next);
        previous.superseded.store(true, Ordering::Release);
        previous
    }
}
