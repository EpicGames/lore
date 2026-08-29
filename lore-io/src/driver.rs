// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Driver construction and backend dispatch.
//!
//! The backend surface is the set of operations [`DriverInner`]'s match arms call: `open`,
//! `read_at`, `read_exact_at`, `read_vectored_at`, `write_at`, `write_all_at`,
//! `write_vectored_at`, `open_read_head`, `read_file_bytes`, `write_file_bytes`,
//! `write_file_segments`, `write_file_segments_atomic`, `sync`, `metadata`, `file_metadata`,
//! `holds_name_exactly`, `set_len`, `rename`, `copy`, `remove_file`, `create_dir_all`, `remove_dir`
//! and `remove_dir_all`. A backend implements all twenty-three as inherent methods on its own
//! type.
//!
//! Every operation dispatches, including the metadata ones a completion backend will keep on the
//! syscall pool anyway — a ring-submitted `statx` is punted to a kernel worker making the same
//! blocking call, so it buys no parallelism. Routing them anyway is what makes a backend able to
//! override any operation and a driver instance self-contained, rather than partly bypassed. It
//! also leaves [`crate::pool`] reachable only from the backends.
//!
//! There is no trait, and none is needed to make that complete: adding a variant to
//! [`DriverInner`] makes every match here non-exhaustive, and filling those arms in requires
//! each operation to exist with a compatible signature, so a backend cannot be half-wired. What
//! a trait would add over that is a single place to read the surface and a bar on signatures
//! drifting between backends — worth revisiting when there is a second one to compare against.
//!
//! Dispatch is static. The enum holds backend values rather than trait objects, so each arm
//! binds a concrete type and the call inlines as any inherent method would; no vtable exists on
//! this path.
//!
//! What the arms cannot check is behaviour, which is what `tests/conformance.rs` is for: adding a
//! backend to its `drivers()` list runs every case against it.
use std::fs::File;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;

use bytes::Bytes;

use crate::buffer::StableBuf;
use crate::buffer::StableBufList;
use crate::buffer::StableBufListMut;
use crate::file::IoFile;
use crate::file::OpenOptions;
#[cfg(target_family = "windows")]
use crate::iocp::IocpDriver;
use crate::psync::PsyncDriver;
#[cfg(target_os = "linux")]
use crate::uring::UringDriver;

/// Largest file the whole-file operations accept.
///
/// [`IoDriver::read_file_bytes`] and [`IoDriver::write_file_bytes`] exist to keep a scan over
/// many small files at one dispatch each. Both hold a pool thread for the whole transfer and hold
/// the whole file resident, so reaching for them with a large file would occupy one of at most
/// `min(2 × cores, 16)` threads for its duration. A caller with a large file wants [`open`] plus
/// [`read_exact_at`] or [`write_all_at`], which read and write a bounded length at a time.
///
/// [`open`]: IoDriver::open
/// [`read_exact_at`]: crate::IoFile::read_exact_at
/// [`write_all_at`]: crate::IoFile::write_all_at
pub const WHOLE_FILE_LIMIT: usize = 8 * 1024 * 1024;

pub(crate) fn check_whole_file_len(len: usize) -> std::io::Result<()> {
    if len > WHOLE_FILE_LIMIT {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{len} bytes exceeds the {WHOLE_FILE_LIMIT} byte whole-file limit; \
                 open the file and use read_exact_at or write_all_at"
            ),
        ));
    }
    Ok(())
}

/// Backend selection for an [`IoDriver`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    /// Probe for the best available backend.
    Auto,
    /// Positional syscalls on the bounded syscall pool.
    Psync,
    /// Completion-based operations on sharded `io_uring` instances.
    #[cfg(target_os = "linux")]
    Uring,
    /// Windows only. Overlapped operations on an I/O completion port.
    #[cfg(target_family = "windows")]
    Iocp,
}

pub(crate) enum DriverInner {
    Psync(PsyncDriver),
    #[cfg(target_os = "linux")]
    Uring(UringDriver),
    #[cfg(target_family = "windows")]
    Iocp(IocpDriver),
}

