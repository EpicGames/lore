// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fs::OpenOptions;
#[cfg(target_family = "unix")]
use std::os::fd::AsRawFd;
#[cfg(target_family = "windows")]
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::time::Duration;

#[cfg(target_family = "windows")]
use windows_sys::Win32::Storage::FileSystem;

/// Delay between lock-file open retries.
const RETRY_DELAY: Duration = Duration::from_millis(10);

/// First delay between non-blocking lock attempts. Short, because most contention is another
/// process finishing a brief critical section: a fixed 10 ms poll would put a floor of about
/// half that on every contended acquisition, where the blocking lock it replaces woke on release.
const LOCK_RETRY_START: Duration = Duration::from_micros(200);

/// Longest delay the backoff grows to, so sustained contention polls no more often than a fixed
/// 10 ms interval would.
const LOCK_RETRY_MAX: Duration = Duration::from_millis(10);

/// How long to wait before reporting that a lock is still contended. The wait itself is
/// unbounded, so without this a peer that never releases is indistinguishable from a hang.
const LOCK_WAIT_WARN: Duration = Duration::from_secs(5);

pub struct FSLock {
    file: std::fs::File,
}

impl FSLock {
    /// Acquires an exclusive lock guarding `path`, waiting asynchronously
    /// — non-blocking lock attempts with timed retries — while another
    /// process holds it, so a contended lock never parks a runtime thread.
    pub async fn acquire_file_lock(
        path: impl AsRef<Path>,
        create_directory_if_necessary: bool,
    ) -> std::io::Result<FSLock> {
        let mut path = path.as_ref().to_path_buf();
        let mut file_name = path
            .file_name()
            .ok_or(std::io::Error::other(
                "Acquiring file lock on path with no file",
            ))?
            .to_owned();
        path.pop();
        if create_directory_if_necessary && !path.exists() {
            std::fs::create_dir_all(&path)?;
        }
        let mut path = path.canonicalize()?;
        file_name.push(".lock");
        path.push(file_name);
        Self::acquire_exact_path(&path).await.map_err(|_err| {
            std::io::Error::other(format!("Failed to acquire lock file \"{path:?}\""))
        })
    }

    /// Directory twin of [`acquire_file_lock`](Self::acquire_file_lock).
    pub async fn acquire_directory_lock(path: impl AsRef<Path>) -> std::io::Result<FSLock> {
        let path = path.as_ref().canonicalize()?.join("lock");
        Self::acquire_exact_path(&path).await
    }

    /// Blocking variant for synchronous contexts (log rotation); parks the
    /// calling thread in the OS lock wait.
    pub fn acquire_directory_lock_blocking(path: impl AsRef<Path>) -> std::io::Result<FSLock> {
        let path = path.as_ref().canonicalize()?.join("lock");
        let mut retry = 2;
        let file = loop {
            match Self::open_lock_file(&path) {
                Ok(file) => break file,
                Err(err) => {
                    retry -= 1;
                    if retry == 0 {
                        return Err(err);
                    }
                    std::thread::sleep(RETRY_DELAY);
                }
            }
        };
        Self::lock_blocking(&file)?;
        Ok(FSLock { file })
    }

