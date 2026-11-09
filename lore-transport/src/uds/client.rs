// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Storage client over the machine server's socket.
//!
//! A process holds one connection per socket, shared by every storage client and session. A
//! request reserves a slot, which is both its credit and the place its response lands, then
//! builds its message and writes it onto the socket unless other messages are queued ahead of it.
//! The reader task fills the slot named by the response's command id and wakes the request.
//!
//! The storage layer's sessions are this client's own. Each is issued on the server over the
//! connection of the moment, and issued again on a new connection by the first command that finds
//! it bound to a closed one: a command the closed connection failed is sent once more there, as
//! the QUIC client reconnects and reissues, waiting out a server that is away with the same
//! backoff. Only a command that fails
//! on the new connection too, or finds no server within that backoff, reaches the caller as a
//! disconnect.
//!
//! A command may thus reach the server twice, as on QUIC: a compare-and-swap the server applied
//! before the connection closed answers its resend with a conflict.
//!
//! The server answers every request it reads, within its handler timeout. A request still
//! unanswered long after that, on a connection that has answered nothing meanwhile, means the
//! connection lost it, so the connection is closed and everything on it moves to a new one. A
//! connection that keeps answering is left alone however long its queue: a request may wait its
//! turn behind the server's admission for longer than any one handler takes.
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicU16;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::atomic::fence;
use std::task::Context as TaskContext;
use std::task::Poll;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use crossbeam_epoch as epoch;
use crossbeam_epoch::Atomic;
use crossbeam_epoch::Guard;
use crossbeam_epoch::Owned;
use crossbeam_utils::CachePadded;
use futures::task::AtomicWaker;
use lore_base::error::Disconnected;
use lore_base::lore_debug;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_base::types::VerifyResult;
use lore_error_set::prelude::*;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;
use zerocopy::IntoBytes;

use super::MAX_MESSAGE;
use super::SERVER_HELLO_SIZE;
use super::ServerHello;
use super::VERSION;
use super::client_hello;
use super::io::FrameReader;
use super::io::Message;
use super::io::SocketWriter;
use super::spawn_io;
use crate::auth::AuthEndpoint;
use crate::connection::RECONNECT_MAX_ATTEMPTS;
use crate::connection::RECONNECT_MAX_DELAY;
use crate::connection::RECONNECT_START_DELAY;
use crate::connection::SuppliedCredentials;
use crate::error::ProtocolError;
use crate::local_socket;
use crate::local_socket::AsyncReadHalf;
use crate::quic::QuicClientError;
use crate::quic::client::BoundSession;
use crate::quic::client::NO_GENERATION;
use crate::quic::command_header::CommandHeader;
use crate::quic::response_reader::handle_error;
use crate::quic::storage_service::Command;
use crate::quic::storage_service::check_query_batch;
use crate::quic::storage_service::check_query_statuses;
use crate::quic::storage_service::command_error;
use crate::quic::storage_service::read_fragment;
use crate::quic::storage_service::read_hash;
use crate::quic::storage_service::read_metadata;
use crate::quic::storage_service::read_resolved;
use crate::quic::storage_service::read_verify;
use crate::quic::storage_service::sessions;
use crate::quic::storage_service::sessions::Authorizer;
use crate::quic::storage_service::sessions::IssueSession;
use crate::quic::storage_service::sessions::Session;
use crate::quic::storage_service::sessions::Sessions;
use crate::quic::storage_service::sessions::not_open;
use crate::quic::storage_service::sessions::session_start_payload;
use crate::quic::storage_service::sessions::started_session_id;
use crate::traits::Storage;

/// Bounds connecting and the handshake, so a server that accepts and never answers cannot hold
/// up every storage client in the process.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Seconds a request may go unanswered, while the connection answers nothing else, before the
/// connection is given up on. Twice the handler timeout the server states, and no less than
/// this, so only a lost response reaches it.
const DEADLINE_SECONDS: u32 = 120;

/// Seconds between looks for requests past their deadline.
const SWEEP_SECONDS: u32 = 10;

/// Spins before a thread waiting on another's brief update yields instead.
const SPINS: u32 = 64;

#[lore_macro::test_pub]
const FREE: u8 = 0;
#[lore_macro::test_pub]
const WAITING: u8 = 1;
/// One thread holds the slot: the reader writing the result, which READY follows, or a thread
/// about to free it.
#[lore_macro::test_pub]
const FILLING: u8 = 2;
#[lore_macro::test_pub]
const READY: u8 = 3;
#[lore_macro::test_pub]
const ABANDONED: u8 = 4;

#[lore_macro::test_pub]
type Outcome = Result<Bytes, QuicClientError>;

