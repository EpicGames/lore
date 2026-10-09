// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore-rbe-worker` — a remote executor that reads and writes the shared cache directly.
//!
//! For every lease the worker fetches the action, materialises its entire input root into a
//! fresh scratch directory, runs the command, stores the outputs, and deletes the scratch
//! directory. Nothing an action produced survives it.
//!
//! Blob content does not travel through the scheduler, and it does not travel through this
//! process either. The worker opens its own handle on the same Lore store the scheduler serves
//! bazel from -- a local tier on disk, tiering to the shared lore-server -- and moves content
//! between that store and the input root by path, so a compile's inputs and outputs are never
//! resident here. Only the lease and the resulting `ActionResult` cross the gRPC connection.
//!
//! The staging directory is the executor-side cache for file content: the first action that
//! needs a header fetches it from the shared cache into staging, every later action links it
//! from there, and an action's outputs are staged the same way for whichever later action on
//! this worker consumes them. The local Lore tier keeps only the small messages (`Action`,
//! `Command`, `Directory`), so file content is on this machine's disk once rather than once in
//! Lore and again in staging. Wiping both is what makes an executor clean, which is what the
//! cold arm of the benchmark does.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use clap::Parser;
use prost::Message as _;
use rbe_lore::LocalCache;
use rbe_lore::LoreBlobStore;
use rbe_lore::Ns;
use rbe_lore::digest;
use rbe_proto::reapi::Action;
use rbe_proto::reapi::ActionResult;
use rbe_proto::reapi::Command;
use rbe_proto::reapi::Digest;
use rbe_proto::reapi::Directory;
use rbe_proto::reapi::DirectoryNode;
use rbe_proto::reapi::ExecutedActionMetadata;
use rbe_proto::reapi::FileNode;
use rbe_proto::reapi::OutputDirectory;
use rbe_proto::reapi::OutputFile;
use rbe_proto::reapi::OutputSymlink;
use rbe_proto::reapi::SymlinkNode;
use rbe_proto::reapi::Tree;
use rbe_proto::worker::CompleteLeaseRequest;
use rbe_proto::worker::HeartbeatRequest;
use rbe_proto::worker::TakeLeaseRequest;
use rbe_proto::worker::TakeLeaseResponse;
use rbe_proto::worker::worker_queue_client::WorkerQueueClient;
use tokio::sync::Mutex;
use tonic::transport::Channel;
use tonic::transport::Endpoint;

const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
/// stdout/stderr at or below this ride inline in the ActionResult instead of costing a store
/// round trip; most actions produce nothing at all.
#[lore_macro::test_pub]
const INLINE_OUTPUT_LIMIT: usize = 8 * 1024;
/// How often this process renews every lease it holds. The server reclaims a lease that goes
/// unrenewed for a few of these, so an action outliving one interval is normal and only a dead
/// worker loses its work.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// Content-addressed input staging, under the scratch root so it shares a filesystem with every
/// input root and can be hardlinked into them. Wiped with the scratch on startup, which is what
/// makes a restarted worker a clean one.
const STAGING_DIR: &str = "staged-inputs";
/// How often the staging directory is measured against its cap. Off the action path, because
/// eviction competes with materialisation for the same disk.
const STAGING_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Parser, Debug)]
#[command(
    name = "lore-rbe-worker",
    about = "Remote executor for lore-rbe-server"
)]
pub struct Args {
    /// The lore-rbe-server to lease work from, e.g. `http://127.0.0.1:8980`.
    #[arg(long, default_value = "http://127.0.0.1:8980")]
    server: String,

    /// Executor-local Lore repository. Always present; created on first use.
    #[arg(long, default_value = "worker-repo")]
    lore_repo: String,

    /// Upstream lore-server holding the shared cache, e.g. `lore://host:41337`. Without it this
    /// worker can only see what its own local tier already holds.
    #[arg(long)]
    lore_server: Option<String>,

    /// Soft cap on the local tier, in bytes; enables Lore's GC. 0 disables GC, which is what a
    /// cold/warm measurement wants -- nothing may be evicted mid-run.
    #[arg(long, default_value_t = 0)]
    cache_size: u64,

    /// Scratch root. Each action gets a fresh subdirectory that is removed afterwards.
    #[arg(long, default_value = "/tmp/lore-rbe-worker")]
    scratch: PathBuf,

    /// How many actions this process runs concurrently. Each slot has its own lease loop and
    /// its own scratch directory; all slots share one Lore store.
    #[arg(long, default_value_t = 4)]
    slots: usize,

    /// Identifies this worker in logs and in `ExecutedActionMetadata.worker`.
    #[arg(long)]
    worker_id: Option<String>,

    /// Fallback PATH for actions whose Command does not set one.
    #[arg(long, default_value = "/usr/local/bin:/usr/bin:/bin")]
    default_path: String,

    /// Default action timeout in seconds when the Action does not specify one.
    #[arg(long, default_value_t = 900)]
    default_timeout: u64,

    /// Log the cache counters every N seconds. 0 disables.
    #[arg(long, default_value_t = 0)]
    stats_interval: u64,

    /// Cap on the content-addressed input staging directory, in bytes. Over it, staged inputs are
    /// evicted oldest first, skipping anything still linked into a live input root. Without a cap
    /// a long-running worker accumulates every input it has ever seen. 0 disables eviction.
    #[arg(long, default_value_t = 32 * 1024 * 1024 * 1024)]
    staging_size: u64,

    /// Keep scratch directories instead of deleting them (for debugging a failing action).
    #[arg(long, default_value_t = false)]
    keep_scratch: bool,
}