/// The backend names [`backend_kind_from_value`] accepts, for the error it reports when a value
/// is not one of them.
#[cfg(target_os = "linux")]
const SUPPORTED_BACKENDS: &str = "auto, psync, uring";
#[cfg(target_family = "windows")]
const SUPPORTED_BACKENDS: &str = "auto, psync, iocp";
#[cfg(not(any(target_os = "linux", target_family = "windows")))]
const SUPPORTED_BACKENDS: &str = "auto, psync";

/// The backend [`BackendKind::Auto`] hands out: `psync`, on every platform.
///
/// The completion backends take most of the synthetic phases, and the earlier rule preferred them
/// wherever one could be created. What overrode that is the smoke suite, which drives the real call
/// sites end to end: it measured a regression under the completion backends and recovered under
/// `psync`. A whole-workload result outranks a per-operation one, and `psync` is also the semantic
/// reference the other two are conformance-tested against, so it is what a caller expressing no
/// preference should get.
///
/// This is a default, not a verdict on the mechanism. Where the end-to-end cost sits is still
/// unmeasured — the synthetic phases and the suite disagree, and neither the ring's submission path
/// nor the completion port's has been profiled under the suite's access pattern.
/// [`BackendKind::Uring`], [`BackendKind::Iocp`] and `LORE_IO_BACKEND` select them explicitly, which
/// is how that investigation gets its A/B without a code change. `lore-io/BENCHMARKS.md` has the
/// per-case numbers.
fn probe() -> DriverInner {
    DriverInner::Psync(PsyncDriver)
}

/// Parses a `LORE_IO_BACKEND` value. Separate from reading the variable so the accepted set and
/// the error are testable without a process-global environment.
fn backend_kind_from_value(value: &str) -> std::io::Result<BackendKind> {
    match value.to_ascii_lowercase().as_str() {
        "" | "auto" => Ok(BackendKind::Auto),
        "psync" => Ok(BackendKind::Psync),
        #[cfg(target_os = "linux")]
        "uring" => Ok(BackendKind::Uring),
        #[cfg(target_family = "windows")]
        "iocp" => Ok(BackendKind::Iocp),
        other => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unsupported LORE_IO_BACKEND \"{other}\" (supported: {SUPPORTED_BACKENDS})"),
        )),
    }
}

/// A file I/O driver instance dispatching to one backend.
///
/// Cloning is cheap and clones share the backend. Most code uses
/// [`IoDriver::global`]; tests and benchmarks construct instances per
/// backend.
#[derive(Clone)]
pub struct IoDriver {
    inner: Arc<DriverInner>,
}

impl std::fmt::Debug for IoDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IoDriver")
            .field("backend", &self.backend_name())
            .finish()
    }
}

impl IoDriver {
    pub fn new(kind: BackendKind) -> std::io::Result<IoDriver> {
        let inner = match kind {
            BackendKind::Auto => probe(),
            BackendKind::Psync => DriverInner::Psync(PsyncDriver),
            #[cfg(target_os = "linux")]
            BackendKind::Uring => DriverInner::Uring(UringDriver::new()?),
            #[cfg(target_family = "windows")]
            BackendKind::Iocp => DriverInner::Iocp(IocpDriver::new()?),
        };
        Ok(IoDriver {
            inner: Arc::new(inner),
        })
    }

    /// Constructs a driver honoring the `LORE_IO_BACKEND` environment
    /// variable, failing on an unrecognised value.
    pub fn from_env() -> std::io::Result<IoDriver> {
        let kind = match std::env::var("LORE_IO_BACKEND") {
            Ok(value) => backend_kind_from_value(&value)?,
            Err(_) => BackendKind::Auto,
        };
        IoDriver::new(kind)
    }