/// Where one outstanding request's response lands, aligned to a cache line so that neighbouring
/// requests share none.
#[lore_macro::test_pub]
#[repr(align(64))]
struct Slot {
    state: AtomicU8,
    /// Bumped on every use, so a late response for an earlier use is recognised and dropped.
    tag: AtomicU16,
    waker: AtomicWaker,
    /// The connection's tick when the slot was armed, for the deadline sweep.
    armed: AtomicU32,
    /// Written by the reader between FILLING and READY, taken by the request after READY.
    result: UnsafeCell<Option<Outcome>>,
}

// SAFETY: `result` is only written by the thread that moved the state from WAITING to FILLING,
// and only read by the request after it observed READY.
unsafe impl Sync for Slot {}

/// Waits out another thread's brief update: spins, then yields.
fn backoff(spins: &mut u32) {
    if *spins < SPINS {
        *spins += 1;
        std::hint::spin_loop();
    } else {
        std::thread::yield_now();
    }
}

/// A lock-free stack of free slot indices. Last freed is first reused, so a request at low
/// concurrency keeps landing on a warm line; the tag in the head word guards against ABA.
#[lore_macro::test_pub]
struct FreeList {
    /// Tag in the high half, index + 1 in the low half; 0 when empty. On a line of its own, as
    /// every request moves it.
    head: CachePadded<AtomicU64>,
    next: Box<[AtomicU32]>,
}

impl FreeList {
    fn new(count: usize) -> Self {
        let list = Self {
            head: CachePadded::new(AtomicU64::new(0)),
            next: (0..count).map(|_| AtomicU32::new(0)).collect(),
        };
        for index in (0..count).rev() {
            list.push(index as u16);
        }
        list
    }

    #[lore_macro::test_pub]
    fn pop(&self) -> Option<u16> {
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            let top = (head & 0xFFFF_FFFF) as u32;
            if top == 0 {
                return None;
            }
            let next = self.next[top as usize - 1].load(Ordering::Relaxed);
            let replaced = (((head >> 32) + 1) << 32) | next as u64;
            match self.head.compare_exchange_weak(
                head,
                replaced,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some((top - 1) as u16),
                Err(current) => head = current,
            }
        }
    }

    #[lore_macro::test_pub]
    fn push(&self, index: u16) {
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            self.next[index as usize].store((head & 0xFFFF_FFFF) as u32, Ordering::Relaxed);
            let replaced = (((head >> 32) + 1) << 32) | (index as u64 + 1);
            match self.head.compare_exchange_weak(
                head,
                replaced,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(current) => head = current,
            }
        }
    }
}

/// Counts a request waiting for a free slot while it waits.
struct Waiting<'a>(&'a AtomicUsize);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One slot per credit the server granted. A request holds its slot until the response arrives,
/// whether or not the caller still waits for it, so the client never runs ahead of the server's
/// window.
#[lore_macro::test_pub]
struct Slots {
    slots: Box<[Slot]>,
    free: FreeList,
    waiters: AtomicUsize,
    available: Notify,
}

impl Slots {
    #[lore_macro::test_pub]
    fn new(count: usize) -> Self {
        let count = count.clamp(1, u16::MAX as usize);
        Self {
            slots: (0..count)
                .map(|_| Slot {
                    state: AtomicU8::new(FREE),
                    tag: AtomicU16::new(0),
                    waker: AtomicWaker::new(),
                    armed: AtomicU32::new(0),
                    result: UnsafeCell::new(None),
                })
                .collect(),
            free: FreeList::new(count),
            waiters: AtomicUsize::new(0),
            available: Notify::new(),
        }
    }

    async fn acquire(&self) -> u16 {
        if let Some(index) = self.free.pop() {
            return index;
        }
        self.waiters.fetch_add(1, Ordering::SeqCst);
        let _waiting = Waiting(&self.waiters);
        loop {
            let available = self.available.notified();
            let mut available = std::pin::pin!(available);
            available.as_mut().enable();
            fence(Ordering::SeqCst);
            if let Some(index) = self.free.pop() {
                return index;
            }
            available.await;
        }
    }

    fn release(&self, index: u16) {
        let slot = &self.slots[index as usize];
        // SAFETY: the caller holds the slot, having moved it to FILLING itself or found it READY
        // as the slot's request, so no other thread touches the cell until it is FREE.
        unsafe { *slot.result.get() = None };
        slot.state.store(FREE, Ordering::Release);
        self.free.push(index);
        fence(Ordering::SeqCst);
        if self.waiters.load(Ordering::SeqCst) > 0 {
            self.available.notify_one();
        }
    }

