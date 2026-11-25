// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use bytes::Bytes;
use lore_transport::quic::QuicClientError;
use lore_transport::quic::QuicServiceError;
use lore_transport::quic::response_reader::*;

#[test]
fn maps_known_service_errors_to_typed_variants() {
    assert!(matches!(
        handle_error(QuicServiceError::NotAuthorized as u32),
        QuicClientError::NotAuthorized
    ));
    assert!(matches!(
        handle_error(QuicServiceError::SlowDown as u32),
        QuicClientError::SlowDown
    ));
    assert!(matches!(
        handle_error(QuicServiceError::NotFound as u32),
        QuicClientError::NotFound
    ));
    assert!(matches!(
        handle_error(QuicServiceError::Oversized as u32),
        QuicClientError::Oversized
    ));
}

#[test]
fn unknown_status_falls_back_to_server_error() {
    assert!(matches!(handle_error(42), QuicClientError::ServerError(42)));
}

#[test]
fn every_answer_of_the_server_shows_its_frame_written() {
    assert!(shows_frame_written(&Ok(Bytes::new())));
    for status in [
        QuicServiceError::InvalidCommand as u32,
        QuicServiceError::NotAuthorized as u32,
        QuicServiceError::Failed as u32,
        QuicServiceError::NotFound as u32,
        QuicServiceError::Oversized as u32,
        QuicServiceError::SlowDown as u32,
        QuicServiceError::ImplementationReserved as u32,
    ] {
        assert!(
            shows_frame_written(&Err(handle_error(status))),
            "status {status}"
        );
    }
}

#[test]
fn a_failure_on_the_client_side_does_not_show_its_frame_written() {
    for error in [
        QuicClientError::Terminated,
        QuicClientError::CrytpoError,
        QuicClientError::Read,
        QuicClientError::WriteChunks,
        QuicClientError::StreamOpen,
    ] {
        assert!(!shows_frame_written(&Err(error.clone())), "{error:?}");
    }
}

mod read_response {
    use std::future::Future;
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::task::Poll;
    use std::time::Duration;
    use std::time::Instant;

    use lore_transport::quic::command_header::CommandHeader;
    use quinn::crypto::rustls::QuicClientConfig;
    use quinn::crypto::rustls::QuicServerConfig;
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::PrivateKeyDer;
    use rustls::pki_types::pem::PemObject;

    use super::*;

    /// More answers than a task's budget allows it to take in one poll.
    const ANSWERS: u32 = 1000;

