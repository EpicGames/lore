// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Duration;

use lore_base::error::InvalidPath;
use lore_base::runtime::LORE_CONTEXT;
use lore_error_set::ForwardAny;
use lore_error_set::error_set;
use lore_revision::event::LoreErrorDetail;
use lore_revision::fs::mount_manager_state::MountManagerState;
use lore_revision::global::external_dir::recovery::AtStartFound;
use lore_revision::global::external_dir::recovery::examine_at_start;
use lore_revision::global::external_dir::recovery::recover_at_start;
use lore_revision::global::external_dir::recovery::recover_in_background;
use lore_revision::global::get_global_data_dir;
use lore_revision::global::global_data_dir_path;
use lore_revision::lore::execution_context;
use lore_revision::service_state::ServiceStateImpl;

use crate::call::setup_execution;
use crate::interface::LoreEvent;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::remote::service_process::mark_running_as_service;

#[error_set]
pub enum ServiceInitializationError {
    InvalidPath,
}

/// Seconds after a write that the service's immutable disk stores flush their index.
const SERVICE_STORE_FLUSH_SECONDS: NonZeroU64 = NonZeroU64::new(30).unwrap();

/// Builds the execution context callback that buffers service-level logs and
/// errors in `service_state`, so a status report can hand them back later.
fn service_log_callback(service_state: Arc<ServiceStateImpl>) -> LoreEventCallback {
    Some(Box::new(move |event| {
        push_service_log(&service_state, event);
    }))
}

/// Buffers `event` in `service_state` where it is a log or an error, so a status report can hand
/// it back later.
pub(crate) fn push_service_log(service_state: &ServiceStateImpl, event: &LoreEvent) {
    match event {
        LoreEvent::Log(data) => {
            service_state.push_log(data.level, data.message.as_str().to_string());
        }
        LoreEvent::Error(data) => {
            service_state.push_log(
                lore_base::log::LoreLogLevel::Error,
                data.error_inner.as_str().to_string(),
            );
        }
        _ => {}
    }
}

/// Brings up the service, and answers whether it has work for [`background_recovery`]: a move
/// of the external root of its global data directory recorded, which the quick steps of recovery
/// run on before anything mounts, or a setting, with leftovers to remove. A global data directory
/// whose root never moved costs a look for each of two files, which finds nothing.
pub async fn initialize_service(
    globals: LoreGlobalArgs,
    service_state: Arc<ServiceStateImpl>,
) -> Result<bool, ServiceInitializationError> {
    mark_running_as_service();
    lore_revision::store::immutable::set_background_flush_delay(SERVICE_STORE_FLUSH_SECONDS);
    service_state.initialize();

    let execution = setup_execution(globals, service_log_callback(service_state));

    LORE_CONTEXT
        .scope(execution, async move {
            let background = recover_before_mounting().await;
            let detail = LoreErrorDetail::from_result(
                &MountManagerState::initialize()
                    .await
                    .forward_any::<ServiceInitializationError>(
                        "Failed initializing mounted repositories",
                    ),
            );

            execution_context().dispatcher.complete(detail).await;
            Ok(background)
        })
        .await
}

/// Examines the external root of the service's global data directory as [`examine_at_start`]
/// does, runs the quick steps of recovery on a recorded move, and answers whether
/// [`background_recovery`] has work.
async fn recover_before_mounting() -> bool {
    let Some(global) = global_data_dir_path() else {
        return false;
    };
    match examine_at_start(&global) {
        AtStartFound::Recorded => {
            Box::pin(recover_at_start(&global, Duration::ZERO)).await;
            true
        }
        AtStartFound::Configured => true,
        AtStartFound::Unmoved => false,
    }
}

/// The work a service runs in a task of its own once it listens, in an execution that buffers its
/// messages in `service_state`: the recovery of a move of the external root that the start left
/// waiting, the mounting of the recovered root's virtual instances, and the removal of what is
/// left of earlier roots.
pub fn background_recovery(
    service_state: Arc<ServiceStateImpl>,
) -> impl Future<Output = ()> + Send + 'static {
    let execution = setup_execution(
        LoreGlobalArgs::default(),
        service_log_callback(service_state),
    );
    LORE_CONTEXT.scope(execution, async {
        let Ok(global) = get_global_data_dir() else {
            return;
        };
        let mount = async {
            if let Some(manager) = MountManagerState::mount_manager()
                && let Err(err) = Box::pin(manager.mount_registered_instances()).await
            {
                lore_revision::lore_warn!(
                    "Mounting the virtual instances of the recovered external root failed: {err}"
                );
            }
        };
        recover_in_background(global, mount).await;
    })
}

#[cfg(test)]
mod tests {
    use lore_base::log::LoreLogLevel;
    use lore_revision::event::LoreEndEventData;

    use super::*;

    #[test]
    fn the_callback_records_events_in_the_state_it_was_given() {
        let state = Arc::new(ServiceStateImpl::new());
        let callback = service_log_callback(state.clone()).expect("A callback is always built");

        callback(&LoreEvent::Log(crate::interface::LoreLogEventData {
            level: LoreLogLevel::Info,
            category: 0,
            timestamp: 0,
            location: crate::interface::LoreString::default(),
            message: crate::interface::LoreString::from("Test log message"),
        }));
        callback(&LoreEvent::Error(crate::interface::LoreErrorEventData {
            error_type: 0,
            error_inner: crate::interface::LoreString::from("Test error message"),
        }));

        let (messages, dropped) = state.drain_logs();
        assert_eq!(dropped, 0);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].level, LoreLogLevel::Info);
        assert_eq!(messages[0].message, "Test log message");
        assert_eq!(
            messages[1].level,
            LoreLogLevel::Error,
            "An error event is recorded at error level"
        );
        assert_eq!(messages[1].message, "Test error message");
    }

    #[test]
    fn the_callback_ignores_events_that_are_not_logs_or_errors() {
        let state = Arc::new(ServiceStateImpl::new());
        let callback = service_log_callback(state.clone()).expect("A callback is always built");

        callback(&LoreEvent::End(LoreEndEventData::default()));

        let (messages, _dropped) = state.drain_logs();
        assert!(messages.is_empty(), "Only logs and errors are buffered");
    }
}