    /// Hands `outcome` to the request waiting on `id`.
    #[lore_macro::test_pub]
    fn complete(&self, id: u32, outcome: Outcome) {
        let index = (id & 0xFFFF) as usize;
        let Some(slot) = self.slots.get(index) else {
            return;
        };
        if slot.tag.load(Ordering::Acquire) != (id >> 16) as u16 {
            return;
        }
        match slot
            .state
            .compare_exchange(WAITING, FILLING, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                // SAFETY: FILLING gives this thread the result cell until READY is published.
                unsafe { *slot.result.get() = Some(outcome) };
                slot.state.store(READY, Ordering::Release);
                slot.waker.wake();
            }
            // The request is gone: whoever claims the slot frees it, once.
            Err(ABANDONED) => {
                if slot
                    .state
                    .compare_exchange(ABANDONED, FILLING, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    self.release(index as u16);
                }
            }
            Err(_) => {}
        }
    }
}

/// A reserved slot, not yet sent. Dropped unsent, it frees the slot.
pub struct Ticket<'a> {
    connection: &'a UdsConnection,
    index: u16,
    tag: u16,
    sent: bool,
}

impl<'a> Ticket<'a> {
    /// Writes `message` under this ticket's command id and returns its response.
    pub fn send(mut self, mut message: Message) -> Response<'a> {
        self.sent = true;
        let connection = self.connection;
        let index = self.index;
        let id = (self.tag as u32) << 16 | index as u32;
        let size = message.size();
        let response = Response {
            slots: &connection.slots,
            index,
            done: false,
        };
        if size > connection.max_message {
            connection
                .slots
                .complete(id, Err(QuicClientError::ClientMessageTooBig));
            return response;
        }
        message.header.command_id = id;
        message.header.size_or_status = size as u32;
        if !connection.writer.send(message) {
            connection
                .slots
                .complete(id, Err(QuicClientError::Terminated));
        }
        response
    }
}

impl Drop for Ticket<'_> {
    /// Claims the slot back as a completion would, since a close may be completing it right now.
    fn drop(&mut self) {
        if self.sent {
            return;
        }
        let slots = &self.connection.slots;
        let slot = &slots.slots[self.index as usize];
        let mut spins = 0;
        loop {
            match slot
                .state
                .compare_exchange(WAITING, FILLING, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) | Err(READY) => {
                    slots.release(self.index);
                    return;
                }
                Err(FILLING) => backoff(&mut spins),
                Err(state) => {
                    debug_assert!(false, "unsent ticket found its slot in state {state}");
                    return;
                }
            }
        }
    }
}

/// Waits for one slot's response. Dropped early, it leaves the slot to be freed when the response
/// arrives. Polled again once it has completed, it stays pending, since its slot may serve
/// another request by then.
pub struct Response<'a> {
    slots: &'a Slots,
    index: u16,
    done: bool,
}

impl Response<'_> {
    fn take(&mut self) -> Outcome {
        let slot = &self.slots.slots[self.index as usize];
        // SAFETY: READY was observed, so the reader is done with the cell.
        let outcome =
            unsafe { (*slot.result.get()).take() }.unwrap_or(Err(QuicClientError::Terminated));
        self.done = true;
        self.slots.release(self.index);
        outcome
    }
}

impl Future for Response<'_> {
    type Output = Outcome;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        if self.done {
            return Poll::Pending;
        }
        let slot = &self.slots.slots[self.index as usize];
        if slot.state.load(Ordering::Acquire) != READY {
            slot.waker.register(cx.waker());
            if slot.state.load(Ordering::Acquire) != READY {
                return Poll::Pending;
            }
        }
        Poll::Ready(self.take())
    }
}

impl Drop for Response<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let slot = &self.slots.slots[self.index as usize];
        let mut spins = 0;
        loop {
            match slot.state.compare_exchange(
                WAITING,
                ABANDONED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(READY) => {
                    let _ = self.take();
                    return;
                }
                Err(FILLING) => backoff(&mut spins),
                Err(state) => {
                    debug_assert!(false, "pending response found its slot in state {state}");
                    return;
                }
            }
        }
    }
}

/// Connection generations, numbered across the process so that a session bound on one connection
/// is never taken for bound on another.
static GENERATIONS: AtomicU32 = AtomicU32::new(0);

/// One connection to the machine server's socket.
pub struct UdsConnection {
    /// Below `NO_GENERATION`, which binds a session to no connection.
    generation: u32,
    writer: Arc<SocketWriter>,
    slots: Slots,
    max_message: usize,
    closed: AtomicBool,
    /// Seconds since connecting, advanced by the deadline sweep.
    tick: AtomicU32,
    /// The tick of the latest response. On a line of its own, as the reader stores it for each.
    answered: CachePadded<AtomicU32>,
    /// Seconds without a response, with a request overdue as long, before the connection is lost.
    deadline: u32,
}

