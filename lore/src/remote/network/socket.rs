// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The service's socket: a [`local_socket`] at a path of the user's own, which `unix.rs` and
//! `windows.rs` choose.
use lore_error_set::prelude::*;
use lore_transport::local_socket;

use crate::remote::network::UdsAcceptError;
use crate::remote::network::UdsConnectionError;
use crate::remote::network::UdsListenerError;
#[cfg(target_family = "unix")]
use crate::remote::network::unix as os;
#[cfg(target_os = "windows")]
use crate::remote::network::windows as os;

/// On Unix, only the user the service runs as may connect, since it runs every command as that
/// user.
const SOCKET_MODE: u32 = 0o600;

/// Whether the service can listen on its socket here, which every platform this builds for allows.
pub fn uds_supported() -> bool {
    true
}

/// The service's socket, listening.
pub struct UdsListener {
    listener: local_socket::Listener,
}

/// The claim on a socket name, held by at most one process from [`UdsListener::claim`] until the
/// listener it becomes is dropped. Without it, every service started at once binds in turn, and
/// all but the last run on, each holding the mounts it made, with nothing able to reach them to
/// stop them.
pub struct UdsListenerClaim {
    claim: local_socket::Claim,
}

impl UdsListener {
    /// Claims the socket `name` names and listens on it, failing while another service holds it.
    pub fn new(name: &str) -> Result<UdsListener, UdsListenerError> {
        let path = os::uds_sock_path(name).internal("locating the service socket")?;
        Self::claim_at(&path)?
            .ok_or_else(|| {
                UdsListenerError::internal(format!(
                    "another Lore service has claimed {}",
                    path.display()
                ))
            })?
            .listen()
    }

    /// Claims `name` without binding it: `None` while another process holds the claim or a
    /// service answers on the socket, as one from a build that takes no claim may.
    pub fn claim(name: &str) -> Result<Option<UdsListenerClaim>, UdsListenerError> {
        Self::claim_at(&os::uds_sock_path(name).internal("locating the service socket")?)
    }

    fn claim_at(path: &std::path::Path) -> Result<Option<UdsListenerClaim>, UdsListenerError> {
        os::prepare_socket_directory()?;
        let claim = local_socket::Claim::acquire(path).internal("claiming the service socket")?;
        Ok(claim.map(|claim| UdsListenerClaim { claim }))
    }

    /// The next connection, waiting for one.
    pub fn accept(&self) -> Result<UdsStream, UdsAcceptError> {
        let stream = self.listener.accept().internal("accept error")?;
        Ok(UdsStream { stream })
    }
}

impl UdsListenerClaim {
    /// Binds the claimed name, removing a stale socket file. Fails if a service answers on it, as
    /// one from a build that takes no claim may.
    pub fn listen(self) -> Result<UdsListener, UdsListenerError> {
        let listener = self
            .claim
            .listen(SOCKET_MODE)
            .internal("listening on the service socket")?;
        Ok(UdsListener { listener })
    }
}

/// A connection on the service's socket, from either end.
pub struct UdsStream {
    stream: local_socket::Stream,
}

impl UdsStream {
    /// The connection, to write to.
    pub fn writer(&mut self) -> &mut impl std::io::Write {
        &mut self.stream
    }

    /// The connection, to read from.
    pub fn reader(&mut self) -> &mut impl std::io::Read {
        &mut self.stream
    }

    /// A second handle to the same connection.
    pub fn try_clone(&self) -> std::io::Result<Self> {
        self.stream.try_clone().map(|stream| Self { stream })
    }

    /// Connects to the service listening on the socket `name` names, failing at once when none
    /// does.
    pub fn connect(name: &str) -> Result<UdsStream, UdsConnectionError> {
        let path = os::uds_sock_path(name).internal("locating the service socket")?;
        let stream =
            local_socket::Stream::connect(&path).internal("connecting to the service socket")?;
        Ok(Self { stream })
    }
}
