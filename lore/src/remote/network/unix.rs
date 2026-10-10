// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use lore_base::fs::lock::FSLock;
use lore_error_set::prelude::*;

use crate::remote::network::UdsAcceptError;
use crate::remote::network::UdsConnectionError;
use crate::remote::network::UdsListenerError;

pub fn uds_supported() -> bool {
    true
}

/// Directory holding this user's socket, for example `/run/user/1000/lore-1000`.
///
/// `XDG_RUNTIME_DIR` is the right base on Linux: it is per-user, mode `0700`,
/// and cleared on logout. macOS has no such variable, but its `TMPDIR` is
/// already per-user (`/var/folders/...`), which makes it the direct analogue of
/// the `%TEMP%` path the Windows implementation uses. `/tmp` is the last resort
/// and *is* shared between users, which is why the socket always goes in a
/// uid-suffixed subdirectory rather than sitting in the base directly.
fn uds_sock_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("TMPDIR").filter(|value| !value.is_empty()))
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);

    // Safety: getuid() cannot fail and reads no memory through pointers.
    let uid = unsafe { libc::getuid() };
    base.join(format!("lore-{uid}"))
}

#[lore_macro::test_pub]
fn uds_sock_path(name: &str) -> PathBuf {
    uds_sock_dir().join(name)
}

pub struct UdsListener {
    listener: UnixListener,
    path: PathBuf,
    /// Released when the listener is dropped, after its socket file is removed.
    _claim: FSLock,
}

/// The claim on a socket name, held by at most one process from [`UdsListener::claim`] until the
/// listener it becomes is dropped. A lock beside the socket rather than the socket itself, whose
/// check, stale-file removal and bind are three steps: without the claim a second process can
/// unlink a socket another has just bound, and both then run as the service. The lock file
/// outlives the listener: unlinking it would let a later claimer lock a new file while an
/// earlier one still holds the old.
pub struct UdsListenerClaim {
    path: PathBuf,
    lock: FSLock,
}

impl UdsListener {
    pub fn new(name: &str) -> Result<UdsListener, UdsListenerError> {
        Self::claim(name)?
            .ok_or_else(|| {
                UdsListenerError::internal(format!(
                    "another Lore service has claimed {}",
                    uds_sock_path(name).display()
                ))
            })?
            .listen()
    }

    /// Claims `name` without binding it: `None` while another process holds the claim or a
    /// service answers on the socket, as one from a build that takes no claim may.
    pub fn claim(name: &str) -> Result<Option<UdsListenerClaim>, UdsListenerError> {
        let dir = uds_sock_dir();
        fs::create_dir_all(&dir)
            .internal_with(|| format!("creating socket directory {}", dir.display()))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .internal_with(|| format!("restricting socket directory {}", dir.display()))?;

        let path = dir.join(name);
        let Some(lock) = FSLock::try_acquire_file_lock(&path)
            .internal_with(|| format!("locking the claim on {}", path.display()))?
        else {
            return Ok(None);
        };
        if UnixStream::connect(&path).is_ok() {
            return Ok(None);
        }
        Ok(Some(UdsListenerClaim { path, lock }))
    }

    pub fn accept(&self) -> Result<UdsStream, UdsAcceptError> {
        let (stream, _address) = self.listener.accept().internal("accept error")?;
        Ok(UdsStream { stream })
    }
}

impl UdsListenerClaim {
    /// Binds the claimed name, removing a stale socket file. Fails if a service answers on it, as
    /// one from a build that takes no claim may.
    pub fn listen(self) -> Result<UdsListener, UdsListenerError> {
        let Self { path, lock } = self;
        if path.exists() {
            if UnixStream::connect(&path).is_ok() {
                return Err(UdsListenerError::internal(format!(
                    "another Lore service is already listening on {}",
                    path.display()
                )));
            }
            fs::remove_file(&path)
                .internal_with(|| format!("removing stale socket {}", path.display()))?;
        }

        let listener =
            UnixListener::bind(&path).internal_with(|| format!("binding {}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .internal_with(|| format!("restricting socket {}", path.display()))?;

        Ok(UdsListener {
            listener,
            path,
            _claim: lock,
        })
    }
}

impl Drop for UdsListener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub struct UdsStream {
    stream: UnixStream,
}

impl UdsStream {
    pub fn writer(&mut self) -> &mut impl std::io::Write {
        &mut self.stream
    }

    pub fn reader(&mut self) -> &mut impl std::io::Read {
        &mut self.stream
    }

    pub fn try_clone(&self) -> std::io::Result<Self> {
        self.stream.try_clone().map(|stream| Self { stream })
    }

    pub fn connect(name: &str) -> Result<UdsStream, UdsConnectionError> {
        let path = uds_sock_path(name);
        let stream = UnixStream::connect(&path)
            .internal_with(|| format!("connecting to {}", path.display()))?;
        Ok(Self { stream })
    }
}
