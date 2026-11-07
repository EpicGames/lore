// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Mounting and unmounting, per the spec's "Mounting" section: unprivileged via `fusermount3`
//! receiving the `/dev/fuse` fd over `SCM_RIGHTS`, with a `mount(2)` fallback when the caller
//! has `CAP_SYS_ADMIN` and `fusermount3` is unavailable or fails.

use std::ffi::CString;
use std::io::IoSliceMut;
use std::mem::size_of;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use crate::error::FuseError;

/// Mount options, per the spec's fixed option set for a lore mount.
pub struct MountOptions {
    pub fsname: String,
    pub max_read: u32,
    /// Overrides the `fusermount3` binary the mount looks for on `PATH`. Tests use this to
    /// exercise the "missing" path deterministically.
    pub fusermount: Option<PathBuf>,
}

/// Whether the kernel passes modifying system calls on to the filesystem. A read-only mount
/// refuses them with `EROFS` before any request is sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MountAccess {
    #[default]
    ReadOnly,
    ReadWrite,
}

impl MountOptions {
    /// The `-o` option string passed to `fusermount3` for a mount with `access`.
    pub fn option_string_for(&self, access: MountAccess) -> String {
        let read_only = match access {
            MountAccess::ReadOnly => "ro,",
            MountAccess::ReadWrite => "",
        };
        format!(
            "{read_only}default_permissions,max_read={},fsname={},subtype=lore,noatime",
            self.max_read, self.fsname
        )
    }

    /// Resolves the `fusermount3` binary: the configured override, or the first executable
    /// `fusermount3` on `PATH`.
    pub fn fusermount_path(&self) -> Result<PathBuf, FuseError> {
        if let Some(path) = &self.fusermount {
            return if is_executable_file(path) {
                Ok(path.clone())
            } else {
                Err(FuseError::FusermountMissing)
            };
        }

        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path_env| std::env::split_paths(&path_env).collect::<Vec<_>>())
            .map(|dir| dir.join("fusermount3"))
            .find(|candidate| is_executable_file(candidate))
            .ok_or(FuseError::FusermountMissing)
    }
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// A live mount: the `/dev/fuse` (or clone-source) fd the kernel handed back, and the path it
/// is mounted at.
pub struct Mount {
    fd: OwnedFd,
    path: PathBuf,
    mounted: AtomicBool,
}

impl Mount {
    /// The master device fd this mount's session clones per worker.
    pub fn device_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// The path this is mounted at.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Unmounts, idempotently: a second call, or a call after `unmount_path` was used
    /// directly, is a no-op rather than an error.
    pub fn unmount(&self, lazy: bool) -> Result<(), FuseError> {
        if !self.mounted.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        unmount_path(&self.path, lazy)
    }
}

/// Mounts `path` read-only with `options`.
pub fn mount(path: &Path, options: &MountOptions) -> Result<Mount, FuseError> {
    mount_with_access(path, options, MountAccess::ReadOnly)
}

/// Mounts `path` with `options` and `access`.
///
/// Tries the unprivileged `fusermount3` path first (spec: "Unprivileged"). Only when that path
/// cannot even be attempted or fails, and the caller has `CAP_SYS_ADMIN` (approximated here by
/// an effective uid of 0), falls back to a direct `mount(2)` (spec: "With `CAP_SYS_ADMIN`").
pub fn mount_with_access(
    path: &Path,
    options: &MountOptions,
    access: MountAccess,
) -> Result<Mount, FuseError> {
    match mount_via_fusermount(path, options, access) {
        Ok(mount) => Ok(mount),
        Err(err) => {
            if is_root() {
                mount_privileged(path, options, access)
            } else {
                Err(err)
            }
        }
    }
}

/// The `mount(2)` flags of the privileged fallback.
#[lore_macro::test_pub]
fn privileged_mount_flags(access: MountAccess) -> libc::c_ulong {
    let common = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOATIME;
    match access {
        MountAccess::ReadOnly => common | libc::MS_RDONLY,
        MountAccess::ReadWrite => common,
    }
}

