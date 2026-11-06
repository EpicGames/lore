// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// A relaying process does no work of its own, so its pools sit at the
/// minimum every pool keeps anyway.
#[test]
fn relay_settings_ask_for_the_least_a_pool_keeps() {
    let relay = TokioSettings::relay_only();

    assert_eq!(relay.worker_threads, Some(MIN_THREADS_PER_POOL));
    assert_eq!(relay.max_blocking_threads, MIN_THREADS_PER_POOL);
    assert_eq!(relay.thread_keep_alive_seconds, default_thread_keep_alive());
    assert_eq!(
        relay.net_threads, None,
        "the net pool keeps the process default, which is already small"
    );
}

/// Sizing for relaying must never ask for more than sizing for working.
#[test]
fn relay_settings_are_never_larger_than_the_default_ones() {
    let relay = TokioSettings::relay_only();
    let default = TokioSettings::default();

    assert!(relay.max_blocking_threads <= default.max_blocking_threads);
    assert!(
        relay.worker_threads.unwrap_or(usize::MAX) <= default.worker_threads.unwrap_or(usize::MAX)
    );
}
use lore_base::runtime::*;

/// The net runtime has `max_blocking_threads(1)`, so blocking work dispatched there
/// starves every later net-side blocking call. Thread names distinguish the pools:
/// core spawns `lore-tokio-*`, net spawns `lore-net-*`.
#[test]
fn blocking_macros_target_core_even_from_a_net_task() {
    let thread_name = net_runtime().block_on(async {
        lore_base::lore_spawn_net!(async {
            lore_base::lore_spawn_blocking!(|| std::thread::current()
                .name()
                .unwrap_or_default()
                .to_string())
            .await
            .expect("blocking task joins")
        })
        .await
        .expect("net task joins")
    });

    assert!(
        thread_name.starts_with("lore-tokio-"),
        "blocking work issued from a net task ran on {thread_name:?}, \
             which is not a core blocking thread"
    );
}

/// The label of the `LORE_CONTEXT` the caller runs within, if one is set.
fn context_label() -> Option<&'static str> {
    try_lore_context().and_then(|context| context.downcast_ref::<&'static str>().copied())
}

#[test]
fn spawns_run_within_the_context_they_were_spawned_under() {
    let context: Arc<dyn Any + Send + Sync> = Arc::new("spawner");
    let labels = net_runtime().block_on(LORE_CONTEXT.scope(context, async {
        let task = lore_base::lore_spawn!(async { context_label() });
        let blocking = lore_base::lore_spawn_blocking!(context_label);
        (
            task.await.expect("task joins"),
            blocking.await.expect("blocking task joins"),
        )
    }));

    assert_eq!(labels, (Some("spawner"), Some("spawner")));
}

#[test]
fn spawns_outside_a_context_run_without_one() {
    let labels = net_runtime().block_on(async {
        let task = lore_base::lore_spawn!(async { context_label() });
        let blocking = lore_base::lore_spawn_blocking!(context_label);
        (
            task.await.expect("task joins"),
            blocking.await.expect("blocking task joins"),
        )
    });

    assert_eq!(labels, (None, None));
}

/// The accessors sit on request paths, so they must resolve to the one
/// runtime built for the process — never rebuild, never serialise.
#[test]
fn accessors_resolve_to_one_runtime_per_process() {
    let expected = (core_runtime().id(), net_runtime().id());
    assert_ne!(expected.0, expected.1, "core and net share a runtime");

    let threads: Vec<_> = (0..8)
        .map(|_| std::thread::spawn(|| (core_runtime().id(), net_runtime().id())))
        .collect();
    for thread in threads {
        assert_eq!(thread.join().expect("thread joins"), expected);
    }
}

/// Driving the work on a `current_thread` caller's own handle from a foreign thread
/// cannot work — `Handle::block_on` does not drive a `current_thread` runtime's tasks,
/// and the thread that would have is parked waiting for this one — so it hangs. The
/// spawned task completing is the proof that the work runs on core instead: it is what
/// the storage close does.
#[tokio::test(flavor = "current_thread")]
async fn shutdown_block_on_runs_spawns_from_a_current_thread_caller() {
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&ran);

    let completed = shutdown_block_on(
        async move {
            lore_base::lore_spawn!(async move { flag.store(true, Ordering::SeqCst) })
                .await
                .expect("spawned task joins");
        },
        Duration::from_secs(10),
    );

    assert!(completed, "shutdown work timed out instead of completing");
    assert!(
        ran.load(Ordering::SeqCst),
        "the task the shutdown work spawned never ran"
    );
}

/// The same from a multi-thread caller, which takes the `block_in_place` path — the one
/// that panics outright on a `current_thread` runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_block_on_runs_spawns_from_a_multi_thread_caller() {
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&ran);

    let completed = shutdown_block_on(
        async move {
            lore_base::lore_spawn!(async move { flag.store(true, Ordering::SeqCst) })
                .await
                .expect("spawned task joins");
        },
        Duration::from_secs(10),
    );

    assert!(completed, "shutdown work timed out instead of completing");
    assert!(ran.load(Ordering::SeqCst), "the spawned task never ran");
}

