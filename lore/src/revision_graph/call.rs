// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Dispatch helper for the revision graph API.
//!
//! [`revision_graph_call`] runs a revision graph verb on a storage handle behind the handle's
//! in-flight guard and completes it with one `Complete` event. Its dispatcher sends only the events
//! [`revision_graph_events`] passes, that `Complete` and the `End` after it.

use std::sync::Arc;

use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::relay::EventDispatcher;

use crate::call::setup_execution_with;
use crate::interface::LoreEventCallback;
use crate::revision_graph::RevisionGraphError;
use crate::storage::call::storage_call_with;
use crate::storage::handle::LoreStore;
use crate::storage::store::StoreInternal;

/// Whether a revision graph call sends `event` to its callback: a record or location complete
/// event, or a log line. Every other event is dropped. `Complete` and `End` reach the callback
/// past this filter.
pub fn revision_graph_events(event: &LoreEvent) -> bool {
    matches!(
        event,
        LoreEvent::Log(_)
            | LoreEvent::RevisionGraphBranch(_)
            | LoreEvent::RevisionGraphInstance(_)
            | LoreEvent::RevisionGraphRevision(_)
            | LoreEvent::RevisionGraphLocationComplete(_)
    )
}

/// Run a revision graph verb on a storage handle as [`storage_call_with`] runs an op: behind the
/// handle's in-flight guard, so `lore_storage_close` waits for the verb's `Complete` before it
/// flushes. The call sends only the events [`revision_graph_events`] passes, then `Complete` and
/// `End`. It runs with `globals.dry_run` and `globals.force` cleared, which every verb ignores, so
/// no context a verb builds discards its mutable writes or keeps its immutable writes local, as a
/// client dry run's contexts do.
#[lore_macro::test_pub]
#[inline]
pub(crate) fn revision_graph_call<Arg, T, F, Fut, ResT>(
    mut globals: LoreGlobalArgs,
    callback: LoreEventCallback,
    store_handle: LoreStore,
    args: Arg,
    caller: T,
    command: F,
) -> impl Future<Output = i32>
where
    Arg: std::fmt::Debug,
    F: FnOnce(Arc<StoreInternal>, Arg) -> Fut,
    Fut: Future<Output = Result<ResT, RevisionGraphError>> + 'static,
{
    globals.dry_run = 0;
    globals.force = 0;
    storage_call_with(
        move || {
            setup_execution_with(
                globals,
                EventDispatcher::new(callback).with_event_filter(revision_graph_events),
            )
        },
        store_handle,
        args,
        caller,
        command,
    )
}