/// Runs the worker until SIGINT or SIGTERM: leases actions from `args.server` and executes them in
/// `args.slots` slots.
pub async fn run(args: Args) -> Result<()> {
    require_system_allocator()?;

    let args = Arc::new(args);
    let worker_id = args
        .worker_id
        .clone()
        .unwrap_or_else(|| format!("worker-{}", uuid::Uuid::new_v4()));

    // A fresh worker must not inherit a previous run's scratch: leftover files there are state
    // an action could read, and every action is supposed to see only its own input root.
    if args.scratch.exists() {
        let _ = std::fs::remove_dir_all(&args.scratch);
    }
    std::fs::create_dir_all(&args.scratch)
        .with_context(|| format!("creating scratch dir {}", args.scratch.display()))?;

    let store = Arc::new(
        LoreBlobStore::open(
            &args.lore_repo,
            args.cache_size,
            args.lore_server.as_deref(),
        )
        .await
        .context("opening the Lore store")?
        // File content lives in staging (see the module docs); a second copy in the local tier
        // would only be written to disk twice.
        .with_local_cache(LocalCache {
            blobs: true,
            files: false,
        }),
    );
    tracing::info!("lore store: {}", store.location());
    if !store.has_upstream() {
        tracing::warn!(
            "no --lore-server: this worker sees only its own local tier, not the shared cache"
        );
    }

    let endpoint: Endpoint = args.server.parse::<Endpoint>()?.tcp_nodelay(true);
    let channel = endpoint
        .connect()
        .await
        .with_context(|| format!("connecting to {}", args.server))?;

    tracing::info!(
        "worker {worker_id} up: {} slots, server {}",
        args.slots,
        args.server
    );

    if args.stats_interval > 0 {
        let store = store.clone();
        let every = Duration::from_secs(args.stats_interval);
        lore_base::lore_spawn!(async move {
            let mut tick = tokio::time::interval(every);
            loop {
                tick.tick().await;
                tracing::info!("{}", store.stats.render());
            }
        });
    }

    let staging = Arc::new(Staging::new(args.scratch.join(STAGING_DIR)));
    if args.staging_size > 0 {
        let staging = staging.clone();
        let cap = args.staging_size;
        lore_base::lore_spawn!(async move {
            let mut tick = tokio::time::interval(STAGING_SWEEP_INTERVAL);
            loop {
                tick.tick().await;
                let staging = staging.clone();
                match rbe_lore::spawn_blocking(move || staging.evict(cap)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => tracing::warn!("evicting staged inputs failed: {err:#}"),
                    Err(err) => tracing::warn!("evicting staged inputs panicked: {err}"),
                }
            }
        });
    }

    let held: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    lore_base::lore_spawn!(heartbeat_loop(
        channel.clone(),
        worker_id.clone(),
        held.clone(),
    ));

    for slot in 0..args.slots {
        let args = args.clone();
        let store = store.clone();
        let staging = staging.clone();
        let channel = channel.clone();
        let held = held.clone();
        let id = format!("{worker_id}/{slot}");
        lore_base::lore_spawn!(async move {
            if let Err(err) = slot_loop(args, store, staging, channel, id.clone(), held).await {
                tracing::error!("slot {id} stopped: {err:#}");
            }
        });
    }

    // Slots never return, so the signal is the only way out. Leases in flight are abandoned;
    // the server reclaims them once their heartbeats stop and hands them to another worker.
    shutdown_signal().await;
    tracing::info!("{}", store.stats.render());
    rbe_lore::shutdown_lore();
    Ok(())
}

/// Refuse to start under Lore's default allocator.
///
/// rpmalloc reserves a ~344 GB virtual heap, and `fork()` from a process holding that much
/// accountable private mapping fails with ENOMEM under `vm.overcommit_memory = 0` -- so every
/// action would fail to spawn. Lore's allocator reads this variable at its first allocation,
/// which happens before `main`, so it has to already be in the environment and cannot be set
/// here. `lore-rbe` sets it when launching a worker.
fn require_system_allocator() -> Result<()> {
    match std::env::var("LORE_ALLOCATOR") {
        Ok(value) if value.contains("system") => Ok(()),
        _ => bail!(
            "LORE_ALLOCATOR must contain `system` in this process's environment: under Lore's \
             default allocator fork() fails and every action fails to spawn"
        ),
    }
}

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

/// Renew every lease this process holds, so the server can tell a slow action from a dead worker.
///
/// One call for all slots: the rate is fixed per process rather than scaling with slot count.
async fn heartbeat_loop(channel: Channel, worker_id: String, held: Arc<Mutex<HashSet<String>>>) {
    let mut wq = WorkerQueueClient::new(channel);
    let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
    loop {
        tick.tick().await;
        let lease_ids: Vec<String> = held.lock().await.iter().cloned().collect();
        if lease_ids.is_empty() {
            continue;
        }
        match wq
            .heartbeat(HeartbeatRequest {
                worker_id: worker_id.clone(),
                lease_ids,
            })
            .await
        {
            Ok(response) => {
                // Not a warning, and not evidence of lost work: a lease completed while this
                // call was in flight is also unknown by the time the server answers. The server
                // logs the reclaim itself, with the action it re-queued, which is the side that
                // can tell the two apart.
                for lease_id in response.into_inner().unknown_lease_ids {
                    tracing::debug!(lease = %lease_id, "the server no longer knows this lease");
                }
            }
            Err(status) => tracing::warn!(%status, "heartbeat failed"),
        }
    }
}

