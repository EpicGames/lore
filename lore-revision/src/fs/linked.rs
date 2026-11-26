// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The linked repositories' states a provider serving links virtually enters (ADR-00036): the
//! state of each (repository, pin) a link pins, opened once and numbered in the order entered, so
//! what is served below a link reads that number's state through that repository's context.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;

use lore_base::types::Hash;
use parking_lot::Mutex;

use crate::lore::RepositoryId;
use crate::node::Node;
use crate::node::NodeID;
use crate::repository::MountKind;
use crate::repository::RepositoryContext;
use crate::state::MAX_LINK_DEPTH;
use crate::state::State;
use crate::state::StateError;

/// A linked repository's state at a pin, read through that repository's context.
pub type Entered = (Arc<RepositoryContext>, Arc<State>);

/// The number of the first chunk's slots, as a power of two; each chunk after doubles.
const FIRST_CHUNK_BITS: u32 = 5;

/// Chunks enough for every number a `u32` holds.
const CHUNKS: usize = (u32::BITS - FIRST_CHUNK_BITS) as usize;

/// What [`LinkedStates::enter`] reached: the node `node_id`, read as `node`, of `state`, of
/// `repository`, and `index`, the number of `state` in the [`LinkedStates`] that entered it, or 0
/// where the node it was given is no link and is answered as it is.
pub struct EnteredNode {
    pub index: u32,
    pub repository: Arc<RepositoryContext>,
    pub state: Arc<State>,
    pub node_id: NodeID,
    pub node: Node,
}

/// The linked states a provider entered, by (repository, pin), each opened once and kept, with
/// the number it was entered under, until the holder drops: a mount holds them until it is mounted
/// again. They are bounded by the distinct (repository, pin) pairs the mount's links have pinned
/// while it ran, so a long-running mount whose links move to new pins often holds a state for each
/// pin it served. Numbers start at 1, leaving 0 for the holder's own state.
///
/// [`Self::get`] answers a number's state with no lock and no reference count taken: each is held
/// in a slot of chunks that only grow, each chunk twice the one before, so a reader borrows it for
/// as long as the holder lives. [`Self::enter`], the cold path, takes a lock.
pub struct LinkedStates {
    by_pin: Mutex<HashMap<(RepositoryId, Hash), u32>>,
    chunks: [OnceLock<Box<[OnceLock<Entered>]>>; CHUNKS],
}

impl Default for LinkedStates {
    fn default() -> Self {
        Self {
            by_pin: Mutex::default(),
            chunks: std::array::from_fn(|_| OnceLock::new()),
        }
    }
}

impl std::fmt::Debug for LinkedStates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkedStates")
            .field("entered", &self.by_pin.lock().len())
            .finish()
    }
}

impl LinkedStates {
    /// The state entered under `index`, with its repository's context; `None` for 0 and for a
    /// number not handed out. Takes no lock and allocates nothing.
    pub fn get(&self, index: u32) -> Option<&Entered> {
        let (chunk, slot) = locate(index)?;
        self.chunks[chunk].get()?.get(slot)?.get()
    }

    /// The number the state of `repository` at `pin` was entered under, if it was.
    pub fn index_of(&self, repository: RepositoryId, pin: Hash) -> Option<u32> {
        self.by_pin.lock().get(&(repository, pin)).copied()
    }

    /// The directory the link `node`, node `node_id` of `state` of `repository`, mounts: its
    /// source node in the linked repository's state at the link's pin, that state opened once
    /// and kept. A link whose source is itself a link is followed, up to [`MAX_LINK_DEPTH`] links
    /// in all, as a tree walk crosses them; `None` past it. A node that is no link is answered as
    /// it is, with index 0. The open of a pin not entered before is boxed ([`open_link`]).
    pub async fn enter(
        &self,
        mut repository: Arc<RepositoryContext>,
        mut state: Arc<State>,
        mut node_id: NodeID,
        mut node: Node,
    ) -> Result<Option<EnteredNode>, StateError> {
        let mut index = 0;
        for crossed in 0..=MAX_LINK_DEPTH {
            if !node.is_link() {
                return Ok(Some(EnteredNode {
                    index,
                    repository,
                    state,
                    node_id,
                    node,
                }));
            }
            if crossed == MAX_LINK_DEPTH {
                break;
            }
            let link = node.linked_node();
            index = if let Some(known) = self.index_of(link.repository, link.revision) {
                known
            } else {
                let opened =
                    Box::pin(open_link(&repository, link.repository, link.revision)).await?;
                self.insert((link.repository, link.revision), opened)?
            };
            (repository, state) = self.entered(index)?.clone();
            node_id = link.node;
            node = state.node(repository.clone(), node_id).await?;
        }
        Ok(None)
    }

    /// [`Self::get`] of a number [`Self::index_of`] or [`Self::insert`] handed out.
    fn entered(&self, index: u32) -> Result<&Entered, StateError> {
        self.get(index)
            .ok_or_else(|| StateError::internal(format!("No linked state entered as {index}")))
    }

    /// Numbers `entered`, the state of `key`, and answers its number: the one an earlier entry of
    /// `key` took where another caller entered it meanwhile, which `entered` then leaves.
    #[lore_macro::test_pub]
    fn insert(&self, key: (RepositoryId, Hash), entered: Entered) -> Result<u32, StateError> {
        let mut by_pin = self.by_pin.lock();
        if let Some(&known) = by_pin.get(&key) {
            return Ok(known);
        }
        let index = u32::try_from(by_pin.len() + 1)
            .ok()
            .filter(|&index| locate(index).is_some())
            .ok_or_else(|| StateError::internal("Too many linked states entered"))?;
        let (chunk, slot) = locate(index).unwrap_or_default();
        let slots = self.chunks[chunk].get_or_init(|| {
            (0..1usize << (chunk as u32 + FIRST_CHUNK_BITS))
                .map(|_| OnceLock::new())
                .collect()
        });
        // Set before the number is published under the lock, so a reader handed the number finds
        // its state.
        if slots[slot].set(entered).is_err() {
            return Err(StateError::internal(format!(
                "Linked state {index} was entered twice"
            )));
        }
        by_pin.insert(key, index);
        Ok(index)
    }
}

/// The chunk and slot of number `index`; `None` for 0 and for a number past the last chunk.
#[lore_macro::test_pub]
fn locate(index: u32) -> Option<(usize, usize)> {
    let at = u64::from(index.checked_sub(1)?) + (1 << FIRST_CHUNK_BITS);
    let bits = u64::BITS - 1 - at.leading_zeros();
    let chunk = (bits - FIRST_CHUNK_BITS) as usize;
    (chunk < CHUNKS).then(|| (chunk, (at - (1 << bits)) as usize))
}

/// The linked repository `linked` at `revision`, reached from `repository`.
///
/// Cold: a pin is opened once per holder. Inline, loading its state would make the future of
/// every request that enters a link as large as opening one, so [`LinkedStates::enter`] boxes it.
pub async fn open_link(
    repository: &Arc<RepositoryContext>,
    linked: RepositoryId,
    revision: Hash,
) -> Result<Entered, StateError> {
    let linked = repository.to_mounted_context_on_first_use(linked, MountKind::Link);
    let state = State::deserialize(linked.clone(), revision).await?;
    Ok((linked, state))
}
