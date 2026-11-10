// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Hooks that record what a handler dispatches them with.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lore_server::hooks::Hook;
use lore_server::hooks::HookContext;
use lore_server::hooks::HookDispatcher;
use lore_server::hooks::HookError;
use lore_server::hooks::HookPoint;
use lore_server::hooks::StatusCode;
use lore_server::hooks::context::METADATA_BRANCH_NAME;
use lore_server::hooks::context::METADATA_IS_SERVICE_ACCOUNT;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;

/// The branch metadata one pre-handler call saw.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Seen {
    pub branch_name: Option<String>,
    pub is_service_account: Option<String>,
}

pub(crate) type SeenLog = Arc<parking_lot::Mutex<Vec<Seen>>>;

struct RecordingHook {
    reject: bool,
    seen: SeenLog,
}

#[async_trait]
impl Hook for RecordingHook {
    fn name(&self) -> &'static str {
        "recording"
    }

    fn hook_points(&self) -> &'static [HookPoint] {
        &[
            HookPoint::BranchPush,
            HookPoint::BranchCreate,
            HookPoint::BranchDelete,
        ]
    }

    async fn pre_handler(&self, ctx: &HookContext) -> Result<(), HookError> {
        self.seen.lock().push(Seen {
            branch_name: ctx.get_metadata(METADATA_BRANCH_NAME).map(str::to_string),
            is_service_account: ctx
                .get_metadata(METADATA_IS_SERVICE_ACCOUNT)
                .map(str::to_string),
        });
        if self.reject {
            return Err(HookError::rejected(
                self.name(),
                "rejected by test",
                StatusCode::PermissionDenied,
            ));
        }
        Ok(())
    }
}

/// A dispatcher with one recording pre-hook on the branch hook points, which
/// rejects every operation when `reject` is set, and the log it records to.
pub(crate) fn recording_dispatcher(reject: bool) -> (Arc<HookDispatcher>, SeenLog) {
    let seen = SeenLog::default();
    let hook = RecordingHook {
        reject,
        seen: seen.clone(),
    };
    (dispatcher_with(hook), seen)
}

/// A post-hook on `points` that sends the hook point of each call's context.
struct PostRecordingHook {
    points: &'static [HookPoint],
    sent: UnboundedSender<HookPoint>,
}

#[async_trait]
impl Hook for PostRecordingHook {
    fn name(&self) -> &'static str {
        "post-recording"
    }

    fn hook_points(&self) -> &'static [HookPoint] {
        self.points
    }

    async fn post_handler(&self, ctx: &HookContext) -> Result<(), HookError> {
        self.sent
            .send(ctx.hook_point())
            .expect("the test waits for the post-hook");
        Ok(())
    }
}

/// A dispatcher with one recording post-hook on `points`, and the receiver of the hook point of
/// each call's context.
pub(crate) fn post_recording_dispatcher(
    points: &'static [HookPoint],
) -> (Arc<HookDispatcher>, UnboundedReceiver<HookPoint>) {
    let (sent, received) = tokio::sync::mpsc::unbounded_channel();
    let hook = PostRecordingHook { points, sent };
    (dispatcher_with(hook), received)
}

/// A dispatcher with `hook` alone, registered under its name.
fn dispatcher_with(hook: impl Hook + 'static) -> Arc<HookDispatcher> {
    Arc::new(HookDispatcher::new(
        vec![(hook.name().to_string(), Box::new(hook))],
        Duration::from_secs(60),
        Duration::from_secs(60),
    ))
}