/// One slot: lease, run, report, forever.
async fn slot_loop(
    args: Arc<Args>,
    store: Arc<LoreBlobStore>,
    staging: Arc<Staging>,
    channel: Channel,
    worker_id: String,
    held: Arc<Mutex<HashSet<String>>>,
) -> Result<()> {
    let mut wq = WorkerQueueClient::new(channel)
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);

    loop {
        let lease: TakeLeaseResponse = match wq
            .take_lease(TakeLeaseRequest {
                worker_id: worker_id.clone(),
                wait_seconds: 20,
            })
            .await
        {
            Ok(r) => r.into_inner(),
            Err(status) => {
                tracing::warn!("take_lease failed ({status}); retrying");
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };
        if !lease.have_work {
            continue;
        }

        held.lock().await.insert(lease.lease_id.clone());

        let action_digest = lease.action_digest.clone().unwrap_or_default();
        let scratch = args
            .scratch
            .join(worker_id.replace('/', "_"))
            .join(&lease.lease_id);

        let started = SystemTime::now();
        let outcome = run_lease(&args, &store, &staging, &lease, &scratch, &worker_id).await;

        let mut req = CompleteLeaseRequest {
            lease_id: lease.lease_id.clone(),
            worker_id: worker_id.clone(),
            result: None,
            failure: String::new(),
            timed_out: false,
            do_not_cache: false,
        };
        match outcome {
            Ok(Outcome {
                mut result,
                timed_out,
                do_not_cache,
            }) => {
                if let Some(md) = result.execution_metadata.as_mut() {
                    md.worker = worker_id.clone();
                    md.worker_start_timestamp = Some(prost_ts(started));
                    md.worker_completed_timestamp = Some(prost_ts(SystemTime::now()));
                }
                tracing::debug!(
                    "lease {} exit={} timed_out={timed_out} ({} outputs)",
                    lease.lease_id,
                    result.exit_code,
                    result.output_files.len() + result.output_directories.len()
                );
                req.result = Some(result);
                req.timed_out = timed_out;
                req.do_not_cache = do_not_cache;
            }
            Err(err) => {
                tracing::warn!(
                    "lease {} for action {} failed: {err:#}",
                    lease.lease_id,
                    digest::fmt(&action_digest)
                );
                req.failure = format!("{err:#}");
            }
        }

        if let Err(status) = wq.complete_lease(req).await {
            tracing::warn!(lease = %lease.lease_id, %status, "complete_lease failed");
        }
        held.lock().await.remove(&lease.lease_id);

        // After reporting, and off this slot's path: removing an input root of a few thousand
        // links is time bazel would otherwise wait on, and the next lease does not need it gone.
        if !args.keep_scratch {
            lore_base::lore_spawn!(async move {
                let _ = tokio::fs::remove_dir_all(&scratch).await;
            });
        }
    }
}

struct Outcome {
    result: ActionResult,
    timed_out: bool,
    /// Read off the `Action` here rather than in the scheduler, which would otherwise need its
    /// own fetch of every action just to learn this one bit.
    do_not_cache: bool,
}

async fn run_lease(
    args: &Args,
    store: &LoreBlobStore,
    staging: &Arc<Staging>,
    lease: &TakeLeaseResponse,
    scratch: &Path,
    worker_id: &str,
) -> Result<Outcome> {
    let action_digest = lease
        .action_digest
        .clone()
        .ok_or_else(|| anyhow!("lease has no action digest"))?;

    let action: Action = fetch_message(store, &action_digest).await?;
    let command_digest = action
        .command_digest
        .clone()
        .ok_or_else(|| anyhow!("Action has no command_digest"))?;

    let root = scratch.join("root");
    tokio::fs::create_dir_all(&root).await?;

    // The `Action` names both the `Command` and the input root, so neither waits on the other.
    let input_root_fetch_start = SystemTime::now();
    let (command, materialised) =
        tokio::join!(fetch_message::<Command>(store, &command_digest), async {
            match action.input_root_digest.as_ref() {
                Some(input_root) => materialise(store, staging, input_root, &root)
                    .await
                    .context("materialising the input root"),
                None => Ok(()),
            }
        });
    let command = command?;
    materialised?;
    let input_root_ready = SystemTime::now();
    store.stats.input_fetch_ms.fetch_add(
        input_root_ready
            .duration_since(input_root_fetch_start)
            .unwrap_or_default()
            .as_millis() as u64,
        Ordering::Relaxed,
    );
    store
        .stats
        .input_fetch_actions
        .fetch_add(1, Ordering::Relaxed);

    let work_dir = if command.working_directory.is_empty() {
        root.clone()
    } else {
        root.join(&command.working_directory)
    };
    tokio::fs::create_dir_all(&work_dir).await?;

    // Declared outputs the action is allowed to assume exist: the REAPI makes the worker
    // responsible for the parent directories, and bazel relies on it -- an output under
    // `bazel-out/k8-fastbuild/bin/...` has no reason to be in the input root.
    let declared = declared_outputs(&command);
    for path in &declared {
        if let Some(parent) = work_dir.join(path).parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
    }

    if command.arguments.is_empty() {
        bail!("Command has no arguments");
    }

    let mut env: BTreeMap<String, String> = command
        .environment_variables
        .iter()
        .map(|v| (v.name.clone(), v.value.clone()))
        .collect();
    // Bazel normally sets PATH itself; supply one when it does not, or argv[0] lookups for a
    // bare program name fail in a way that looks like a missing input.
    env.entry("PATH".into())
        .or_insert_with(|| args.default_path.clone());

    let timeout = action
        .timeout
        .as_ref()
        .map(|d| Duration::from_secs(d.seconds.max(0) as u64))
        .filter(|d| !d.is_zero())
        .unwrap_or_else(|| Duration::from_secs(args.default_timeout));

    let mut cmd = tokio::process::Command::new(&command.arguments[0]);
    cmd.args(&command.arguments[1..])
        .current_dir(&work_dir)
        .env_clear()
        .envs(&env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let exec_start = SystemTime::now();
    let child = cmd
        .spawn()
        .with_context(|| format!("spawning {:?}", command.arguments))?;

    let (exit_code, stdout, stderr, timed_out) =
        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(out) => {
                let out = out.context("waiting for the action")?;
                (
                    out.status.code().unwrap_or(-1),
                    out.stdout,
                    out.stderr,
                    false,
                )
            }
            Err(_) => (
                -1,
                Vec::new(),
                format!("action exceeded its {timeout:?} timeout\n").into_bytes(),
                true,
            ),
        };
    let exec_end = SystemTime::now();

    // Outputs are collected even for a failing action: bazel wants whatever was produced, and
    // a non-zero exit is a legitimate, reportable result rather than an error.
    let mut collected = collect_outputs(&work_dir, &declared).await?;

    let stdout_digest = stage_stream_output(&mut collected, &stdout);
    let stderr_digest = stage_stream_output(&mut collected, &stderr);

    upload(store, &collected, &scratch.join("upload"))
        .await
        .context("storing outputs")?;
    adopt_outputs(staging, &collected.on_disk).await;

    let result = ActionResult {
        output_files: collected.files,
        output_symlinks: collected.symlinks,
        output_directories: collected.dirs,
        exit_code,
        stdout_raw: if stdout_digest.is_none() {
            stdout
        } else {
            Default::default()
        },
        stdout_digest,
        stderr_raw: if stderr_digest.is_none() {
            stderr
        } else {
            Default::default()
        },
        stderr_digest,
        execution_metadata: Some(ExecutedActionMetadata {
            worker: worker_id.to_string(),
            queued_timestamp: None,
            worker_start_timestamp: Some(prost_ts(input_root_fetch_start)),
            worker_completed_timestamp: Some(prost_ts(SystemTime::now())),
            input_fetch_start_timestamp: Some(prost_ts(input_root_fetch_start)),
            input_fetch_completed_timestamp: Some(prost_ts(input_root_ready)),
            execution_start_timestamp: Some(prost_ts(exec_start)),
            execution_completed_timestamp: Some(prost_ts(exec_end)),
            output_upload_start_timestamp: Some(prost_ts(exec_end)),
            output_upload_completed_timestamp: Some(prost_ts(SystemTime::now())),
            virtual_execution_duration: None,
            auxiliary_metadata: Vec::new(),
        }),
        // Deprecated split symlink lists; modern clients read `output_symlinks`.
        ..Default::default()
    };

    Ok(Outcome {
        result,
        timed_out,
        do_not_cache: action.do_not_cache,
    })
}

