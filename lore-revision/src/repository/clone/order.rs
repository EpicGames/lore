// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The order in which a tree-walk clone's discovery walk hands files to the writers: depth-first
//! tree order, with a directory's files before the subtrees of its subdirectories, but for the
//! walked files handed ahead while the writers run low on work ([`LOW_WATER`]).

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::ops::Bound;
use std::sync::Arc;

use tokio::sync::Notify;

use super::CloneWorkItem;
use crate::node::BLOCK_NODE_COUNT;

/// Pending items that may start walking at once: the window of [`Order`]. A walked item dispatches
/// the items of the directories it reaches, so the window bounds how many items discovery walks
/// ahead of the first entry while a block loads. It is large enough that block loads overlap as
/// they do in a walk that discovery runs without limit; [`CAP`], not the window, bounds the files
/// the order holds.
pub const WINDOW: usize = 8_192;

/// Files walks may reserve: held from the start of a walk until the files it walked are sent to
/// the work channel. The first entry of the order walks past it, so the order holds at most `CAP`
/// plus one segment of files, which with [`super::ORDERED_WORK_CHANNEL_CAPACITY`] bounds the files
/// discovered and not yet written.
pub const CAP: usize = 150_000;

/// Files in the work channel below which the order releases a walked segment ahead of the first
/// entry rather than wait for it. It is above the writers' [`super::CLONE_FILE_MAX`] files in
/// flight, so the writers wait for order only while the channel holds files for them.
pub const LOW_WATER: usize = 12_000;

/// Files a walk reserves before it starts: a segment walks the nodes of one block.
const SEGMENT_FILES: usize = BLOCK_NODE_COUNT;

/// The tag of a segment of a directory's chain in an [`OrderKey`].
const SEGMENT: u32 = 0;

/// The tag of a subtree, a directory or link in a directory's chain, in an [`OrderKey`].
const SUBTREE: u32 = 1;

/// An item's place in depth-first tree order, or none for a walk without an order.
///
/// A key is a sequence of `u32`, each 4 big-endian bytes, compared as bytes, so a prefix sorts
/// first. A directory's chain is numbered by position from 0, counting every node. A directory or
/// link at position `j` of the chain of directory `D` has the key `K(D) ++ [1, j]`, and the root's
/// key is empty. The segment of `D`'s chain that starts at position `s` has the key
/// `K(D) ++ [0, s]`. So a directory's segments come in chain order before any subtree of its
/// entries, and the subtrees follow in chain order. Each key an item dispatches sorts after its
/// own.
#[lore_macro::test_pub]
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct OrderKey(Option<Arc<[u8]>>);

impl OrderKey {
    /// The first segment of the root's chain.
    #[lore_macro::test_pub]
    pub(super) fn root() -> Self {
        Self(Some(Arc::new(word_pair(SEGMENT, 0))))
    }

    /// The position in its directory's chain where the segment with this key starts.
    #[lore_macro::test_pub]
    pub(super) fn segment_start(&self) -> u32 {
        self.0
            .as_deref()
            .and_then(|bytes| bytes.last_chunk::<4>())
            .map_or(0, |word| u32::from_be_bytes(*word))
    }

    /// The segment of the same directory's chain that starts at `position`.
    #[lore_macro::test_pub]
    pub(super) fn continuation(&self, position: u32) -> Self {
        self.with_tail(4, position.to_be_bytes())
    }

    /// The subtree of the directory or link at `position` in the chain of this segment.
    #[lore_macro::test_pub]
    pub(super) fn subtree(&self, position: u32) -> Self {
        self.with_tail(8, word_pair(SUBTREE, position))
    }

    /// The first segment of the chain of the directory or link at `position` in the chain of this
    /// segment: [`Self::subtree`] then [`Self::first_segment`], built in one allocation.
    #[lore_macro::test_pub]
    pub(super) fn child_segment(&self, position: u32) -> Self {
        let mut tail = [0; 16];
        tail[..8].copy_from_slice(&word_pair(SUBTREE, position));
        tail[8..].copy_from_slice(&word_pair(SEGMENT, 0));
        self.with_tail(8, tail)
    }

    /// The first segment of the chain of the subtree with this key.
    #[lore_macro::test_pub]
    pub(super) fn first_segment(&self) -> Self {
        self.with_tail(0, word_pair(SEGMENT, 0))
    }

