// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod auth;
pub mod connection;
pub mod error;
pub mod grpc;
pub mod local_socket;
pub mod quic;
pub mod session;
#[cfg(feature = "test-util")]
pub mod test_util;
pub mod tls;
pub mod traits;
pub mod types;
pub mod uds;
pub mod util;

use std::sync::OnceLock;

pub use connection::*;
pub use error::*;
use lore_base::version::LORE_LIBRARY_VERSION;
pub use session::*;
pub use traits::*;
pub use types::*;

/// Names the application embedding Lore, ahead of everything else in the user agent.
const USER_AGENT_ENV_VAR: &str = "LORE_USER_AGENT";

/// Names the Lore binary, overriding [`set_fallback_user_agent_product`].
const USER_AGENT_PRODUCT_ENV_VAR: &str = "LORE_USER_AGENT_PRODUCT";

static USER_AGENT: OnceLock<String> = OnceLock::new();
static USER_AGENT_PRODUCT: OnceLock<Option<String>> = OnceLock::new();

/// The product name identifying this binary in the user agent, if one is set.
///
/// Resolves on first call and is fixed from then on: `LORE_USER_AGENT_PRODUCT` when set, else
/// whatever [`set_fallback_user_agent_product`] supplied, else none.
pub fn user_agent_product() -> Option<&'static str> {
    USER_AGENT_PRODUCT
        .get_or_init(|| non_empty_env(USER_AGENT_PRODUCT_ENV_VAR))
        .as_deref()
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
    if non_empty_env(USER_AGENT_PRODUCT_ENV_VAR).is_some() {
        return false;
    }
    USER_AGENT_PRODUCT.set(Some(name)).is_ok()
}

/// User agent string for all transport connections:
/// `[application] [product] lore/<library version>`.
///
/// `LORE_USER_AGENT` names the application, and [`user_agent_product`] names the product. Each is
/// omitted when unset. The `lore/<version>` token is always present, regardless of the application
/// or product name. The string is resolved on the first call and remains fixed afterward.
pub fn user_agent() -> &'static str {
    USER_AGENT
        .get_or_init(|| {
            compose_user_agent(
                non_empty_env(USER_AGENT_ENV_VAR).as_deref(),
                user_agent_product(),
            )
        })
        .as_str()
}

/// Identifies a connection opened by one subsystem of a binary: [`user_agent`] with `component`
/// appended as an RFC 9110 comment. `component` must not contain parentheses.
pub fn user_agent_with_component(component: &str) -> String {
    format!("{} ({component})", user_agent())
}

/// Joins whichever of `application` and `product` are present, in that order, ahead of the
/// `lore/<library version>` token.
#[lore_macro::test_pub]
fn compose_user_agent(application: Option<&str>, product: Option<&str>) -> String {
    let lib_version = LORE_LIBRARY_VERSION.as_str();
    match (application, product) {
        (Some(application), Some(product)) => format!("{application} {product} lore/{lib_version}"),
        (Some(prefix), None) | (None, Some(prefix)) => format!("{prefix} lore/{lib_version}"),
        (None, None) => format!("lore/{lib_version}"),
    }
}

/// Reads `name`, trimmed, treating an unset or blank variable as absent.
fn non_empty_env(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}
