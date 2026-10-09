// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore-rbe-server` — a Bazel Remote Execution API v2 endpoint whose cache is a Lore store.
//!
//! One process serves everything bazel needs (`Capabilities`, `ContentAddressableStorage`,
//! `ByteStream`, `ActionCache`, `Execution`) plus the internal queue its workers poll. The CAS
//! and the Action Cache are both Lore foreign-key namespaces; see the `rbe-lore` crate.
//!
//! This is the endpoint bazel talks to, and the store behind it is the same one the workers
//! open for themselves. Blob content therefore does not travel through here on its way to an
//! executor: bazel writes the input root in, a worker reads it out of its own tier of the same
//! store, and only the small messages -- leases and `ActionResult`s -- cross this process.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use anyhow::Result;
use clap::Parser;
use rbe_lore::LocalCache;
use rbe_lore::LoreBlobStore;
use rbe_proto::bytestream::byte_stream_server::ByteStreamServer;
use rbe_proto::reapi::action_cache_server::ActionCacheServer;
use rbe_proto::reapi::capabilities_server::CapabilitiesServer;
use rbe_proto::reapi::content_addressable_storage_server::ContentAddressableStorageServer;
use rbe_proto::reapi::execution_server::ExecutionServer;
use rbe_proto::worker::worker_queue_server::WorkerQueueServer;
use rbe_server::ac;
use rbe_server::cas;
use rbe_server::exec;
use tonic::transport::Server;

/// gRPC frame ceiling. Bazel's default `--experimental_remote_grpc_max_message_size` territory
/// is well under this; the batch limit we advertise (4 MiB) is what actually bounds requests.
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(
    name = "lore-rbe-server",
    about = "Bazel remote execution + remote cache backed by a Lore store"
)]
struct Args {
    /// Address to serve REAPI (and the internal worker queue) on.
    #[arg(long, default_value = "127.0.0.1:8980")]
    listen: String,

    /// Local-tier Lore repository. Always present; created on first use.
    #[arg(long, default_value = "cache-repo")]
    lore_repo: String,

    /// Upstream lore-server holding the shared cache, e.g. `lore://host:41337`. Without it the
    /// cache is local to this machine only.
    #[arg(long)]
    lore_server: Option<String>,

    /// Soft cap on the local tier, in bytes; enables Lore's GC. 0 disables GC, which is what a
    /// cold/warm measurement wants -- nothing may be evicted mid-run.
    #[arg(long, default_value_t = 0)]
    cache_size: u64,

    /// Verify that an Action Cache hit's referenced blobs are still in the CAS before serving
    /// it. Costs one batched existence check per hit; without it a GC that evicted an output
    /// blob turns into a build failure rather than a re-execution.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    verify_ac: bool,

    /// Keep a copy of what passes through in the local tier as well as publishing it upstream.
    /// Worth it when the lore-server is across a network; pure duplication when it runs on this
    /// machine, where every upload and every download would be written to the same disk twice.
    /// Ignored without --lore-server, where the local tier is the only copy.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    local_cache: bool,

    /// Log the cache counters every N seconds. 0 disables.
    #[arg(long, default_value_t = 0)]
    stats_interval: u64,

    /// Append `<hash> <size>` for every blob a client uploads to this file, for attributing what
    /// bazel sent. Off by default.
    #[arg(long)]
    log_cas_writes: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    let addr: SocketAddr = args
        .listen
        .parse()
        .with_context(|| format!("parsing --listen {}", args.listen))?;

    let store = Arc::new(
        LoreBlobStore::open(
            &args.lore_repo,
            args.cache_size,
            args.lore_server.as_deref(),
        )
        .await
        .context("opening the Lore store")?
        .with_local_cache(LocalCache {
            blobs: args.local_cache,
            files: args.local_cache,
        }),
    );
    tracing::info!(
        "lore store: {} (local copies: {})",
        store.location(),
        if args.local_cache { "kept" } else { "not kept" }
    );
    if !store.has_upstream() {
        tracing::warn!(
            "no --lore-server: the cache is the local tier only, not shared with other machines"
        );
    }

    let scheduler = exec::Scheduler::new(store.clone(), args.verify_ac);

    if args.stats_interval > 0 {
        let store = store.clone();
        lore_base::lore_spawn!(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(args.stats_interval));
            loop {
                tick.tick().await;
                tracing::info!("{}", store.stats.render());
            }
        });
    }

    let writes = match &args.log_cas_writes {
        Some(path) => Some(Arc::new(cas::WriteLog::create(path).with_context(
            || format!("creating --log-cas-writes {}", path.display()),
        )?)),
        None => None,
    };
    let cas = cas::CasService::new(store.clone(), writes.clone());
    let bytestream = cas::ByteStreamService::new(store.clone(), writes);
    let ac = ac::ActionCacheService::new(store.clone(), args.verify_ac);
    let caps = cas::CapabilitiesService;

    tracing::info!("serving REAPI on {addr}");
    let serve = Server::builder()
        .add_service(
            CapabilitiesServer::new(caps)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(
            ContentAddressableStorageServer::new(cas)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(
            ByteStreamServer::new(bytestream)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(
            ActionCacheServer::new(ac)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(
            ExecutionServer::new(exec::ExecutionService::new(scheduler.clone()))
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(
            WorkerQueueServer::new(exec::WorkerQueueService::new(scheduler.clone()))
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .serve_with_shutdown(addr, shutdown_signal());

    serve.await.context("serving")?;

    tracing::info!("{}", store.stats.render());
    // Flush Lore before exiting, or a warm cache does not survive the restart.
    rbe_lore::shutdown_lore();
    Ok(())
}

/// Shut down on SIGINT *or* SIGTERM. SIGTERM matters as much as ctrl-C here: scripts stop the
/// server with `kill`, and exiting without reaching `shutdown_lore()` below leaves Lore's
/// close-time flush undone, so the local tier does not survive the restart.
async fn shutdown_signal() {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!("cannot listen for SIGTERM ({err}); ctrl-C only");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT; shutting down"),
        _ = term.recv() => tracing::info!("SIGTERM; shutting down"),
    }
}
