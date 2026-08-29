// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(all(test, feature = "integration_tests"))]
mod replicated_store_tests {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::util::time::RetryPolicy;
    use lore_server::quic::quinn::QuinnConfigBuilder;
    use lore_server::quic::quinn::QuinnServer;
    use lore_server::quic::replication_store_service::client::ReplicationStoreClient;
    use lore_server::quic::replication_store_service::client_container::ClientContainerConfig;
    use lore_server::quic::replication_store_service::client_container::QuicClientFactory;
    use lore_server::quic::tests::TestHandlerFactory;
    use lore_server::store::replicated_store::ReplicatedStore;
    use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
    use lore_storage::local::immutable_store::ImmutableStoreSettings;
    use lore_transport::quic::client::CertificateSettings;

    use crate::setup_execution;

    /// Starts a QUIC replication server backed by a local immutable store and returns
    /// `(replicated_store, _server)` where `replicated_store` is the [`ReplicatedStore`]
    /// client connected to it. The server is kept alive for as long as `_server` is held.
    async fn start_replication_server()
    -> (Arc<ReplicatedStore<ReplicationStoreClient>>, QuinnServer) {
        let backend_immutable = lore_storage::local::immutable_store::create(
            None::<&str>,
            ImmutableStoreCreateOptions::none(),
            false,
            ImmutableStoreSettings {
                // The ReplicatedStore declares isolates_partitions() → true, so the server
                // it talks to must also isolate — otherwise partition-matched results would
                // pass through where the battery expects none.
                isolate_partitions: true,
                ..Default::default()
            },
        )
        .await
        .expect("backend immutable store");

        let backend_mutable = lore_storage::local::mutable_store::create(
            None::<&str>,
            lore_storage::MutableStoreSettings::default(),
            backend_immutable.clone(),
        )
        .await
        .expect("backend mutable store");

        let udp = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind udp");
        let addr: SocketAddr = udp.local_addr().expect("udp local addr");

        let (cert_file, pkey_file, _ca) =
            lore_server::quic::tests::server_certs().expect("test certificate paths");

        let quic = QuinnServer::start(
            QuinnConfigBuilder::new()
                .socket(udp)
                .cert_file(cert_file)
                .pkey_file(pkey_file)
                .stream_handler_factory(Box::new(TestHandlerFactory::new(
                    backend_immutable,
                    backend_mutable,
                )))
                .build()
                .expect("quinn config"),
        )
        .expect("quinn server start");

        // Plain `quic://` rather than `quics://` so the client skips verification of the
        // self-signed test certificate.
        let remote_url = format!("quic://127.0.0.1:{}", addr.port());

        let factory = QuicClientFactory::new(
            remote_url,
            CertificateSettings {
                custom_ca: None,
                client: None,
            },
        );

        let container_config = ClientContainerConfig {
            regenerate_retry_policy: RetryPolicy::builder()
                .with_initial_backoff_millis(50)
                .with_max_backoff_millis(1_000)
                .with_limit(10)
                .build(),
            connection_lost_sleep: Duration::from_millis(100),
        };

        let store = ReplicatedStore::new(
            Arc::new(factory),
            container_config,
            Duration::from_secs(60),
            Duration::from_secs(60),
        )
        .await
        .expect("ReplicatedStore creation should succeed");

        (store, quic)
    }

    /// The contract, against the replicated store backed by a live QUIC replication service.
    ///
    /// Unlike the unit test that exercises the replicated store against a mock client, this test
    /// wires a real connection so that the full path — protocol encoding, network dispatch,
    /// server-side handler, and response decoding — is exercised against the battery.
    ///
    /// The server is configured the way a real one is, isolating partitions, because the pair
    /// satisfies the client store's declared read scope only if the server it talks to answers
    /// exact associations.
    #[tokio::test]
    async fn satisfies_the_immutable_store_contract() {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let (replicated_store, _server) = start_replication_server().await;

                lore_storage::conformance::verify_immutable_store(
                    replicated_store,
                    lore_storage::conformance::Capabilities::new("ReplicatedStore/quic")
                        .over_wire(),
                )
                .await;
            })
            .await;
    }
}
