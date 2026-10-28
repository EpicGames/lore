// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ffi::OsString;
use std::net::TcpStream;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::FromRawSocket;
use std::os::windows::io::RawSocket;
use std::path::PathBuf;

use WinSock::WSADATA;
use WinSock::WSAStartup;
use lore_base::fs::lock::FSLock;
use lore_error_set::prelude::*;
use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Networking::WinSock;
use windows_sys::Win32::Networking::WinSock::INVALID_SOCKET;
use windows_sys::Win32::Networking::WinSock::SOCKADDR;
use windows_sys::Win32::Networking::WinSock::SOCKADDR_UN;
use windows_sys::Win32::Networking::WinSock::SOCKET;
use windows_sys::Win32::Networking::WinSock::WSAGetLastError;
use windows_sys::Win32::Storage::FileSystem::DeleteFileW;
use windows_sys::Win32::Storage::FileSystem::GetTempPathW;

use crate::remote::network::UdsAcceptError;
use crate::remote::network::UdsConnectionError;
use crate::remote::network::UdsListenerError;

const LISTENER_BACKLOG: i32 = 10;

pub fn uds_supported() -> bool {
    true
}

/// A socket closed when dropped, so that a failure between creating one and
/// handing it to a [`TcpStream`] does not leak it.
///
/// Every failure below is one a caller retries rather than gives up on. Nothing
/// listening is the expected answer while a service starts, and a stop waits for
/// the socket to be released by connecting until it fails, so both loops run
/// their whole timeout at one attempt every 20ms — up to five hundred failed
/// connects apiece. A socket left open per attempt is five hundred handles per
/// start or stop, and the attempt discards the error, so nothing would say so.
struct OwnedSocket(SOCKET);

impl OwnedSocket {
    /// Creates a unix stream socket, or says why it could not.
    fn new() -> Result<Self, String> {
        // Safety: Necessary to call windows APIs
        let socket = unsafe { WinSock::socket(WinSock::AF_UNIX as i32, WinSock::SOCK_STREAM, 0) };
        if socket == INVALID_SOCKET {
            // Safety: Necessary to call windows API
            return Err(format!("failed to create socket: {}", unsafe {
                WSAGetLastError()
            }));
        }
        Ok(Self(socket))
    }

    fn get(&self) -> SOCKET {
        self.0
    }

    /// Hands the socket to a [`TcpStream`], which closes it from here on.
    ///
    /// A unix domain socket rather than a TCP one, which is a liberty taken
    /// throughout this file: stream sockets are meant to behave alike, and this
    /// is how the socket gets an owner that closes it.
    fn into_stream(self) -> TcpStream {
        // Ownership passes to the stream, so this must not also close it.
        // `ManuallyDrop` rather than `mem::forget`, which `clippy::mem_forget`
        // rejects and CI builds with `-D warnings`.
        let socket = std::mem::ManuallyDrop::new(self);
        // Safety: a socket this owned, created above and not closed.
        unsafe { TcpStream::from_raw_socket(socket.0 as RawSocket) }
    }
}

impl Drop for OwnedSocket {
    fn drop(&mut self) {
        // Safety: Necessary to call windows APIs. Closes a socket this owns and
        // has not given away — [`into_stream`](Self::into_stream) forgets it.
        unsafe { WinSock::closesocket(self.0) };
    }
}

pub struct UdsListener {
    socket: OwnedSocket,
    /// Released when the listener is dropped, after its socket is closed.
    _claim: FSLock,
}

/// The claim on a socket name, held by at most one process from [`UdsListener::claim`] until the
/// listener it becomes is dropped. The same claim, taken the same way, as `unix.rs` takes, so that
/// a service starts or refuses alike on every system.
///
/// A lock beside the socket rather than the socket itself: binding deletes the socket file first,
/// and Windows lets that delete succeed under a service still listening on it, which keeps running
/// unreachable. Without the claim, every service started at once binds in turn, and all but the
/// last run on - each holding the mounts it made - with nothing able to reach them to stop them.
/// The lock file outlives the listener, as on Unix.
pub struct UdsListenerClaim {
    name: String,
    lock: FSLock,
}

impl UdsListenerClaim {
    /// Binds the claimed name, removing a stale socket file. Fails if a service answers on it, the
    /// check `unix.rs` makes before it binds.
    pub fn listen(self) -> Result<UdsListener, UdsListenerError> {
        let Self { name, lock } = self;
        if UdsStream::connect(&name).is_ok() {
            return Err(UdsListenerError::internal(format!(
                "another Lore service is already listening on {}",
                uds_sock_file(&name).display()
            )));
        }
        let socket = UdsListener::bind_and_listen(&name)?;
        Ok(UdsListener {
            socket,
            _claim: lock,
        })
    }
}