/// `output_paths` is the v2.1+ form and supersedes the split lists; fall back to the union for
/// an older client. The split lists are deprecated in the API, which is why reading them is the
/// fallback and not the rule.
#[lore_macro::test_pub]
#[allow(deprecated)]
fn declared_outputs(command: &Command) -> Vec<String> {
    if !command.output_paths.is_empty() {
        return command.output_paths.clone();
    }
    command
        .output_files
        .iter()
        .chain(command.output_directories.iter())
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Input root materialisation
// ---------------------------------------------------------------------------------------------

/// Rebuild a `Directory` tree on disk, staging each distinct input once and linking it into
/// place.
///
/// Directory protos are fetched a level at a time; the content never becomes a buffer here,
/// because `get_file_many` writes it straight to disk.
///
/// It is written to a content-addressed staging directory rather than to the input root, and
/// hardlinked from there. A hermetic C++ action's input root is 376 MB over 3160 files of which
/// only 140 MB is distinct — the clang binary alone appears three times under different names —
/// so writing every path separately makes each action rewrite the whole toolchain. Linking makes
/// the second and later appearances free, both inside one action and across every action this
/// worker runs.
///
/// Staged files are read-only, so an action that writes to its own input fails on itself instead
/// of corrupting the copy every other action is linked to. The executable bit lives in the inode
/// and so is part of the staging key: content needed both ways is staged twice.
///
/// The tree is laid out on disk in one blocking task once everything is staged, not one async
/// filesystem call per entry: an input root is thousands of links, and each `tokio::fs` call is
/// a hop to the blocking pool, which every slot of this process shares.
async fn materialise(
    store: &LoreBlobStore,
    staging: &Staging,
    root_digest: &Digest,
    root: &Path,
) -> Result<()> {
    let mut level = vec![(root.to_path_buf(), root_digest.clone())];
    let mut files: Vec<(Digest, PathBuf, bool)> = Vec::new();
    // Below the root, which exists already. Breadth first, so every parent precedes its children.
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut symlinks: Vec<(PathBuf, String)> = Vec::new();

    while !level.is_empty() {
        let digests: Vec<Digest> = level.iter().map(|(_, d)| d.clone()).collect();
        let blobs = fetch_blobs(store, &digests).await?;

        let mut next = Vec::new();
        for ((path, digest), bytes) in level.iter().zip(&blobs) {
            let dir = Directory::decode(bytes.as_slice())
                .with_context(|| format!("decoding Directory {}", digest::fmt(digest)))?;

            for f in &dir.files {
                let d = f
                    .digest
                    .clone()
                    .ok_or_else(|| anyhow!("FileNode {} has no digest", f.name))?;
                files.push((d, path.join(&f.name), f.is_executable));
            }
            for s in &dir.symlinks {
                symlinks.push((path.join(&s.name), s.target.clone()));
            }
            for sub in &dir.directories {
                let d = sub
                    .digest
                    .clone()
                    .ok_or_else(|| anyhow!("DirectoryNode {} has no digest", sub.name))?;
                let sub_path = path.join(&sub.name);
                dirs.push(sub_path.clone());
                next.push((sub_path, d));
            }
        }
        level = next;
    }

    // Held until every link below is made. Until then a staged input has no link but its own,
    // and the pins are all that keep the eviction sweep from taking it.
    let _pins = staging.ensure(store, &files).await?;

    let links: Vec<(PathBuf, PathBuf, Digest)> = files
        .into_iter()
        .map(|(d, path, executable)| (staging.path_for(&d.hash, executable), path, d))
        .collect();
    rbe_lore::spawn_blocking(move || lay_out(&dirs, &symlinks, &links))
        .await
        .map_err(|e| anyhow!("laying out the input root panicked: {e}"))?
}

/// Create an input root's directories, symlinks and links to staged files. Blocking.
#[lore_macro::test_pub]
fn lay_out(
    dirs: &[PathBuf],
    symlinks: &[(PathBuf, String)],
    links: &[(PathBuf, PathBuf, Digest)],
) -> Result<()> {
    for dir in dirs {
        std::fs::create_dir(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    for (link, target) in symlinks {
        // A stale link of the same name would make this fail; the scratch dir is fresh, so the
        // only way that happens is a malformed tree.
        std::os::unix::fs::symlink(target, link)
            .with_context(|| format!("symlink {} -> {target}", link.display()))?;
    }
    for (staged, path, d) in links {
        std::fs::hard_link(staged, path).with_context(|| {
            format!(
                "linking {} into {} (staging must share a filesystem with the input root)",
                digest::fmt(d),
                path.display()
            )
        })?;
    }
    Ok(())
}

/// Stage an action's outputs, so a later action on this worker that consumes one links it
/// instead of fetching it back from the shared cache. Best effort: an output that cannot be
/// staged is fetched by whichever action needs it, as before.
async fn adopt_outputs(staging: &Arc<Staging>, outputs: &[(Digest, PathBuf)]) {
    if outputs.is_empty() {
        return;
    }
    let staging = staging.clone();
    let outputs = outputs.to_vec();
    match rbe_lore::spawn_blocking(move || staging.adopt(&outputs)).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => tracing::debug!("staging outputs failed: {err:#}"),
        Err(err) => tracing::debug!("staging outputs panicked: {err}"),
    }
}

/// One staged file: content, plus the executable bit, which lives in the inode and so cannot be
/// shared by a file needed both ways.
#[lore_macro::test_pub]
type StageKey = (String, i64, bool);

/// Content-addressed input staging, shared by every slot in the process.
///
/// One reassembled inode per key, hardlinked into every input root that names it. Slots
/// coordinate through `inflight`: without it, slots starting together each reassemble the same
/// toolchain, measured at 1.94x more bytes fetched than the distinct set.
#[lore_macro::test_pub]
struct Staging {
    dir: PathBuf,
    /// Staged paths some slot is reassembling right now.
    inflight: std::sync::Mutex<HashSet<PathBuf>>,
    /// Broadcast whenever a stage finishes or is abandoned.
    progress: tokio::sync::Notify,
    /// What eviction needs to know about each staged path. A staged file's own mtime says when
    /// it was staged rather than when it was last wanted, and linking it does not change that,
    /// so ordering by mtime evicts the toolchain every action uses first.
    uses: std::sync::Mutex<HashMap<PathBuf, Use>>,
}

struct Use {
    /// When an input root last asked for it.
    last: Instant,
    /// Input roots between asking for it and linking it.
    pins: usize,
}

impl Staging {
    #[lore_macro::test_pub]
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            inflight: Default::default(),
            progress: tokio::sync::Notify::new(),
            uses: Default::default(),
        }
    }

    #[lore_macro::test_pub]
    fn path_for(&self, hash: &str, executable: bool) -> PathBuf {
        self.dir.join(if executable {
            format!("{hash}.x")
        } else {
            hash.to_string()
        })
    }

    /// Reassemble whatever `files` needs and does not have, once per distinct content and once
    /// across the process rather than once per slot.
    ///
    /// The returned pins keep everything `files` names from eviction until they are dropped, so
    /// they have to outlive the links into the input root.
    async fn ensure(
        &self,
        store: &LoreBlobStore,
        files: &[(Digest, PathBuf, bool)],
    ) -> Result<Pins<'_>> {
        // Distinct first. An input root names the same content many times over, so testing
        // presence per path would be one stat per file where one per key does.
        let mut wanted: BTreeMap<StageKey, PathBuf> = BTreeMap::new();
        for (d, _, executable) in files {
            wanted
                .entry((d.hash.clone(), d.size_bytes, *executable))
                .or_insert_with(|| self.path_for(&d.hash, *executable));
        }
        // Pinned before the presence check, so the sweep cannot remove anything between that
        // check and its link.
        let pins = self.pin(wanted.values().cloned().collect());
        wanted.retain(|_, staged| !staged.exists());

        while !wanted.is_empty() {
            tokio::fs::create_dir_all(&self.dir).await?;
            let (claim, waiting) = self.claim(wanted);
            self.reassemble(store, &claim).await?;
            drop(claim);
            // Whatever another slot abandoned is staged here instead. Its failure is its own, and
            // waiting on it must not turn into failing every action that shares the content.
            wanted = self.wait_for(waiting).await;
        }
        Ok(pins)
    }

    /// Record that an input root wants `paths`, and keep them from eviction until it has linked
    /// them.
    #[lore_macro::test_pub]
    fn pin(&self, paths: Vec<PathBuf>) -> Pins<'_> {
        let now = Instant::now();
        let mut uses = self.uses.lock().unwrap();
        for path in &paths {
            let used = uses
                .entry(path.clone())
                .or_insert(Use { last: now, pins: 0 });
            used.last = now;
            used.pins += 1;
        }
        Pins {
            staging: self,
            paths,
        }
    }

    /// Split what is wanted into what this slot will stage and what another slot already is.
    #[lore_macro::test_pub]
    fn claim(&self, wanted: BTreeMap<StageKey, PathBuf>) -> (Claim<'_>, Vec<(StageKey, PathBuf)>) {
        let mut inflight = self.inflight.lock().unwrap();
        let (mut mine, mut waiting) = (Vec::new(), Vec::new());
        for (key, staged) in wanted {
            if inflight.insert(staged.clone()) {
                mine.push((key, staged));
            } else {
                waiting.push((key, staged));
            }
        }
        (
            Claim {
                staging: self,
                mine,
            },
            waiting,
        )
    }

    async fn reassemble(&self, store: &LoreBlobStore, claim: &Claim<'_>) -> Result<()> {
        if claim.mine.is_empty() {
            return Ok(());
        }
        // Written under a temporary name so a half-reassembled file is never linkable, and
        // removed again if anything below fails: otherwise a failing action leaves its bytes on
        // disk with nothing left holding a reference to reclaim them by.
        let temps = Temps(
            claim
                .mine
                .iter()
                .map(|_| self.dir.join(format!(".{}.tmp", uuid::Uuid::new_v4())))
                .collect(),
        );
        let entries: Vec<(String, i64, &Path)> = claim
            .mine
            .iter()
            .zip(&temps.0)
            .map(|(((hash, size, _), _), temp)| (hash.clone(), *size, temp.as_path()))
            .collect();

        let written = store.get_file_many(&entries).await?;
        land(&claim.mine, &temps.0, &written).await
    }

    /// Link files an action produced into staging under their content keys. The output's own
    /// inode becomes the staged file, made read-only first because every input root that links
    /// it shares it. Content already staged is left as it is. Blocking.
    #[lore_macro::test_pub]
    fn adopt(&self, outputs: &[(Digest, PathBuf)]) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        std::fs::create_dir_all(&self.dir)?;
        let now = Instant::now();
        for (d, path) in outputs {
            let Ok(meta) = std::fs::symlink_metadata(path) else {
                continue;
            };
            if !meta.is_file() || digest::is_empty_digest(d) {
                continue;
            }
            let executable = is_executable(&meta);
            let staged = self.path_for(&d.hash, executable);
            if staged.exists() {
                continue;
            }
            let mode = if executable { 0o555 } else { 0o444 };
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
            match std::fs::hard_link(path, &staged) {
                Ok(()) => {
                    self.uses
                        .lock()
                        .unwrap()
                        .entry(staged)
                        .or_insert(Use { last: now, pins: 0 })
                        .last = now;
                }
                // Another slot staged the same content in between; either copy will do.
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }

    /// Wait for the slots that claimed `waiting` to produce it, and hand back whatever they
    /// abandoned instead.
    #[lore_macro::test_pub]
    async fn wait_for(&self, waiting: Vec<(StageKey, PathBuf)>) -> BTreeMap<StageKey, PathBuf> {
        let mut abandoned = BTreeMap::new();
        for (key, path) in waiting {
            loop {
                // Enabled before the check: a stage completing in between would otherwise leave
                // this slot waiting on a notification that has already been sent.
                let notified = self.progress.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if path.exists() {
                    break;
                }
                if !self.inflight.lock().unwrap().contains(&path) {
                    abandoned.insert(key, path);
                    break;
                }
                notified.await;
            }
        }
        abandoned
    }

    /// Evict staged inputs once the directory exceeds `cap` bytes, least recently wanted first.
    ///
    /// Left alone however long unused: anything pinned, because an input root is about to link
    /// it; anything with more than one link, because an input root has; and temporaries, which
    /// are not staged inputs yet. Removing the staging entry of an input an action has already
    /// linked would not break that action, whose link keeps the inode, but the next action to
    /// want it would fetch it again.
    ///
    /// Blocking; run it off the async runtime.
    #[lore_macro::test_pub]
    fn evict(&self, cap: u64) -> Result<()> {
        use std::os::unix::fs::MetadataExt;

        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Ok(()); // nothing staged yet
        };
        let mut candidates: Vec<(Option<Instant>, u64, PathBuf)> = Vec::new();
        let mut total = 0u64;
        for entry in entries {
            let entry = entry?;
            // Gone since the listing: renamed into place, or removed.
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            total += meta.len();
            let temporary = entry.file_name().as_encoded_bytes().starts_with(b".");
            if !temporary && meta.nlink() == 1 {
                candidates.push((None, meta.len(), entry.path()));
            }
        }
        if total <= cap {
            return Ok(());
        }

        {
            let uses = self.uses.lock().unwrap();
            for (last, _, path) in &mut candidates {
                *last = uses.get(path).map(|used| used.last);
            }
        }
        // Never asked for since it was staged sorts first.
        candidates.sort_by_key(|(last, ..)| *last);

        let before = total;
        for (_, size, path) in candidates {
            if total <= cap {
                break;
            }
            // Decided and done under the lock `pin` takes, so no input root can pin this between
            // the check and the removal.
            let mut uses = self.uses.lock().unwrap();
            if uses.get(&path).is_some_and(|used| used.pins > 0) {
                continue;
            }
            // Linked since the listing.
            if std::fs::symlink_metadata(&path).map_or(true, |meta| meta.nlink() > 1) {
                continue;
            }
            if std::fs::remove_file(&path).is_ok() {
                uses.remove(&path);
                total -= size;
            }
        }
        tracing::info!(
            freed_mib = (before - total) / (1024 * 1024),
            held_mib = total / (1024 * 1024),
            "evicted staged inputs"
        );
        Ok(())
    }
}

