// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use lore_error_set::prelude::*;

use crate::remote::LORE_SERVICE_SOCKET_NAME;
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

fn uds_sock_path() -> PathBuf {
    uds_sock_dir().join(LORE_SERVICE_SOCKET_NAME)
}

pub struct UdsListener {
    listener: UnixListener,
    path: PathBuf,
}

impl UdsListener {
    pub fn new() -> Result<UdsListener, UdsListenerError> {
        let dir = uds_sock_dir();
        fs::create_dir_all(&dir)
            .internal_with(|| format!("creating socket directory {}", dir.display()))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .internal_with(|| format!("restricting socket directory {}", dir.display()))?;

        let path = dir.join(LORE_SERVICE_SOCKET_NAME);

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

        Ok(Self { listener, path })
    }

    pub fn accept(&self) -> Result<UdsStream, UdsAcceptError> {
        let (stream, _address) = self.listener.accept().internal("accept error")?;
        Ok(UdsStream { stream })
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

    pub fn connect() -> Result<UdsStream, UdsConnectionError> {
        let path = uds_sock_path();
        let stream = UnixStream::connect(&path)
            .internal_with(|| format!("connecting to {}", path.display()))?;
        Ok(Self { stream })
    }
}
