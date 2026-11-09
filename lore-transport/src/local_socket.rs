// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Stream sockets at a path in the filesystem: `AF_UNIX`, on Unix and on Windows alike.
//!
//! A server claims its socket's path before it binds it, so that of two processes starting at
//! once only one serves, and so that a socket another process still answers on is never removed
//! from under it: Windows lets a bound socket file be deleted, and its server then runs on
//! unreachable. The claim is a lock file beside the socket. It is held until the listener drops,
//! which removes the socket file first. The lock file stays: removing it would let a later
//! claimer lock a new file while an earlier holder still has the old one.
//!
//! [`Listener`] and [`Stream`] block, for callers that give them threads of their own.
//! [`AsyncListener`], [`AsyncStream`] and [`connect`] put the same sockets on the runtime. Tokio
//! has no `AF_UNIX` types on Windows, so there the async types are its TCP ones holding an
//! `AF_UNIX` socket: stream sockets read, write and report readiness alike, whatever their family.
use std::io;
use std::path::Path;
use std::path::PathBuf;

use lore_base::fs::lock::FSLock;
use socket2::Domain;
use socket2::SockAddr;
use socket2::Socket;
use socket2::Type;

/// Connections a listener queues before they are accepted.
const BACKLOG: i32 = 1024;

/// Flags of every write to a socket here. On Linux `MSG_NOSIGNAL`, so that a write to a peer that
/// has gone fails with `EPIPE` rather than raise `SIGPIPE`, whose default action ends a process
/// that hosts the library and has not ignored it. Elsewhere none: on macOS `Socket::new` and the
/// runtime open every socket with `SO_NOSIGPIPE`, and an accepted socket takes it from its
/// listener; Windows has no such signal.
#[cfg(target_os = "linux")]
const SEND_FLAGS: std::ffi::c_int = libc::MSG_NOSIGNAL;
#[cfg(not(target_os = "linux"))]
const SEND_FLAGS: std::ffi::c_int = 0;

/// A connected socket on the runtime.
#[cfg(target_family = "unix")]
pub type AsyncStream = tokio::net::UnixStream;
/// The read side of an [`AsyncStream`] split in two.
#[cfg(target_family = "unix")]
pub type AsyncReadHalf = tokio::net::unix::OwnedReadHalf;
/// The write side of an [`AsyncStream`] split in two.
#[cfg(target_family = "unix")]
pub type AsyncWriteHalf = tokio::net::unix::OwnedWriteHalf;

/// A connected socket on the runtime.
#[cfg(target_family = "windows")]
pub type AsyncStream = tokio::net::TcpStream;
/// The read side of an [`AsyncStream`] split in two.
#[cfg(target_family = "windows")]
pub type AsyncReadHalf = tokio::net::tcp::OwnedReadHalf;
/// The write side of an [`AsyncStream`] split in two.
#[cfg(target_family = "windows")]
pub type AsyncWriteHalf = tokio::net::tcp::OwnedWriteHalf;

/// Names `path` in an error from `doing` something with it.
fn at<'a>(path: &'a Path, doing: &'static str) -> impl FnOnce(io::Error) -> io::Error + 'a {
    move |err| io::Error::new(err.kind(), format!("{doing} {}: {err}", path.display()))
}

/// The address of the socket at `path`, which fails for a path longer than an address holds.
fn address(path: &Path) -> io::Result<SockAddr> {
    SockAddr::unix(path).map_err(at(path, "addressing"))
}

fn new_socket() -> io::Result<Socket> {
    Socket::new(Domain::UNIX, Type::STREAM, None)
}