/// Move every reassembled input into place, and only then report the first one that could not
/// be.
///
/// Inputs that arrived are staged even when another did not: other slots may be waiting on any of
/// them, and an input missing from the CAS is the failure of the action that named it, not of
/// every action that shares its toolchain.
#[lore_macro::test_pub]
async fn land(mine: &[(StageKey, PathBuf)], temps: &[PathBuf], written: &[bool]) -> Result<()> {
    let mut first_error = None;
    for (((hash, size, executable), staged), (temp, written)) in
        mine.iter().zip(temps.iter().zip(written))
    {
        let outcome = if *written {
            place(temp, staged, *executable).await
        } else {
            Err(anyhow!(
                "input {} is not in the CAS",
                digest::fmt(&Digest {
                    hash: hash.clone(),
                    size_bytes: *size,
                })
            ))
        };
        if let Err(err) = outcome {
            first_error.get_or_insert(err);
        }
    }
    first_error.map_or(Ok(()), Err)
}

async fn place(temp: &Path, staged: &Path, executable: bool) -> Result<()> {
    set_read_only(temp, executable).await?;
    tokio::fs::rename(temp, staged)
        .await
        .with_context(|| format!("staging {}", staged.display()))
}

/// Unpins on drop; see `Staging::ensure`.
#[lore_macro::test_pub]
struct Pins<'a> {
    staging: &'a Staging,
    paths: Vec<PathBuf>,
}

