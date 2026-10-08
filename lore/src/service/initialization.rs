// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use std::sync::Arc;

use lore_base::error::InvalidPath;
use lore_base::runtime::LORE_CONTEXT;
use lore_error_set::ForwardStrict;
use lore_error_set::error_set;
use lore_revision::event::LoreErrorDetail;
use lore_revision::fs::swfs::mount_manager_state::MountManagerState;
use lore_revision::lore::execution_context;
use lore_revision::service_state::ServiceStateImpl;

use crate::call::setup_execution;
use crate::interface::LoreEvent;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;

#[error_set]
pub enum ServiceInitializationError {
    InvalidPath,
}

/// Builds the execution context callback that buffers service-level logs and
/// errors in `service_state`, so a status report can hand them back later.
fn service_log_callback(service_state: Arc<ServiceStateImpl>) -> LoreEventCallback {
    Some(Box::new(move |event| match event {
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
    }))
}

pub async fn initialize_service(
    globals: LoreGlobalArgs,
    service_state: Arc<ServiceStateImpl>,
) -> Result<(), ServiceInitializationError> {
    service_state.initialize();

    let execution = setup_execution(globals, service_log_callback(service_state));

    LORE_CONTEXT
        .scope(execution, async move {
            let result = MountManagerState::initialize()
                .await
                .forward::<ServiceInitializationError>("Failed initializing SWFS repositories");
            let detail = LoreErrorDetail::from_result(result);

            execution_context().dispatcher.complete(detail).await
        })
        .await;

    Ok(())
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
