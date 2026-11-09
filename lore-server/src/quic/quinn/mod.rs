// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod config;
mod metrics;
mod quinn_server;
pub mod service_store;

use std::path::Path;
use std::sync::Arc;

pub(crate) use config::DEFAULT_IDLE_TIMEOUT_MILLIS;
pub use config::QuinnConfigBuilder;
use lore_transport::tls;
pub use quinn_server::QuinnServer;
#[cfg(feature = "test-util")]
pub use quinn_server::SocketConnectionIdGenerator;
use rustls::RootCertStore;
use rustls::server::WebPkiClientVerifier;
use rustls::server::danger::ClientCertVerifier;

pub fn build_cert_verifier(
    ca_path: impl AsRef<Path>,
) -> anyhow::Result<Arc<dyn ClientCertVerifier>> {
    let mut root_store = RootCertStore::empty();
    let ca_certs = tls::load_certs(ca_path)?;
    for cert in ca_certs {
        root_store.add(cert)?;
    }
    let verifier = WebPkiClientVerifier::builder(Arc::new(root_store)).build()?;
    Ok(verifier)
}
