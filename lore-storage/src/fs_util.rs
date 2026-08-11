// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;

#[cfg(not(target_family = "windows"))]
pub fn sync_dir<P: AsRef<Path>>(path: P) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(path.as_ref())?;
    let fd = dir.as_raw_fd();
    // SAFETY: Safe to call libc function to flush directory changes
    let result = unsafe { libc::fsync(fd) };
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_family = "windows")]
pub fn sync_dir<P: AsRef<Path>>(_path: P) -> std::io::Result<()> {
    // No-op on Windows, there is no API to flush a directory
    Ok(())
}

pub async fn unlink_recursive<P: AsRef<Path>>(absolute_path: P) -> tokio::io::Result<()> {
    let absolute_path = absolute_path.as_ref();
    lore_base::lore_trace!("Deleting {}", absolute_path.display());
    let driver = lore_io::IoDriver::global();
    let metadata = match driver.metadata(absolute_path).await {
        Ok(metadata) => metadata,
        // Nothing to delete is a delete that has already happened. Any other failure — a
        // permission error on the parent, for one — is reported rather than read as absence,
        // which would report a successful delete of something still there.
        Err(err) if err.kind() == tokio::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };

    if metadata.is_dir() {
        if let Err(err) = driver.remove_dir_all(absolute_path).await {
            if err.kind() == tokio::io::ErrorKind::NotFound {
                return Ok(());
            }
            lore_base::lore_debug!(
                "Error deleting directory {}: {} - retry after setting write permission",
                absolute_path.display(),
                err
            );

            let mut permissions = metadata.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            // Blocking on purpose: the driver has no permissions operation, and this runs only
            // after a delete has already failed, so it is bounded to the retry path.
            let _ = std::fs::set_permissions(absolute_path, permissions);
            if let Err(err) = driver.remove_dir_all(absolute_path).await {
                if err.kind() == tokio::io::ErrorKind::NotFound {
                    return Ok(());
                }
                return Err(err);
            }
        }
    } else if let Err(err) = driver.remove_file(absolute_path).await {
        if err.kind() == tokio::io::ErrorKind::NotFound {
            return Ok(());
        }

        let mut permissions = metadata.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        // Blocking for the same reason as the directory retry above.
        let _ = std::fs::set_permissions(absolute_path, permissions);
        if let Err(err) = driver.remove_file(absolute_path).await {
            if err.kind() == tokio::io::ErrorKind::NotFound {
                return Ok(());
            }
            return Err(err);
        }
    }

    Ok(())
}

/// First wait between attempts at a transiently failed file operation, in milliseconds.
const RETRY_START_MILLIS: u64 = 10;

/// Longest wait the backoff grows to, in milliseconds.
const RETRY_MAXIMUM_MILLIS: u64 = 10_000;

/// Attempts before a transient failure is reported. With the schedule above this spends roughly
/// fifteen minutes on an operation that keeps failing, which is a deliberate budget: whoever holds
/// the file is expected to let go, and failing would fail the caller's whole operation.
const RETRY_LIMIT: usize = 100;

/// Whether the operation is worth reissuing rather than reporting.
///
/// Positional reads and writes are idempotent, so reissuing is always safe. What the set buys is
/// the other direction: a permanent failure — a short read from a truncated file, an offset past
/// the end, a permission problem — is reported at once instead of after the whole retry budget
/// above.
pub(crate) fn is_transient(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
    ) || is_transient_for_platform(error)
}

/// Another process holding the file, or a byte range inside it, fails an operation that succeeds
/// as soon as it lets go — a virus scanner or a search indexer walking the store, or one of this
/// process's own readers, which grant no write access for as long as they are open. A sharing
/// violation is raised when a handle is opened and a lock violation against a handle already open,
/// and both belong here.
#[cfg(target_family = "windows")]
fn is_transient_for_platform(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code)
            if code == windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32
                || code == windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION as i32
    )
}

/// No platform-specific transient conditions: no kernel here fails a file operation because
/// another process holds the file open.
#[cfg(not(target_family = "windows"))]
fn is_transient_for_platform(_error: &std::io::Error) -> bool {
    false
}

/// Runs `operation`, reissuing it with backoff for as long as it fails transiently.
///
/// Takes a plain closure returning a future rather than an `AsyncFnMut`: the latter's
/// higher-ranked lifetime defeats `Send` inference for the callers that spawn these operations.
pub(crate) async fn retry_transient<T, F, O>(mut operation: F) -> std::io::Result<T>
where
    F: FnMut() -> O,
    O: std::future::Future<Output = std::io::Result<T>>,
{
    let mut retry = crate::retry(RETRY_START_MILLIS, RETRY_MAXIMUM_MILLIS, RETRY_LIMIT);
    loop {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                if !is_transient(&error) || !retry.wait().await {
                    return Err(error);
                }
            }
        }
    }
}
