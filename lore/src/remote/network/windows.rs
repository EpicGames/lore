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

use crate::remote::LORE_SERVICE_SOCKET_NAME;
use crate::remote::network::UdsAcceptError;
use crate::remote::network::UdsConnectionError;
use crate::remote::network::UdsListenerError;

const LISTENER_BACKLOG: i32 = 10;

pub fn uds_supported() -> bool {
    true
}

pub struct UdsListener {
    socket: RawSocket,
}

impl UdsListener {
    pub fn new() -> Result<UdsListener, UdsListenerError> {
        let wide_file_name = uds_sock_path();

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

        let sock = uds_socket();
        let addr: SOCKADDR_UN =
            uds_sockaddr().ok_or_else(|| UdsListenerError::internal("bad temp path"))?;
        // Safety: Necessary to call windows APIs, only const pointers are passed to windows
        unsafe {
            if WinSock::bind(
                sock,
                &addr as *const SOCKADDR_UN as *const SOCKADDR,
                std::mem::size_of_val(&addr) as i32,
            ) != 0
            {
                return Err(UdsListenerError::internal(format!(
                    "failed to bind: {}",
                    WSAGetLastError()
                )));
            }

            if WinSock::listen(sock, LISTENER_BACKLOG) != 0 {
                return Err(UdsListenerError::internal(format!(
                    "failed to listen: {}",
                    WSAGetLastError()
                )));
            }
        }
        Ok(Self {
            socket: sock as RawSocket,
        })
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
                self.socket as SOCKET,
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

    pub fn connect() -> Result<UdsStream, UdsConnectionError> {
        if !wsa_startup() {
            // Safety: Necessary to call windows API
            return Err(UdsConnectionError::internal(format!(
                "failed to start winsock: {}",
                unsafe { WSAGetLastError() }
            )));
        }

        let sock = uds_socket();
        let addr: SOCKADDR_UN =
            uds_sockaddr().ok_or_else(|| UdsConnectionError::internal("bad temp path"))?;

        // Safety: Needed to call windows API. Only const pointers are passed to windows.
        // TcpStream::from_raw_socket requires the appropriate handle to be put in, we're fudging
        // things a little bit by putting a unix domain socket into a TcpStream, but the actual
        // stream sockets are supposed to behave identically.
        unsafe {
            if WinSock::connect(
                sock,
                &addr as *const SOCKADDR_UN as *const SOCKADDR,
                size_of_val(&addr) as i32,
            ) != 0
            {
                return Err(UdsConnectionError::internal(format!(
                    "failed to connect: {}",
                    WSAGetLastError()
                )));
            }
            Ok(UdsStream {
                stream: TcpStream::from_raw_socket(sock as std::os::windows::io::RawSocket),
            })
        }
    }
}

fn wsa_startup() -> bool {
    let mut data = WSADATA::default();
    // Safety: Necessary to call windows APIs, the mutable pointer passed in is properly initialized
    // in rust to a safe value
    let result = unsafe { WSAStartup(2 << 8 | 2, &mut data) };
    result == 0
}

fn uds_socket() -> WinSock::SOCKET {
    // Safety: Necessary to call windows APIs
    unsafe { WinSock::socket(WinSock::AF_UNIX as i32, WinSock::SOCK_STREAM, 0) }
}

fn uds_sock_path() -> Vec<u16> {
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
    path.extend(LORE_SERVICE_SOCKET_NAME.encode_utf16());
    // Reinsert the null terminator.
    path.push(0);
    path
}

fn uds_sockaddr() -> Option<SOCKADDR_UN> {
    let path_string = String::from_utf16(&uds_sock_path()).ok()?;
    let path_bytes: Vec<i8> = path_string.as_bytes().iter().map(|v| *v as i8).collect();
    let mut path: [i8; 108] = [0; 108];
    path[0..path_bytes.len()].copy_from_slice(&path_bytes);

    Some(SOCKADDR_UN {
        sun_family: WinSock::AF_UNIX,
        sun_path: path,
    })
}
