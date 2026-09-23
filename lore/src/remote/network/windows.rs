// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::net::TcpStream;
use std::os::windows::io::FromRawSocket;
use std::os::windows::io::RawSocket;

use WinSock::WSADATA;
use WinSock::WSAStartup;
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
}

impl UdsListener {
    pub fn new(name: &str) -> Result<UdsListener, UdsListenerError> {
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
                &addr as *const SOCKADDR_UN as *const SOCKADDR,
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
        Ok(Self { socket: sock })
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
                &mut addr as *mut SOCKADDR_UN as *mut SOCKADDR,
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
                &addr as *const SOCKADDR_UN as *const SOCKADDR,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn written_path(addr: &SOCKADDR_UN) -> String {
        let bytes: Vec<u8> = addr
            .sun_path
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8)
            .collect();
        String::from_utf8(bytes).expect("the path was written as UTF-8")
    }

    #[test]
    fn a_path_that_fits_is_written_with_its_terminator() {
        let path = r"C:\Temp\lore_service";

        let addr = sockaddr_for_path(path).expect("a short path must fit");

        assert_eq!(addr.sun_family, WinSock::AF_UNIX);
        assert_eq!(written_path(&addr), path);
        assert_eq!(addr.sun_path[path.len()], 0, "the path must be terminated");
    }

    /// A temporary directory or configured socket name long enough to overrun
    /// `sun_path` is reported rather than overrunning the copy.
    #[test]
    fn a_path_longer_than_the_address_is_refused() {
        let path = format!(r"C:\Temp\{}", "n".repeat(200));

        // `SOCKADDR_UN` has no `Debug`, so the success arm is unwrapped by hand
        // rather than through `expect_err`.
        let Err(error) = sockaddr_for_path(&path) else {
            panic!("an overlong path must be refused");
        };

        assert!(error.contains("more than"), "{error}");
        assert!(
            error.contains(&path),
            "the failure must name the path: {error}"
        );
    }

    /// The terminator needs a byte of its own, so the longest usable path is one
    /// short of the array rather than exactly its length.
    #[test]
    fn a_path_that_exactly_fills_the_address_is_refused() {
        let exact = "a".repeat(SUN_PATH_CAPACITY);

        assert!(
            sockaddr_for_path(&exact).is_err(),
            "a path filling every byte leaves no room to terminate it"
        );
        assert!(
            sockaddr_for_path(&exact[1..]).is_ok(),
            "one byte shorter must fit"
        );
    }
}