#[test]
fn shutdown_block_on_runs_without_a_runtime() {
    let completed = shutdown_block_on(
        async {
            tokio::task::yield_now().await;
        },
        Duration::from_secs(10),
    );

    assert!(completed, "shutdown work timed out instead of completing");
}

/// Bounded, not best-effort: a `current_thread` caller may hold the only thread that
/// could drive part of the work, so shutdown has to be able to give up on it.
#[tokio::test(flavor = "current_thread")]
async fn shutdown_block_on_gives_up_instead_of_hanging() {
    let completed = shutdown_block_on(std::future::pending::<()>(), Duration::from_millis(50));

    assert!(
        !completed,
        "a future that cannot finish must report timeout"
    );
}

#[test]
fn runtime_returns_valid_handle() {
    let handle = runtime();
    handle.block_on(async {
        tokio::task::yield_now().await;
    });
}

#[test]
fn runtime_with_settings_returns_valid_handle() {
    let settings = TokioSettings {
        max_blocking_threads: 4,
        thread_keep_alive_seconds: 5,
        worker_threads: Some(2),
        net_threads: None,
    };
    let handle = runtime_with_settings(Some(settings));
    handle.block_on(async {
        tokio::task::yield_now().await;
    });
}

/// The syscall pool's request on a host with this many cores, so the cases
/// below do not depend on the machine running them.
const IO_REQUEST: usize = 16;

#[test]
fn default_thread_counts_match_design_formulas() {
    let counts = default_thread_counts(8, IO_REQUEST);
    assert_eq!(counts.worker, 8);
    assert_eq!(
        counts.blocking,
        CORE_BLOCKING_THREADS + NET_BLOCKING_THREADS
    );
    assert_eq!(counts.net, DEFAULT_NET_THREADS);
    assert_eq!(counts.io, IO_REQUEST);
}

#[test]
fn apportion_returns_defaults_when_within_limit() {
    let defaults = default_thread_counts(8, IO_REQUEST);
    let total = defaults.total();
    assert_eq!(apportion_thread_counts(defaults, total), defaults);
    assert_eq!(apportion_thread_counts(defaults, total + 100), defaults);
}

#[test]
fn apportion_fills_budget_exactly_above_the_floor() {
    let defaults = default_thread_counts(64, IO_REQUEST);
    for limit in (POOL_COUNT * MIN_THREADS_PER_POOL)..=defaults.total() {
        let counts = apportion_thread_counts(defaults, limit);
        assert_eq!(counts.total(), limit, "limit {limit} not used exactly");
        for pool in counts.as_array() {
            assert!(pool >= MIN_THREADS_PER_POOL);
        }
    }
}

#[test]
fn apportion_floors_below_min_total() {
    let counts = apportion_thread_counts(default_thread_counts(64, IO_REQUEST), 1);
    assert_eq!(counts.as_array(), [MIN_THREADS_PER_POOL; POOL_COUNT]);
}

#[test]
fn apportion_at_limit_64_on_64_core_host() {
    let defaults = default_thread_counts(64, IO_REQUEST);
    assert_eq!(defaults.total(), 85);
    let counts = apportion_thread_counts(defaults, 64);
    assert_eq!(counts.worker, 48);
    assert_eq!(counts.blocking, MIN_THREADS_PER_POOL);
    assert_eq!(counts.net, MIN_THREADS_PER_POOL);
    assert_eq!(counts.io, 12);
    assert_eq!(counts.total(), 64);
}

/// A net request raises the pool's ideal, and the limit still binds the total —
/// asking for 64 net threads under a 64-thread ceiling cannot buy 64 of them
/// on top of the worker and blocking pools.
#[test]
fn a_net_request_is_scaled_by_the_limit() {
    let requested = ThreadCounts {
        worker: 20,
        blocking: CORE_BLOCKING_THREADS + NET_BLOCKING_THREADS,
        net: 64,
        io: IO_REQUEST,
    };
    let counts = apportion_thread_counts(requested, 64);
    assert_eq!(counts.total(), 64);
    assert!(counts.net < requested.net);
    assert!(
        counts.net > counts.worker,
        "the largest request keeps the largest share"
    );
}

