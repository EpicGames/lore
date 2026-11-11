// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::any::Any;
use std::sync::Arc;

use lore_base::env::AUTH_PATH_VAR;
use lore_base::env::AUTH_STORE_VAR;
use lore_base::env::CallEnvironment;
use lore_base::env::GLOBAL_PATH_VAR;
use lore_base::runtime::LORE_CONTEXT;
use lore_revision::interface::ExecutionContext;
use lore_revision::relay::EventDispatcher;

fn context(environment: Option<CallEnvironment>) -> Arc<dyn Any + Send + Sync> {
    let context = ExecutionContext::new_client(Default::default(), EventDispatcher::new(None));
    Arc::new(match environment {
        Some(environment) => context.with_environment(environment),
        None => context,
    })
}

/// A relayed call reads its caller's values, an unset one included, rather than this process's.
#[tokio::test]
async fn a_relayed_call_reads_its_callers_environment() {
    let environment = CallEnvironment {
        values: [Some("/caller/global".to_string()), None, None],
    };

    let (global, auth) = LORE_CONTEXT
        .scope(context(Some(environment)), async {
            (
                lore_base::env::var(GLOBAL_PATH_VAR),
                lore_base::env::var(AUTH_PATH_VAR),
            )
        })
        .await;

    assert_eq!(global.as_deref(), Some("/caller/global"));
    assert_eq!(
        auth, None,
        "a variable the caller left unset is unset for the call"
    );
}

/// A call made in this process, and code outside any call, read this process's environment.
#[tokio::test]
async fn a_local_call_reads_this_processes_environment() {
    let in_call = LORE_CONTEXT
        .scope(context(None), async {
            lore_base::env::var(GLOBAL_PATH_VAR)
        })
        .await;

    assert_eq!(in_call, std::env::var(GLOBAL_PATH_VAR).ok());
    assert_eq!(
        lore_base::env::var(GLOBAL_PATH_VAR),
        std::env::var(GLOBAL_PATH_VAR).ok()
    );
}

/// A task a relayed call spawns runs under the call's context, so it reads the call's environment.
#[tokio::test]
async fn a_task_a_relayed_call_spawns_reads_its_environment() {
    let environment = CallEnvironment {
        values: [None, Some("/caller/auth".to_string()), None],
    };

    let auth = LORE_CONTEXT
        .scope(context(Some(environment)), async {
            lore_base::lore_spawn!(async { lore_base::env::var(AUTH_PATH_VAR) })
                .await
                .expect("the task must finish")
        })
        .await;

    assert_eq!(auth.as_deref(), Some("/caller/auth"));
}

/// A relayed call carries its caller's choice of where the token store keeps its key, so a
/// service started under another configuration follows the caller's.
#[tokio::test]
async fn a_relayed_call_reads_its_callers_token_store_choice() {
    let environment = CallEnvironment {
        values: [None, None, Some("fallback".to_string())],
    };

    let store = LORE_CONTEXT
        .scope(context(Some(environment)), async {
            lore_base::env::var(AUTH_STORE_VAR)
        })
        .await;

    assert_eq!(store.as_deref(), Some("fallback"));
}

/// The context a mount is built under, the service's for a relayed caller, reads the caller's
/// environment, so the mount finds the credentials its caller finds, under the service's globals.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_serving_context_reads_its_callers_environment_under_the_services_globals() {
    let globals = lore_revision::interface::LoreGlobalArgs {
        correlation_id: "service".to_string().into(),
        ..Default::default()
    };
    let service = ExecutionContext::new_client(globals, EventDispatcher::new(None));
    let serving: Arc<dyn Any + Send + Sync> = Arc::new(service.serving(CallEnvironment {
        values: [None, Some("/caller/auth".to_string()), None],
    }));

    let (global, auth, correlation_id) = LORE_CONTEXT
        .scope(serving, async {
            lore_base::lore_spawn!(async {
                (
                    lore_base::env::var(GLOBAL_PATH_VAR),
                    lore_base::env::var(AUTH_PATH_VAR),
                    lore_revision::runtime::execution_context()
                        .globals()
                        .correlation_id
                        .to_string(),
                )
            })
            .await
            .expect("the task must finish")
        })
        .await;

    assert_eq!(auth.as_deref(), Some("/caller/auth"));
    assert_eq!(global, None, "the caller left the global path unset");
    assert_eq!(correlation_id, "service");
}
