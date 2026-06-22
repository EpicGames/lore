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

pub struct FSLock {
    file: std::fs::File,
}

impl FSLock {
    pub fn acquire_file_lock(path: impl AsRef<Path>) -> std::io::Result<Self> {
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
        Self::acquire_exact_path(&path).map_err(|_err| {
            std::io::Error::other(format!("Failed to acquire lock file \"{path:?}\""))
        })
    }

    pub fn acquire_directory_lock(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().canonicalize()?.join("lock");
        Self::acquire_exact_path(&path)
    }

    fn acquire_exact_path(path: impl AsRef<Path> + Copy) -> std::io::Result<Self> {
        let mut retry = 2;
        let file = loop {
            let file = OpenOptions::new()
                .create(false)
                .truncate(false)
                .write(false)
                .read(true)
                .open(path);
            if let Ok(file) = file {
                break file;
            }

            let file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .read(true)
                .open(path);
            if let Ok(file) = file {
                break file;
            }

            retry -= 1;
            if retry == 0 {
                return Err(file.unwrap_err());
            }
            std::thread::sleep(Duration::from_millis(10));
        };

        #[cfg(target_family = "windows")]
        {
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
                Ok(Self { file })
            }
        }

        #[cfg(not(target_family = "windows"))]
        {
            // Safety: Calling OS functions
            let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if ret < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(Self { file })
            }
        }
    }
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