impl Drop for Pins<'_> {
    fn drop(&mut self) {
        let mut uses = self.staging.uses.lock().unwrap();
        for path in &self.paths {
            if let Some(used) = uses.get_mut(path) {
                used.pins -= 1;
            }
        }
    }
}

/// Releases this slot's claims however staging ended, so a failure cannot leave another slot
/// waiting for a file nobody is going to produce.
#[lore_macro::test_pub]
struct Claim<'a> {
    staging: &'a Staging,
    mine: Vec<(StageKey, PathBuf)>,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        let mut inflight = self.staging.inflight.lock().unwrap();
        for (_, staged) in &self.mine {
            inflight.remove(staged);
        }
        drop(inflight);
        self.staging.progress.notify_waiters();
    }
}

/// Removes staging temporaries that did not reach their final name. A successful rename leaves
/// nothing behind, so removing every path unconditionally costs one failed unlink each and needs
/// no bookkeeping about which of them succeeded.
#[lore_macro::test_pub]
struct Temps(Vec<PathBuf>);

impl Drop for Temps {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

async fn set_read_only(path: &Path, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if executable { 0o555 } else { 0o444 };
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .await
        .with_context(|| format!("setting mode {mode:o} on {}", path.display()))
}

// ---------------------------------------------------------------------------------------------
// Output collection
// ---------------------------------------------------------------------------------------------

#[lore_macro::test_pub]
#[derive(Default)]
struct Collected {
    files: Vec<OutputFile>,
    dirs: Vec<OutputDirectory>,
    symlinks: Vec<OutputSymlink>,
    /// Output content, still where the action wrote it. Stored from there rather than read in.
    on_disk: Vec<(Digest, PathBuf)>,
    /// `Tree` and `Directory` messages describing the outputs, plus any stdout or stderr too
    /// large to inline. Built here, so unlike `on_disk` these are the only outputs that are
    /// resident.
    messages: HashMap<Digest, Vec<u8>>,
}

async fn collect_outputs(work_dir: &Path, declared: &[String]) -> Result<Collected> {
    let mut out = Collected::default();

    for rel in declared {
        let abs = work_dir.join(rel);
        let meta = match tokio::fs::symlink_metadata(&abs).await {
            Ok(m) => m,
            // A declared output the action chose not to produce is not an error here; bazel
            // decides whether it needed it.
            Err(_) => continue,
        };

        if meta.is_symlink() {
            let target = tokio::fs::read_link(&abs).await?;
            out.symlinks.push(OutputSymlink {
                path: rel.clone(),
                target: target.to_string_lossy().into_owned(),
                node_properties: None,
            });
        } else if meta.is_file() {
            let d = digest::of_file(&abs).await?;
            out.files.push(OutputFile {
                path: rel.clone(),
                digest: Some(d.clone()),
                is_executable: is_executable(&meta),
                contents: Default::default(),
                node_properties: None,
            });
            out.on_disk.push((d, abs));
        } else if meta.is_dir() {
            let (root, children) = build_tree(&abs, &mut out).await?;
            let tree = Tree {
                root: Some(root),
                children,
            };
            let bytes = tree.encode_to_vec();
            let d = digest::of(&bytes);
            out.dirs.push(OutputDirectory {
                path: rel.clone(),
                tree_digest: Some(d.clone()),
                is_topologically_sorted: false,
                root_directory_digest: None,
            });
            out.messages.insert(d, bytes);
        }
    }

    Ok(out)
}

/// Walk an output directory into `(root Directory, all descendant Directories)`, recording every
/// file's content for upload. Entry lists are sorted by name, which the REAPI requires for a
/// `Directory` digest to be canonical.
async fn build_tree(dir: &Path, out: &mut Collected) -> Result<(Directory, Vec<Directory>)> {
    let mut children = Vec::new();
    let root = build_dir(dir, out, &mut children).await?;
    Ok((root, children))
}

async fn build_dir(
    dir: &Path,
    out: &mut Collected,
    children: &mut Vec<Directory>,
) -> Result<Directory> {
    let mut files: Vec<FileNode> = Vec::new();
    let mut dirs: Vec<DirectoryNode> = Vec::new();
    let mut symlinks: Vec<SymlinkNode> = Vec::new();

    let mut entries = tokio::fs::read_dir(dir).await?;
    // Recursion in an async fn needs an explicit box; collect the subdirectories first and
    // recurse below so the future stays a normal loop.
    let mut subdirs: Vec<(String, PathBuf)> = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let meta = tokio::fs::symlink_metadata(&path).await?;
        if meta.is_symlink() {
            let target = tokio::fs::read_link(&path).await?;
            symlinks.push(SymlinkNode {
                name,
                target: target.to_string_lossy().into_owned(),
                node_properties: None,
            });
        } else if meta.is_file() {
            let d = digest::of_file(&path).await?;
            files.push(FileNode {
                name,
                digest: Some(d.clone()),
                is_executable: is_executable(&meta),
                node_properties: None,
            });
            out.on_disk.push((d, path));
        } else if meta.is_dir() {
            subdirs.push((name, path));
        }
    }