    /// This key without its last `cut` bytes, followed by `tail`. Collected from an iterator of
    /// known length, the key is built in one allocation.
    fn with_tail<const N: usize>(&self, cut: usize, tail: [u8; N]) -> Self {
        Self(self.0.as_deref().map(|bytes| {
            bytes[..bytes.len().saturating_sub(cut)]
                .iter()
                .copied()
                .chain(tail)
                .collect()
        }))
    }
}

/// `first` and `second` as 8 big-endian bytes.
fn word_pair(first: u32, second: u32) -> [u8; 8] {
    let mut pair = [0; 8];
    pair[..4].copy_from_slice(&first.to_be_bytes());
    pair[4..].copy_from_slice(&second.to_be_bytes());
    pair
}

/// The window, cap and low-water mark of an [`Order`].
#[lore_macro::test_pub]
#[derive(Clone, Copy, Debug)]
pub(super) struct OrderLimits {
    /// Pending items that may start walking at once, at least one ([`WINDOW`]).
    window: usize,
    /// Files walks may reserve ([`CAP`]).
    cap: usize,
    /// Files in the work channel below which a walked segment is released out of order
    /// ([`LOW_WATER`]).
    low_water: usize,
}

impl OrderLimits {
    /// The limits of a clone.
    pub(super) const CLONE: Self = Self {
        window: WINDOW,
        cap: CAP,
        low_water: LOW_WATER,
    };
}

/// What an [`Order`] counted over a walk.
#[lore_macro::test_pub]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct OrderCounts {
    /// Walked segments that held files.
    pub(super) batches: u64,
    /// Segments released ahead of the first entry ([`LOW_WATER`]).
    pub(super) out_of_order: u64,
    /// The most files reserved at once, walked or in a walk.
    pub(super) most_reserved: usize,
}

/// What an [`Order`] answers the emitter ([`Order::next`]).
#[lore_macro::test_pub]
pub(super) enum Release {
    /// The files of a walked segment, to send in this order.
    Files(Vec<CloneWorkItem>),
    /// Nothing to send yet.
    Wait,
    /// The walk finished, and every walked file was released.
    Drained,
    /// The walk stopped.
    Stopped,
}

/// A started item, a walked segment or a held link of an [`Order`].
enum Entry {
    /// An item a block task walks.
    Walking,
    /// The files a walked segment holds, not yet released.
    Walked(Vec<CloneWorkItem>),
    /// A link whose tree is not yet dispatched.
    Link,
}

/// The order of a tree-walk clone's discovery walk, which the walks of the links it enters share.
///
/// Every dispatched item is pending until its block task may start it ([`Self::start`]): while it
/// is in the window, which holds the smallest pending keys, and files can be reserved for a
/// segment, or it is the first entry, which needs no reservation. Its walk collects its files, and
/// the order releases the first entry's files once it is walked ([`Self::next`]). A link's place is
/// held from when the walk reaches it until its tree is dispatched. So the first entry always
/// walks, and each entry in turn is released.
///
/// Waking is the caller's: [`Self::insert`] and [`Self::start`] take the `Notify` of the block
/// task whose item it is, which the order notifies when the item enters the window, becomes the
/// first entry, or may reserve again. A task waiting to reserve is woken only while a segment fits,
/// one at a time, first in, first out, and each task that starts wakes the next.
#[lore_macro::test_pub]
pub(super) struct Order {
    limits: OrderLimits,
    state: parking_lot::Mutex<OrderState>,
    /// Wakes the emitter: a segment walked, an entry removed, the walk finished or stopped.
    changed: Notify,
}

/// The entries of an [`Order`] and its counts.
#[derive(Default)]
struct OrderState {
    /// Dispatched items not yet started, with the `Notify` of the block task holding each.
    pending: BTreeMap<OrderKey, Arc<Notify>>,
    /// Started items, walked segments and links.
    entries: BTreeMap<OrderKey, Entry>,
    /// The largest pending key in the window, which holds the smallest pending keys.
    window_last: Option<OrderKey>,
    /// Pending keys in the window.
    window_len: usize,
    /// Files reserved by walks or held by walked segments.
    reserved: usize,
    /// Block tasks waiting for reserved files to be released, first in, first out.
    cap_waiters: VecDeque<Arc<Notify>>,
    /// The addresses of the `Notify` of each task in `cap_waiters`, which holds each task once.
    cap_waiting: HashSet<usize>,
    finished: bool,
    stopped: bool,
    counts: OrderCounts,
}

