// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Message framing shared by both ends of the storage socket.
use std::collections::VecDeque;
use std::io;
use std::io::IoSlice;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Buf;
use bytes::Bytes;
use bytes::BytesMut;
use crossbeam_queue::SegQueue;
use smallvec::SmallVec;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;

use super::HEADER_SIZE;
use crate::local_socket;
use crate::local_socket::AsyncStream;
use crate::local_socket::AsyncWriteHalf;
use crate::quic::command_header::CommandHeader;

/// Socket reads land in buffers this large, so many small messages cost one read.
const READ_CHUNK: usize = 1024 * 1024;
/// Slices per `writev`, few enough to keep the slice array on the stack.
const WRITE_SLICES: usize = 64;
/// Buffers up to this size are copied next to their header rather than written as their own
/// `iovec`, so a run of small messages goes out as one contiguous buffer.
const INLINE_MAX: usize = 512;
const ARENA_CHUNK: usize = 64 * 1024;
/// Fixed fields a message carries in place. Those past it are carried as a payload of their own.
const INLINE_CAPACITY: usize = 128;

/// One message, its fixed fields held in place and its payloads as the caller's buffers.
pub struct Message {
    pub header: CommandHeader,
    inline: [u8; INLINE_CAPACITY],
    inline_len: usize,
    payloads: SmallVec<[Bytes; 2]>,
}

impl Message {
    /// A message of `header` with no fields or payloads yet.
    pub fn new(header: CommandHeader) -> Self {
        Self {
            header,
            inline: [0; INLINE_CAPACITY],
            inline_len: 0,
            payloads: SmallVec::new(),
        }
    }

    /// Appends fixed fields, which follow every payload already added.
    pub fn put(&mut self, bytes: &[u8]) -> &mut Self {
        let end = self.inline_len + bytes.len();
        if self.payloads.is_empty() && end <= INLINE_CAPACITY {
            self.inline[self.inline_len..end].copy_from_slice(bytes);
            self.inline_len = end;
        } else {
            self.payloads.push(Bytes::copy_from_slice(bytes));
        }
        self
    }

    /// Appends `payload`, written as the caller's buffer; an empty one adds nothing.
    pub fn payload(&mut self, payload: Bytes) -> &mut Self {
        if !payload.is_empty() {
            self.payloads.push(payload);
        }
        self
    }

    /// Appends a handler's response parts: small leading ones in place, the rest as they are.
    pub fn parts(&mut self, parts: Vec<Bytes>) -> &mut Self {
        for part in parts {
            if self.payloads.is_empty() && self.inline_len + part.len() <= INLINE_CAPACITY {
                self.put(&part);
            } else {
                self.payload(part);
            }
        }
        self
    }

    /// The body's size, header excluded.
    pub fn size(&self) -> usize {
        self.inline_len + self.payloads.iter().map(Bytes::len).sum::<usize>()
    }
}

/// What waits to be written, laid out for `writev`: headers, fixed fields and small payloads
/// copied into one growing buffer, larger payloads queued as they are.
#[derive(Default)]
struct Pending {
    queue: VecDeque<Bytes>,
    arena: BytesMut,
    /// Bytes encoded and not yet written.
    bytes: usize,
    /// How much of `bytes` the backlog counts already.
    counted: usize,
}

impl Pending {
    /// `counted` when the sender already added the message to the backlog.
    fn encode(&mut self, message: Message, counted: bool) {
        let size = HEADER_SIZE + message.size();
        self.bytes += size;
        if counted {
            self.counted += size;
        }
        if self.arena.capacity() - self.arena.len() < HEADER_SIZE + INLINE_CAPACITY + INLINE_MAX {
            self.arena.reserve(ARENA_CHUNK);
        }
        self.arena.extend_from_slice(&message.header.to_bytes_v4());
        self.arena
            .extend_from_slice(&message.inline[..message.inline_len]);
        for payload in message.payloads {
            if payload.len() <= INLINE_MAX {
                self.arena.extend_from_slice(&payload);
            } else {
                self.seal();
                self.queue.push_back(payload);
            }
        }
    }

    /// Queues whatever was copied since the last large payload.
    fn seal(&mut self) {
        if !self.arena.is_empty() {
            self.queue.push_back(self.arena.split().freeze());
        }
    }

    /// Drops everything unwritten; the backlog still counts it until settled.
    fn clear(&mut self) {
        self.queue.clear();
        self.arena.clear();
        self.bytes = 0;
    }

