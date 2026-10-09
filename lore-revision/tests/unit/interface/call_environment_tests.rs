// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::any::Any;
use std::sync::Arc;

use lore_base::env::AUTH_PATH_VAR;
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
        values: [Some("/caller/global".to_string()), None],
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
        values: [None, Some("/caller/auth".to_string())],
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