impl Order {
    #[lore_macro::test_pub]
    pub(super) fn new(limits: OrderLimits) -> Self {
        Self {
            limits: OrderLimits {
                window: limits.window.max(1),
                ..limits
            },
            state: parking_lot::Mutex::default(),
            changed: Notify::new(),
        }
    }

    /// Adds the pending item `key`, whose block task `wake` wakes.
    #[lore_macro::test_pub]
    pub(super) fn insert(&self, key: OrderKey, wake: &Arc<Notify>) {
        let mut state = self.state.lock();
        if !state.stopped {
            state.insert_pending(key, wake, self.limits.window);
        }
    }

    /// Starts the pending item `key`, whose block task `wake` wakes, where it is in the window and
    /// a segment's files can be reserved for it or it is the first entry. Answers the files
    /// reserved, or `None` where it cannot start yet; it is woken when that may change.
    #[lore_macro::test_pub]
    pub(super) fn start(&self, key: &OrderKey, wake: &Arc<Notify>) -> Option<usize> {
        let mut state = self.state.lock();
        if state.stopped {
            return None;
        }
        let started = state.start(key, wake, self.limits.cap);
        state.wake_cap_waiter(self.limits.cap);
        started
    }

    /// Records the walk of `key`, which [`Self::start`] reserved `reserved` files for, as holding
    /// `files`, and removes it where it holds none.
    #[lore_macro::test_pub]
    pub(super) fn walked(&self, key: &OrderKey, files: Vec<CloneWorkItem>, reserved: usize) {
        {
            let mut state = self.state.lock();
            if state.stopped {
                return;
            }
            state.reserved = state.reserved - reserved + files.len();
            state.counts.most_reserved = state.counts.most_reserved.max(state.reserved);
            state.wake_cap_waiter(self.limits.cap);
            if files.is_empty() {
                state.entries.remove(key);
                state.wake_first();
            } else {
                state.counts.batches += 1;
                if let Some(entry) = state.entries.get_mut(key) {
                    *entry = Entry::Walked(files);
                }
            }
        }
        self.changed.notify_one();
    }

    /// Holds the place of the link `key` until [`Self::release_link`].
    pub(super) fn hold_link(&self, key: OrderKey) {
        let mut state = self.state.lock();
        if !state.stopped {
            state.entries.insert(key, Entry::Link);
        }
    }

    /// Releases the place [`Self::hold_link`] held for `key`.
    pub(super) fn release_link(&self, key: &OrderKey) {
        {
            let mut state = self.state.lock();
            state.entries.remove(key);
            state.wake_first();
        }
        self.changed.notify_one();
    }

    /// The next files to send, with `queued` files in the work channel: the first entry's once it
    /// is walked, or below the low-water mark the first walked segment.
    #[lore_macro::test_pub]
    pub(super) fn next(&self, queued: usize) -> Release {
        let mut state = self.state.lock();
        if state.stopped {
            return Release::Stopped;
        }
        let first_pending = state.pending.first_key_value().map(|(key, _)| key);
        let first_walked = matches!(
            state.entries.first_key_value(),
            Some((key, Entry::Walked(_))) if first_pending.is_none_or(|pending| key < pending)
        );
        let released = if first_walked {
            state.entries.pop_first()
        } else if queued < self.limits.low_water
            && let Some(key) = state
                .entries
                .iter()
                .find(|(_, entry)| matches!(entry, Entry::Walked(_)))
                .map(|(key, _)| key.clone())
        {
            state.counts.out_of_order += 1;
            state.entries.remove_entry(&key)
        } else {
            None
        };
        match released {
            Some((_, Entry::Walked(files))) => {
                state.reserved -= files.len();
                state.wake_cap_waiter(self.limits.cap);
                state.wake_first();
                Release::Files(files)
            }
            _ if state.finished && state.pending.is_empty() && state.entries.is_empty() => {
                Release::Drained
            }
            _ => Release::Wait,
        }
    }

    /// Waits for a change [`Self::next`] may answer differently after.
    pub(super) fn changed(&self) -> impl Future<Output = ()> + '_ {
        self.changed.notified()
    }

    /// Marks the walk finished: nothing more is inserted.
    pub(super) fn finish(&self) {
        self.state.lock().finished = true;
        self.changed.notify_one();
    }

    /// Stops the order, dropping what it holds.
    pub(super) fn stop(&self) {
        {
            let mut state = self.state.lock();
            state.stopped = true;
            state.pending.clear();
            state.entries.clear();
            state.cap_waiters.clear();
            state.cap_waiting.clear();
        }
        self.changed.notify_one();
    }

    #[lore_macro::test_pub]
    pub(super) fn counts(&self) -> OrderCounts {
        self.state.lock().counts
    }
}

