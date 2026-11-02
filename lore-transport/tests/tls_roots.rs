// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Tests of the client root store with an empty OS trust store. They point `SSL_CERT_FILE` at a
//! missing file, so they run in a process of their own.

use std::sync::Mutex;

use lore_base::test_util::TempDir;
use lore_transport::tls::client_root_store;
use lore_transport::tls::grpc_client_tls_config;
use tonic::transport::Endpoint;
use webpki_roots::TLS_SERVER_ROOTS;

static TRUST_STORE: Mutex<Option<TempDir>> = Mutex::new(None);

/// A constructor because the environment is process-wide: this runs before the harness starts
/// the threads that would make writing to it a data race.
#[ctor::ctor]
fn point_trust_store_at_missing_file() {
    let dir = TempDir::new("lore-transport-tls-roots-");

    // Safety: constructors run before `main`, so this is the single-threaded
    // window where writing to the environment has no reader to race.
    unsafe {
        std::env::set_var("SSL_CERT_FILE", dir.child("ca.pem"));
        std::env::remove_var("SSL_CERT_DIR");
    }
    if let Ok(mut held) = TRUST_STORE.lock() {
        *held = Some(dir);
    }
}

#[ctor::dtor]
fn remove_trust_store() {
    if let Ok(mut held) = TRUST_STORE.lock() {
        drop(held.take());
    }
}

#[test]
fn an_empty_trust_store_falls_back_to_the_bundled_roots() {
    Endpoint::from_static("https://lore.example.com")
        .tls_config(grpc_client_tls_config())
        .expect("gRPC TLS configures without OS certificates");

    assert_eq!(client_root_store().roots, TLS_SERVER_ROOTS.to_vec());
}
