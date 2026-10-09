// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore-rbe-index` — make content that is already in Lore resolvable under the CAS keys bazel
//! asks for, so bazel finds it present and never uploads it.
//!
//! ```sh
//! lore-rbe-index stamp   <workspace>                          # between `lore stage` and `lore commit`
//! lore-rbe-index publish <checkout> --lore-server lore://host:41337
//! lore-rbe-index xattr   <checkout> [--clear]
//! lore-rbe-index seed    <dir>... --lore-server lore://host:41337
//! ```
//!
//! `stamp` records each file's SHA-256 as Lore file metadata, once, at commit. `publish` then
//! publishes every file of a checkout under `cas:<sha256>:<size>` in the durable partition,
//! through the checkout's own Lore store: the server already holds the content under the
//! repository's partition, so Lore copies it into ours instead of uploading it. `xattr` writes the
//! same SHA-256 where bazel can read it instead of hashing (`--unix_digest_hash_attribute_name`).
//!
//! `seed` is the older path for content that is not in a Lore repository, such as a toolchain
//! bazel downloaded: every regular file below the given directories is hashed and uploaded into
//! the same durable partition.
//!
//! See `SOURCES.md`.

use std::collections::HashSet;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use clap::Subcommand;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use rbe_lore::LoreBlobStore;
use rbe_lore::digest;
use rbe_lore::workspace::Recorded;
use rbe_lore::workspace::checkout_files;
use rbe_lore::workspace::{self};

/// Concurrent per-file Lore metadata reads and file hashes.
const PARALLEL: usize = 16;

#[derive(Parser, Debug)]
#[command(
    name = "lore-rbe-index",
    about = "Publish content that is already in Lore under the CAS keys bazel asks for"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Record the SHA-256 of every staged, still existing file as Lore file metadata (`sha256`,
    /// with the size it was taken from). Run after `lore stage`, before `lore commit`, on every
    /// commit: Lore keeps a file's metadata when its content changes, so a changed file that is not
    /// re-stamped carries a stale hash forward.
    Stamp {
        /// The workspace whose staged files to stamp.
        workspace: PathBuf,
    },
    /// Publish every file of a checkout under its CAS key in the durable partition, through the
    /// checkout's own Lore store, so content the server already holds is copied, not uploaded.
    Publish {
        /// A checkout that made the push itself, or was cloned with `--cache`.
        checkout: PathBuf,
        /// The loreserver that holds the repository and the build cache.
        #[arg(long)]
        lore_server: String,
    },
    /// Write each file's SHA-256 as an extended attribute for bazel's
    /// `--unix_digest_hash_attribute_name`, or remove it with `--clear`.
    Xattr {
        checkout: PathBuf,
        /// The attribute. Its value is the raw 32-byte digest.
        #[arg(long, default_value = "user.sha256")]
        name: String,
        #[arg(long)]
        clear: bool,
    },
    /// Upload every regular file below the given directories into the durable partition, for
    /// content that is not in a Lore repository.
    Seed {
        /// Directories to publish. Every regular file below each is stored under its SHA-256.
        #[arg(required = true)]
        roots: Vec<PathBuf>,
        /// Local-tier Lore repository. Created on first use.
        #[arg(long, default_value = "seed-repo")]
        lore_repo: String,
        /// Upstream loreserver to publish to.
        #[arg(long)]
        lore_server: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let result = match Cli::parse().cmd {
        Cmd::Stamp { workspace } => stamp(&workspace).await,
        Cmd::Publish {
            checkout,
            lore_server,
        } => publish(&checkout, &lore_server).await,
        Cmd::Xattr {
            checkout,
            name,
            clear,
        } => xattr(&checkout, &name, clear).await,
        Cmd::Seed {
            roots,
            lore_repo,
            lore_server,
        } => seed(&roots, &lore_repo, &lore_server).await,
    };
    rbe_lore::shutdown_lore();
    result
}

fn absolute(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))
}

