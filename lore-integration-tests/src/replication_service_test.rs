// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(all(test, feature = "grpc_integration_tests"))]
mod replication_service_tests {
    use std::collections::HashSet;
    use std::error::Error;
    use std::sync::Arc;

    use lore_base::lore_spawn;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Partition;
    use lore_proto::PutRequest;
    use lore_proto::ReplicationPutRequest;
    use lore_proto::rpc::replication_service_client::ReplicationServiceClient;
    use lore_revision::fragment::generate_random;
    use lore_revision::util;
    use lore_server::store::grpc_replica::ReplicationClient;
    use lore_server::store::grpc_replica::ReplicationClientError;
    use tokio::sync::mpsc;
    use tokio::task::JoinSet;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::transport::Certificate;
    use tonic::transport::Channel;
    use tonic::transport::ClientTlsConfig;
    use tonic::transport::Identity;

    use crate::setup_execution;

    async fn get_channel(
        suffix: Option<&'static str>,
    ) -> Result<ReplicationServiceClient<Channel>, Box<dyn Error>> {
        let suffix = suffix.unwrap_or("");

        let ca_cert = std::fs::read(format!("../certs/ca{suffix}.crt"))?;
        let ca_cert = Certificate::from_pem(ca_cert);

        let client_cert = std::fs::read(format!("../certs/client{suffix}.crt"))?;
        let client_key = std::fs::read(format!("../certs/client{suffix}.key"))?;
        let client_identity = Identity::from_pem(client_cert, client_key);

        let tls = ClientTlsConfig::new()
            .domain_name("localhost")
            .ca_certificate(ca_cert)
            .identity(client_identity);

        Ok(ReplicationServiceClient::new(
            Channel::from_static("https://127.0.0.1:41340")
                .tls_config(tls)?
                .connect()
                .await?,
        ))
    }

    #[tokio::test]
    async fn test_replication_put() -> Result<(), Box<dyn Error>> {
        let mut channel = get_channel(None /* certs suffix */).await?;

        let (tx, rx) = mpsc::channel::<ReplicationPutRequest>(10);

        let stream = ReceiverStream::new(rx);

        let mut addresses: HashSet<Address> = HashSet::new();
        for _ in 0..10 {
            let request = put_request();
            addresses.insert(
                request
                    .put_request
                    .as_ref()
                    .map(|r| r.address.as_ref().unwrap().into())
                    .unwrap(),
            );
            tx.send(request).await?;
        }

        let mut response = channel.put(stream).await?.into_inner();

        // Drop the sender so the client side of the connection closes
        drop(tx);

        let mut seen = HashSet::new();
        while let Some(message) = response.message().await? {
            if let Some(address) = message.address {
                seen.insert(address.into());
            }
        }

        assert_eq!(addresses, seen);

        Ok(())
    }

    #[tokio::test]
    async fn test_replication_client() -> Result<(), Box<dyn Error>> {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let client = ReplicationClient::new(
                    get_channel(None /* certs suffix */).await.unwrap(),
                    500, /* buffer */
                    util::time::RetryPolicy::builder()
                        .with_initial_backoff_millis(50)
                        .with_max_backoff_millis(1000)
                        .with_limit(3)
                        .build(),
                );

                let client = Arc::new(client);
                let mut join_set = JoinSet::new();
                for _ in 0..100 {
                    let client = client.clone();
                    lore_spawn!(join_set, async move {
                        let repository = rand::random::<Partition>();
                        let (fragment, address, payload) = generate_random();

                        client
                            .put(repository, address, fragment, Some(payload))
                            .await
                    });
                }

                while let Some(result) = join_set.join_next().await {
                    result.expect("task failed").expect("task failed");
                }
            })
            .await;

        Ok(())
    }

    #[tokio::test]
    async fn test_replication_client_stream_full() -> Result<(), Box<dyn Error>> {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let client = ReplicationClient::new(
                    get_channel(None /* certs suffix */).await.unwrap(),
                    1, /* buffer, to ensure that we get a slow-down limit to 1 message at a time */
                    util::time::RetryPolicy::builder()
                        .with_initial_backoff_millis(50)
                        .with_max_backoff_millis(1000)
                        .with_limit(3)
                        .build(),
                );

                let client = Arc::new(client);
                let mut join_set = JoinSet::new();
                for i in 0..2 {
                    let client = client.clone();
                    lore_spawn!(join_set, async move {
                        let repository = rand::random::<Partition>();
                        let (fragment, address, payload) = generate_random();

                        let result = client
                            .put(repository, address, fragment, Some(payload))
                            .await;

                        if i == 0 {
                            result
                        } else {
                            match result {
                                Err(ReplicationClientError::SlowDown) => Ok(()),
                                _ => result,
                            }
                        }
                    });
                }

                while let Some(result) = join_set.join_next().await {
                    result.expect("task failed").expect("task failed");
                }
            })
            .await;

        Ok(())
    }

    #[tokio::test]
    async fn test_invalid_cert_causes_mtls_failure() -> Result<(), Box<dyn Error>> {
        let result = get_channel(Some("-bad") /* certs suffix */).await;
        assert!(result.is_err());

        let transport_error: Box<tonic::transport::Error> = result
            .unwrap_err()
            .downcast::<tonic::transport::Error>()
            .expect("expected tonic transport error");

        // Unfortunately there's no good way to verify the error is a specific type of error short
        // of just formatting it. This is, of course, prone to fail if anything in tonic changes how
        // errors are formatted.
        assert_eq!(
            format!("{transport_error:?}"),
            "tonic::transport::Error(Transport, ConnectError(Custom { kind: InvalidData, error: InvalidCertificate(BadSignature) }))"
        );

        Ok(())
    }

    fn put_request() -> ReplicationPutRequest {
        let repository = rand::random::<Context>();
        let (fragment, address, payload) = generate_random();

        ReplicationPutRequest {
            repository_id: repository.into(),
            put_request: Some(PutRequest {
                address: Some(address.into()),
                fragment: Some(fragment.into()),
                payload: Some(payload),
            }),
        }
    }
}