impl OrderState {
    /// Starts the pending `key`, whose block task `wake` wakes, as [`Order::start`] describes,
    /// within `cap`; queues the task to wait for reserved files to be released where none fit.
    fn start(&mut self, key: &OrderKey, wake: &Arc<Notify>, cap: usize) -> Option<usize> {
        if !self.in_window(key) {
            return None;
        }
        let reserved = if self.reserved + SEGMENT_FILES <= cap {
            SEGMENT_FILES
        } else if self.is_first(key) {
            0
        } else {
            if self.cap_waiting.insert(Arc::as_ptr(wake) as usize) {
                self.cap_waiters.push_back(wake.clone());
            }
            return None;
        };
        let key = self.remove_pending(key)?;
        self.entries.insert(key, Entry::Walking);
        self.cap_waiting.remove(&(Arc::as_ptr(wake) as usize));
        self.reserved += reserved;
        self.counts.most_reserved = self.counts.most_reserved.max(self.reserved);
        Some(reserved)
    }

    /// Adds the pending `key`, which joins the window where it is among its smallest keys, and
    /// wakes its block task where it does.
    fn insert_pending(&mut self, key: OrderKey, wake: &Arc<Notify>, window: usize) {
        let joins = if self.window_len < window {
            self.window_len += 1;
            if self.window_last.as_ref().is_none_or(|last| key > *last) {
                self.window_last = Some(key.clone());
            }
            true
        } else {
            self.window_last.as_ref().is_some_and(|last| key < *last)
        };
        self.pending.insert(key, wake.clone());
        if joins {
            if self.window_len == window
                && let Some(last) = &self.window_last
                && self.pending.len() > window
            {
                self.window_last = self
                    .pending
                    .range(..last)
                    .next_back()
                    .map(|(key, _)| key.clone());
            }
            wake.notify_one();
        }
    }

    /// Whether the pending `key` is in the window.
    fn in_window(&self, key: &OrderKey) -> bool {
        self.pending.contains_key(key) && self.window_last.as_ref().is_some_and(|last| key <= last)
    }

    /// Whether the pending `key` is the first entry.
    fn is_first(&self, key: &OrderKey) -> bool {
        self.pending
            .first_key_value()
            .is_some_and(|(first, _)| first == key)
            && self
                .entries
                .first_key_value()
                .is_none_or(|(entry, _)| key < entry)
    }

    /// Removes the pending `key` from the window, which the smallest pending key past it joins, and
    /// answers the key.
    fn remove_pending(&mut self, key: &OrderKey) -> Option<OrderKey> {
        let (key, _) = self.pending.remove_entry(key)?;
        self.window_len -= 1;
        if self.window_last.as_ref() == Some(&key) {
            self.window_last = self
                .pending
                .range(..&key)
                .next_back()
                .map(|(key, _)| key.clone());
        }
        let joining = match &self.window_last {
            Some(last) => self
                .pending
                .range((Bound::Excluded(last), Bound::Unbounded))
                .next(),
            None => self.pending.first_key_value(),
        };
        if let Some((joining, wake)) = joining {
            wake.notify_one();
            self.window_last = Some(joining.clone());
            self.window_len += 1;
        }
        Some(key)
    }

    /// Wakes the block task of the first entry, where it is pending.
    fn wake_first(&self) {
        if let Some((key, wake)) = self.pending.first_key_value()
            && self
                .entries
                .first_key_value()
                .is_none_or(|(entry, _)| key < entry)
        {
            wake.notify_one();
        }
    }

    /// Wakes the block task waiting longest to reserve files, where a segment fits within `cap`. A
    /// task whose item started meanwhile left `cap_waiting` ([`Self::start`]), and its entry is
    /// skipped, so a wake reaches no task that has stopped waiting, its walk ended included.
    fn wake_cap_waiter(&mut self, cap: usize) {
        if self.reserved + SEGMENT_FILES > cap {
            return;
        }
        while let Some(wake) = self.cap_waiters.pop_front() {
            if self.cap_waiting.remove(&(Arc::as_ptr(&wake) as usize)) {
                wake.notify_one();
                return;
            }
        }
    }
}