    /// Writes until the queue empties or the socket would block. `Ok(true)` once empty. What went
    /// out leaves `bytes` whatever the outcome.
    fn write(&mut self, stream: &AsyncWriteHalf) -> io::Result<bool> {
        self.seal();
        loop {
            if self.queue.is_empty() {
                return Ok(true);
            }
            let attempt = {
                let slices: SmallVec<[IoSlice<'_>; WRITE_SLICES]> = self
                    .queue
                    .iter()
                    .take(WRITE_SLICES)
                    .map(|buffer| IoSlice::new(buffer))
                    .collect();
                local_socket::try_send_vectored(stream.as_ref(), &slices)
            };
            let mut written = match attempt {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(written) => written,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(err) => return Err(err),
            };
            self.bytes -= written;
            while written > 0 {
                let Some(front) = self.queue.front_mut() else {
                    return Err(io::Error::other(
                        "the socket took more bytes than were queued",
                    ));
                };
                if front.len() <= written {
                    written -= front.len();
                    self.queue.pop_front();
                } else {
                    front.advance(written);
                    written = 0;
                }
            }
        }
    }
}

/// Combining rounds a sender does for others before leaving the rest to the drain task.
const COMBINE_ROUNDS: usize = 4;

/// Wakes a task waiting for room. While no one waits, a wake is a single load.
///
/// Whoever makes room does so with a sequentially consistent operation before calling
/// [`Wakeup::wake`], so a waiter that looks for room after announcing itself either finds it or
/// is woken.
#[derive(Default)]
pub struct Wakeup {
    waiting: AtomicBool,
    notify: Notify,
}

impl Wakeup {
    /// Wakes every task waiting, if any is.
    pub fn wake(&self) {
        if self.waiting.load(Ordering::SeqCst) && self.waiting.swap(false, Ordering::SeqCst) {
            self.notify.notify_waiters();
        }
    }

    /// Waits until `ready` holds, looking again after each wake. Any number of tasks may wait.
    pub async fn until(&self, mut ready: impl FnMut() -> bool) {
        loop {
            if ready() {
                return;
            }
            let woken = self.notify.notified();
            let mut woken = std::pin::pin!(woken);
            woken.as_mut().enable();
            self.waiting.store(true, Ordering::SeqCst);
            std::sync::atomic::fence(Ordering::SeqCst);
            if ready() {
                return;
            }
            woken.await;
        }
    }
}

/// What a writer holds unwritten, for a server bounding its memory. A message counts from when a
/// sender queues it for someone else, or from when the socket refuses part of it, until it is
/// written or dropped; one written at once by its own sender never counts at all.
pub struct Backlog {
    /// This writer's unwritten bytes.
    pub queued: AtomicUsize,
    /// Unwritten bytes of every writer sharing a budget with this one.
    pub shared: Arc<AtomicUsize>,
    /// Woken when this writer's backlog shrinks.
    pub drained: Arc<Wakeup>,
    /// Woken when the shared backlog shrinks.
    pub shared_drained: Arc<Wakeup>,
}

impl Backlog {
    fn grow(&self, bytes: usize) {
        self.queued.fetch_add(bytes, Ordering::Relaxed);
        self.shared.fetch_add(bytes, Ordering::Relaxed);
    }

    fn shrink(&self, bytes: usize) {
        self.queued.fetch_sub(bytes, Ordering::SeqCst);
        self.shared.fetch_sub(bytes, Ordering::SeqCst);
        self.drained.wake();
        self.shared_drained.wake();
    }
}

impl Drop for Backlog {
    /// Returns what a sender queued after the writer closed and no one was left to drop.
    fn drop(&mut self) {
        let left = *self.queued.get_mut();
        if left > 0 {
            self.shared.fetch_sub(left, Ordering::SeqCst);
            self.shared_drained.wake();
        }
    }
}

/// The write side of a socket, shared by everyone sending on it.
///
/// A sender that finds no one writing takes the write side and writes its message itself, along
/// with any others queued meanwhile, so an idle connection costs no hop to another task. A sender
/// that finds someone writing pushes its message onto a lock-free queue for them, so a busy
/// connection batches many messages per `writev`. When the socket pushes back, or a sender has
/// combined for others for a few rounds, the drain task takes over. A message is always written
/// whole before the next one starts.
pub struct SocketWriter {
    stream: AsyncWriteHalf,
    incoming: SegQueue<Message>,
    /// Held by whoever writes: a combining sender or the drain task. Taken and released with
    /// sequentially consistent swaps, so a sender that pushes after the holder's last look at the
    /// queue always sees the release and takes over.
    writing: AtomicBool,
    /// Touched by the holder of `writing`, and by `close`, so the lock is barely contended.
    pending: parking_lot::Mutex<Pending>,
    closed: AtomicBool,
    backlog: Option<Arc<Backlog>>,
    /// How long the peer may take nothing before the connection is given up on.
    stall: Option<Duration>,
    wake: Notify,
    /// Woken by `close` for whoever waits on the socket, which a shut-down socket does not wake
    /// on every platform.
    closing: Wakeup,
}

impl SocketWriter {
    /// A writer that, given `stall`, closes once the peer has taken nothing for that long, as a
    /// QUIC peer that stops acknowledging is dropped at its idle timeout.
    pub fn new(
        stream: AsyncWriteHalf,
        backlog: Option<Arc<Backlog>>,
        stall: Option<Duration>,
    ) -> Arc<Self> {
        Arc::new(Self {
            stream,
            incoming: SegQueue::new(),
            writing: AtomicBool::new(false),
            pending: parking_lot::Mutex::new(Pending::default()),
            closed: AtomicBool::new(false),
            backlog,
            stall,
            wake: Notify::new(),
            closing: Wakeup::default(),
        })
    }