/// Writes as much of `buffers` to `stream` as it takes without blocking. A peer that has gone
/// fails the write with `EPIPE` rather than raising `SIGPIPE`: on Linux the write passes
/// `MSG_NOSIGNAL`, which a vectored write through the stream does not, and elsewhere the socket
/// raises no such signal.
pub fn try_send_vectored(stream: &AsyncStream, buffers: &[io::IoSlice<'_>]) -> io::Result<usize> {
    #[cfg(target_os = "linux")]
    {
        stream.try_io(tokio::io::Interest::WRITABLE, || {
            socket2::SockRef::from(stream).send_vectored_with_flags(buffers, SEND_FLAGS)
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        stream.try_write_vectored(buffers)
    }
}

/// A connected socket that blocks. A write to a peer that has gone fails with `EPIPE` rather than
/// raise `SIGPIPE`.
pub struct Stream {
    socket: Socket,
}

impl Stream {
    /// Connects to whatever listens at `path`, and fails at once when nothing does.
    pub fn connect(path: &Path) -> io::Result<Self> {
        let socket = new_socket()?;
        socket
            .connect(&address(path)?)
            .map_err(at(path, "connecting to"))?;
        Ok(Self { socket })
    }

    /// A second handle to the same connection.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            socket: self.socket.try_clone()?,
        })
    }
}

impl io::Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        io::Read::read(&mut self.socket, buf)
    }
}

impl io::Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.socket.send_with_flags(buf, SEND_FLAGS)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(&mut self.socket)
    }
}

/// The claim on a socket's path, held by at most one process from [`Claim::acquire`] until the
/// listener it becomes drops.
pub struct Claim {
    path: PathBuf,
    lock: FSLock,
}

impl Claim {
    /// Claims `path`, whose directory must exist: `None` while another process holds the claim,
    /// or while something answers on the socket, as a server from a build that takes no claim
    /// may.
    pub fn acquire(path: &Path) -> io::Result<Option<Self>> {
        // The lock file lies in the socket's directory, which a bare file name has none of.
        let path = std::path::absolute(path).map_err(at(path, "resolving"))?;
        let Some(lock) =
            FSLock::try_acquire_file_lock(&path).map_err(at(&path, "locking the claim on"))?
        else {
            return Ok(None);
        };
        if Stream::connect(&path).is_ok() {
            return Ok(None);
        }
        Ok(Some(Self { path, lock }))
    }

    /// The claimed path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Binds the claimed path and listens on it, first removing whatever file is at the path once
    /// nothing answers on it, a socket or not. On Unix the socket file gets the permission bits
    /// `mode`; on Windows it keeps what its directory grants, and lets the machine's users connect
    /// when `mode` lets others write. Either holds before the socket takes its first connection.
    pub fn listen(self, mode: u32) -> io::Result<Listener> {
        let Self { path, lock } = self;
        let socket_address = address(&path)?;
        if Stream::connect(&path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("a server already listens on {}", path.display()),
            ));
        }
        match std::fs::remove_file(&path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => {
                return Err(at(&path, "removing the file at")(err));
            }
            _ => {}
        }
        let socket = new_socket()?;
        socket.bind(&socket_address).map_err(at(&path, "binding"))?;
        // From here the socket file is this listener's, to remove however it ends.
        let bound = Bound { path, _lock: lock };
        // A client connects only once the socket listens, so none connects under the umask's mode.
        set_mode(&bound.path, mode)?;
        socket
            .listen(BACKLOG)
            .map_err(at(&bound.path, "listening on"))?;
        Ok(Listener { socket, bound })
    }
}

/// A bound socket's path and its claim. Dropping it removes the socket file, then gives up the
/// claim.
struct Bound {
    path: PathBuf,
    _lock: FSLock,
}

impl Drop for Bound {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(target_family = "unix")]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(at(path, "setting permissions on"))
}

/// Lets the machine's users connect to the socket at `path` when `mode` lets others write it, as
/// connecting takes on Unix. Otherwise the socket keeps what its directory grants.
#[cfg(target_family = "windows")]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    if mode & 0o002 == 0 {
        return Ok(());
    }
    security::let_users_connect(path).map_err(at(path, "setting permissions on"))
}

/// Creates `directory`, the place for a server's sockets, when it is missing. Its permissions are
/// left as the operator set them, as a container's bind mount does.
#[cfg(target_family = "unix")]
pub fn prepare_directory(directory: &Path) -> io::Result<()> {
    std::fs::create_dir_all(directory).map_err(at(directory, "creating"))
}