    for (name, path) in subdirs {
        let child = Box::pin(build_dir(&path, out, children)).await?;
        let bytes = child.encode_to_vec();
        let d = digest::of(&bytes);
        out.messages.insert(d.clone(), bytes);
        children.push(child);
        dirs.push(DirectoryNode {
            name,
            digest: Some(d),
        });
    }

    files.sort_by(|a, b| a.name.cmp(&b.name));
    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    symlinks.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Directory {
        files,
        directories: dirs,
        symlinks,
        node_properties: None,
    })
}

/// Inline stdout/stderr when it is small, otherwise stage it as a CAS blob and return its
/// digest. Most actions produce nothing, so the inline path is the common one.
#[lore_macro::test_pub]
fn stage_stream_output(collected: &mut Collected, bytes: &[u8]) -> Option<Digest> {
    if bytes.len() <= INLINE_OUTPUT_LIMIT {
        return None;
    }
    let d = digest::of(bytes);
    collected.messages.insert(d.clone(), bytes.to_vec());
    Some(d)
}

fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

// ---------------------------------------------------------------------------------------------
// Cache transfer
// ---------------------------------------------------------------------------------------------

async fn fetch_message<M: prost::Message + Default>(
    store: &LoreBlobStore,
    digest: &Digest,
) -> Result<M> {
    let bytes = fetch_blobs(store, std::slice::from_ref(digest)).await?;
    M::decode(bytes[0].as_slice()).with_context(|| format!("decoding {}", digest::fmt(digest)))
}