    /// Whether the writer is closed, so that nothing more is written.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Writes `message` now, or queues it for whoever is writing. Returns `false` once closed.
    pub fn send(&self, message: Message) -> bool {
        if self.is_closed() {
            return false;
        }
        if !self.writing.swap(true, Ordering::SeqCst) {
            self.combine(Some(message));
            return true;
        }
        // Counted before it is queued, so the holder never settles it before it is added.
        if let Some(backlog) = &self.backlog {
            backlog.grow(HEADER_SIZE + message.size());
        }
        self.incoming.push(message);
        // The holder may have looked at the queue for the last time before the push.
        if !self.writing.swap(true, Ordering::SeqCst) {
            self.combine(None);
        }
        true
    }

    /// Brings the backlog in line with what `pending` holds unwritten.
    fn settle(&self, pending: &mut Pending) {
        let Some(backlog) = &self.backlog else {
            return;
        };
        if pending.bytes > pending.counted {
            backlog.grow(pending.bytes - pending.counted);
        } else if pending.bytes < pending.counted {
            backlog.shrink(pending.counted - pending.bytes);
        }
        pending.counted = pending.bytes;
    }

    /// Takes in what others queued and writes as the holder of `writing`.
    fn write_pending(&self, own: Option<Message>) -> io::Result<bool> {
        let mut pending = self.pending.lock();
        if self.is_closed() {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        // What others queued first: a sender that queued a message and then took `writing` for
        // its next one finds the first still queued, so each sender's messages leave in order.
        while let Some(message) = self.incoming.pop() {
            pending.encode(message, true);
        }
        if let Some(message) = own {
            pending.encode(message, false);
        }
        let result = pending.write(&self.stream);
        self.settle(&mut pending);
        result
    }

    /// Writes as the holder of `writing`: whatever others queued, then `own`.
    fn combine(&self, mut own: Option<Message>) {
        for _ in 0..COMBINE_ROUNDS {
            match self.write_pending(own.take()) {
                Ok(true) => {}
                Ok(false) => {
                    // The socket pushes back: the drain task takes over, holding `writing`.
                    self.wake.notify_one();
                    return;
                }
                Err(_) => {
                    self.close();
                    return;
                }
            }
            self.writing.swap(false, Ordering::SeqCst);
            if self.incoming.is_empty() || self.writing.swap(true, Ordering::SeqCst) {
                return;
            }
        }
        // Still busy after several rounds: the drain task takes over, holding `writing`.
        self.wake.notify_one();
    }

    /// Shuts the socket down and drops everything unwritten. Whoever waits on the socket in
    /// [`closed`](Self::closed), the drain included, and whoever waits on the backlog is woken to
    /// see the close.
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = socket2::SockRef::from(AsRef::<AsyncStream>::as_ref(&self.stream))
            .shutdown(std::net::Shutdown::Both);
        {
            let mut pending = self.pending.lock();
            while let Some(message) = self.incoming.pop() {
                pending.counted += HEADER_SIZE + message.size();
            }
            pending.clear();
            self.settle(&mut pending);
        }
        if let Some(backlog) = &self.backlog {
            backlog.drained.wake();
            backlog.shared_drained.wake();
        }
        self.closing.wake();
        self.wake.notify_one();
    }

