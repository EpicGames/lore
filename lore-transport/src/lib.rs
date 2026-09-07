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

static USER_AGENT: OnceLock<String> = OnceLock::new();

/// User agent string for all transport connections. Reads from the `LORE_USER_AGENT` env var,
/// falls back to `lore-transport/{version}`.
pub fn user_agent() -> &'static str {
    USER_AGENT
        .get_or_init(|| {
            std::env::var("LORE_USER_AGENT")
                .unwrap_or_else(|_| format!("lore-transport/{}", LORE_LIBRARY_VERSION.as_str()))
        })
        .as_str()
}

pub fn set_user_agent(name: String) -> bool {
    USER_AGENT.set(name).is_ok()
}