/// Fetch the blobs this process has to decode: `Action`, `Command` and `Directory` messages,
/// all small. File content does not come through here -- see `materialise`.
///
/// A miss is an error. Every one of these was named by something already fetched, so its absence
/// means the input root is not fully in the cache and the action cannot run.
async fn fetch_blobs(store: &LoreBlobStore, digests: &[Digest]) -> Result<Vec<Vec<u8>>> {
    let keys: Vec<(String, i64)> = digests.iter().map(digest::key_of).collect();
    store
        .get_many(Ns::Cas, &keys)
        .await?
        .into_iter()
        .zip(digests)
        .map(|(blob, d)| blob.ok_or_else(|| anyhow!("blob {} is not in the CAS", digest::fmt(d))))
        .collect()
}

/// Store everything the action produced.
///
/// `exists_many` first, because an output that is already there -- an identical compile result
/// from another machine, an empty stderr -- is common and re-storing it is pure waste.
///
/// Content the action wrote is stored from where it lies. Messages built in this process become
/// files under `staging` first: `put_file_resolved` is the write that survives being delegated
/// to a Lore service over IPC, so routing everything through it keeps that a deployment choice
/// rather than a rewrite.
async fn upload(store: &LoreBlobStore, collected: &Collected, staging: &Path) -> Result<()> {
    let mut digests: Vec<&Digest> = collected
        .on_disk
        .iter()
        .map(|(d, _)| d)
        .chain(collected.messages.keys())
        .filter(|d| !digest::is_empty_digest(d))
        .collect();
    digests.sort_unstable_by(|a, b| (&a.hash, a.size_bytes).cmp(&(&b.hash, b.size_bytes)));
    digests.dedup();
    if digests.is_empty() {
        return Ok(());
    }

    let keys: Vec<(String, i64)> = digests.iter().map(|d| digest::key_of(d)).collect();
    let missing: HashSet<&Digest> = store
        .exists_many(Ns::Cas, &keys)
        .await?
        .iter()
        .zip(&digests)
        .filter(|(present, _)| !**present)
        .map(|(_, d)| *d)
        .collect();
    if missing.is_empty() {
        return Ok(());
    }

    let mut staged: Vec<(Digest, PathBuf)> = Vec::new();
    for (d, bytes) in &collected.messages {
        if !missing.contains(d) {
            continue;
        }
        if staged.is_empty() {
            tokio::fs::create_dir_all(staging).await?;
        }
        let path = staging.join(&d.hash);
        tokio::fs::write(&path, bytes)
            .await
            .with_context(|| format!("staging {} for upload", digest::fmt(d)))?;
        staged.push((d.clone(), path));
    }

    let entries: Vec<(String, i64, &Path)> = collected
        .on_disk
        .iter()
        .filter(|(d, _)| missing.contains(d))
        .chain(staged.iter())
        .map(|(d, path)| (d.hash.clone(), d.size_bytes, path.as_path()))
        .collect();
    store.put_file_many(&entries).await
}

// ---------------------------------------------------------------------------------------------

fn prost_ts(t: SystemTime) -> prost_types::Timestamp {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    prost_types::Timestamp {
        seconds: d.as_secs() as i64,
        nanos: d.subsec_nanos() as i32,
    }
}