    /// Completes once the writer is closed, for a task to race against its wait on the socket.
    pub async fn closed(&self) {
        self.closing.until(|| self.is_closed()).await;
    }

    /// Waits until the socket takes more, failing once the peer has taken nothing for the stall
    /// time.
    async fn writable(&self) -> io::Result<()> {
        match self.stall {
            Some(stall) => tokio::time::timeout(stall, self.stream.writable())
                .await
                .unwrap_or_else(|_elapsed| Err(io::ErrorKind::TimedOut.into())),
            None => self.stream.writable().await,
        }
    }

    /// Takes over writing whenever a sender hands it the write side, until the socket closes.
    pub async fn drain(self: Arc<Self>) {
        loop {
            self.wake.notified().await;
            loop {
                if self.is_closed() {
                    return;
                }
                let unwritten = {
                    let mut pending = self.pending.lock();
                    while let Some(message) = self.incoming.pop() {
                        pending.encode(message, true);
                    }
                    pending.seal();
                    !pending.queue.is_empty()
                };
                if !unwritten {
                    self.writing.swap(false, Ordering::SeqCst);
                    if self.incoming.is_empty() || self.writing.swap(true, Ordering::SeqCst) {
                        break;
                    }
                    continue;
                }
                let writable = tokio::select! {
                    biased;
                    () = self.closed() => return,
                    writable = self.writable() => writable,
                };
                if writable.is_err() || self.write_pending(None).is_err() {
                    self.close();
                    return;
                }
            }
        }
    }
}

/// Reads whole messages off a socket. A body is a slice of the read buffer, never a copy.
///
/// Each read leaves room for the largest message after the bytes it may fill, so a message that
/// starts in a read always completes in the same buffer.
pub struct FrameReader<R> {
    read: R,
    buffer: BytesMut,
    max_message: usize,
    /// Responses carry a status in place of a size; requests never set the error bit.
    responses: bool,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    /// A reader of messages from `read` whose bodies are at most `max_message` bytes: responses,
    /// which may carry a status, where `responses`, else requests.
    pub fn new(read: R, max_message: usize, responses: bool) -> Self {
        Self {
            read,
            buffer: BytesMut::new(),
            max_message,
            responses,
        }
    }

    /// The message at the front of the buffer and its body size, once its header has arrived.
    fn front(&self) -> io::Result<Option<(CommandHeader, usize)>> {
        if self.buffer.len() < HEADER_SIZE {
            return Ok(None);
        }
        let header = CommandHeader::from_bytes_v4(&self.buffer[..HEADER_SIZE]);
        if header.error && !self.responses {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request with the error bit set",
            ));
        }
        let size = if header.error {
            0
        } else {
            header.size_or_status as usize
        };
        if size > self.max_message {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("message of {size} bytes exceeds {}", self.max_message),
            ));
        }
        Ok(Some((header, size)))
    }

    /// The next message, or `None` at a clean end of stream. An errored response carries its
    /// status in the size field and has no body.
    pub async fn next(&mut self) -> io::Result<Option<(CommandHeader, Bytes)>> {
        let tail = HEADER_SIZE + self.max_message;
        loop {
            let missing = match self.front()? {
                Some((header, size)) if self.buffer.len() >= HEADER_SIZE + size => {
                    self.buffer.advance(HEADER_SIZE);
                    let body = if size == 0 {
                        Bytes::new()
                    } else {
                        self.buffer.split_to(size).freeze()
                    };
                    return Ok(Some((header, body)));
                }
                Some((_, size)) => HEADER_SIZE + size - self.buffer.len(),
                None => HEADER_SIZE - self.buffer.len(),
            };
            if self.buffer.is_empty() && self.buffer.capacity() < 2 * tail {
                self.buffer.reserve(READ_CHUNK.max(2 * tail));
            }
            if self.buffer.capacity() - self.buffer.len() < missing {
                // A guard only: a message starts in a read that leaves room for the largest after
                // it, or in an empty buffer of twice that, so the rest of it always fits.
                self.buffer.reserve(missing);
            }
            let free = self.buffer.capacity() - self.buffer.len();
            let limit = if free > tail + missing {
                free - tail
            } else {
                missing
            };
            let read = (&mut self.read)
                .take(limit as u64)
                .read_buf(&mut self.buffer)
                .await?;
            if read == 0 {
                return if self.buffer.is_empty() {
                    Ok(None)
                } else {
                    Err(io::ErrorKind::UnexpectedEof.into())
                };
            }
        }
    }
}