fn is_root() -> bool {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

fn mount_via_fusermount(
    path: &Path,
    options: &MountOptions,
    access: MountAccess,
) -> Result<Mount, FuseError> {
    let fusermount = options.fusermount_path()?;

    let mut fds: [RawFd; 2] = [-1, -1];
    // SAFETY: `fds` is a valid, appropriately sized out-parameter.
    let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    if ret != 0 {
        return Err(FuseError::MountFailed(format!(
            "socketpair: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: both fds were just created by `socketpair` above and are open and valid.
    let parent_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: see above.
    let child_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    // The child execs `fusermount3`; only `child_end` should survive that exec.
    set_cloexec(&parent_end)?;

    let mut command = Command::new(&fusermount);
    command
        .arg("-o")
        .arg(options.option_string_for(access))
        .arg("--")
        .arg(path)
        .env("_FUSE_COMMFD", child_end.as_raw_fd().to_string());

    let mut child = command.spawn().map_err(|error| {
        FuseError::MountFailed(format!("spawning {}: {error}", fusermount.display()))
    })?;

    // The parent's copy of the child's end is no longer needed; dropping it here (rather than
    // waiting for the whole `Mount` build to finish) means EOF on `parent_end` reflects only
    // the child's own fd table, not this process holding a spare reference open.
    drop(child_end);

    let received = receive_fd(&parent_end);
    let status = child.wait().map_err(|error| {
        FuseError::MountFailed(format!("waiting for {}: {error}", fusermount.display()))
    })?;

    let fd = received?;
    if !status.success() {
        return Err(FuseError::MountFailed(format!(
            "{} exited with {status}",
            fusermount.display()
        )));
    }

    Ok(Mount {
        fd,
        path: path.to_path_buf(),
        mounted: AtomicBool::new(true),
    })
}

fn set_cloexec(fd: &OwnedFd) -> Result<(), FuseError> {
    // SAFETY: `fd` is a valid, open fd for the duration of this call.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
    if flags < 0 {
        return Err(FuseError::MountFailed(format!(
            "fcntl(F_GETFD): {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: see above.
    let ret = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    if ret < 0 {
        return Err(FuseError::MountFailed(format!(
            "fcntl(F_SETFD): {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Receives the `/dev/fuse` fd `fusermount3` sends back over `SCM_RIGHTS`.
fn receive_fd(sock: &OwnedFd) -> Result<OwnedFd, FuseError> {
    let mut iobuf = [0u8; 1];
    let mut iov = [IoSliceMut::new(&mut iobuf)];
    let mut cmsg_buf = [0u8; 128];

    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = iov.as_mut_ptr().cast();
    msg.msg_iovlen = iov.len() as _;
    msg.msg_control = cmsg_buf.as_mut_ptr().cast();
    msg.msg_controllen = cmsg_buf.len() as _;

    // SAFETY: `msg` points at valid, appropriately sized buffers for the duration of the call.
    let received = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, 0) };
    if received <= 0 {
        let reason = if received == 0 {
            "fusermount3 closed its end without sending a file descriptor".to_string()
        } else {
            format!("recvmsg: {}", std::io::Error::last_os_error())
        };
        return Err(FuseError::MountFailed(reason));
    }

    // SAFETY: `msg` was just populated by `recvmsg` above.
    let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    if cmsg.is_null() {
        return Err(FuseError::MountFailed(
            "fusermount3 sent no control message".to_string(),
        ));
    }
    // SAFETY: `cmsg` was checked non-null above and points into `cmsg_buf`.
    let cmsg_ref = unsafe { &*cmsg };
    if cmsg_ref.cmsg_level != libc::SOL_SOCKET || cmsg_ref.cmsg_type != libc::SCM_RIGHTS {
        return Err(FuseError::MountFailed(
            "fusermount3 sent an unexpected control message".to_string(),
        ));
    }

    let mut raw_fd: RawFd = -1;
    // SAFETY: `cmsg` carries `SCM_RIGHTS` data, checked above, at least `size_of::<RawFd>()`
    // bytes since the kernel only ever encodes whole fds.
    unsafe {
        let data = libc::CMSG_DATA(cmsg);
        std::ptr::copy_nonoverlapping(data, (&raw mut raw_fd).cast(), size_of::<RawFd>());
    }
    if raw_fd < 0 {
        return Err(FuseError::MountFailed(
            "fusermount3 sent an invalid file descriptor".to_string(),
        ));
    }

    // SAFETY: the kernel just handed this process fresh ownership of `raw_fd` via `SCM_RIGHTS`.
    Ok(unsafe { OwnedFd::from_raw_fd(raw_fd) })
}

/// Direct `mount(2)`, for a caller with `CAP_SYS_ADMIN` when `fusermount3` is unavailable.
fn mount_privileged(
    path: &Path,
    options: &MountOptions,
    access: MountAccess,
) -> Result<Mount, FuseError> {
    // SAFETY: opening a device node the process is trusted to talk to directly.
    let raw = unsafe { libc::open(c"/dev/fuse".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if raw < 0 {
        return Err(FuseError::MountFailed(format!(
            "open(/dev/fuse): {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: `raw` was just returned by a successful `open` above.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let data = format!(
        "fd={},rootmode=40000,user_id={uid},group_id={gid},default_permissions,max_read={}",
        fd.as_raw_fd(),
        options.max_read
    );

    let source = CString::new(options.fsname.as_str()).map_err(|e| {
        FuseError::MountFailed(format!(
            "mount source contains a NUL at byte {}",
            e.nul_position()
        ))
    })?;
    let target = CString::new(path.as_os_str().as_bytes()).map_err(|e| {
        FuseError::MountFailed(format!(
            "mount target contains a NUL at byte {}",
            e.nul_position()
        ))
    })?;
    let fstype = CString::new("fuse.lore").map_err(|e| {
        FuseError::MountFailed(format!(
            "mount fstype contains a NUL at byte {}",
            e.nul_position()
        ))
    })?;
    let data_c = CString::new(data).map_err(|e| {
        FuseError::MountFailed(format!(
            "mount data contains a NUL at byte {}",
            e.nul_position()
        ))
    })?;

    // SAFETY: all four pointers are valid, NUL-terminated `CString`s kept alive for the call.
    let ret = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            privileged_mount_flags(access),
            data_c.as_ptr().cast(),
        )
    };
    if ret != 0 {
        return Err(FuseError::MountFailed(format!(
            "mount(2): {}",
            std::io::Error::last_os_error()
        )));
    }

    Ok(Mount {
        fd,
        path: path.to_path_buf(),
        mounted: AtomicBool::new(true),
    })
}

/// Unmounts whatever is mounted at `path`: `fusermount3 -u [-z]` first, falling back to
/// `umount2(2)` when `fusermount3` is unavailable or fails.
const UNMOUNT_BUSY_RETRIES: u32 = 5;
const UNMOUNT_BUSY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(20);

pub fn unmount_path(path: &Path, lazy: bool) -> Result<(), FuseError> {
    // SAFETY: geteuid has no preconditions.
    let privileged = unsafe { libc::geteuid() } == 0;
    if let Some(fusermount) = locate_fusermount3() {
        let mut reason = String::new();
        // Desktop indexers and file managers probe new mounts and briefly hold them busy, so a
        // plain unmount gets a few short retries before it is reported as busy.
        for attempt in 0..UNMOUNT_BUSY_RETRIES {
            let mut command = Command::new(&fusermount);
            command.arg("-u");
            if lazy {
                command.arg("-z");
            }
            command.arg(path);
            match command.output() {
                Ok(output) if output.status.success() => return Ok(()),
                Ok(output) => {
                    reason = String::from_utf8_lossy(&output.stderr).trim().to_string();
                }
                Err(error) => reason = format!("running fusermount3: {error}"),
            }
            if lazy || !reason.contains("busy") || attempt + 1 == UNMOUNT_BUSY_RETRIES {
                break;
            }
            std::thread::sleep(UNMOUNT_BUSY_BACKOFF);
        }
        // Only root can unmount directly; for anyone else umount2 would just mask the real
        // reason behind EPERM.
        if !privileged {
            return Err(FuseError::UnmountFailed(reason));
        }
    }

    let target = CString::new(path.as_os_str().as_bytes()).map_err(|e| {
        FuseError::UnmountFailed(format!(
            "unmount target contains a NUL at byte {}",
            e.nul_position()
        ))
    })?;
    let flags = if lazy { libc::MNT_DETACH } else { 0 };
    // SAFETY: `target` is a valid, NUL-terminated `CString` kept alive for the call.
    let ret = unsafe { libc::umount2(target.as_ptr(), flags) };
    if ret != 0 {
        return Err(FuseError::UnmountFailed(format!(
            "umount2: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

#[lore_macro::test_pub]
fn locate_fusermount3() -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path_env| std::env::split_paths(&path_env).collect::<Vec<_>>())
        .map(|dir| dir.join("fusermount3"))
        .find(|candidate| is_executable_file(candidate))
}

/// Whether the mount table holds a mount of `source`, the `fsname` it was mounted with, read from
/// `/proc/self/mountinfo` so the mount itself is never touched. Matched on the source alone,
/// which is unique per instance, because the path a mount is listed under changes when a parent
/// directory is renamed. A table that cannot be read reports it as mounted.
pub fn is_mounted(source: &str) -> bool {
    match std::fs::read("/proc/self/mountinfo") {
        Ok(table) => mount_table_lists(&table, source),
        Err(_) => true,
    }
}

/// Whether the `/proc/self/mountinfo` contents `table` list a mount of `source`: the second field
/// after the ` - ` separator, which follows a variable number of optional fields.
#[lore_macro::test_pub]
fn mount_table_lists(table: &[u8], source: &str) -> bool {
    let source = mountinfo_escaped(source.as_bytes());
    table.split(|&byte| byte == b'\n').any(|line| {
        line.windows(3)
            .position(|window| window == b" - ")
            .is_some_and(|separator| {
                line[separator + 3..].split(|&byte| byte == b' ').nth(1) == Some(source.as_slice())
            })
    })
}

/// `bytes` as the kernel writes a path into `/proc/self/mountinfo`: space, tab, newline and
/// backslash as three-digit octal escapes.
fn mountinfo_escaped(bytes: &[u8]) -> Vec<u8> {
    let mut escaped = Vec::with_capacity(bytes.len());
    for &byte in bytes {
        if matches!(byte, b' ' | b'\t' | b'\n' | b'\\') {
            escaped.extend_from_slice(format!("\\{byte:03o}").as_bytes());
        } else {
            escaped.push(byte);
        }
    }
    escaped
}

/// Whether `path` is a stale FUSE mount: its device end closed without a matching unmount, so
/// the kernel answers every access with `ENOTCONN`.
pub fn is_stale_mount(path: &Path) -> bool {
    let Ok(target) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `target` is a valid, NUL-terminated `CString`, and `buf` is a valid out-parameter
    // sized for `libc::statfs`.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::statfs(target.as_ptr(), &mut buf) };
    if ret == 0 {
        return false;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOTCONN)
}
