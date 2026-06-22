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
}

/// RAII guard that owns the [`DashMap`] entry for an in-flight request.
///
/// Calling [`broadcast`](Self::broadcast) sends the result to all waiting
/// receivers and then removes the entry. If the guard is dropped without
/// broadcasting (e.g. the producing future is cancelled), the entry is also
/// removed so subsequent callers retry instead of hanging on a dead sender.
pub struct InflightGuard<Key, Output>
where
    Key: Debug + Eq + Hash,
    Output: Debug,
{
    requests: Arc<DashMap<Key, Sender<Output>>>,
    key: Option<Key>,
    sender: Sender<Output>,
}

impl<Key, Output> InflightGuard<Key, Output>
where
    Key: Debug + Eq + Hash,
    Output: Debug + Clone,
{
    /// Broadcast the result to all waiting receivers and remove the map entry.
    pub fn broadcast(mut self, result: &Output) {
        if let Some(key) = self.key.take() {
            self.requests.remove(&key);
            let _ = self.sender.send(result.clone());
        }
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

#[derive(Debug)]
pub struct InflightOutput<Key, Output>
where
    Key: Debug + Eq + Hash,
    Output: Debug,
{
    requests: Arc<DashMap<Key, Sender<Output>>>,
}

impl<Key, Output> Default for InflightOutput<Key, Output>
where
    Key: Debug + Eq + Hash,
    Output: Debug,
{
    fn default() -> Self {
        Self {
            requests: Default::default(),
        }
    }
}

impl<Key, Output> InflightOutput<Key, Output>
where
    Key: Debug + Eq + Hash + Clone,
    Output: Debug + Clone,
{
    pub fn request(&self, key: Key) -> RequestRole<Key, Output> {
        match self.requests.entry(key) {
            Entry::Occupied(entry) => {
                let receiver = entry.get().subscribe();
                RequestRole::ResultAwaiter(receiver)
            }
            Entry::Vacant(entry) => {
                let (broadcaster, _) = broadcast::channel(1);
                let key = entry.key().clone();
                entry.insert(broadcaster.clone());
                RequestRole::RequestMaker(InflightGuard {
                    requests: self.requests.clone(),
                    key: Some(key),
                    sender: broadcaster,
                })
            }
        }
    }
}
