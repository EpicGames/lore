// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use quinn::AsyncTimer;
use quinn::AsyncUdpSocket;
use quinn::Runtime;
use quinn::TokioRuntime;

/// A quinn [`Runtime`] that puts every task quinn spawns on the network runtime.
///
/// [`TokioRuntime`] spawns through `tokio::spawn`, which resolves the *ambient* runtime, so with
/// it the choice is re-made at every quinn call that spawns — both endpoint constructions,
/// `connect`, and awaiting an `Incoming`. Holding the runtime in the endpoint answers all of them
/// at once.
///
/// Spawning is the only override: `wrap_udp_socket` still registers the socket with whichever
/// reactor is current, so building an endpoint belongs inside a `net_runtime().enter()` guard.
///
/// Tasks carry no `LORE_CONTEXT`: they outlive the command that opens the connection, so a
/// captured context would attribute every later command's transport work to it.
#[derive(Debug)]
pub struct NetRuntime;

impl Runtime for NetRuntime {
    fn new_timer(&self, at: Instant) -> Pin<Box<dyn AsyncTimer>> {
        TokioRuntime.new_timer(at)
    }

    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>) {
        lore_base::lore_spawn_net_nocontext!(future);
    }

    fn wrap_udp_socket(&self, socket: std::net::UdpSocket) -> io::Result<Arc<dyn AsyncUdpSocket>> {
        TokioRuntime.wrap_udp_socket(socket)
    }

    fn now(&self) -> Instant {
        TokioRuntime.now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the type: a spawn lands on net however the caller was reached. Without the
    /// runtime it would follow the caller, which here is the test's own runtime.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn spawns_on_net_rather_than_the_calling_runtime() {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        NetRuntime.spawn(Box::pin(async move {
            let thread = std::thread::current()
                .name()
                .unwrap_or_default()
                .to_string();
            let _ = sender.send(thread);
        }));

        let thread = receiver.await.expect("spawned task ran");
        assert!(
            thread.starts_with("lore-net"),
            "quinn task ran on {thread} rather than a net worker"
        );
    }
}