impl UdsConnection {
    /// Opens a connection whose reader and drain run on the runtime this runs on, which must be the
    /// socket's I/O runtime: a socket registers with the runtime it is opened on.
    ///
    /// Cold: once per process and socket, and again after the connection drops.
    async fn connect_here(path: PathBuf) -> Result<Arc<Self>, ProtocolError> {
        let disconnected = |err: std::io::Error| {
            lore_debug!("Socket {} unavailable: {err}", path.display());
            ProtocolError::from(Disconnected)
        };
        let handshake = async {
            let mut stream = local_socket::connect(&path).await?;
            stream.write_all(&client_hello()).await?;
            let mut hello = [0u8; SERVER_HELLO_SIZE];
            stream.read_exact(&mut hello).await?;
            Ok::<_, std::io::Error>((stream, hello))
        };
        let (stream, hello) = tokio::time::timeout(CONNECT_TIMEOUT, handshake)
            .await
            .map_err(|_elapsed| disconnected(std::io::ErrorKind::TimedOut.into()))?
            .map_err(disconnected)?;
        let hello = ServerHello::from_bytes(&hello).ok_or_else(|| {
            ProtocolError::internal(format!("{} is not a Lore storage socket", path.display()))
        })?;
        check_hello(&hello, &path)?;

        let (read, write) = stream.into_split();
        // The server stops reading only while its admission waits on handlers, which its handler
        // timeout bounds; the deadline sweep covers the rest.
        let writer = SocketWriter::new(write, None, None);
        let connection = Arc::new(Self {
            generation: GENERATIONS.fetch_add(1, Ordering::Relaxed) % NO_GENERATION,
            writer: writer.clone(),
            slots: Slots::new(hello.credits as usize),
            max_message: hello.max_message as usize,
            closed: AtomicBool::new(false),
            tick: AtomicU32::new(0),
            answered: CachePadded::new(AtomicU32::new(0)),
            deadline: DEADLINE_SECONDS.max(2 * hello.handler_timeout_seconds as u32),
        });
        spawn_io(writer.drain());
        spawn_io(sweep(Arc::downgrade(&connection)));
        spawn_io(read_loop(
            FrameReader::new(read, connection.max_message, true),
            connection.clone(),
        ));
        lore_debug!(
            "Connected to {} (credits {})",
            path.display(),
            hello.credits
        );
        Ok(connection)
    }

    /// Whether the connection is closed, by either side.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst) || self.writer.is_closed()
    }

    /// Whether a request has gone unanswered past its deadline while nothing else was answered.
    #[lore_macro::test_pub]
    fn stalled(&self, tick: u32) -> bool {
        // Signed, as a response or a request can take a tick newer than the one looked at.
        let overdue = |since: u32| tick.wrapping_sub(since) as i32 > self.deadline as i32;
        if !overdue(self.answered.load(Ordering::Relaxed)) {
            return false;
        }
        // Acquire, so a slot seen armed is seen with its arming tick. An abandoned request still
        // holds its slot until its response arrives.
        self.slots.slots.iter().any(|slot| {
            matches!(slot.state.load(Ordering::Acquire), WAITING | ABANDONED)
                && overdue(slot.armed.load(Ordering::Relaxed))
        })
    }

    /// Moves the connection's clock on by `seconds`, as the sweep does once a second.
    #[lore_macro::test_pub]
    fn advance(&self, seconds: u32) -> u32 {
        self.tick.fetch_add(seconds, Ordering::Relaxed) + seconds
    }

    /// Fails every outstanding request and shuts the socket down. The storage clients reopen
    /// their sessions on the next connection.
    fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.writer.close();
        for (index, slot) in self.slots.slots.iter().enumerate() {
            let id = (slot.tag.load(Ordering::Acquire) as u32) << 16 | index as u32;
            self.slots.complete(id, Err(QuicClientError::Terminated));
        }
    }

    /// Reserves a slot for one request. The request's message is built after, so it is not held
    /// across the wait.
    pub async fn reserve(&self) -> Result<Ticket<'_>, QuicClientError> {
        if self.is_closed() {
            return Err(QuicClientError::Terminated);
        }
        let index = self.slots.acquire().await;
        let slot = &self.slots.slots[index as usize];
        let tag = slot.tag.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        slot.armed
            .store(self.tick.load(Ordering::Relaxed), Ordering::Relaxed);
        let ticket = Ticket {
            connection: self,
            index,
            tag,
            sent: false,
        };
        slot.state.swap(WAITING, Ordering::SeqCst);
        // A close that swept the slots before this one was armed would never answer it.
        if self.is_closed() {
            return Err(QuicClientError::Terminated);
        }
        Ok(ticket)
    }

    /// One empty round trip through the server's dispatch: the floor under every command.
    pub async fn ping(&self) -> Result<Duration, QuicClientError> {
        let start = Instant::now();
        let ticket = self.reserve().await?;
        ticket
            .send(Message::new(CommandHeader::new_with_session(
                Command::ClientIdentify as u8,
                0,
                0,
                0,
            )))
            .await?;
        Ok(start.elapsed())
    }
}