    /// Opens the lock file and takes the OS lock, retrying while another holder has it.
    ///
    /// The wait is non-blocking attempts with backoff rather than a blocking lock, so no runtime
    /// thread is parked on a lock another process holds. Two properties change with that, and
    /// both are deliberate. Kernel queueing is gone: a blocking lock queues its waiters, while
    /// pollers race for whichever attempt lands after a release, so a waiter can be starved under
    /// sustained contention. And the wait stays unbounded, matching the blocking lock callers had
    /// before, which means a peer that never releases would otherwise look exactly like a hang —
    /// hence the warning once the wait passes [`LOCK_WAIT_WARN`].
    pub async fn acquire_exact_path(path: &Path) -> std::io::Result<FSLock> {
        let file = Self::open_with_retry(path).await?;

        let started = std::time::Instant::now();
        let mut delay = LOCK_RETRY_START;
        let mut warned = false;
        loop {
            match Self::try_lock(&file) {
                Ok(()) => return Ok(FSLock { file }),
                Err(err) if is_lock_contended(&err) => {
                    if !warned && started.elapsed() >= LOCK_WAIT_WARN {
                        crate::lore_warn!(
                            "Still waiting for lock \"{}\" held by another process after {} seconds",
                            path.display(),
                            started.elapsed().as_secs()
                        );
                        warned = true;
                    }
                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 2, LOCK_RETRY_MAX);
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Takes the OS lock guarding `path` only if no other holder has it, without waiting.
    ///
    /// For work that can be put off: a caller that finds the lock held tries again later,
    /// rather than queueing behind a holder whose hold has no bound.
    ///
    /// # Errors
    ///
    /// [`std::io::Error`] if the lock file cannot be opened, or if the lock cannot be
    /// attempted for a reason other than another holder having it.
    pub async fn try_acquire_exact_path(path: &Path) -> std::io::Result<Option<FSLock>> {
        let file = Self::open_with_retry(path).await?;
        match Self::try_lock(&file) {
            Ok(()) => Ok(Some(FSLock { file })),
            Err(err) if is_lock_contended(&err) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Opens the lock file at `path`, trying once more after [`RETRY_DELAY`] if the first
    /// open fails.
    async fn open_with_retry(path: &Path) -> std::io::Result<std::fs::File> {
        let mut retry = 2;
        loop {
            match Self::open_lock_file(path) {
                Ok(file) => return Ok(file),
                Err(err) => {
                    retry -= 1;
                    if retry == 0 {
                        return Err(err);
                    }
                    tokio::time::sleep(RETRY_DELAY).await;
                }
            }
        }
    }

    fn open_lock_file(path: &Path) -> std::io::Result<std::fs::File> {
        if let Ok(file) = OpenOptions::new()
            .create(false)
            .truncate(false)
            .write(false)
            .read(true)
            .open(path)
        {
            return Ok(file);
        }

        OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .read(true)
            .open(path)
    }

    #[cfg(target_family = "windows")]
    fn try_lock(file: &std::fs::File) -> std::io::Result<()> {
        // Safety: Calling OS functions
        let ret = unsafe {
            let mut overlapped = std::mem::zeroed();
            FileSystem::LockFileEx(
                file.as_raw_handle(),
                FileSystem::LOCKFILE_EXCLUSIVE_LOCK | FileSystem::LOCKFILE_FAIL_IMMEDIATELY,
                0,
                !0,
                !0,
                &mut overlapped,
            )
        };
        if ret == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(not(target_family = "windows"))]
    fn try_lock(file: &std::fs::File) -> std::io::Result<()> {
        // Safety: Calling OS functions
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if ret < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(target_family = "windows")]
    fn lock_blocking(file: &std::fs::File) -> std::io::Result<()> {
        // Safety: Calling OS functions
        let ret = unsafe {
            let mut overlapped = std::mem::zeroed();
            FileSystem::LockFileEx(
                file.as_raw_handle(),
                FileSystem::LOCKFILE_EXCLUSIVE_LOCK,
                0,
                !0,
                !0,
                &mut overlapped,
            )
        };
        if ret == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(not(target_family = "windows"))]
    fn lock_blocking(file: &std::fs::File) -> std::io::Result<()> {
        // Safety: Calling OS functions
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if ret < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(target_family = "windows")]
fn is_lock_contended(err: &std::io::Error) -> bool {
    err.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION as i32)
}

#[cfg(not(target_family = "windows"))]
fn is_lock_contended(err: &std::io::Error) -> bool {
    err.raw_os_error() == Some(libc::EWOULDBLOCK)
}

impl Drop for FSLock {
    fn drop(&mut self) {
        #[cfg(target_family = "windows")]
        {
            // Safety: Calling OS functions
            unsafe { FileSystem::UnlockFile(self.file.as_raw_handle(), 0, 0, !0, !0) };
        }

        #[cfg(not(target_family = "windows"))]
        {
            // Safety: Calling OS functions
            unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}