impl UdsListener {
    pub fn new(name: &str) -> Result<UdsListener, UdsListenerError> {
        Self::claim(name)?
            .ok_or_else(|| {
                UdsListenerError::internal(format!(
                    "another Lore service has claimed {}",
                    uds_sock_file(name).display()
                ))
            })?
            .listen()
    }

    /// Claims `name` without binding it: `None` while another process holds the claim or a
    /// service answers on the socket, the two conditions `unix.rs` refuses a claim on.
    pub fn claim(name: &str) -> Result<Option<UdsListenerClaim>, UdsListenerError> {
        let path = uds_sock_file(name);
        let Some(lock) = FSLock::try_acquire_file_lock(&path)
            .internal_with(|| format!("locking the claim on {}", path.display()))?
        else {
            return Ok(None);
        };
        if UdsStream::connect(name).is_ok() {
            return Ok(None);
        }
        Ok(Some(UdsListenerClaim {
            name: name.to_string(),
            lock,
        }))
    }

    /// Binds `name` and listens on it, deleting the socket file already there. Only under the
    /// claim: the delete succeeds under a service still listening on the file.
    fn bind_and_listen(name: &str) -> Result<OwnedSocket, UdsListenerError> {
        let wide_file_name = uds_sock_path(name);
        // Ahead of the delete, so an address that cannot be built does not first
        // remove the socket a running service is listening on.
        let addr: SOCKADDR_UN = uds_sockaddr(name).map_err(UdsListenerError::internal)?;

        // Safety: Necessary to call windows APIs, only const pointers are passed to windows
        unsafe {
            if DeleteFileW(wide_file_name.as_ptr()) == 0 {
                let err = GetLastError();
                if err != ERROR_FILE_NOT_FOUND {
                    return Err(UdsListenerError::internal(format!(
                        "old URC service still holding file {}",
                        String::from_utf16_lossy(&wide_file_name)
                    )));
                }
            }
        }

        if !wsa_startup() {
            // Safety: Necessary to call windows API
            return Err(UdsListenerError::internal(format!(
                "failed to start winsock: {}",
                unsafe { WSAGetLastError() }
            )));
        }

        // Owned from here, so the bind and listen failures below close it rather
        // than leaving it open for the life of the process that tried to serve.
        let sock = OwnedSocket::new().map_err(UdsListenerError::internal)?;
        // Safety: Necessary to call windows APIs, only const pointers are passed to windows
        unsafe {
            if WinSock::bind(
                sock.get(),
                (&raw const addr).cast::<SOCKADDR>(),
                std::mem::size_of_val(&addr) as i32,
            ) != 0
            {
                return Err(UdsListenerError::internal(format!(
                    "failed to bind: {}",
                    WSAGetLastError()
                )));
            }

            if WinSock::listen(sock.get(), LISTENER_BACKLOG) != 0 {
                return Err(UdsListenerError::internal(format!(
                    "failed to listen: {}",
                    WSAGetLastError()
                )));
            }
        }
        Ok(sock)
    }

    pub fn accept(&self) -> Result<UdsStream, UdsAcceptError> {
        let mut addr = SOCKADDR_UN::default();
        let mut addr_size = std::mem::size_of::<SOCKADDR_UN>() as i32;
        // Safety: Needed to call windows API. Mutable pointers passed with values initialized by rust.
        // TcpStream::from_raw_socket requires the appropriate handle to be put in, we're fudging
        // things a little bit by putting a unix domain socket into a TcpStream, but the actual
        // stream sockets are supposed to behave identically.
        unsafe {
            let res = WinSock::accept(
                self.socket.get(),
                (&raw mut addr).cast::<SOCKADDR>(),
                &mut addr_size as *mut i32,
            );
            if res == INVALID_SOCKET {
                return Err(UdsAcceptError::internal(format!(
                    "accept error: {}",
                    WSAGetLastError()
                )));
            }
            Ok(UdsStream {
                stream: TcpStream::from_raw_socket(res as RawSocket),
            })
        }
    }
}

pub struct UdsStream {
    stream: TcpStream,
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
        if !wsa_startup() {
            // Safety: Necessary to call windows API
            return Err(UdsConnectionError::internal(format!(
                "failed to start winsock: {}",
                unsafe { WSAGetLastError() }
            )));
        }

