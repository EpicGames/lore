// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! TLS certificate helpers shared across the transport layer.
//!
//! This module owns the PEM loading primitives used by both the QUIC client
//! and the server endpoints, the roots clients verify servers against
//! ([`client_root_store`]), plus [`generate_self_signed`] for producing
//! ephemeral certificates when none have been supplied via configuration
//! (zero-config local development).

use std::path::Path;
use std::sync::Arc;
use std::sync::LazyLock;

use lore_base::lore_warn;
use rcgen::CertifiedKey;
use rcgen::generate_simple_self_signed;
use rustls::RootCertStore;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::PrivateKeyDer;
use rustls::pki_types::pem::PemObject;
use rustls_native_certs::load_native_certs;
use tonic::transport::ClientTlsConfig;
use webpki_roots::TLS_SERVER_ROOTS;

use crate::error::ProtocolError;

/// Load one or more certificates from a PEM file.
pub fn load_certs(path: impl AsRef<Path>) -> Result<Vec<CertificateDer<'static>>, ProtocolError> {
    CertificateDer::pem_file_iter(path)
        .map_err(|e| ProtocolError::internal(format!("failed to read certificate file: {e}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ProtocolError::internal(format!("invalid certificate: {e}")))
}

/// Load a private key from a PEM file.
pub fn load_private_key(path: impl AsRef<Path>) -> Result<PrivateKeyDer<'static>, ProtocolError> {
    PrivateKeyDer::from_pem_file(path).map_err(|e| match e {
        rustls::pki_types::pem::Error::NoItemsFound => {
            ProtocolError::internal("no private keys found")
        }
        other => ProtocolError::internal(format!("malformed private key: {other}")),
    })
}

/// A freshly generated self-signed certificate and its private key, both
/// PEM-encoded.
///
/// Intended for ephemeral, zero-config use where no certificate has been
/// supplied via configuration. These certificates are not trusted by any CA and
/// must never be relied on for production traffic.
pub struct SelfSignedCert {
    /// PEM-encoded certificate.
    pub cert_pem: String,
    /// PEM-encoded private key (PKCS#8).
    pub key_pem: String,
}

/// Generate an ephemeral self-signed certificate covering the given subject
/// alternative names.
///
/// Each entry that parses as an IP address is added as an IP SAN; everything
/// else is treated as a DNS name. The returned key/cert pair lives only as long
/// as the caller keeps it — nothing is persisted by this function.
pub fn generate_self_signed(
    subject_alt_names: Vec<String>,
) -> Result<SelfSignedCert, ProtocolError> {
    let CertifiedKey { cert, signing_key } = generate_simple_self_signed(subject_alt_names)
        .map_err(|e| {
            ProtocolError::internal(format!("failed to generate self-signed certificate: {e}"))
        })?;

    Ok(SelfSignedCert {
        cert_pem: cert.pem(),
        key_pem: signing_key.serialize_pem(),
    })
}

static CLIENT_ROOT_STORE: LazyLock<Arc<RootCertStore>> = LazyLock::new(|| {
    let mut store = RootCertStore {
        roots: TLS_SERVER_ROOTS.to_vec(),
    };
    let native_certs = load_native_certs();
    if native_certs.certs.is_empty() {
        lore_warn!(
            "no certificates loaded from the OS trust store, continuing with the built-in webpki roots: {:?}",
            native_certs.errors
        );
    }
    store.add_parsable_certificates(native_certs.certs);
    Arc::new(store)
});

/// Roots for verifying servers: the bundled webpki roots and the certificates in the OS trust
/// store, loaded once per process.
pub fn client_root_store() -> Arc<RootCertStore> {
    CLIENT_ROOT_STORE.clone()
}

/// TLS configuration for gRPC clients, trusting [`client_root_store`].
pub fn grpc_client_tls_config() -> ClientTlsConfig {
    ClientTlsConfig::new()
        .assume_http2(true)
        .trust_anchors(client_root_store().roots.iter().cloned())
}