/// Advances the connection's tick and closes it once a request outlives its deadline.
async fn sweep(connection: Weak<UdsConnection>) {
    let mut ticks = tokio::time::interval(Duration::from_secs(1));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        let Some(connection) = connection.upgrade() else {
            return;
        };
        if connection.is_closed() {
            return;
        }
        let tick = connection.advance(1);
        if tick % SWEEP_SECONDS != 0 {
            continue;
        }
        if connection.stalled(tick) {
            lore_base::lore_warn!(
                "The storage socket answered nothing for {}s with requests waiting; reconnecting",
                connection.deadline
            );
            connection.close();
            return;
        }
    }
}

async fn read_loop(mut read: FrameReader<AsyncReadHalf>, connection: Arc<UdsConnection>) {
    let mut closed = std::pin::pin!(connection.writer.closed());
    loop {
        let message = tokio::select! {
            biased;
            message = read.next() => message,
            () = &mut closed => break,
        };
        let (header, body) = match message {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(err) => {
                lore_debug!("Socket read failed: {err}");
                break;
            }
        };
        connection
            .answered
            .store(connection.tick.load(Ordering::Relaxed), Ordering::Relaxed);
        let outcome = if header.error {
            Err(handle_error(header.size_or_status))
        } else {
            Ok(body)
        };
        connection.slots.complete(header.command_id, outcome);
    }
    connection.close();
}

static CONNECTIONS: LazyLock<tokio::sync::Mutex<HashMap<PathBuf, Arc<UdsConnection>>>> =
    LazyLock::new(Default::default);

/// The process's connection to `path`, opened on first use and again after it closes.
pub async fn shared_connection(path: &Path) -> Result<Arc<UdsConnection>, ProtocolError> {
    if let Some(connection) = open_connection(&*CONNECTIONS.lock().await, path) {
        return Ok(connection);
    }
    // The connect and its publishing run in one task, so a caller dropped during the handshake
    // leaves the connection to the next caller rather than a second connection to the server.
    let path = path.to_path_buf();
    spawn_io(async move {
        let mut connections = CONNECTIONS.lock().await;
        if let Some(connection) = open_connection(&connections, &path) {
            return Ok(connection);
        }
        let connection = UdsConnection::connect_here(path.clone()).await?;
        connections.insert(path, connection.clone());
        Ok(connection)
    })
    .await
    .internal("socket connect task")?
}

fn open_connection(
    connections: &HashMap<PathBuf, Arc<UdsConnection>>,
    path: &Path,
) -> Option<Arc<UdsConnection>> {
    connections
        .get(path)
        .filter(|connection| !connection.is_closed())
        .cloned()
}

/// Refuses a server that speaks another version of the storage socket, which is not retried, or
/// states a larger message than the protocol allows, which the reader would reserve twice over.
fn check_hello(hello: &ServerHello, path: &Path) -> Result<(), ProtocolError> {
    if hello.version != VERSION {
        return Err(ProtocolError::internal(format!(
            "{} speaks storage socket version {}, not {VERSION}",
            path.display(),
            hello.version
        )));
    }
    if hello.max_message as usize > MAX_MESSAGE {
        return Err(ProtocolError::internal(format!(
            "{} states messages of {} bytes, more than {MAX_MESSAGE}",
            path.display(),
            hello.max_message
        )));
    }
    Ok(())
}

/// The live socket connection: read by every command without a lock, replaced by a reconnect.
struct Current(Atomic<Arc<UdsConnection>>);

impl Current {
    fn new(connection: Arc<UdsConnection>) -> Self {
        Self(Atomic::new(connection))
    }

    /// The connection, held while `guard` is.
    fn load<'g>(&self, guard: &'g Guard) -> &'g Arc<UdsConnection> {
        // SAFETY: never null, and a replaced connection is freed only past every reader pinned
        // when it was unlinked, or by `drop`, which no reader outlives.
        unsafe { self.0.load(Ordering::Acquire, guard).deref() }
    }

    fn get(&self) -> Arc<UdsConnection> {
        self.load(&epoch::pin()).clone()
    }

    fn replace(&self, connection: Arc<UdsConnection>) {
        let guard = epoch::pin();
        let replaced = self
            .0
            .swap(Owned::new(connection), Ordering::AcqRel, &guard);
        // SAFETY: unlinked above; a reader still pinned on it holds it until its guard drops.
        unsafe { guard.defer_destroy(replaced) };
    }
}

impl Drop for Current {
    fn drop(&mut self) {
        // SAFETY: `drop` holds the only reference, so no reader is pinned on the connection.
        drop(unsafe { std::mem::take(&mut self.0).try_into_owned() });
    }
}

