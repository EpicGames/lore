// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The service socket held through a move of the external root: no service can start on it.
//! Every call on it is answered with the move reply, or refused with the move as the reason where
//! the caller is of a build whose messages differ, and every stop is refused. A request of
//! another protocol is closed on unanswered.
use std::pin::Pin;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use lore_base::error::ServiceUnavailable;
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

use crate::remote::call::EXTERNAL_ROOT_MOVING;
use crate::remote::message::FRAME_HEAD_SIZE;
use crate::remote::message::FrameKind;
use crate::remote::message::Header;
use crate::remote::message::MessageError;
use crate::remote::message::MessageToClient;
use crate::remote::message::Schemas;
use crate::remote::message::frame_header;
use crate::remote::message::write_hello;
use crate::remote::message::write_message;
use crate::remote::message::write_refusal;
use crate::remote::message::write_stop_result;
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
    /// Claims the socket `name` names, as a service does, and answers each request on it as the
    /// module describes, closing a connection whose read waits longer than `read_timeout`. `None`
    /// while another process holds the claim or a service answers on the socket. Called on the
    /// runtime.
    pub fn claim(
        name: &str,
        read_timeout: Duration,
    ) -> Result<Option<OccupiedSocket>, UdsListenerError> {
        let Some(claim) = UdsListener::claim(name)? else {
            return Ok(None);
        };
        let replies = Arc::new(
            Replies::encode().forward::<UdsListenerError>("encoding the held socket's replies")?,
        );
        let _entered = lore_base::runtime::runtime().enter();
        let listener = claim.listen()?.into_async()?;
        Ok(Some(OccupiedSocket {
            accept: lore_spawn!(serve(listener, replies, read_timeout)),
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

/// Why the held socket refuses a stop.
#[lore_macro::test_pub]
pub(crate) fn stop_refusal() -> String {
    format!("A stop needs a running Lore service, and {EXTERNAL_ROOT_MOVING}")
}

/// What the held socket answers a request with, as the bytes that go on the socket.
struct Replies {
    /// To a call of this build: this build's hello, then the move reply.
    call: Vec<u8>,
    /// To a call of a build whose schemas differ, which couldn't read the move reply: a refusal
    /// that gives the move as its reason.
    other_build: Vec<u8>,
    /// To a stop: `ServiceUnavailable` and [`stop_refusal`], as no service runs to stop.
    stop: Vec<u8>,
}

impl Replies {
    fn encode() -> Result<Replies, MessageError> {
        let mut call = Vec::new();
        write_hello(&mut call)?;
        write_message(&mut call, &MessageToClient::<LoreEvent>::ExternalRootMove)?;
        let mut other_build = Vec::new();
        write_refusal(&mut other_build, EXTERNAL_ROOT_MOVING)?;
        let mut stop = Vec::new();
        write_stop_result(&mut stop, ServiceUnavailable::FFI_CODE, &stop_refusal())?;
        Ok(Replies {
            call,
            other_build,
            stop,
        })
    }

    /// The reply to what a client asked for.
    fn to(&self, asked: Asked) -> &[u8] {
        match asked {
            Asked::Call(schemas) if schemas == Schemas::THIS_BUILD => &self.call,
            Asked::Call(_) => &self.other_build,
            Asked::Stop => &self.stop,
        }
    }
}

/// What a client asked the held socket for.
#[lore_macro::test_pub]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Asked {
    /// A call, opened with the schemas of the client's build.
    Call(Schemas),
    /// A stop.
    Stop,
}

/// Answers each connection `listener` accepts from `replies`, on a task of its own, until the
/// task is aborted.
async fn serve(listener: AsyncListener, replies: Arc<Replies>, read_timeout: Duration) {
    loop {
        match listener.accept().await {
            Ok(connection) => {
                // Detached: a connection ends on its own, within its read timeout.
                drop(lore_spawn!(answer(
                    connection,
                    Arc::clone(&replies),
                    read_timeout
                )));
            }
            Err(error) => {
                lore_warn!("Failed to accept a connection on the held service socket: {error}");
            }
        }
    }
}

/// Reads one request from `connection`, skipping its payload, then writes its reply and closes. A
/// read that waits longer than `read_timeout`, or a request this protocol doesn't open with,
/// closes the connection unanswered.
async fn answer(mut connection: AsyncStream, replies: Arc<Replies>, read_timeout: Duration) {
    if let Ok(asked) = skip_request(&mut connection, read_timeout).await {
        let _ = connection.write_all(replies.to(asked)).await;
    }
}

/// Reads one request from `reader`: a hello and the call that follows it, or a stop. The call's
/// payload is read through a fixed buffer that it discards, so a request of any size costs no
/// memory of its size. Fails once a read waits longer than `read_timeout`, or the reader ends
/// first, or the request is of another protocol or isn't one a client opens a connection with.
#[lore_macro::test_pub]
async fn skip_request(
    reader: &mut (impl AsyncRead + Unpin),
    read_timeout: Duration,
) -> Result<Asked, MessageError> {
    let mut idle = pin!(tokio::time::sleep(read_timeout));
    let opening = read_header(reader, idle.as_mut(), read_timeout).await?;
    match opening.kind {
        FrameKind::Stop if opening.payload_size == 1 => {
            let mut force = [0; 1];
            fill(reader, &mut force, idle.as_mut(), read_timeout).await?;
            Ok(Asked::Stop)
        }
        FrameKind::Hello if opening.payload_size as usize == Schemas::SIZE => {
            let mut hello = [0; Schemas::SIZE];
            fill(reader, &mut hello, idle.as_mut(), read_timeout).await?;
            let call = read_header(reader, idle.as_mut(), read_timeout).await?;
            if call.kind != FrameKind::Message {
                return Err(MessageError::internal("a hello that no call follows"));
            }
            skip_payload(reader, call.payload_size, idle.as_mut(), read_timeout).await?;
            Ok(Asked::Call(Schemas::from_payload(&hello)?))
        }
        kind => Err(MessageError::internal(format!(
            "a request that opens with a frame of kind {kind:?} and {} bytes, where a hello or a \
             stop was expected",
            opening.payload_size
        ))),
    }
}

/// Reads the head of one frame from `reader`, as [`fill`] reads.
async fn read_header(
    reader: &mut (impl AsyncRead + Unpin),
    idle: Pin<&mut Sleep>,
    read_timeout: Duration,
) -> Result<Header, MessageError> {
    let mut head = [0; FRAME_HEAD_SIZE];
    fill(reader, &mut head, idle, read_timeout).await?;
    frame_header(&head)
}

/// Fills `buffer` from `reader`, each read as [`read_within`] reads.
async fn fill(
    reader: &mut (impl AsyncRead + Unpin),
    buffer: &mut [u8],
    mut idle: Pin<&mut Sleep>,
    read_timeout: Duration,
) -> Result<(), MessageError> {
    let mut filled = 0;
    while filled < buffer.len() {
        filled += read_within(reader, &mut buffer[filled..], idle.as_mut(), read_timeout).await?;
    }
    Ok(())
}

/// Reads `size` bytes from `reader` through a fixed buffer and discards them, each read as
/// [`read_within`] reads.
async fn skip_payload(
    reader: &mut (impl AsyncRead + Unpin),
    size: u32,
    mut idle: Pin<&mut Sleep>,
    read_timeout: Duration,
) -> Result<(), MessageError> {
    let mut left = u64::from(size);
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
