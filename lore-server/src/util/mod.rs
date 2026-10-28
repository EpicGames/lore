// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod cert_metrics;
pub mod core_hop;
pub mod local_store_monitor;

use lore_revision::interface::LoreGlobalArgs;

use crate::auth::jwt::AuthorizationToken;

pub const REPLICATION_USER_ID: &str = "<replication-user>";

pub fn setup_execution(
    context_label: &'static str,
    correlation_id: String,
    user_id: String,
) -> std::sync::Arc<lore_revision::interface::ExecutionContext> {
    let mut ctx = lore_revision::interface::ExecutionContext::new_server(
        LoreGlobalArgs {
            correlation_id: correlation_id.into(),
            ..Default::default()
        },
        lore_revision::relay::EventDispatcher::no_dispatch(),
        user_id,
    );
    ctx.set_caller_state(std::sync::Arc::new(
        crate::execution_state::ServerExecutionState {
            span: tracing::Span::current(),
            context_label,
        },
    ));
    std::sync::Arc::new(ctx)
}

pub fn get_user_id_from_token_ref(maybe_token: Option<&AuthorizationToken>) -> String {
    if let Some(token) = maybe_token {
        token.identity().to_string()
    } else {
        "<unknown>".to_string()
    }
}

pub fn get_user_id_from_token(token: Option<AuthorizationToken>) -> String {
    get_user_id_from_token_ref(token.as_ref())
}