/// Creates `directory`, the place for a server's sockets, when it is missing, and refuses one that
/// another account could place a socket of its own in for clients to find. A directory this
/// creates lets the system, the administrators and its owner change it, and the machine's users
/// list it. One that exists must be owned by one of those or by this process's user, and let no
/// one else change it.
#[cfg(target_family = "windows")]
pub fn prepare_directory(directory: &Path) -> io::Result<()> {
    if !directory.is_dir() {
        if let Some(parent) = directory.parent() {
            std::fs::create_dir_all(parent).map_err(at(parent, "creating"))?;
        }
        security::create_directory(directory).map_err(at(directory, "creating"))?;
    }
    security::refuse_shared(directory)
}

/// A listening socket that blocks in [`accept`](Self::accept).
pub struct Listener {
    socket: Socket,
    bound: Bound,
}

impl Listener {
    /// The path the listener is bound at.
    pub fn path(&self) -> &Path {
        &self.bound.path
    }

    /// The next connection, waiting for one.
    pub fn accept(&self) -> io::Result<Stream> {
        let (socket, _peer) = self.socket.accept()?;
        Ok(Stream { socket })
    }

    /// The same listener on the runtime. Its readiness comes from the runtime this is called on,
    /// and an accepted connection's from the runtime [`AsyncListener::accept`] is polled on.
    pub fn into_async(self) -> io::Result<AsyncListener> {
        let Self { socket, bound } = self;
        socket.set_nonblocking(true)?;
        #[cfg(target_family = "unix")]
        let listener = tokio::net::UnixListener::from_std(socket.into())?;
        #[cfg(target_family = "windows")]
        let listener = tokio::net::TcpStream::from_std(socket.into())?;
        Ok(AsyncListener { listener, bound })
    }
}

/// A listening socket on the runtime.
pub struct AsyncListener {
    #[cfg(target_family = "unix")]
    listener: tokio::net::UnixListener,
    /// The listening socket, held as a stream: the runtime has no listener type for `AF_UNIX`
    /// here, and a stream is how it polls a socket it did not create. A pending connection makes
    /// a listening socket readable.
    #[cfg(target_family = "windows")]
    listener: tokio::net::TcpStream,
    bound: Bound,
}

impl AsyncListener {
    /// The path the listener is bound at.
    pub fn path(&self) -> &Path {
        &self.bound.path
    }
}

#[cfg(target_family = "unix")]
impl AsyncListener {
    /// The next connection.
    pub async fn accept(&self) -> io::Result<AsyncStream> {
        let (stream, _peer) = self.listener.accept().await?;
        Ok(stream)
    }
}

