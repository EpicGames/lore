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
    pub async fn acquire_file_lock(path: impl AsRef<Path>) -> std::io::Result<FSLock> {
        let mut path = path.as_ref().to_path_buf();
        let mut file_name = path
            .file_name()
            .ok_or(std::io::Error::other(
                "Acquiring file lock on path with no file",
            ))?
            .to_owned();
        path.pop();
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
    async fn acquire_exact_path(path: &Path) -> std::io::Result<FSLock> {
        let mut retry = 2;
        let file = loop {
            match Self::open_lock_file(path) {
                Ok(file) => break file,
                Err(err) => {
                    retry -= 1;
                    if retry == 0 {
                        return Err(err);
                    }
                    tokio::time::sleep(RETRY_DELAY).await;
                }
            }
        };

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory to lock, removed when the test ends.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
            std::fs::create_dir_all(&path).expect("failed to create temp dir");
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The lock excludes a second acquirer, which waits rather than failing. Both `flock` and
    /// `LockFileEx` contend between separate handles on one file, so one process is enough to
    /// observe it: the second acquisition is still pending when the window closes, where a
    /// failing implementation would have returned an error and a broken one would have returned
    /// a guard.
    #[tokio::test]
    async fn a_second_acquirer_waits_while_the_lock_is_held() {
        let dir = TempDir::new("lore-base-lock-held");
        let held = FSLock::acquire_directory_lock(dir.path())
            .await
            .expect("first acquisition");

        // Several poll intervals, so the second acquirer has attempted and backed off repeatedly.
        let waited = tokio::time::timeout(
            Duration::from_millis(50),
            FSLock::acquire_directory_lock(dir.path()),
        )
        .await;

        assert!(
            waited.is_err(),
            "a second acquirer must neither take a held lock nor fail on it"
        );
        drop(held);
    }

    /// Dropping the guard releases the OS lock, so the next acquisition completes. Bounded by a
    /// timeout because the wait is otherwise unbounded: a lock that was not released would hang
    /// the test rather than fail it.
    #[tokio::test]
    async fn the_lock_is_acquirable_once_the_guard_drops() {
        let dir = TempDir::new("lore-base-lock-released");
        let held = FSLock::acquire_directory_lock(dir.path())
            .await
            .expect("first acquisition");
        drop(held);

        tokio::time::timeout(
            Duration::from_secs(5),
            FSLock::acquire_directory_lock(dir.path()),
        )
        .await
        .expect("the lock must be acquirable once the guard drops")
        .expect("second acquisition");
    }
}
