// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Runs library calls off the UI thread.
//!
//! `lore::call_delegation::run_command` blocks on the library's runtime, so each call runs on a
//! thread of its own. Its events arrive on the library's worker threads and are passed on, as
//! messages, to a sink the caller supplies.

use std::thread::JoinHandle;

use lore::call_delegation::run_command;
use lore::interface::LoreArray;
use lore::interface::LoreEvent;
use lore::interface::LoreGlobalArgs;
use lore::interface::LoreRepositoryStatusArgs;

use crate::status::StatusMessage;

/// A status call on one repository instance.
#[derive(Debug, Clone)]
pub struct StatusRequest {
    /// The path of the instance's working tree.
    pub instance_path: String,
    /// Walk the working tree and reconcile it against the current revision, as
    /// `lore status --scan` does, rather than report the tracked state.
    pub scan: bool,
}

/// Starts a status call on a thread of its own and passes each message it produces to `sink`,
/// ending with one [`StatusMessage::Complete`].
pub fn spawn_status(
    request: StatusRequest,
    sink: impl Fn(StatusMessage) + Send + Sync + 'static,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("lore-desktop-status".into())
        .spawn(move || run_status(request, sink))
}

fn run_status(request: StatusRequest, sink: impl Fn(StatusMessage) + Send + Sync + 'static) {
    let mut globals = LoreGlobalArgs {
        repository_path: request.instance_path.as_str().into(),
        ..Default::default()
    };
    if let Err(err) = globals.validate() {
        sink(StatusMessage::Complete(Err(err.to_string())));
        return;
    }
    let args = LoreRepositoryStatusArgs {
        staged: 1,
        scan: request.scan.into(),
        check_dirty: 0,
        reset: 0,
        sync_point: 0,
        revision_only: 0,
        count: 0,
        paths: LoreArray::default(),
    };
    let callback: Box<dyn Fn(&LoreEvent) + Send + Sync> = Box::new(move |event| {
        if let Some(message) = StatusMessage::from_event(event) {
            sink(message);
        }
    });
    // The call reports its outcome, success or failure, through its `Complete` event.
    run_command(globals, args.into(), Some(callback));
}
