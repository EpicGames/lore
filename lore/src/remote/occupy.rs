// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The service socket held through a move of the external root: no service can start on it, and
//! every request on it is answered with the move reply.
use std::pin::Pin;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use lore_revision::event::LoreEvent;
use lore_revision::lore_warn;
use lore_transport::local_socket::AsyncListener;
use lore_transport::local_socket::AsyncStream;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio::time::Sleep;

use crate::remote::message::MESSAGE_HEAD_SIZE;
use crate::remote::message::MessageError;
use crate::remote::message::MessageToClient;
use crate::remote::message::blocking_read_header;
use crate::remote::message::write_message;
use crate::remote::network::UdsListener;
use crate::remote::network::UdsListenerError;

/// How long a read from a connection to the held socket waits for data before the connection is
/// closed unanswered, so a stalled client holds nothing for the length of a move.
pub const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The buffer a request's payload is read through and discarded, held by each connection's task.
const SKIP_BUFFER_SIZE: usize = 8 << 10;

/// The service socket, held while a move of the external root runs. Each connection is answered on
/// a task of its own. Dropping it gives up the socket once the runtime drops the accept task;
/// [`release`](Self::release) waits until it has.
pub struct OccupiedSocket {
    accept: JoinHandle<()>,
}

impl OccupiedSocket {
    /// Claims the socket `name` names, as a service does, and answers every request on it with the
    /// move reply, closing a connection whose read waits longer than `read_timeout`. `None` while
    /// another process holds the claim or a service answers on the socket. Called on the runtime.
    pub fn claim(
        name: &str,
        read_timeout: Duration,
    ) -> Result<Option<OccupiedSocket>, UdsListenerError> {
        let Some(claim) = UdsListener::claim(name)? else {
            return Ok(None);
        };
        let reply: Arc<[u8]> = move_reply()
            .forward::<UdsListenerError>("encoding the move reply")?
            .into();
        let _entered = lore_base::runtime::runtime().enter();
        let listener = claim.listen()?.into_async()?;
        Ok(Some(OccupiedSocket {
            accept: lore_spawn!(serve(listener, reply, read_timeout)),
        }))
    }

    /// Stops answering and gives up the socket, waiting until another process can claim it.
    pub async fn release(mut self) {
        self.accept.abort();
        let _ = (&mut self.accept).await;
    }
}

impl Drop for OccupiedSocket {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

/// The move reply as it goes on the socket, its header included.
#[lore_macro::test_pub]
fn move_reply() -> Result<Vec<u8>, MessageError> {
    let mut reply = Vec::new();
    write_message(&mut reply, &MessageToClient::<LoreEvent>::ExternalRootMove)?;
    Ok(reply)
}

/// Answers each connection `listener` accepts with `reply`, on a task of its own, until the task
/// is aborted.
async fn serve(listener: AsyncListener, reply: Arc<[u8]>, read_timeout: Duration) {
    loop {
        match listener.accept().await {
            Ok(connection) => {
                // Detached: a connection ends on its own, within its read timeout.
                drop(lore_spawn!(answer(
                    connection,
                    Arc::clone(&reply),
                    read_timeout
                )));
            }
            Err(error) => {
                lore_warn!("Failed to accept a connection on the held service socket: {error}");
            }
        }
    }
}

/// Reads one request from `connection`, skipping its payload, then writes `reply` and closes. A
/// read that waits longer than `read_timeout` closes the connection unanswered.
async fn answer(mut connection: AsyncStream, reply: Arc<[u8]>, read_timeout: Duration) {
    if skip_request(&mut connection, read_timeout).await.is_ok() {
        let _ = connection.write_all(&reply).await;
    }
}

/// Reads the head of one message from `reader`, then its payload through a fixed buffer that it
/// discards, so a request of any size costs no memory of its size. Fails once a read waits longer
/// than `read_timeout`, or the reader ends first.
#[lore_macro::test_pub]
async fn skip_request(
    reader: &mut (impl AsyncRead + Unpin),
    read_timeout: Duration,
) -> Result<(), MessageError> {
    let mut idle = pin!(tokio::time::sleep(read_timeout));
    let mut head = [0; MESSAGE_HEAD_SIZE];
    let mut filled = 0;
    while filled < head.len() {
        filled += read_within(reader, &mut head[filled..], idle.as_mut(), read_timeout).await?;
    }
    let header = blocking_read_header(&mut head.as_slice())?
        .ok_or_else(|| MessageError::internal("reading the request's head"))?;

    let mut left = u64::from(header.payload_size);
    let mut buffer = [0; SKIP_BUFFER_SIZE];
    while left > 0 {
        let size = buffer
            .len()
            .min(usize::try_from(left).unwrap_or(usize::MAX));
        let read = read_within(reader, &mut buffer[..size], idle.as_mut(), read_timeout).await?;
        left -= read as u64;
    }
    Ok(())
}

/// One read from `reader` into `buffer`, failing at its end or once `idle` fires first, which then
/// fires `read_timeout` after the read.
async fn read_within(
    reader: &mut (impl AsyncRead + Unpin),
    buffer: &mut [u8],
    mut idle: Pin<&mut Sleep>,
    read_timeout: Duration,
) -> Result<usize, MessageError> {
    let read = tokio::select! {
        read = reader.read(buffer) => read,
        () = idle.as_mut() => {
            return Err(MessageError::internal("timed out reading the request"));
        }
    };
    match read.internal("reading the request")? {
        0 => Err(MessageError::internal(
            "the client closed the connection within its request",
        )),
        read => {
            idle.reset(Instant::now() + read_timeout);
            Ok(read)
        }
    }
}
