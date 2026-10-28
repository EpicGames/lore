// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A pre-hook that records the branch metadata a handler dispatches it with.

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
pub(crate) fn recording_dispatcher(reject: bool) -> (HookDispatcher, SeenLog) {
    let seen = SeenLog::default();
    let hook = RecordingHook {
        reject,
        seen: seen.clone(),
    };
    let dispatcher = HookDispatcher::new(
        vec![("recording".to_string(), Box::new(hook))],
        Duration::from_secs(60),
        Duration::from_secs(60),
    );
    (dispatcher, seen)
}