        // Owned from here. This is the hot path for the leak: a connect that finds
        // no service is how both the start and the stop loops make progress, so
        // the failure below is taken hundreds of times per wait.
        let sock = OwnedSocket::new().map_err(UdsConnectionError::internal)?;
        let addr: SOCKADDR_UN = uds_sockaddr(name).map_err(UdsConnectionError::internal)?;

        // Safety: Needed to call windows API. Only const pointers are passed to windows.
        unsafe {
            if WinSock::connect(
                sock.get(),
                (&raw const addr).cast::<SOCKADDR>(),
                size_of_val(&addr) as i32,
            ) != 0
            {
                return Err(UdsConnectionError::internal(format!(
                    "failed to connect: {}",
                    WSAGetLastError()
                )));
            }
        }

        Ok(UdsStream {
            stream: sock.into_stream(),
        })
    }
}

/// Starts Winsock, once for the process.
///
/// Every connect calls this, and a wait makes hundreds of connects, so it is not
/// repeated: each `WSAStartup` takes a reference that a matching `WSACleanup`
/// would have to release, and Winsock stays up for the life of the process
/// either way. The result is remembered so a failure is still reported to each
/// caller rather than only to the first.
fn wsa_startup() -> bool {
    static STARTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *STARTED.get_or_init(|| {
        let mut data = WSADATA::default();
        // Safety: Necessary to call windows APIs, the mutable pointer passed in is properly
        // initialized in rust to a safe value
        let result = unsafe { WSAStartup(2 << 8 | 2, &mut data) };
        result == 0
    })
}

/// The socket file for `socket_name`, beside which its claim's lock file lies.
#[lore_macro::test_pub]
fn uds_sock_file(socket_name: &str) -> PathBuf {
    let wide = uds_sock_path(socket_name);
    // Without the terminator.
    PathBuf::from(OsString::from_wide(&wide[..wide.len() - 1]))
}

fn uds_sock_path(socket_name: &str) -> Vec<u16> {
    let mut path = Vec::new();
    // Safety: Necessary to call windows APIs. The buffer length required by windows is allocated in
    // buffer passed mutably to windows
    unsafe {
        // Don't use GetTempPath2, because the temp path should be consistent regardless of if this is running as a system process or not.
        let len = GetTempPathW(0, std::ptr::null_mut());
        path.resize(len as usize, 0);
        GetTempPathW(len, path.as_mut_ptr());
    }
    // Append the file name, taking into account the null terminator.
    path.resize(path.len() - 1, 0);
    path.extend(socket_name.encode_utf16());
    // Reinsert the null terminator.
    path.push(0);
    path
}

/// Bytes an address holds for a socket path, including its terminator. The
/// field is fixed at this size, as the Unix one is.
#[lore_macro::test_pub]
const SUN_PATH_CAPACITY: usize = 108;

/// Builds the address a socket is bound to or connected on, or says why the
/// path cannot be one.
///
/// The path grows with the temporary directory and with the name
/// `LORE_SERVICE_SOCKET` gives it, so one too long for the address is something
/// a caller can arrive at by configuration. Reported rather than truncated: a
/// truncated path names a different socket, which would silently divide callers
/// between two services.
fn uds_sockaddr(name: &str) -> Result<SOCKADDR_UN, String> {
    let path_string = String::from_utf16(&uds_sock_path(name))
        .map_err(|_err| "the socket path is not valid text".to_string())?;
    sockaddr_for_path(path_string.trim_end_matches('\0'))
}

/// Writes `path` into an address, or says why it does not fit.
///
/// Takes the path rather than reading it, so the bound can be tested: the
/// longest usable path is one byte short of the array, since the terminator
/// needs a byte of its own.
#[lore_macro::test_pub]
fn sockaddr_for_path(path: &str) -> Result<SOCKADDR_UN, String> {
    let path_bytes = path.as_bytes();
    if path_bytes.len() >= SUN_PATH_CAPACITY {
        return Err(format!(
            "the socket path takes {} bytes, more than the {} an address holds \
             alongside its terminator: {path}",
            path_bytes.len(),
            SUN_PATH_CAPACITY - 1
        ));
    }

    // Zeroed, so writing fewer bytes than the array holds leaves the path
    // terminated.
    let mut sun_path: [i8; SUN_PATH_CAPACITY] = [0; SUN_PATH_CAPACITY];
    for (slot, byte) in sun_path.iter_mut().zip(path_bytes) {
        *slot = *byte as i8;
    }

    Ok(SOCKADDR_UN {
        sun_family: WinSock::AF_UNIX,
        sun_path,
    })
}