/// A storage client over the machine server's socket, on the process's connection to it.
pub struct UdsStorageClient {
    path: PathBuf,
    current: Current,
    /// The open sessions, bound as the server issued them on a connection's generation.
    sessions: Sessions,
    /// Held while a reconnect runs, so concurrent failures reconnect once.
    reconnecting: tokio::sync::Mutex<()>,
    /// When a reconnect last waited out its whole backoff and gave up; cleared by the next one
    /// that connects.
    gave_up: parking_lot::Mutex<Option<Instant>>,
    /// The reconnect backoff: first and longest wait in milliseconds, and the number of waits.
    backoff: (u64, u64, usize),
    authorizer: Authorizer,
    /// This client, for the task that issues a session again ([`sessions::rebind`]).
    this: Weak<UdsStorageClient>,
}

impl UdsStorageClient {
    /// A storage client over the socket at `path`. It connects once, failing at once where no
    /// server answers; a later reconnect waits out the reconnect backoff.
    pub async fn connect(
        path: PathBuf,
        recipient_domain: String,
        auth_endpoint: &AuthEndpoint,
        identity: &str,
        credentials: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<Self>, ProtocolError> {
        Self::connect_with_backoff(
            path,
            recipient_domain,
            auth_endpoint,
            identity,
            credentials,
            (
                RECONNECT_START_DELAY,
                RECONNECT_MAX_DELAY,
                RECONNECT_MAX_ATTEMPTS,
            ),
        )
        .await
    }

    /// A client whose reconnects wait out `backoff`: first and longest wait in milliseconds, and
    /// the number of waits.
    #[lore_macro::test_pub]
    async fn connect_with_backoff(
        path: PathBuf,
        recipient_domain: String,
        auth_endpoint: &AuthEndpoint,
        identity: &str,
        credentials: &Arc<SuppliedCredentials>,
        backoff: (u64, u64, usize),
    ) -> Result<Arc<Self>, ProtocolError> {
        let current = shared_connection(&path).await?;
        Ok(Arc::new_cyclic(|this| Self {
            path,
            current: Current::new(current),
            sessions: Sessions::default(),
            reconnecting: tokio::sync::Mutex::new(()),
            gave_up: parking_lot::Mutex::new(None),
            backoff,
            authorizer: Authorizer::new(auth_endpoint, &recipient_domain, identity, credentials),
            this: this.clone(),
        }))
    }

    /// Records a reconnect giving up now, as one that waited out its backoff does.
    #[cfg(feature = "test-util")]
    pub fn record_give_up(&self) {
        *self.gave_up.lock() = Some(Instant::now());
    }

    /// Whether a reconnect has given up since the last one that connected.
    #[cfg(feature = "test-util")]
    pub fn given_up(&self) -> bool {
        self.gave_up.lock().is_some()
    }

    /// The commands holding the last issue of the session `session_id` names.
    #[cfg(feature = "test-util")]
    pub fn awaiting_issue(&self, session_id: u32) -> usize {
        self.sessions.awaiting_issue(session_id)
    }

    /// The live socket connection, reopened if the last one closed. The reopen is boxed: it is cold,
    /// once per lost connection.
    async fn live(&self) -> Result<Arc<UdsConnection>, ProtocolError> {
        let current = self.current.get();
        if !current.is_closed() {
            return Ok(current);
        }
        Box::pin(self.reconnect()).await
    }

    /// Opens a new connection, waiting out a server that is away, as a restart takes it, with the
    /// QUIC client's reconnect backoff. One reconnect runs at a time. One that waited its turn
    /// takes the connection the one before opened, or fails at once if that one gave up after it
    /// began waiting, so the commands of an outage fail together rather than one backoff after
    /// another.
    async fn reconnect(&self) -> Result<Arc<UdsConnection>, ProtocolError> {
        let waiting_since = Instant::now();
        let _reconnecting = self.reconnecting.lock().await;
        let current = self.current.get();
        if !current.is_closed() {
            return Ok(current);
        }
        if self
            .gave_up
            .lock()
            .is_some_and(|gave_up| gave_up > waiting_since)
        {
            return Err(ProtocolError::from(Disconnected));
        }
        let (start, maximum, attempts) = self.backoff;
        let mut retry = crate::util::retry(start, maximum, attempts);
        loop {
            match shared_connection(&self.path).await {
                Ok(fresh) => {
                    self.current.replace(fresh.clone());
                    *self.gave_up.lock() = None;
                    return Ok(fresh);
                }
                Err(err @ ProtocolError::Disconnected(_)) => {
                    if !retry.wait().await {
                        *self.gave_up.lock() = Some(Instant::now());
                        return Err(err);
                    }
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// The live connection, and the id the session `session_id` is bound to on it: `None` when
    /// the session is bound to another connection or none.
    fn bound(&self, session_id: u32) -> Result<(Arc<UdsConnection>, Option<u32>), ProtocolError> {
        let guard = epoch::pin();
        let session = self
            .sessions
            .find(session_id, &guard)
            .ok_or_else(|| not_open(session_id))?;
        let bound = session.bound();
        let connection = self.current.load(&guard);
        let id = (bound.generation == connection.generation).then_some(bound.id);
        Ok((connection.clone(), id))
    }

    /// Sends one command on `connection`, building its message once a slot is reserved.
    async fn send(
        connection: &UdsConnection,
        command: Command,
        server_id: u32,
        build: &(impl Fn(&mut Message) + Sync),
    ) -> Result<Bytes, QuicClientError> {
        let ticket = connection.reserve().await?;
        let mut message = Message::new(CommandHeader::new_with_session(
            command as u8,
            0,
            0,
            server_id,
        ));
        build(&mut message);
        ticket.send(message).await
    }

    /// Sends a command on `session_id` where the server issued it, once more on the live
    /// connection if it was issued on another or the one it went out on closed before answering.
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments.
    #[allow(clippy::manual_async_fn)]
    fn session_call(
        &self,
        command: Command,
        session_id: u32,
        build: impl Fn(&mut Message) + Sync,
    ) -> impl Future<Output = Result<Bytes, ProtocolError>> {
        async move {
            let (connection, Some(server_id)) = self.bound(session_id)? else {
                return Box::pin(self.resend(command, session_id, &build)).await;
            };
            match Self::send(&connection, command, server_id, &build).await {
                Err(QuicClientError::Terminated) => {
                    Box::pin(self.resend(command, session_id, &build)).await
                }
                result => result.map_err(|err| command_error(&command, err)),
            }
        }
    }

    /// Sends `command` on `session_id` over the live connection, reconnecting and issuing the
    /// session there first where needed. Cold: after a lost connection or another's reconnect, and
    /// boxed where [`Self::session_call`] awaits it.
    #[cold]
    async fn resend(
        &self,
        command: Command,
        session_id: u32,
        build: &(impl Fn(&mut Message) + Sync),
    ) -> Result<Bytes, ProtocolError> {
        let connection = self.live().await?;
        sessions::rebind(self, session_id, connection.generation).await?;
        let (connection, Some(server_id)) = self.bound(session_id)? else {
            return Err(ProtocolError::from(Disconnected));
        };
        Self::send(&connection, command, server_id, build)
            .await
            .map_err(|err| command_error(&command, err))
    }

    /// Reads the fragment at `address` and its payload, for [`Storage::get`] and
    /// [`Storage::get_priority`] alike: the socket carries no priority.
    async fn get_fragment(
        &self,
        session_id: u32,
        address: &Address,
    ) -> Result<(Fragment, Bytes), ProtocolError> {
        let mut payload = self
            .session_call(Command::Get, session_id, |message| {
                message.put(address.as_bytes());
            })
            .await?;
        let fragment = read_fragment(&mut payload, "get")?;
        Ok((fragment, payload))
    }
}

impl IssueSession for UdsStorageClient {
    fn authorizer(&self) -> &Authorizer {
        &self.authorizer
    }

    fn sessions(&self) -> &Sessions {
        &self.sessions
    }

    fn upgrade(&self) -> Option<Arc<Self>> {
        self.this.upgrade()
    }

    fn is_live(&self, bound: BoundSession) -> bool {
        let current = self.current.get();
        bound.generation == current.generation && !current.is_closed()
    }

    async fn stop(&self, bound: BoundSession) {
        let connection = self.current.get();
        if connection.generation != bound.generation || connection.is_closed() {
            return;
        }
        let _ = Self::send(
            &connection,
            Command::Authorize,
            bound.id,
            &|message: &mut Message| {
                message.put(&[1]);
            },
        )
        .await;
    }

    /// Starts the session on the live connection, and once more on a new connection should that
    /// one close before the server answers, as the QUIC client resends it.
    async fn send_issue(&self, session: &Session, token: &str) -> Result<(), ProtocolError> {
        let payload = session_start_payload(session.partition, &session.correlation_id, token);
        let mut resent = false;
        loop {
            let connection = self.live().await?;
            session.issue_on(connection.generation);
            let sent = Self::send(
                &connection,
                Command::Authorize,
                0,
                &|message: &mut Message| {
                    message.payload(payload.clone());
                },
            )
            .await;
            let response = match sent {
                Err(QuicClientError::Terminated) if !resent => {
                    resent = true;
                    continue;
                }
                sent => sent.map_err(|err| command_error(&Command::Authorize, err))?,
            };
            session.bind(started_session_id(&response)?);
            return Ok(());
        }
    }
}

#[async_trait]
impl Storage for UdsStorageClient {
    async fn session_start(
        &self,
        partition: Partition,
        correlation_id: &str,
    ) -> Result<u32, ProtocolError> {
        sessions::start(self, partition, correlation_id).await
    }

    async fn session_stop(&self, session_id: u32) -> Result<(), ProtocolError> {
        sessions::stop(self, session_id).await
    }

    async fn get(
        &self,
        session_id: u32,
        address: &Address,
    ) -> Result<(Fragment, Bytes), ProtocolError> {
        self.get_fragment(session_id, address).await
    }

    async fn get_priority(
        &self,
        session_id: u32,
        address: &Address,
    ) -> Result<(Fragment, Bytes), ProtocolError> {
        self.get_fragment(session_id, address).await
    }

    async fn get_resolved(
        &self,
        session_id: u32,
        key: &Hash,
        context: &Context,
        flags: u32,
    ) -> Result<(Hash, Fragment, Bytes), ProtocolError> {
        let mut payload = self
            .session_call(Command::GetResolved, session_id, |message| {
                message
                    .put(key.as_bytes())
                    .put(context.as_bytes())
                    .put(&flags.to_le_bytes());
            })
            .await?;
        let (resolved, fragment) = read_resolved(&mut payload)?;
        Ok((resolved, fragment, payload))
    }

    async fn get_metadata(
        &self,
        session_id: u32,
        address: &Address,
    ) -> Result<Fragment, ProtocolError> {
        let payload = self
            .session_call(Command::GetMetadata, session_id, |message| {
                message.put(address.as_bytes());
            })
            .await?;
        read_metadata(&payload)
    }

    async fn put(
        &self,
        session_id: u32,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
    ) -> Result<(), ProtocolError> {
        self.session_call(Command::Put, session_id, |message| {
            message
                .put(address.as_bytes())
                .put(fragment.as_bytes())
                .payload(payload.clone().unwrap_or_default());
        })
        .await
        .map(|_| ())
    }

    async fn put_resolved(
        &self,
        session_id: u32,
        key: &Hash,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
    ) -> Result<(), ProtocolError> {
        self.session_call(Command::PutResolved, session_id, |message| {
            message
                .put(key.as_bytes())
                .put(address.as_bytes())
                .put(fragment.as_bytes())
                .payload(payload.clone().unwrap_or_default());
        })
        .await
        .map(|_| ())
    }

    async fn query(&self, session_id: u32, address: &[Address]) -> Result<Bytes, ProtocolError> {
        check_query_batch(address.len())?;
        let payload = self
            .session_call(Command::Query, session_id, |message| {
                message.payload(Bytes::copy_from_slice(address.as_bytes()));
            })
            .await?;
        check_query_statuses(&payload, address.len())?;
        Ok(payload)
    }

    async fn verify(
        &self,
        session_id: u32,
        address: &Address,
        heal: bool,
    ) -> Result<VerifyResult, ProtocolError> {
        let payload = self
            .session_call(Command::Verify, session_id, |message| {
                message.put(address.as_bytes()).put(&[u8::from(heal)]);
            })
            .await?;
        read_verify(&payload, address)
    }

    async fn copy(
        &self,
        session_id: u32,
        source_partition: Partition,
        source_address: Address,
        target_context: Context,
    ) -> Result<(), ProtocolError> {
        self.session_call(Command::Copy, session_id, |message| {
            message
                .put(source_partition.as_bytes())
                .put(source_address.as_bytes())
                .put(target_context.as_bytes());
        })
        .await
        .map(|_| ())
    }

    async fn mutable_load(
        &self,
        session_id: u32,
        key: &Hash,
        key_type: KeyType,
    ) -> Result<Hash, ProtocolError> {
        let payload = self
            .session_call(Command::MutableLoad, session_id, |message| {
                message.put(key.as_bytes()).put(&[key_type as u8]);
            })
            .await?;
        read_hash(&payload, "mutable_load")
    }

    async fn mutable_store(
        &self,
        session_id: u32,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), ProtocolError> {
        self.session_call(Command::MutableStore, session_id, |message| {
            message
                .put(key.as_bytes())
                .put(value.as_bytes())
                .put(&[key_type as u8]);
        })
        .await
        .map(|_| ())
    }

    async fn mutable_compare_and_swap(
        &self,
        session_id: u32,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, ProtocolError> {
        let payload = self
            .session_call(Command::MutableCas, session_id, |message| {
                message
                    .put(key.as_bytes())
                    .put(expected.as_bytes())
                    .put(value.as_bytes())
                    .put(&[key_type as u8]);
            })
            .await?;
        read_hash(&payload, "mutable_cas")
    }
}
