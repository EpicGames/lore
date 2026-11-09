// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::Arc;

use dashmap::DashMap;
use dashmap::Entry;
use tokio::sync::broadcast;
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::Sender;

/// What a caller of [`InflightOutput::request`] does for its key.
pub enum RequestRole<Key, Output>
where
    Key: Debug + Eq + Hash,
    Output: Debug,
{
    /// This caller is responsible for producing the result. The [`InflightGuard`]
    /// must be kept alive until the result has been broadcast — dropping it removes
    /// the entry from the map so that future callers start fresh instead of
    /// subscribing to a dead sender.
    RequestMaker(InflightGuard<Key, Output>),

    /// Another caller is already producing the result. Wait on the receiver.
    ResultAwaiter(Receiver<Output>),

    /// Another caller produced the result, and this is the value it holds for later callers while
    /// it finishes the work the result started (see [`InflightGuard::broadcast_and_hold`]).
    Ready(Output),
}

/// A key's entry: a result in production, which callers subscribe to, or, once it is produced, the
/// value held for the callers that come before its holder lets it go. The held value keeps the
/// producer's sender, which closes as the hold drops, for [`InflightOutput::settled`] to wait on.
#[derive(Debug)]
enum Inflight<Output> {
    Producing(Sender<Output>),
    Held(Output, Sender<Output>),
}

/// RAII guard that owns the [`DashMap`] entry for an in-flight request.
///
/// Calling [`broadcast`](Self::broadcast) removes the entry and then sends the
/// result to all waiting receivers. If the guard is dropped without
/// broadcasting (for example, the producing future is cancelled), the entry is
/// also removed so subsequent callers retry instead of hanging on a dead sender.
/// A guard returned by [`broadcast_and_hold`](Self::broadcast_and_hold)
/// removes the held value as it drops.
pub struct InflightGuard<Key, Output>
where
    Key: Debug + Eq + Hash,
    Output: Debug,
{
    requests: Arc<DashMap<Key, Inflight<Output>>>,
    key: Option<Key>,
    sender: Sender<Output>,
}

impl<Key, Output> InflightGuard<Key, Output>
where
    Key: Debug + Eq + Hash,
    Output: Debug + Clone,
{
    /// Removes the map entry, then broadcasts `result` to all waiting receivers.
    pub fn broadcast(mut self, result: &Output) {
        if let Some(key) = self.key.take() {
            self.requests.remove(&key);
            let _ = self.sender.send(result.clone());
        }
    }

    /// Broadcasts `sent` to the waiting receivers, and hands `held` to every caller that requests
    /// the key until the returned guard is dropped. `held` is in place before `sent` is sent, so a
    /// caller arriving between the two takes `held`.
    #[must_use = "the value is held only while the guard lives"]
    pub fn broadcast_and_hold(self, sent: &Output, held: Output) -> Self {
        if let Some(key) = &self.key
            && let Some(mut entry) = self.requests.get_mut(key)
        {
            *entry = Inflight::Held(held, self.sender.clone());
        }
        let _ = self.sender.send(sent.clone());
        self
    }
}

impl<Key, Output> Drop for InflightGuard<Key, Output>
where
    Key: Debug + Eq + Hash,
    Output: Debug,
{
    fn drop(&mut self) {
        // If `broadcast` was not called, clean up so the next caller retries.
        if let Some(key) = self.key.take() {
            self.requests.remove(&key);
        }
    }
}

/// The results in production or held, per key, that callers of [`Self::request`] share.
#[derive(Debug)]
pub struct InflightOutput<Key, Output>
where
    Key: Debug + Eq + Hash,
    Output: Debug,
{
    requests: Arc<DashMap<Key, Inflight<Output>>>,
}

impl<Key, Output> Default for InflightOutput<Key, Output>
where
    Key: Debug + Eq + Hash,
    Output: Debug,
{
    fn default() -> Self {
        InflightOutput {
            requests: Default::default(),
        }
    }
}

impl<Key, Output> InflightOutput<Key, Output>
where
    Key: Debug + Eq + Hash + Clone,
    Output: Debug + Clone,
{
    /// Claim the right to produce the result for `key`, get a receiver for
    /// the result a concurrent caller is already producing, or the result one
    /// holds.
    ///
    /// The `entry` shard write lock is confined to the `match` below: its arms
    /// only construct or subscribe to a broadcast channel or clone a held
    /// result, none awaits or touches `requests` again, and the guard is
    /// dropped before this function returns — so no caller can hold it across
    /// a suspension point.
    #[allow(clippy::disallowed_methods)]
    pub fn request(&self, key: Key) -> RequestRole<Key, Output> {
        match self.requests.entry(key) {
            Entry::Occupied(entry) => match entry.get() {
                Inflight::Producing(sender) => RequestRole::ResultAwaiter(sender.subscribe()),
                Inflight::Held(result, _) => RequestRole::Ready(result.clone()),
            },
            Entry::Vacant(entry) => {
                let (broadcaster, _) = broadcast::channel(1);
                let key = entry.key().clone();
                entry.insert(Inflight::Producing(broadcaster.clone()));
                RequestRole::RequestMaker(InflightGuard {
                    requests: self.requests.clone(),
                    key: Some(key),
                    sender: broadcaster,
                })
            }
        }
    }

    /// Waits until nothing is in production or held for `key`: a result in production has been
    /// sent, and a held one's guard has dropped, so the work it started has ended.
    ///
    /// The shard read lock is taken only to subscribe, and dropped before the wait.
    #[allow(clippy::disallowed_methods)]
    pub async fn settled(&self, key: &Key) {
        loop {
            let mut receiver = match self.requests.get(key).as_deref() {
                None => return,
                Some(Inflight::Producing(sender) | Inflight::Held(_, sender)) => sender.subscribe(),
            };
            // The result sent, or the close as the last sender drops; either way, look again.
            let _ = receiver.recv().await;
        }
    }
}
