// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod io_pool;
mod log;
mod metrics;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod process;
mod protocol;
mod resource;
mod trace;

use std::sync::Mutex;
use std::time::Duration;

use lore_base::lore_spawn_blocking;
use lore_base::runtime::core_blocking_threads;
use lore_base::runtime::core_runtime;
use lore_base::runtime::thread_counts;
use lore_server::telemetry::*;
use lore_telemetry::ExporterConfig;
use tokio::net::TcpListener;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::registry::Registry;

fn max_level(directives: &str) -> Option<LevelFilter> {
    <EnvFilter as Layer<Registry>>::max_level_hint(&log_filter(directives))
}

/// A standalone server start sets no `RUST_LOG`, and an operator running
/// one wants to see the warnings it emits.
#[test]
fn no_directives_enable_warnings() {
    assert_eq!(max_level(""), Some(LevelFilter::WARN));
}

#[test]
fn a_directive_overrides_the_default() {
    assert_eq!(max_level("info"), Some(LevelFilter::INFO));
}

/// A large deployment cuts the output back down to errors alone.
#[test]
fn errors_only_remains_available() {
    assert_eq!(max_level("error"), Some(LevelFilter::ERROR));
}

#[test]
fn an_unparsable_directive_leaves_the_default_in_force() {
    assert_eq!(max_level("=!=not a directive=!="), Some(LevelFilter::WARN));
}

/// Listeners on one port of both loopback addresses, as `localhost` may resolve to either. The
/// IPv6 listener is absent on a host without IPv6.
async fn loopback_collector() -> (TcpListener, Option<TcpListener>, u16) {
    for _ in 0..10 {
        let ipv4 = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binding the collector");
        let port = ipv4.local_addr().expect("the collector's address").port();
        match TcpListener::bind(("::1", port)).await {
            Ok(ipv6) => return (ipv4, Some(ipv6), port),
            Err(error) if error.kind() != std::io::ErrorKind::AddrInUse => {
                return (ipv4, None, port);
            }
            // Another process holds this port on IPv6, and would take the exporter's connection,
            // so try another port.
            Err(_) => {}
        }
    }
    panic!("no port is free on both loopback addresses");
}

/// An export that runs to completion on the calling thread, as the SDK's processors run theirs.
type Export = Box<dyn FnOnce() + Send>;

/// Asserts that the exporter `build` makes for a configuration connects to its collector, named
/// by hostname so the address is resolved, while every core blocking thread is held.
pub(crate) fn assert_connects_while_the_core_blocking_pool_is_held(
    build: impl FnOnce(&ExporterConfig) -> Export,
) {
    // Two of these at once could each park part of the pool and wait forever for the rest.
    static HOLDING_THE_POOL: Mutex<()> = Mutex::new(());
    let _turn = HOLDING_THE_POOL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    core_runtime().block_on(async {
        let mut releases = Vec::new();
        for _ in 0..core_blocking_threads(thread_counts()) {
            let (parked, ready) = tokio::sync::oneshot::channel::<()>();
            let (release, released) = std::sync::mpsc::channel::<()>();
            drop(lore_spawn_blocking!(move || {
                let _ = parked.send(());
                let _ = released.recv();
            }));
            tokio::time::timeout(Duration::from_secs(10), ready)
                .await
                .expect("a core blocking thread comes free to park")
                .expect("a blocking thread parks");
            releases.push(release);
        }
        let (ipv4, ipv6, port) = loopback_collector().await;

        let export = build(&ExporterConfig {
            endpoint: format!("http://localhost:{port}"),
            queue_size: 1,
            timeout: 10_000,
        });
        let export = std::thread::spawn(export);
        let accepted = tokio::time::timeout(Duration::from_secs(10), async {
            match &ipv6 {
                Some(ipv6) => tokio::select! {
                    accepted = ipv4.accept() => accepted,
                    accepted = ipv6.accept() => accepted,
                },
                None => ipv4.accept().await,
            }
        })
        .await;
        drop(releases);

        accepted
            .expect("connecting must not wait on the core blocking pool")
            .expect("accepting the exporter's connection");
        drop((ipv4, ipv6));
        export.join().expect("the export thread finishes");
    });
}