/// Sizing for storage connections gives the core runtime one thread fewer than the
/// processor count, down to the floor, and the net runtime a thread for each
/// connection, from its default up to half of the two together. A process that
/// opens the connections takes those threads from the core runtime, so it runs as
/// many CPU-bound threads as with one connection; one that serves them adds them.
#[test]
fn connections_divide_the_shared_threads_at_every_core_count() {
    for cores in 1..=256 {
        let defaults = default_thread_counts(cores, IO_REQUEST);
        let one = threads_for_connections(defaults, NetConnections::Opened(1));
        assert_eq!(
            (one.worker, one.net),
            ((cores - 1).max(MIN_THREADS_PER_POOL), DEFAULT_NET_THREADS),
            "one connection on {cores} cores"
        );
        for connections in 1..=10 {
            let opened = threads_for_connections(defaults, NetConnections::Opened(connections));
            let served = threads_for_connections(defaults, NetConnections::Served(connections));
            let case =
                format!("{connections} connections on {cores} cores gave {opened:?}, {served:?}");
            assert_eq!(opened.worker + opened.net, one.worker + one.net, "{case}");
            assert!(opened.net >= DEFAULT_NET_THREADS, "{case}");
            assert!(opened.net <= connections.max(DEFAULT_NET_THREADS), "{case}");
            assert!(opened.worker >= opened.net, "{case}");
            assert_eq!(
                (served.worker, served.net),
                (one.worker, opened.net),
                "{case}"
            );
            for counts in [opened, served] {
                assert_eq!(
                    (counts.blocking, counts.io),
                    (defaults.blocking, defaults.io),
                    "{case}"
                );
            }
        }
    }
}

/// (core, net) threads from small to large hosts, for 1, 4 and 10 opened
/// connections and for the service's 10 served ones: few cores keep the floor, and
/// a host with enough cores gives each connection a thread at little cost to the
/// core runtime.
#[test]
fn connections_on_small_and_large_hosts() {
    use NetConnections::Opened;
    use NetConnections::Served;
    let sized = |cores, connections| {
        let counts = threads_for_connections(default_thread_counts(cores, IO_REQUEST), connections);
        (counts.worker, counts.net)
    };
    for (cores, expected) in [
        (1, [(2, 2), (2, 2), (2, 2), (2, 2)]),
        (2, [(2, 2), (2, 2), (2, 2), (2, 2)]),
        (4, [(3, 2), (3, 2), (3, 2), (3, 2)]),
        (8, [(7, 2), (5, 4), (5, 4), (7, 4)]),
        (16, [(15, 2), (13, 4), (9, 8), (15, 8)]),
        (32, [(31, 2), (29, 4), (23, 10), (31, 10)]),
        (128, [(127, 2), (125, 4), (119, 10), (127, 10)]),
    ] {
        assert_eq!(
            [Opened(1), Opened(4), Opened(10), Served(10)]
                .map(|connections| sized(cores, connections)),
            expected,
            "{cores} cores"
        );
    }
}

/// An explicit net request, as a serving process's one per processor or
/// `LORE_NET_THREADS` makes, takes precedence over the connections and leaves the
/// core runtime at one thread per processor. Without one the pools are sized for
/// the connections, and a process that set none keeps the defaults.
#[test]
fn an_explicit_net_request_takes_precedence_over_the_connections() {
    let defaults = default_thread_counts(16, IO_REQUEST);
    assert_eq!(requested_counts(defaults, None, None), defaults);
    assert_eq!(
        requested_counts(defaults, Some(16), None),
        ThreadCounts {
            net: 16,
            ..defaults
        }
    );
    for connections in [
        NetConnections::Opened(1),
        NetConnections::Opened(10),
        NetConnections::Served(10),
    ] {
        assert_eq!(
            requested_counts(defaults, None, Some(connections)),
            threads_for_connections(defaults, connections)
        );
        assert_eq!(
            requested_counts(defaults, Some(16), Some(connections)),
            ThreadCounts {
                net: 16,
                ..defaults
            }
        );
    }
}

/// Building a runtime settles the connections, so both runtimes are sized for the
/// same ones: connections set afterwards are refused.
#[test]
fn connections_are_refused_once_a_runtime_exists() {
    let _ = net_runtime();
    assert!(!set_net_connections(NetConnections::Opened(4)));
}

/// The limit is a ceiling on the total whatever the per-pool requests are, so
/// no knob can raise the process above what an embedder asked for. The
/// per-pool floor is a property of scaling down rather than of every result:
/// a request that already fits is honoured as written, including one below it.
#[test]
fn the_limit_bounds_the_total_for_any_request() {
    for worker in [2, 8, 64, 256] {
        for blocking in [1, 2, 128] {
            for net in [1, 2, 64, 512] {
                for io in [1, 16, 128] {
                    let requested = ThreadCounts {
                        worker,
                        blocking,
                        net,
                        io,
                    };
                    for limit in [POOL_COUNT * MIN_THREADS_PER_POOL, 16, 64, 1024] {
                        let counts = apportion_thread_counts(requested, limit);
                        assert!(
                            counts.total() <= limit,
                            "{requested:?} at limit {limit} gave {counts:?}"
                        );
                        if requested.total() <= limit {
                            assert_eq!(counts, requested, "a request that fits is untouched");
                            continue;
                        }
                        for pool in counts.as_array() {
                            assert!(
                                pool >= MIN_THREADS_PER_POOL,
                                "{requested:?} at limit {limit} starved a pool: {counts:?}"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn guarded_task_completes() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    let completed = Arc::new(AtomicBool::new(false));
    let completed_clone = completed.clone();

    runtime_spawn_guarded(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        completed_clone.store(true, Ordering::Release);
    });

    runtime_flush_guarded().await;
    assert!(completed.load(Ordering::Acquire));
}