    /// The process-wide driver, selected once from the environment.
    ///
    /// An unrecognised `LORE_IO_BACKEND` reports the problem and falls back to the probed
    /// backend rather than failing. The variable exists for diagnosis and rollback, and this
    /// crate runs inside host applications: a typo in it should not be able to take the host
    /// down on its first file read. Callers that would rather refuse to run misconfigured use
    /// [`from_env`](Self::from_env), which returns the error.
    pub fn global() -> &'static IoDriver {
        static GLOBAL: OnceLock<IoDriver> = OnceLock::new();
        GLOBAL.get_or_init(|| {
            IoDriver::from_env().unwrap_or_else(|error| {
                eprintln!("lore-io: {error}; using the probed backend instead");
                IoDriver::new(BackendKind::Auto).expect("the probed backend is always available")
            })
        })
    }

    /// Ring statistics when this driver is on the `uring` backend, `None` otherwise.
    #[cfg(target_os = "linux")]
    pub fn uring_stats(&self) -> Option<crate::uring::UringStats> {
        match &*self.inner {
            DriverInner::Psync(_) => None,
            DriverInner::Uring(driver) => Some(driver.stats()),
        }
    }

    /// The name of the selected backend, for logs and benchmarks.
    /// Completion-port statistics when this driver is on the `iocp` backend, `None` otherwise.
    #[cfg(target_family = "windows")]
    pub fn iocp_stats(&self) -> Option<crate::iocp::IocpStats> {
        match &*self.inner {
            DriverInner::Psync(_) => None,
            DriverInner::Iocp(driver) => Some(driver.stats()),
        }
    }

    pub fn backend_name(&self) -> &'static str {
        match &*self.inner {
            DriverInner::Psync(_) => "psync",
            #[cfg(target_os = "linux")]
            DriverInner::Uring(_) => "uring",
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(_) => "iocp",
        }
    }

    pub async fn open(
        &self,
        path: impl AsRef<Path>,
        options: &OpenOptions,
    ) -> std::io::Result<IoFile> {
        let std_options = options.to_std();
        let path = path.as_ref().to_path_buf();
        let file = match &*self.inner {
            DriverInner::Psync(driver) => driver.open(std_options, path).await?,
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => driver.open(std_options, path).await?,
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => driver.open(std_options, path).await?,
        };
        Ok(IoFile::new(self.clone(), Arc::new(file)))
    }

    /// Opens a file and reads its first `head_len` bytes (or the whole
    /// file when smaller) in a single backend dispatch — open, stat,
    /// read. Returns the open file, its metadata, and the head bytes.
    ///
    /// For header-then-body access patterns: when the file fits in the
    /// head, the caller is done after one dispatch; otherwise the
    /// returned handle serves the follow-up positional reads.
    pub async fn open_read_head(
        &self,
        path: impl AsRef<Path>,
        options: &OpenOptions,
        head_len: usize,
    ) -> std::io::Result<(IoFile, std::fs::Metadata, Bytes)> {
        let std_options = options.to_std();
        let path = path.as_ref().to_path_buf();
        let (file, metadata, head) = match &*self.inner {
            DriverInner::Psync(driver) => {
                driver.open_read_head(std_options, path, head_len).await?
            }
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => {
                driver.open_read_head(std_options, path, head_len).await?
            }
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => driver.open_read_head(std_options, path, head_len).await?,
        };
        Ok((IoFile::new(self.clone(), Arc::new(file)), metadata, head))
    }

    /// The storage layer's atomic durable whole-file write as a single
    /// backend dispatch: creates `temporary_path` (per `options`), writes
    /// the gathered contents of all segments, syncs file data and
    /// metadata, renames over `final_path`, and best-effort syncs the
    /// parent directory.
    ///
    /// On error the temporary file may be left behind; callers own its
    /// cleanup.
    ///
    /// The segments are consumed rather than handed back, and dropped as soon as the kernel has
    /// the bytes — before the sync and the rename, which touch the page cache and the directory
    /// rather than the caller's memory. A caller holding a lock to keep the segments stable
    /// therefore holds it across the gather alone. The drop happens on the thread running the
    /// operation, so anything it releases is released there.
    pub async fn write_file_segments_atomic<B: StableBufList>(
        &self,
        temporary_path: impl AsRef<Path>,
        final_path: impl AsRef<Path>,
        options: &OpenOptions,
        buffers: B,
    ) -> std::io::Result<()> {
        let std_options = options.to_std();
        let temporary_path = temporary_path.as_ref().to_path_buf();
        let final_path = final_path.as_ref().to_path_buf();
        match &*self.inner {
            DriverInner::Psync(driver) => {
                driver
                    .write_file_segments_atomic(std_options, temporary_path, final_path, buffers)
                    .await
            }
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => {
                driver
                    .write_file_segments_atomic(std_options, temporary_path, final_path, buffers)
                    .await
            }
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => {
                driver
                    .write_file_segments_atomic(std_options, temporary_path, final_path, buffers)
                    .await
            }
        }
    }

    /// Creates (or truncates, per `options`) `path` and writes the
    /// gathered contents of all segments in a single backend dispatch —
    /// open, vectored write, optional sync-all, close. The whole-file
    /// write twin of [`open_read_head`](Self::open_read_head) for
    /// multi-segment payloads.
    ///
    /// Releases the segments after the gather, as
    /// [`write_file_segments_atomic`](Self::write_file_segments_atomic) does.
    pub async fn write_file_segments<B: StableBufList>(
        &self,
        path: impl AsRef<Path>,
        options: &OpenOptions,
        buffers: B,
        durable: bool,
    ) -> std::io::Result<()> {
        let std_options = options.to_std();
        let path = path.as_ref().to_path_buf();
        match &*self.inner {
            DriverInner::Psync(driver) => {
                driver
                    .write_file_segments(std_options, path, buffers, durable)
                    .await
            }
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => {
                driver
                    .write_file_segments(std_options, path, buffers, durable)
                    .await
            }
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => {
                driver
                    .write_file_segments(std_options, path, buffers, durable)
                    .await
            }
        }
    }

    /// The metadata of `path`.
    ///
    /// Takes the path by value so a caller building one for the call hands it
    /// over rather than having it copied, which a walk does once per component.
    pub async fn metadata(&self, path: impl Into<PathBuf>) -> std::io::Result<std::fs::Metadata> {
        let path = path.into();
        match &*self.inner {
            DriverInner::Psync(driver) => driver.metadata(path).await,
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => driver.metadata(path).await,
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => driver.metadata(path).await,
        }
    }

    /// Whether the filesystem holds `path` spelled exactly the way it is given.
    ///
    /// A case-insensitive filesystem keeps one spelling of a name and answers a
    /// lookup in any other, so `Path::exists` cannot answer this: it says yes for
    /// a neighbouring spelling. On Windows a single `FindFirstFileExW` can,
    /// where the alternative is listing the parent and comparing every child -
    /// and a caller resolving a path does that once per component.
    ///
    /// `Some(false)` is "not under this spelling", never "no such path", and
    /// `None` is "this platform cannot say": macOS has no call for it and
    /// answers `None` to everything, as does Windows for a path longer than the
    /// lookup is built for. A caller that needs to know whether the name is
    /// there in some other spelling has to read the directory, and so does one
    /// that cannot act on `None`.
    ///
    /// Takes the path by value so a caller building one for the call hands it
    /// over rather than having it copied.
    pub async fn holds_name_exactly(&self, path: impl Into<PathBuf>) -> Option<bool> {
        let path = path.into();
        match &*self.inner {
            DriverInner::Psync(driver) => driver.holds_name_exactly(path).await,
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => driver.holds_name_exactly(path).await,
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => driver.holds_name_exactly(path).await,
        }
    }

    pub async fn rename(
        &self,
        from: impl AsRef<Path>,
        to: impl AsRef<Path>,
    ) -> std::io::Result<()> {
        let from = from.as_ref().to_path_buf();
        let to = to.as_ref().to_path_buf();
        match &*self.inner {
            DriverInner::Psync(driver) => driver.rename(from, to).await,
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => driver.rename(from, to).await,
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => driver.rename(from, to).await,
        }
    }

    pub async fn remove_file(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref().to_path_buf();
        match &*self.inner {
            DriverInner::Psync(driver) => driver.remove_file(path).await,
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => driver.remove_file(path).await,
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => driver.remove_file(path).await,
        }
    }

    /// Lists `path`, resolving each entry's metadata, a chunk of entries per dispatch.
    ///
    /// The open is awaited, so a directory that cannot be read fails here rather than as an
    /// empty listing, and the first chunk arrives with it. The stream holds one chunk, so a
    /// directory of any width costs the same memory and a consumer that stops early stops the
    /// walk.
    ///
    /// This is a driver operation rather than an inline `read_dir` because a listing is not one
    /// syscall: it is `getdents` plus a stat per child, and a caller wanting the children's
    /// metadata otherwise pays all of them on whatever thread polls it.
    pub async fn read_dir(&self, path: impl AsRef<Path>) -> std::io::Result<crate::DirStream> {
        let path = path.as_ref().to_path_buf();
        match &*self.inner {
            DriverInner::Psync(driver) => driver.read_dir(path).await,
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => driver.read_dir(path).await,
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => driver.read_dir(path).await,
        }
    }

    pub async fn create_dir_all(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref().to_path_buf();
        match &*self.inner {
            DriverInner::Psync(driver) => driver.create_dir_all(path).await,
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => driver.create_dir_all(path).await,
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => driver.create_dir_all(path).await,
        }
    }

    /// Sets a path's permissions.
    ///
    /// Takes `std::fs::Permissions` rather than a mode, so what a permission is stays the
    /// platform's business and a caller reads one off `metadata` and hands it back changed.
    pub async fn set_permissions(
        &self,
        path: impl AsRef<Path>,
        permissions: std::fs::Permissions,
    ) -> std::io::Result<()> {
        let path = path.as_ref().to_path_buf();
        match &*self.inner {
            DriverInner::Psync(driver) => driver.set_permissions(path, permissions).await,
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => driver.set_permissions(path, permissions).await,
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => driver.set_permissions(path, permissions).await,
        }
    }

    pub async fn remove_dir(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref().to_path_buf();
        match &*self.inner {
            DriverInner::Psync(driver) => driver.remove_dir(path).await,
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => driver.remove_dir(path).await,
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => driver.remove_dir(path).await,
        }
    }

    pub async fn copy(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> std::io::Result<u64> {
        let from = from.as_ref().to_path_buf();
        let to = to.as_ref().to_path_buf();
        match &*self.inner {
            DriverInner::Psync(driver) => driver.copy(from, to).await,
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => driver.copy(from, to).await,
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => driver.copy(from, to).await,
        }
    }

    pub async fn remove_dir_all(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref().to_path_buf();
        match &*self.inner {
            DriverInner::Psync(driver) => driver.remove_dir_all(path).await,
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => driver.remove_dir_all(path).await,
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => driver.remove_dir_all(path).await,
        }
    }

    pub(crate) async fn file_metadata_raw(
        &self,
        file: Arc<File>,
    ) -> std::io::Result<std::fs::Metadata> {
        match &*self.inner {
            DriverInner::Psync(driver) => driver.file_metadata(file).await,
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => driver.file_metadata(file).await,
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => driver.file_metadata(file).await,
        }
    }

    pub(crate) async fn set_len_raw(&self, file: Arc<File>, len: u64) -> std::io::Result<()> {
        match &*self.inner {
            DriverInner::Psync(driver) => driver.set_len(file, len).await,
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => driver.set_len(file, len).await,
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => driver.set_len(file, len).await,
        }
    }

    /// Reads an entire file into pooled memory as a single backend
    /// dispatch — open, stat, read to the stat length, close — and
    /// returns it as [`Bytes`] without copying.
    ///
    /// The whole-file read twin of
    /// [`write_file_bytes`](Self::write_file_bytes): small-file scans pay
    /// one dispatch per file instead of separate open and read round
    /// trips.
    ///
    /// Fails with `InvalidInput` for a file larger than [`WHOLE_FILE_LIMIT`].
    pub async fn read_file_bytes(&self, path: impl AsRef<Path>) -> std::io::Result<Bytes> {
        let path = path.as_ref().to_path_buf();
        match &*self.inner {
            DriverInner::Psync(driver) => driver.read_file_bytes(path).await,
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => driver.read_file_bytes(path).await,
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => driver.read_file_bytes(path).await,
        }
    }

    /// Writes `data` as the entire contents of `path` in a single backend
    /// dispatch — create (or truncate), write, optionally sync file data,
    /// stat, close — and returns the resulting metadata.
    ///
    /// The whole-file write twin of
    /// [`read_file_bytes`](Self::read_file_bytes), matching the atomic
    /// whole-file write pattern used by the storage layer.
    ///
    /// Fails with `InvalidInput` for data larger than [`WHOLE_FILE_LIMIT`]. The check runs
    /// before the file is opened, so a rejected call cannot have truncated an existing file.
    pub async fn write_file_bytes(
        &self,
        path: impl AsRef<Path>,
        data: Bytes,
        durable: bool,
    ) -> std::io::Result<std::fs::Metadata> {
        check_whole_file_len(data.len())?;
        let path = path.as_ref().to_path_buf();
        match &*self.inner {
            DriverInner::Psync(driver) => driver.write_file_bytes(path, data, durable).await,
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => driver.write_file_bytes(path, data, durable).await,
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => driver.write_file_bytes(path, data, durable).await,
        }
    }

    pub(crate) async fn read_at_raw(
        &self,
        file: Arc<File>,
        max_len: usize,
        offset: u64,
    ) -> std::io::Result<Bytes> {
        match &*self.inner {
            DriverInner::Psync(driver) => driver.read_at(file, max_len, offset).await,
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => driver.read_at(file, max_len, offset).await,
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => driver.read_at(file, max_len, offset).await,
        }
    }

    pub(crate) async fn read_exact_at_raw(
        &self,
        file: Arc<File>,
        len: usize,
        offset: u64,
    ) -> std::io::Result<Bytes> {
        match &*self.inner {
            DriverInner::Psync(driver) => driver.read_exact_at(file, len, offset).await,
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => driver.read_exact_at(file, len, offset).await,
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => driver.read_exact_at(file, len, offset).await,
        }
    }

    pub(crate) async fn write_all_at_raw<B: StableBuf>(
        &self,
        file: Arc<File>,
        buffer: B,
        len: usize,
        offset: u64,
    ) -> std::io::Result<B> {
        match &*self.inner {
            DriverInner::Psync(driver) => driver.write_all_at(file, buffer, len, offset).await,
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => driver.write_all_at(file, buffer, len, offset).await,
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => driver.write_all_at(file, buffer, len, offset).await,
        }
    }

    pub(crate) async fn write_at_raw<B: StableBuf>(
        &self,
        file: Arc<File>,
        buffer: B,
        buffer_offset: usize,
        len: usize,
        offset: u64,
    ) -> std::io::Result<(B, usize)> {
        match &*self.inner {
            DriverInner::Psync(driver) => {
                driver
                    .write_at(file, buffer, buffer_offset, len, offset)
                    .await
            }
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => {
                driver
                    .write_at(file, buffer, buffer_offset, len, offset)
                    .await
            }
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => {
                driver
                    .write_at(file, buffer, buffer_offset, len, offset)
                    .await
            }
        }
    }

    pub(crate) async fn read_vectored_at_raw<B: StableBufListMut>(
        &self,
        file: Arc<File>,
        buffers: B,
        skip: usize,
        offset: u64,
    ) -> std::io::Result<(B, usize)> {
        match &*self.inner {
            DriverInner::Psync(driver) => {
                driver.read_vectored_at(file, buffers, skip, offset).await
            }
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => {
                driver.read_vectored_at(file, buffers, skip, offset).await
            }
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => driver.read_vectored_at(file, buffers, skip, offset).await,
        }
    }

    pub(crate) async fn write_vectored_at_raw<B: StableBufList>(
        &self,
        file: Arc<File>,
        buffers: B,
        skip: usize,
        offset: u64,
    ) -> std::io::Result<(B, usize)> {
        match &*self.inner {
            DriverInner::Psync(driver) => {
                driver.write_vectored_at(file, buffers, skip, offset).await
            }
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => {
                driver.write_vectored_at(file, buffers, skip, offset).await
            }
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => {
                driver.write_vectored_at(file, buffers, skip, offset).await
            }
        }
    }

    pub(crate) async fn sync_raw(&self, file: Arc<File>, data_only: bool) -> std::io::Result<()> {
        match &*self.inner {
            DriverInner::Psync(driver) => driver.sync(file, data_only).await,
            #[cfg(target_os = "linux")]
            DriverInner::Uring(driver) => driver.sync(file, data_only).await,
            #[cfg(target_family = "windows")]
            DriverInner::Iocp(driver) => driver.sync(file, data_only).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recognised_backend_value_selects_it() {
        assert_eq!(
            backend_kind_from_value("psync").unwrap(),
            BackendKind::Psync
        );
        assert_eq!(
            backend_kind_from_value("PSYNC").unwrap(),
            BackendKind::Psync
        );
        assert_eq!(backend_kind_from_value("auto").unwrap(), BackendKind::Auto);
        assert_eq!(backend_kind_from_value("").unwrap(), BackendKind::Auto);
        #[cfg(target_os = "linux")]
        assert_eq!(
            backend_kind_from_value("uring").unwrap(),
            BackendKind::Uring
        );
        #[cfg(target_family = "windows")]
        assert_eq!(backend_kind_from_value("iocp").unwrap(), BackendKind::Iocp);
    }

    /// A backend this build has no code for is rejected rather than silently falling back, so a
    /// value that works on one platform does not quietly mean something else on another.
    #[test]
    #[cfg(not(target_os = "linux"))]
    fn a_backend_absent_from_this_build_is_rejected() {
        let error = backend_kind_from_value("uring")
            .expect_err("a backend this platform has no code for must not be accepted");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    /// The mirror of the case above: the Windows completion backend must not be selectable on a
    /// platform that has no code for it either.
    #[test]
    #[cfg(not(target_family = "windows"))]
    fn the_completion_port_backend_is_rejected_off_windows() {
        let error = backend_kind_from_value("iocp")
            .expect_err("a backend this platform has no code for must not be accepted");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    /// The value names the accepted set, because the caller reaching for this variable is
    /// diagnosing something and a bare rejection tells them nothing.
    #[test]
    fn an_unrecognised_backend_value_is_a_reportable_error() {
        let error = backend_kind_from_value("iouring")
            .expect_err("a backend that does not exist must not be accepted");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        let message = format!("{error}");
        assert!(message.contains("iouring"), "{message}");
        assert!(
            message.contains(&format!("supported: {SUPPORTED_BACKENDS}")),
            "{message}"
        );
    }

    /// Whatever the environment holds, the process-wide driver resolves rather than panicking.
    /// Which backend it lands on depends on the machine, so the name is only checked against the
    /// set this build can produce.
    #[test]
    fn the_global_driver_always_resolves() {
        let name = IoDriver::global().backend_name();
        assert!(["psync", "uring", "iocp"].contains(&name), "{name}");
    }

    /// The probe never fails: a machine that cannot give it a ring gets the portable backend.
    #[test]
    fn the_probe_always_yields_a_backend() {
        let driver = IoDriver::new(BackendKind::Auto).expect("auto must always resolve");
        assert!(
            ["psync", "uring", "iocp"].contains(&driver.backend_name()),
            "{driver:?}"
        );
    }
}