#[cfg(target_family = "windows")]
impl AsyncListener {
    /// The next connection.
    pub async fn accept(&self) -> io::Result<AsyncStream> {
        loop {
            self.listener.readable().await?;
            // A connection another waiter took leaves nothing to accept, and clears the readiness.
            let accepted = self.listener.try_io(tokio::io::Interest::READABLE, || {
                socket2::SockRef::from(&self.listener).accept()
            });
            match accepted {
                Ok((socket, _peer)) => {
                    socket.set_nonblocking(true)?;
                    return tokio::net::TcpStream::from_std(socket.into());
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                Err(err) => return Err(err),
            }
        }
    }
}

/// Connects to whatever listens at `path`, on the runtime.
#[cfg(target_family = "unix")]
pub async fn connect(path: &Path) -> io::Result<AsyncStream> {
    tokio::net::UnixStream::connect(path)
        .await
        .map_err(at(path, "connecting to"))
}

/// Connects to whatever listens at `path`, on the runtime.
#[cfg(target_family = "windows")]
pub async fn connect(path: &Path) -> io::Result<AsyncStream> {
    let socket = new_socket()?;
    socket.set_nonblocking(true)?;
    // A connect that would block goes on without the caller; the socket turns writable once it
    // has connected or failed.
    match socket.connect(&address(path)?) {
        Err(err) if err.kind() != io::ErrorKind::WouldBlock => {
            return Err(at(path, "connecting to")(err));
        }
        _ => {}
    }
    let stream = tokio::net::TcpStream::from_std(socket.into())?;
    stream.writable().await?;
    match stream.take_error()? {
        Some(err) => Err(at(path, "connecting to")(err)),
        None => Ok(stream),
    }
}

#[cfg(target_family = "windows")]
mod security {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Foundation::GENERIC_ALL;
    use windows_sys::Win32::Foundation::GENERIC_WRITE;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::ACCESS_ALLOWED_ACE;
    use windows_sys::Win32::Security::ACE_HEADER;
    use windows_sys::Win32::Security::ACL;
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::Authorization::GetNamedSecurityInfoW;
    use windows_sys::Win32::Security::Authorization::SDDL_REVISION_1;
    use windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT;
    use windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW;
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::EqualSid;
    use windows_sys::Win32::Security::GetAce;
    use windows_sys::Win32::Security::GetSecurityDescriptorDacl;
    use windows_sys::Win32::Security::GetTokenInformation;
    use windows_sys::Win32::Security::INHERIT_ONLY_ACE;
    use windows_sys::Win32::Security::IsWellKnownSid;
    use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;
    use windows_sys::Win32::Security::PSID;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Security::TOKEN_QUERY;
    use windows_sys::Win32::Security::TOKEN_USER;
    use windows_sys::Win32::Security::TokenUser;
    use windows_sys::Win32::Security::UNPROTECTED_DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::WinBuiltinAdministratorsSid;
    use windows_sys::Win32::Security::WinCreatorOwnerRightsSid;
    use windows_sys::Win32::Security::WinLocalSystemSid;
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
    use windows_sys::Win32::Storage::FileSystem::DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_ADD_FILE;
    use windows_sys::Win32::Storage::FileSystem::FILE_ADD_SUBDIRECTORY;
    use windows_sys::Win32::Storage::FileSystem::FILE_DELETE_CHILD;
    use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;
    use windows_sys::Win32::Storage::FileSystem::WRITE_OWNER;
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
    use windows_sys::Win32::System::SystemServices::ACCESS_DENIED_ACE_TYPE;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    use windows_sys::Win32::System::Threading::OpenProcessToken;

    /// A socket directory: changes for the system, the administrators and the directory's owner,
    /// listing and traversal for users (`S-1-5-32-545`). Protected, so it inherits nothing from
    /// its parent.
    const DIRECTORY: &str =
        "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;OW)(A;;FRFX;;;S-1-5-32-545)";

    /// Reading and writing a socket, which connecting to it takes, for users (`S-1-5-32-545`).
    const CONNECT: &str = "D:(A;;FRFW;;;S-1-5-32-545)";

    /// Rights over a directory that let their holder place, replace or remove an entry in it.
    const CHANGE: u32 = FILE_ADD_FILE
        | FILE_ADD_SUBDIRECTORY
        | FILE_DELETE_CHILD
        | DELETE
        | WRITE_DAC
        | WRITE_OWNER
        | GENERIC_WRITE
        | GENERIC_ALL;

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn os_error(status: u32) -> io::Error {
        io::Error::from_raw_os_error(status as i32)
    }

