// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod cert_metrics;
pub mod core_hop;

use lore_revision::interface::LoreGlobalArgs;

use crate::auth::jwt::AuthorizationToken;
use crate::auth::jwt::ResourcePermission;

#[cfg(test)]
pub fn address_with_random_context(address: lore_storage::Address) -> lore_storage::Address {
    lore_storage::Address {
        context: rand::random::<lore_storage::Context>(),
        hash: address.hash,
    }
}

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

#[cfg(test)]
pub fn setup_test_execution() -> std::sync::Arc<lore_revision::interface::ExecutionContext> {
    std::sync::Arc::new(lore_revision::interface::ExecutionContext::new_client(
        LoreGlobalArgs::default(),
        lore_revision::relay::EventDispatcher::no_dispatch(),
    ))
}

pub fn get_user_id_from_token_ref(maybe_token: Option<&AuthorizationToken>) -> String {
    if let Some(token) = maybe_token {
        token.user_id.clone()
    } else {
        "<unknown>".to_string()
    }
}

pub fn get_user_id_from_token(token: Option<AuthorizationToken>) -> String {
    get_user_id_from_token_ref(token.as_ref())
}

pub fn resources_from_token(token: Option<AuthorizationToken>) -> Vec<ResourcePermission> {
    if let Some(token) = token
        && let Some(resources) = token.resources
    {
        return resources;
    }

    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_execution_stores_server_execution_state() {
        let span = tracing::info_span!("test_request");
        let _guard = span.enter();

        let ctx = setup_execution("test", "test-corr".to_string(), "test-user".to_string());

        let state = ctx
            .caller_state()
            .expect("caller_state should be set")
            .clone();
        let downcasted =
            std::sync::Arc::downcast::<crate::execution_state::ServerExecutionState>(state);
        assert!(downcasted.is_ok());
    }
}