    /// A loopback client connection and a server endpoint, the two sides of one QUIC connection.
    async fn loopback() -> (quinn::Connection, quinn::Connection, quinn::Endpoint) {
        let certificate = lore_transport::tls::generate_self_signed(vec!["127.0.0.1".to_string()])
            .expect("self-signed certificate");
        let der =
            CertificateDer::from_pem_slice(certificate.cert_pem.as_bytes()).expect("certificate");
        let key = PrivateKeyDer::from_pem_slice(certificate.key_pem.as_bytes()).expect("key");
        let provider = Arc::new(rustls::crypto::ring::default_provider());

        let server_tls = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("TLS 1.3")
            .with_no_client_auth()
            .with_single_cert(vec![der.clone()], key)
            .expect("server certificate");
        let server_config = quinn::ServerConfig::with_crypto(Arc::new(
            QuicServerConfig::try_from(server_tls).expect("QUIC server crypto"),
        ));
        let loopback: SocketAddr = "127.0.0.1:0".parse().expect("loopback address");
        let server = quinn::Endpoint::server(server_config, loopback).expect("server endpoint");

        let mut roots = rustls::RootCertStore::empty();
        roots.add(der).expect("trusted certificate");
        let client_tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("TLS 1.3")
            .with_root_certificates(roots)
            .with_no_client_auth();
        let mut client = quinn::Endpoint::client(loopback).expect("client endpoint");
        client.set_default_client_config(quinn::ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(client_tls).expect("QUIC client crypto"),
        )));

        let connecting = client
            .connect(server.local_addr().expect("server address"), "127.0.0.1")
            .expect("connect");
        let incoming = server.accept().await.expect("incoming connection");
        let (client_connection, server_connection) =
            tokio::join!(async { connecting.await.expect("client side") }, async {
                incoming.await.expect("server side")
            });
        (client_connection, server_connection, server)
    }

    /// A client stream, the server's side of it, and the connection's sides kept alive with them.
    async fn open() -> (
        quinn::RecvStream,
        quinn::SendStream,
        quinn::Connection,
        (quinn::Connection, quinn::Endpoint),
    ) {
        let (client, server, endpoint) = loopback().await;
        let (mut request, response) = client.open_bi().await.expect("open a stream");
        request.write_all(&[0]).await.expect("announce the stream");
        let (sending, mut announced) = server.accept_bi().await.expect("accept the stream");
        announced
            .read_exact(&mut [0])
            .await
            .expect("read the announcement");
        (response, sending, client, (server, endpoint))
    }

    /// Writes `answers` on `sending` and finishes it, once the client has acknowledged all of
    /// them, so they are buffered on the client's side.
    async fn buffer(mut sending: quinn::SendStream, answers: &[Bytes]) {
        for answer in answers {
            sending.write_all(answer).await.expect("write an answer");
        }
        sending.finish().expect("finish the answers");
        let stopped = sending.stopped().await.expect("the answers acknowledged");
        assert_eq!(stopped, None);
    }

    /// The commands `1..=count`, waiting on a fresh set, and their answers' receivers.
    fn waiting(
        count: u32,
    ) -> (
        Arc<PendingCommands>,
        Vec<tokio::sync::oneshot::Receiver<Result<Bytes, QuicClientError>>>,
    ) {
        let pending = Arc::new(PendingCommands::new(Arc::default()));
        let receivers = (1..=count)
            .map(|command_id| {
                let (sender, receiver) = tokio::sync::oneshot::channel();
                assert!(pending.wait_for(command_id, sender, None));
                receiver
            })
            .collect();
        (pending, receivers)
    }

    /// Polls `reading` once, in a poll of the test that starts with a full budget, and answers
    /// whether it gave way.
    async fn gives_way(reading: Pin<&mut impl Future>) -> bool {
        tokio::task::yield_now().await;
        let mut reading = reading;
        std::future::poll_fn(|cx| Poll::Ready(reading.as_mut().poll(cx).is_pending())).await
    }

    /// Reads `stream` as a client stream's reader, with chunks of at most `max_chunk_size`.
    fn read(
        stream: quinn::RecvStream,
        connection: quinn::Connection,
        pending: Arc<PendingCommands>,
        max_chunk_size: usize,
    ) -> impl Future<Output = Result<(), QuicClientError>> {
        read_response(
            stream,
            max_chunk_size,
            pending,
            Arc::new(AtomicU64::new(0)),
            Instant::now(),
            true,
            StreamIdentity {
                connection,
                index: 0,
            },
        )
    }

    /// A reader whose stream already holds the answers to many commands gives way to other tasks
    /// before it has taken them all, so it never holds its worker for as long as data keeps
    /// arriving.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn a_reader_gives_way_while_its_stream_holds_many_answers() {
        let answers: Vec<Bytes> = (1..=ANSWERS)
            .map(|command_id| {
                Bytes::copy_from_slice(
                    &CommandHeader::new_with_session(1, command_id, 0, 1).to_bytes_v4(),
                )
            })
            .collect();
        let (stream, sending, connection, _server) = open().await;
        let (pending, mut answered) = waiting(ANSWERS);
        let mut reading = std::pin::pin!(read(stream, connection, pending, 64 * 1024));
        // The reader's first read finds the stream empty, so quinn has nothing buffered to merge
        // into fewer chunks.
        assert!(gives_way(reading.as_mut()).await);
        buffer(sending, &answers).await;

        let gave_way = gives_way(reading.as_mut()).await;
        let mut answers_taken = vec![false; answered.len()];
        for (receiver, taken) in answered.iter_mut().zip(&mut answers_taken) {
            if let Ok(answer) = receiver.try_recv() {
                assert_eq!(answer.expect("a success"), Bytes::new());
                *taken = true;
            }
        }
        let taken_in_one_poll = answers_taken.iter().filter(|&&taken| taken).count();
        assert!(
            gave_way && taken_in_one_poll < answered.len(),
            "the reader took {taken_in_one_poll} of {ANSWERS} answers in one poll"
        );

        tokio::time::timeout(Duration::from_secs(10), reading)
            .await
            .expect("the reader finished")
            .expect("the answers were read");
        for (receiver, taken) in answered.iter_mut().zip(answers_taken) {
            if !taken {
                let answer = receiver.try_recv().expect("an answer");
                assert_eq!(answer.expect("a success"), Bytes::new());
            }
        }
    }

    /// A reader whose stream already holds one answer, in more chunks than a task's budget allows
    /// in one poll, gives way to other tasks before it has read the whole answer. quinn gives the
    /// reader what it has buffered as it arrived, about a packet a chunk.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn a_reader_gives_way_while_its_stream_holds_one_large_answer() {
        let payload = Bytes::from(vec![0x5au8; 512 * 1024]);
        let header = CommandHeader::new_with_session(1, 1, payload.len(), 1).to_bytes_v4();
        let (stream, sending, connection, _server) = open().await;
        let (pending, mut answered) = waiting(1);
        let mut reading = std::pin::pin!(read(stream, connection, pending, 1024 * 1024));
        // The reader's first read finds the stream empty, so quinn has nothing buffered to merge
        // into fewer chunks.
        assert!(gives_way(reading.as_mut()).await);
        buffer(sending, &[Bytes::copy_from_slice(&header), payload.clone()]).await;

        assert!(
            gives_way(reading.as_mut()).await,
            "the reader read the whole answer in one poll"
        );
        let early = answered[0].try_recv();
        assert!(
            matches!(early, Err(tokio::sync::oneshot::error::TryRecvError::Empty)),
            "the answer was delivered in one poll: {early:?}"
        );

        tokio::time::timeout(Duration::from_secs(10), reading)
            .await
            .expect("the reader finished")
            .expect("the answer was read");
        let answer = answered[0].try_recv().expect("an answer");
        assert_eq!(answer.expect("a success"), payload);
    }
}
