// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod auth;
pub mod connection;
pub mod error;
pub mod grpc;
pub mod quic;
pub mod session;
pub mod tls;
pub mod traits;
pub mod types;
pub mod util;

use std::sync::OnceLock;

pub use connection::*;
pub use error::*;
use lore_base::version::LORE_LIBRARY_VERSION;
pub use session::*;
pub use traits::*;
pub use types::*;

/// The env var to read to retrieve a user agent `product` override
const USER_AGENT_PRODUCT_ENV_VAR: &str = "LORE_USER_AGENT_PRODUCT";

static USER_AGENT: OnceLock<String> = OnceLock::new();
static USER_AGENT_PRODUCT: OnceLock<String> = OnceLock::new();

/// The product name identifying this binary in the user agent.
///
/// Resolves on first call and is fixed from then on: `LORE_USER_AGENT_PRODUCT` when set, else
/// whatever [`set_fallback_user_agent_product`] supplied, else `lore-transport`.
pub fn user_agent_product() -> &'static str {
    USER_AGENT_PRODUCT
        .get_or_init(|| {
            std::env::var(USER_AGENT_PRODUCT_ENV_VAR).unwrap_or("lore-transport".to_string())
        })
        .as_str()
}

/// Supplies the product for [`user_agent_product`] to report, unless
/// `LORE_USER_AGENT_PRODUCT` is set, which takes precedence.
///
/// Call before anything opens a transport connection — the product resolves on first read, and a
/// later call cannot change it.
///
/// Returns whether `name` is the product now in effect. `false` covers both the environment
/// overriding it and the product having already resolved, so a caller needing to tell those apart
/// must read the variable itself.
pub fn set_fallback_user_agent_product(name: String) -> bool {
    if std::env::var(USER_AGENT_PRODUCT_ENV_VAR).is_ok() {
        return false;
    }
    USER_AGENT_PRODUCT.set(name).is_ok()
}

/// User agent string for all transport connections.
///
/// Resolves on first call and is fixed from then on: `LORE_USER_AGENT` when set, else
/// [`user_agent_product`] and the library version.
pub fn user_agent() -> &'static str {
    USER_AGENT
        .get_or_init(|| {
            std::env::var("LORE_USER_AGENT")
                .unwrap_or_else(|_| make_user_agent(user_agent_product()))
        })
        .as_str()
}

pub fn make_user_agent(product: &str) -> String {
    let lib_version = LORE_LIBRARY_VERSION.as_str();
    format!("{product}/{lib_version}")
}

/// Identifies a connection opened by one subsystem of a binary. `component` is
/// emitted as an RFC 9110 comment and must not contain parentheses.
pub fn make_user_agent_with_component(product: &str, component: &str) -> String {
    let product = make_user_agent(product);
    format!("{product} ({component})")
}