async fn stamp(workspace: &Path) -> Result<()> {
    let started = Instant::now();
    let workspace = absolute(workspace)?;
    let files = workspace::staged_files(&workspace).await?;
    let entries: Vec<(PathBuf, String, u64)> = stream::iter(files)
        .map(|path| async move {
            let d = digest::of_file(&path)
                .await
                .with_context(|| format!("hashing {}", path.display()))?;
            let size = d.size_bytes.max(0) as u64;
            Ok::<_, anyhow::Error>((path, d.hash, size))
        })
        .buffered(PARALLEL)
        .try_collect()
        .await?;
    workspace::set_sha256(&workspace, &entries).await?;
    println!(
        "stamp: files={} seconds={:.2}",
        entries.len(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// A file of a checkout with the digest it is published under, and what its metadata said.
struct Indexed {
    path: PathBuf,
    hash: String,
    size: i64,
    /// `Valid` when `hash` came from the metadata; otherwise `hash` was computed here.
    recorded: Recorded,
}

impl Indexed {
    fn hash_from_metadata(&self) -> bool {
        matches!(self.recorded, Recorded::Valid(_))
    }
}

/// Each file's digest, from its `sha256` metadata where that is present and matches the file's
/// size. Any other file is hashed locally instead: slower, never wrong, and counted so the caller
/// can see it happened.
async fn index_checkout(checkout: &Path) -> Result<Vec<Indexed>> {
    let files = checkout_files(checkout)?;
    stream::iter(files)
        .map(|path| async move {
            let size = std::fs::metadata(&path)
                .with_context(|| format!("reading {}", path.display()))?
                .len();
            let recorded = workspace::get_sha256(checkout, &path, size).await?;
            let hash = match &recorded {
                Recorded::Valid(hash) => hash.clone(),
                Recorded::Missing | Recorded::Stale => {
                    digest::of_file(&path)
                        .await
                        .with_context(|| format!("hashing {}", path.display()))?
                        .hash
                }
            };
            Ok::<_, anyhow::Error>(Indexed {
                path,
                hash,
                size: size as i64,
                recorded,
            })
        })
        .buffered(PARALLEL)
        .try_collect()
        .await
}

/// How many files had no `sha256`, and how many had one recorded for other content.
fn count_unrecorded(indexed: &[Indexed]) -> (usize, usize) {
    let missing = indexed
        .iter()
        .filter(|f| f.recorded == Recorded::Missing)
        .count();
    let stale = indexed
        .iter()
        .filter(|f| f.recorded == Recorded::Stale)
        .count();
    (missing, stale)
}

async fn publish(checkout: &Path, lore_server: &str) -> Result<()> {
    let started = Instant::now();
    let checkout = absolute(checkout)?;
    let indexed = index_checkout(&checkout).await?;
    let (missing, stale) = count_unrecorded(&indexed);
    if missing + stale > 0 {
        tracing::warn!(
            "hashed here: {missing} files with no {key}, {stale} whose {key} is for other content",
            key = workspace::SHA256_KEY
        );
    }

    // The same content at two paths is one key. The empty blob is never stored.
    let mut seen: HashSet<&str> = HashSet::new();
    let batch: Vec<(String, i64, &Path)> = indexed
        .iter()
        .filter(|f| !digest::is_empty_blob(&f.hash, f.size) && seen.insert(f.hash.as_str()))
        .map(|f| (f.hash.clone(), f.size, f.path.as_path()))
        .collect();
    let bytes: u64 = batch.iter().map(|(_, size, _)| (*size).max(0) as u64).sum();

    let store = LoreBlobStore::open_checkout(&checkout.to_string_lossy(), lore_server)
        .await
        .context("opening the checkout's Lore store")?;
    store
        .seed_file_many(&batch)
        .await
        .context("publishing to the durable partition")?;

    println!(
        "publish: files={} distinct={} bytes={bytes} missing_sha256={missing} stale_sha256={stale} \
         seconds={:.2}",
        indexed.len(),
        batch.len(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

async fn xattr(checkout: &Path, name: &str, clear: bool) -> Result<()> {
    let started = Instant::now();
    let checkout = absolute(checkout)?;
    let c_name = CString::new(name).context("attribute name")?;
    if clear {
        let files = checkout_files(&checkout)?;
        for path in &files {
            remove_xattr(path, &c_name)?;
        }
        println!(
            "xattr: cleared={} seconds={:.2}",
            files.len(),
            started.elapsed().as_secs_f64()
        );
        return Ok(());
    }

    let indexed = index_checkout(&checkout).await?;
    let (missing, stale) = count_unrecorded(&indexed);
    for file in &indexed {
        if !file.hash_from_metadata() {
            // Bazel uses the attribute without checking it against the content, so it is only
            // ever written from the revision's own record for content of this size, never from a
            // hash of whatever is on disk now. Any other file gets none, and bazel hashes it.
            remove_xattr(&file.path, &c_name)?;
            continue;
        }
        let raw = hex::decode(&file.hash).context("decoding sha256")?;
        set_xattr(&file.path, &c_name, &raw)?;
    }
    println!(
        "xattr: written={} missing_sha256={missing} stale_sha256={stale} seconds={:.2}",
        indexed.len() - missing - stale,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).with_context(|| format!("path {}", path.display()))
}

fn set_xattr(path: &Path, name: &CString, value: &[u8]) -> Result<()> {
    let p = c_path(path)?;
    // SAFETY: both strings are NUL-terminated and outlive the call; `value` is valid for its
    // length.
    let rc = unsafe {
        libc::setxattr(
            p.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if rc != 0 {
        bail!(
            "setxattr {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

fn remove_xattr(path: &Path, name: &CString) -> Result<()> {
    let p = c_path(path)?;
    // SAFETY: both strings are NUL-terminated and outlive the call.
    let rc = unsafe { libc::removexattr(p.as_ptr(), name.as_ptr()) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ENODATA) {
            bail!("removexattr {}: {err}", path.display());
        }
    }
    Ok(())
}

async fn seed(roots: &[PathBuf], lore_repo: &str, lore_server: &str) -> Result<()> {
    // GC off: the whole point of this partition is that nothing evicts a compiler.
    let store = LoreBlobStore::open(lore_repo, 0, Some(lore_server))
        .await
        .context("opening the Lore store")?;
    tracing::info!("lore store: {}", store.location());

    let mut files = Vec::new();
    for root in roots {
        if !root.is_dir() {
            bail!("not a directory: {}", root.display());
        }
        collect(root, &mut files).with_context(|| format!("walking {}", root.display()))?;
    }
    tracing::info!("found {} files", files.len());

    // The same content appears at many paths in a toolchain -- a header included from two repos,
    // a library symlinked into place. Publishing it once per path would be that many redundant
    // uploads of bytes the store already deduplicates.
    let mut seen: HashSet<String> = HashSet::new();
    let mut entries: Vec<(String, i64, PathBuf)> = Vec::new();
    for path in files {
        let d = digest::of_file(&path).await?;
        if digest::is_empty_digest(&d) || !seen.insert(d.hash.clone()) {
            continue;
        }
        entries.push((d.hash, d.size_bytes, path));
    }

    let bytes: u64 = entries
        .iter()
        .map(|(_, size, _)| (*size).max(0) as u64)
        .sum();
    tracing::info!(
        "publishing {} distinct blobs, {:.1} MiB",
        entries.len(),
        bytes as f64 / (1024.0 * 1024.0)
    );

    let batch: Vec<(String, i64, &Path)> = entries
        .iter()
        .map(|(hash, size, path)| (hash.clone(), *size, path.as_path()))
        .collect();
    store
        .seed_file_many(&batch)
        .await
        .context("publishing to the durable partition")?;

    tracing::info!("{}", store.stats.render());
    Ok(())
}

/// Every regular file below `dir`, following symlinks to files but never into directories.
///
/// A toolchain repository is full of symlinks pointing back into the output base; following the
/// directory ones would walk the same tree repeatedly and can cycle. A link to a file is worth
/// following, because its content is what an action reads.
fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let link = std::fs::symlink_metadata(&path)?;
        if link.is_dir() {
            collect(&path, out)?;
        } else if link.is_file() || std::fs::metadata(&path).is_ok_and(|target| target.is_file()) {
            out.push(path);
        }
    }
    Ok(())
}