    fn shared(reason: &str) -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, reason.to_string())
    }

    /// A security descriptor the system allocated, freed when dropped.
    struct Descriptor(PSECURITY_DESCRIPTOR);

    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: the system allocated the descriptor with `LocalAlloc`, and nothing else
            // frees it.
            unsafe { LocalFree(self.0) };
        }
    }

    impl Descriptor {
        fn parse(sddl: &str) -> io::Result<Self> {
            let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
            let mut descriptor = ptr::null_mut();
            // SAFETY: `sddl` is NUL terminated, and the descriptor is written to a local.
            let parsed = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            };
            if parsed == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(descriptor))
        }

        /// The descriptor's access list, which lives as long as the descriptor.
        fn dacl(&self) -> io::Result<*mut ACL> {
            let (mut present, mut defaulted, mut dacl) = (0, 0, ptr::null_mut());
            // SAFETY: the descriptor is valid while `self` is, and the outputs are locals.
            let read = unsafe {
                GetSecurityDescriptorDacl(self.0, &mut present, &mut dacl, &mut defaulted)
            };
            if read == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(dacl)
        }
    }

    /// A handle the process opened, closed when dropped.
    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle is open, and nothing else closes it.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// This process's user, as its token names it: a `TOKEN_USER` whose SID lies in the same
    /// buffer, held in words for the structure's alignment.
    struct ProcessUser(Vec<u64>);

    impl ProcessUser {
        fn read() -> io::Result<Self> {
            let mut token = ptr::null_mut();
            // SAFETY: the process's pseudo handle needs no closing, and the token is written to a
            // local.
            if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let token = Handle(token);
            let mut size = 0;
            // SAFETY: a query of the size alone, which writes nothing but `size`.
            unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut size) };
            let mut buffer = vec![0u64; (size as usize).div_ceil(size_of::<u64>())];
            // SAFETY: the buffer holds `size` bytes, aligned for a `TOKEN_USER`.
            let read = unsafe {
                GetTokenInformation(
                    token.0,
                    TokenUser,
                    buffer.as_mut_ptr().cast(),
                    size,
                    &mut size,
                )
            };
            if read == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(buffer))
        }

        fn sid(&self) -> PSID {
            // SAFETY: the buffer holds the `TOKEN_USER` `read` wrote.
            unsafe { (*self.0.as_ptr().cast::<TOKEN_USER>()).User.Sid }
        }
    }

    pub(super) fn create_directory(directory: &Path) -> io::Result<()> {
        let descriptor = Descriptor::parse(DIRECTORY)?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        // SAFETY: the path is NUL terminated, and the attributes and their descriptor outlive the
        // call.
        if unsafe { CreateDirectoryW(wide(directory).as_ptr(), &attributes) } == 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32) {
                return Err(err);
            }
        }
        Ok(())
    }

    pub(super) fn let_users_connect(path: &Path) -> io::Result<()> {
        let descriptor = Descriptor::parse(CONNECT)?;
        // SAFETY: the path is NUL terminated, and the list lives in `descriptor` through the call.
        let status = unsafe {
            SetNamedSecurityInfoW(
                wide(path).as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                descriptor.dacl()?,
                ptr::null(),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(os_error(status));
        }
        Ok(())
    }

    /// Refuses `directory` unless the system, the administrators or this process's user owns it,
    /// and only they and the directory's owner may change it.
    pub(super) fn refuse_shared(directory: &Path) -> io::Result<()> {
        let refused = |err: io::Error| super::at(directory, "using socket directory")(err);
        let user = ProcessUser::read().map_err(refused)?;
        let (mut owner, mut dacl, mut descriptor) =
            (ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
        // SAFETY: the path is NUL terminated; the owner and list point into the descriptor.
        let status = unsafe {
            GetNamedSecurityInfoW(
                wide(directory).as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(refused(os_error(status)));
        }
        let _descriptor = Descriptor(descriptor);
        // SAFETY: the SIDs lie in buffers that outlive these calls.
        let trusted = |sid: PSID| unsafe {
            IsWellKnownSid(sid, WinLocalSystemSid) != 0
                || IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0
                || IsWellKnownSid(sid, WinCreatorOwnerRightsSid) != 0
                || EqualSid(sid, user.sid()) != 0
        };
        if !trusted(owner) {
            return Err(refused(shared("another account owns it")));
        }
        if dacl.is_null() {
            return Err(refused(shared(
                "it has no access list, so anyone may change it",
            )));
        }
        // SAFETY: the list lies in the descriptor, and every entry `GetAce` yields within it.
        unsafe {
            for index in 0..u32::from((*dacl).AceCount) {
                let mut entry = ptr::null_mut();
                if GetAce(dacl, index, &mut entry) == 0 {
                    return Err(refused(io::Error::last_os_error()));
                }
                let header = *entry.cast::<ACE_HEADER>();
                let kind = u32::from(header.AceType);
                if kind == ACCESS_DENIED_ACE_TYPE
                    || u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0
                {
                    continue;
                }
                if kind != ACCESS_ALLOWED_ACE_TYPE {
                    return Err(refused(shared("it holds an access entry of another kind")));
                }
                let allowed = entry.cast::<ACCESS_ALLOWED_ACE>();
                let sid = ptr::addr_of_mut!((*allowed).SidStart).cast();
                if (*allowed).Mask & CHANGE != 0 && !trusted(sid) {
                    return Err(refused(shared("another account may change it")));
                }
            }
        }
        Ok(())
    }
}
