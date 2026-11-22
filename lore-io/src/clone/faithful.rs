// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The entries of a faithful copy on Linux and macOS: a file's data with its holes, and the
//! owner, extended attributes, access control lists, permission bits and times to the nanosecond
//! of files, directories and links.
//!
//! Holes come from [`crate::holes::next_data`], so an unwritten extent is copied as a hole, as it
//! reads; the ranges of data between them are copied with `copy_file_range` on Linux, or through
//! a buffer of the thread's where it can't copy across the two file systems and on macOS, which
//! has no such call. APFS fills a hole of under 16 MiB beside data when it writes that data out,
//! so a copy there keeps only the larger holes, as the source does.
//!
//! On Linux a POSIX ACL is the `system.posix_acl_access` attribute, and a directory's default ACL
//! `system.posix_acl_default`, so they are copied with the other extended attributes. On macOS
//! an extended ACL is not an attribute `listxattr` lists, and is copied with `acl_get_fd_np` and
//! `acl_set_fd_np`, or their `_link_np` forms for a link. `fcopyfile` would copy attributes and
//! ACLs too, but it fails or skips them for the file as a whole, where the copy decides for each
//! attribute, and it sets no time to the nanosecond.
//!
//! Each entry takes its owner first, since `chown` clears the set-user-ID and set-group-ID bits
//! and the `security.capability` attribute; then its extended attributes, while the copy can
//! still be written, which `user.*` attributes take; then its permission bits, and its times.
//! On macOS its extended ACL comes last, since a deny entry can keep even the owner from setting
//! the rest. A directory takes all of these once its last entry is in place, since an entry
//! created in it changes its times, and would inherit an ACL set before it. The copy's root does
//! too, and loses the ACL it inherited from its parent before the copy starts. On Linux, source
//! files and directories are opened with `O_NOATIME` where the caller owns them, so reading them
//! leaves their access times as they are. Each entry takes the access time it had before the copy
//! read it, since on macOS, which has no `O_NOATIME`, reading a file or listing a directory sets
//! it. A link's times are read before the link itself, which sets its access time.
//!
//! An entry marked immutable or append-only fails the copy, since the move couldn't remove the
//! old root that holds it: `FS_IMMUTABLE_FL` and `FS_APPEND_FL` on Linux, read from `statx`'s
//! attributes, and `UF_IMMUTABLE`, `UF_APPEND`, `SF_IMMUTABLE` and `SF_APPEND` on macOS, read
//! from `st_flags`, with the status the copy takes of each entry anyway. The other flags, which
//! mark how a file is shown, backed up or stored, such as a compressed or hidden file's, are left
//! out.
//!
//! An attribute or ACL a source link answers as unsupported is one the link doesn't have, and the
//! copy goes on without it; what the source has and the target can't hold fails the copy, which
//! names the call. On macOS a link's ACL is read and set through a descriptor of the link itself. A
//! `security.*` attribute that the system's policy refuses to set, as a label or a capability may
//! be on Linux, or that the target refuses as unsupported while it holds one, as the system gives
//! each new entry, keeps the value the target gives it, and is counted in [`Kept`]. A caller
//! that isn't root lists no `trusted.*` attribute, so none is copied.
//!
//! On macOS, each file is flushed with `fsync` on its copying thread, which writes it to its
//! drive; the copy's one `F_FULLFSYNC`, after the last file, empties the drive's cache.

use std::cell::Cell;
use std::ffi::CStr;
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use super::COPY_BUFFER;
use super::Kept;
use super::TestPoints;
use super::entry;
use super::entry::OpenedSource;
use super::failed_call;
use super::refused;
use super::source::OpenDirectory;
use super::source::Source;

thread_local! {
    /// The buffer a thread copies data through, and the buffers it reads attribute names and
    /// values into, kept from one entry to the next.
    static BUFFERS: Cell<(Vec<u8>, Vec<u8>, Vec<u8>)> =
        const { Cell::new((Vec::new(), Vec::new(), Vec::new())) };
}

/// What the entries of one faithful copy share: the count of what the target kept, and the copy's
/// test points.
#[derive(Clone, Copy)]
pub struct Copying<'a> {
    pub kept: &'a Kept,
    pub points: &'a TestPoints,
}

/// Copies the file `source` names to `destination`, which must not exist, with its holes, owner,
/// extended attributes, ACL, permission bits and times, and copies a link found there as
/// [`link`] does. Refuses a file with more than one link.
pub fn file(source: Source<'_>, destination: &Path, copying: Copying<'_>) -> io::Result<()> {
    let input = match open_source(source)? {
        OpenedSource::File(input) => input,
        OpenedSource::Link => return link(source, destination, copying),
    };
    let status = Status::of(Entry::Fd(input.as_raw_fd()))?;
    status.refuse_links_and_flags(&source, copying.points)?;
    let output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)?;
    let copied = copy_keeping_holes(&input, &output, status.len).and_then(|()| {
        let after = Status {
            atime: status.atime,
            ..Status::of(Entry::Fd(input.as_raw_fd()))?
        };
        finish(
            Node::Fd(input.as_raw_fd()),
            output.as_raw_fd(),
            &after,
            copying,
        )?;
        flush_file(&output, copying.points)
    });
    if copied.is_err() {
        drop(output);
        let _ = std::fs::remove_file(destination);
    }
    copied
}

/// Opens the file `source` names, on Linux leaving its access time as it is.
fn open_source(source: Source<'_>) -> io::Result<OpenedSource> {
    #[cfg(target_os = "linux")]
    {
        entry::open_source_keeping_atime(source)
    }
    #[cfg(not(target_os = "linux"))]
    {
        entry::open_source(source)
    }
}

/// Flushes the copied file `output` on macOS, as the module describes.
#[cfg(target_os = "macos")]
fn flush_file(output: &File, points: &TestPoints) -> io::Result<()> {
    // SAFETY: the descriptor is open for the call.
    check(unsafe { libc::fsync(output.as_raw_fd()) })?;
    #[cfg(feature = "test-util")]
    points
        .flushed_files
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    #[cfg(not(feature = "test-util"))]
    let _ = points;
    Ok(())
}

/// Nothing on Linux, where the copy's one `syncfs` flushes every file.
#[cfg(target_os = "linux")]
#[allow(clippy::unnecessary_wraps)]
fn flush_file(_output: &File, _points: &TestPoints) -> io::Result<()> {
    Ok(())
}

/// Copies the symbolic link `source` names to `destination` as a link, with its owner, extended
/// attributes, ACL and times. Refuses a link with more than one name.
pub fn link(source: Source<'_>, destination: &Path, copying: Copying<'_>) -> io::Result<()> {
    let name = source.name_for_call()?;
    let status = Status::of(Entry::Named(source.parent_fd(), &name))?;
    status.refuse_links_and_flags(&source, copying.points)?;
    entry::link(source, destination)?;
    let from = CString::new(source.as_os_str().as_bytes())?;
    let to = CString::new(destination.as_os_str().as_bytes())?;
    let finished = (|| {
        set_owner(Node::Link(&to), &status, copying)?;
        copy_attributes(Node::Link(&from), Node::Link(&to), copying)?;
        #[cfg(target_os = "macos")]
        set_link_mode(&to, status.mode)
            .map_err(|err| failed_call(err, "setting the permission bits"))?;
        let times = status.times();
        // SAFETY: the path is NUL-terminated, `times` holds two entries, and the call doesn't
        // follow the link.
        check(unsafe {
            libc::utimensat(
                libc::AT_FDCWD,
                to.as_ptr(),
                times.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        })
        .map_err(|err| failed_call(err, "setting the times"))?;
        copy_acl(Node::Link(&from), Node::Link(&to))
    })();
    if finished.is_err() {
        let _ = std::fs::remove_file(destination);
    }
    finished
}

/// Gives the link at `path` the permission bits of `mode`, which a link holds of its own on macOS.
#[cfg(target_os = "macos")]
fn set_link_mode(path: &CStr, mode: u32) -> io::Result<()> {
    let mode = libc::mode_t::try_from(mode & entry::PERMISSION_BITS).map_err(io::Error::other)?;
    // SAFETY: the path is NUL-terminated, and the call doesn't follow the link.
    check(unsafe {
        libc::fchmodat(
            libc::AT_FDCWD,
            path.as_ptr(),
            mode,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })
}

/// Gives the directory `destination` the owner, extended attributes, ACL, permission bits and
/// times of `source`, the directory it copies, once every entry is in place.
pub fn finish_directory(
    source: &OpenDirectory,
    destination: &Path,
    copying: Copying<'_>,
) -> io::Result<()> {
    let output = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(destination)?;
    let mut status = Status::of(Entry::Fd(source.fd().as_raw_fd()))?;
    status.refuse_links_and_flags(destination, copying.points)?;
    if let Some(atime) = source.atime() {
        status.atime = atime;
    }
    finish(
        Node::Fd(source.fd().as_raw_fd()),
        output.as_raw_fd(),
        &status,
        copying,
    )
}

/// Refuses the root of a copy, `source`, where [`refuses_flags`] refuses its flags, before the
/// copy starts rather than once its last entry is in place.
pub fn refuse_root(source: &Path, points: &TestPoints) -> io::Result<()> {
    let root = File::open(source)?;
    Status::of(Entry::Fd(root.as_raw_fd()))?.refuse_links_and_flags(source, points)
}

/// Clears the ACL that `destination`, the empty root of a copy, inherited from its parent, so that
/// no entry the copy creates in it inherits one. A file system that holds no ACLs has none.
pub fn clear_inherited_acl(destination: &Path) -> io::Result<()> {
    let root = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(destination)?;
    #[cfg(target_os = "linux")]
    for name in [c"system.posix_acl_default", c"system.posix_acl_access"] {
        // SAFETY: the descriptor is open, and the name is NUL-terminated.
        match check(unsafe { libc::fremovexattr(root.as_raw_fd(), name.as_ptr()) }) {
            Err(err) if matches!(err.raw_os_error(), Some(NO_ATTRIBUTE | libc::ENOTSUP)) => {}
            done => done.map_err(|err| failed_on(err, "removing", name))?,
        }
    }
    #[cfg(target_os = "macos")]
    crate::acl::clear_extended(root.as_raw_fd())
        .map_err(|err| failed_call(err, "clearing the extended ACL"))?;
    Ok(())
}

/// Flushes the file system that holds `destination` to its device: with one `syncfs` on Linux,
/// and on macOS with one `F_FULLFSYNC`, which empties the drive's cache of the files each copying
/// thread flushed.
pub fn flush_file_system(destination: &Path, points: &TestPoints) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let directory = File::open(destination)?;
        // SAFETY: the descriptor is open for the call.
        check(unsafe { libc::syncfs(directory.as_raw_fd()) })?;
    }
    #[cfg(target_os = "macos")]
    crate::psync::sync_dir(destination)?;
    #[cfg(feature = "test-util")]
    points
        .flushed
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    #[cfg(not(feature = "test-util"))]
    let _ = points;
    Ok(())
}

/// An entry whose status is taken: an open file or directory, or the entry a name in an open
/// directory names, a link not followed.
#[derive(Clone, Copy)]
enum Entry<'a> {
    Fd(RawFd),
    Named(RawFd, &'a CStr),
}

/// What the faithful copy reads of an entry, with one call: `statx` on Linux, whose attributes
/// tell an immutable or append-only entry, and `fstat` or `fstatat` on macOS, whose `st_flags`
/// does.
struct Status {
    uid: libc::uid_t,
    gid: libc::gid_t,
    mode: u32,
    is_directory: bool,
    links: u64,
    len: u64,
    atime: (i64, i64),
    mtime: (i64, i64),
    /// `stx_attributes` on Linux, and `st_flags` on macOS.
    flags: u64,
}

impl Status {
    /// The status of `entry`.
    #[cfg(target_os = "linux")]
    fn of(entry: Entry<'_>) -> io::Result<Self> {
        // SAFETY: zeroed is a valid `statx`, which the call fills.
        let mut stat: libc::statx = unsafe { std::mem::zeroed() };
        let (fd, name, flags) = match entry {
            Entry::Fd(fd) => (fd, c"", libc::AT_EMPTY_PATH),
            Entry::Named(fd, name) => (fd, name, libc::AT_SYMLINK_NOFOLLOW),
        };
        // SAFETY: the descriptor is open, the name is NUL-terminated, and `stat` is writable.
        check(unsafe {
            libc::statx(
                fd,
                name.as_ptr(),
                flags,
                libc::STATX_BASIC_STATS,
                &raw mut stat,
            )
        })?;
        Ok(Self {
            uid: stat.stx_uid,
            gid: stat.stx_gid,
            mode: u32::from(stat.stx_mode),
            is_directory: u32::from(stat.stx_mode) & libc::S_IFMT == libc::S_IFDIR,
            links: u64::from(stat.stx_nlink),
            len: stat.stx_size,
            atime: (stat.stx_atime.tv_sec, i64::from(stat.stx_atime.tv_nsec)),
            mtime: (stat.stx_mtime.tv_sec, i64::from(stat.stx_mtime.tv_nsec)),
            flags: stat.stx_attributes,
        })
    }

    /// The status of `entry`.
    #[cfg(target_os = "macos")]
    fn of(entry: Entry<'_>) -> io::Result<Self> {
        // SAFETY: zeroed is a valid `stat`, which the call fills.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: the descriptor is open, the name is NUL-terminated, and `stat` is writable.
        check(unsafe {
            match entry {
                Entry::Fd(fd) => libc::fstat(fd, &raw mut stat),
                Entry::Named(fd, name) => {
                    libc::fstatat(fd, name.as_ptr(), &raw mut stat, libc::AT_SYMLINK_NOFOLLOW)
                }
            }
        })?;
        Ok(Self {
            uid: stat.st_uid,
            gid: stat.st_gid,
            mode: u32::from(stat.st_mode),
            is_directory: stat.st_mode & libc::S_IFMT == libc::S_IFDIR,
            links: u64::from(stat.st_nlink),
            len: u64::try_from(stat.st_size).unwrap_or(0),
            atime: (stat.st_atime, stat.st_atime_nsec),
            mtime: (stat.st_mtime, stat.st_mtime_nsec),
            flags: u64::from(stat.st_flags),
        })
    }

    /// The access and modification times, as `futimens` and `utimensat` take them.
    fn times(&self) -> [libc::timespec; 2] {
        [
            timespec(self.atime.0, self.atime.1),
            timespec(self.mtime.0, self.mtime.1),
        ]
    }

    /// Refuses the entry at `path`, a path of the source or of its copy, where it isn't a
    /// directory and has more than one link, or [`refuses_flags`] refuses its flags. A test build
    /// adds the flags `points` stands in for an entry of its name.
    fn refuse_links_and_flags(&self, path: &Path, points: &TestPoints) -> io::Result<()> {
        if self.links > 1 && !self.is_directory {
            return Err(refused(&format!("has {} links", self.links)));
        }
        #[cfg(feature = "test-util")]
        let flags = self.flags | points.flags_standing_in(path);
        #[cfg(not(feature = "test-util"))]
        let (flags, _) = (self.flags, (path, points));
        if refuses_flags(flags) {
            return Err(refused("is immutable or append-only"));
        }
        Ok(())
    }
}

/// Whether an entry with the flags `flags`, `stx_attributes` on Linux and `st_flags` on macOS,
/// is one the copy refuses: one marked immutable or append-only, which the move couldn't remove
/// with the old root. The other flags mark how a file is shown, backed up or stored, and are left
/// out.
#[lore_macro::test_pub]
fn refuses_flags(flags: u64) -> bool {
    #[cfg(target_os = "linux")]
    let refused = libc::STATX_ATTR_IMMUTABLE | libc::STATX_ATTR_APPEND;
    #[cfg(target_os = "macos")]
    let refused = libc::UF_IMMUTABLE | libc::UF_APPEND | libc::SF_IMMUTABLE | libc::SF_APPEND;
    #[allow(clippy::cast_sign_loss)]
    let refused = refused as u64;
    flags & refused != 0
}

/// An entry whose metadata is read or set: an open file or directory, or a link at a path, not
/// followed.
#[derive(Clone, Copy)]
enum Node<'a> {
    Fd(RawFd),
    Link(&'a CStr),
}

/// Gives `fd`, an open file or directory, `from`'s owner, extended attributes, permission bits,
/// times and ACL, as `status` reports them, in the order the module describes.
fn finish(from: Node<'_>, fd: RawFd, status: &Status, copying: Copying<'_>) -> io::Result<()> {
    set_owner(Node::Fd(fd), status, copying)?;
    copy_attributes(from, Node::Fd(fd), copying)?;
    let mode =
        libc::mode_t::try_from(status.mode & entry::PERMISSION_BITS).map_err(io::Error::other)?;
    // SAFETY: the descriptor is open for the call.
    check(unsafe { libc::fchmod(fd, mode) })
        .map_err(|err| failed_call(err, "setting the permission bits"))?;
    let times = status.times();
    // SAFETY: the descriptor is open for the call, and `times` holds two entries.
    check(unsafe { libc::futimens(fd, times.as_ptr()) })
        .map_err(|err| failed_call(err, "setting the times"))?;
    copy_acl(from, Node::Fd(fd))
}

/// Gives `node` the owner and group of `status`. Where the caller may not give the group, one it
/// isn't a member of, which answers `EPERM`, gives the owner alone, and counts the entry as one
/// that kept the target's group. A refused owner fails, naming the owner and group asked for.
fn set_owner(node: Node<'_>, status: &Status, copying: Copying<'_>) -> io::Result<()> {
    let (uid, gid) = (status.uid, status.gid);
    let failed = |err| failed_call(err, &format!("setting the owner {uid}:{gid}"));
    match chown(node, uid, gid, copying) {
        Ok(()) => Ok(()),
        Err(err) if err.raw_os_error() == Some(libc::EPERM) => {
            chown(node, uid, libc::gid_t::MAX, copying).map_err(failed)?;
            copying.kept.count_group();
            Ok(())
        }
        Err(err) => Err(failed(err)),
    }
}

/// Gives `node` the owner `uid` and the group `gid`, or leaves its group where `gid` is
/// `gid_t::MAX`, which the calls read as -1. A test build stands in for the refusals the copy's
/// test points name.
fn chown(
    node: Node<'_>,
    uid: libc::uid_t,
    gid: libc::gid_t,
    #[cfg_attr(not(feature = "test-util"), allow(unused_variables))] copying: Copying<'_>,
) -> io::Result<()> {
    #[cfg(feature = "test-util")]
    if let Some(refusal) = copying.points.owner_refusal(gid != libc::gid_t::MAX) {
        return Err(refusal);
    }
    // SAFETY: the descriptor is open, or the path NUL-terminated, for the call, which doesn't
    // follow a link.
    check(unsafe {
        match node {
            Node::Fd(fd) => libc::fchown(fd, uid, gid),
            Node::Link(path) => libc::lchown(path.as_ptr(), uid, gid),
        }
    })
}

/// A time as `utimensat` takes it.
fn timespec(seconds: i64, nanoseconds: i64) -> libc::timespec {
    libc::timespec {
        tv_sec: seconds,
        tv_nsec: nanoseconds,
    }
}

/// Copies every extended attribute `from` lists to `to`, as the module describes.
fn copy_attributes(from: Node<'_>, to: Node<'_>, copying: Copying<'_>) -> io::Result<()> {
    #[cfg_attr(target_os = "linux", allow(unused_mut))]
    let (mut data, mut names, mut value) = BUFFERS.take();
    let copied = (|| {
        list(from, &mut names)
            .map_err(|err| failed_call(err, "listing the extended attributes"))?;
        let mut rest = names.as_slice();
        while let Ok(name) = CStr::from_bytes_until_nul(rest) {
            rest = &rest[name.to_bytes_with_nul().len()..];
            #[cfg(target_os = "macos")]
            if name == RESOURCE_FORK {
                copy_resource_fork(from, to, &mut data, copying)?;
                continue;
            }
            match get(from, name, &mut value) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(err) if is_link(from) && err.raw_os_error() == Some(libc::ENOTSUP) => continue,
                Err(err) => return Err(failed_on(err, "reading", name)),
            }
            #[cfg(feature = "test-util")]
            copying
                .points
                .value_held
                .fetch_max(value.len(), std::sync::atomic::Ordering::Relaxed);
            #[cfg(feature = "test-util")]
            let set = match copying.points.attribute_refusal(name.to_bytes()) {
                Some(refusal) => Err(refusal),
                None => set(to, name, &value),
            };
            #[cfg(not(feature = "test-util"))]
            let set = set(to, name, &value);
            match set {
                Err(err)
                    if target_keeps(name.to_bytes(), &err)
                        || assigned_by_system(to, name, &err, copying) =>
                {
                    copying.kept.count(name.to_bytes());
                }
                done => done.map_err(|err| failed_on(err, "setting", name))?,
            }
        }
        Ok(())
    })();
    BUFFERS.set((data, names, value));
    copied
}

/// The attribute that holds a file's resource fork on macOS, which can be as large as a file's
/// data, unlike the other attributes.
#[cfg(target_os = "macos")]
const RESOURCE_FORK: &CStr = c"com.apple.ResourceFork";

/// Copies `from`'s resource fork to `to` through `buffer`, a buffer's length at a time at its
/// position in the fork, so that the copy holds no more of it at once than the buffer does. A fork
/// that is gone is left out, and one that shrinks as it is read is copied as far as it reads.
#[cfg(target_os = "macos")]
fn copy_resource_fork(
    from: Node<'_>,
    to: Node<'_>,
    buffer: &mut Vec<u8>,
    #[cfg_attr(not(feature = "test-util"), allow(unused_variables))] copying: Copying<'_>,
) -> io::Result<()> {
    let read_at = |buffer: *mut libc::c_void, size: usize, position: u32| match from {
        // SAFETY: the descriptor is open, the name is NUL-terminated, and `buffer` is null or
        // writable for `size` bytes.
        Node::Fd(fd) => unsafe {
            libc::fgetxattr(fd, RESOURCE_FORK.as_ptr(), buffer, size, position, 0)
        },
        // SAFETY: the path and name are NUL-terminated, and `buffer` is null or writable for
        // `size` bytes.
        Node::Link(path) => unsafe {
            libc::getxattr(
                path.as_ptr(),
                RESOURCE_FORK.as_ptr(),
                buffer,
                size,
                position,
                libc::XATTR_NOFOLLOW,
            )
        },
    };
    let write_at = |chunk: &[u8], position: u32| {
        let (bytes, len) = (chunk.as_ptr().cast(), chunk.len());
        // SAFETY: the descriptor or path is open or NUL-terminated, the name is NUL-terminated,
        // and `chunk` is readable for its length.
        check(unsafe {
            match to {
                Node::Fd(fd) => {
                    libc::fsetxattr(fd, RESOURCE_FORK.as_ptr(), bytes, len, position, 0)
                }
                Node::Link(path) => libc::setxattr(
                    path.as_ptr(),
                    RESOURCE_FORK.as_ptr(),
                    bytes,
                    len,
                    position,
                    libc::XATTR_NOFOLLOW,
                ),
            }
        })
    };
    let gone_or_failed = || {
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(NO_ATTRIBUTE) => Ok(()),
            _ => Err(err),
        }
    };
    let Ok(size) = usize::try_from(read_at(std::ptr::null_mut(), 0, 0)) else {
        return gone_or_failed();
    };
    if size == 0 {
        return write_at(&[], 0);
    }
    buffer.resize(COPY_BUFFER, 0);
    let mut position = 0u32;
    while (position as usize) < size {
        let wanted = buffer.len().min(size - position as usize);
        let Ok(read) = usize::try_from(read_at(buffer.as_mut_ptr().cast(), wanted, position))
        else {
            return gone_or_failed();
        };
        #[cfg(feature = "test-util")]
        copying
            .points
            .value_held
            .fetch_max(read, std::sync::atomic::Ordering::Relaxed);
        if read == 0 {
            break;
        }
        write_at(&buffer[..read], position)?;
        position = u32::try_from(read)
            .ok()
            .and_then(|read| position.checked_add(read))
            .ok_or_else(|| io::Error::other("a resource fork of 4 GiB or more"))?;
    }
    Ok(())
}

/// Whether the failure `err` to set the attribute `name` on the copy leaves the attribute as the
/// target sets it, rather than failing the copy: a `security.*` attribute that the system's
/// policy refuses, with `EPERM` or `EACCES`.
#[lore_macro::test_pub]
fn target_keeps(name: &[u8], err: &io::Error) -> bool {
    name.starts_with(b"security.") && matches!(err.raw_os_error(), Some(libc::EPERM | libc::EACCES))
}

/// Whether the failure `err` to set the `security.*` attribute `name` on `to` leaves the value the
/// system gave `to` as it made it: the target refuses the set with `ENOTSUP` but holds the attribute.
fn assigned_by_system(to: Node<'_>, name: &CStr, err: &io::Error, copying: Copying<'_>) -> bool {
    if !name.to_bytes().starts_with(b"security.") || err.raw_os_error() != Some(libc::ENOTSUP) {
        return false;
    }
    #[cfg(feature = "test-util")]
    if copying.points.target_holds_refused(name.to_bytes()) {
        return true;
    }
    #[cfg(not(feature = "test-util"))]
    let _ = copying;
    holds(to, name)
}

/// Whether `node` holds the extended attribute `name`.
fn holds(node: Node<'_>, name: &CStr) -> bool {
    let (buffer, size) = (std::ptr::null_mut(), 0);
    // SAFETY: the descriptor is open or the path NUL-terminated, the name is NUL-terminated, and
    // a size of zero writes nothing.
    let sized = unsafe {
        match node {
            #[cfg(target_os = "linux")]
            Node::Fd(fd) => libc::fgetxattr(fd, name.as_ptr(), buffer, size),
            #[cfg(target_os = "linux")]
            Node::Link(path) => libc::lgetxattr(path.as_ptr(), name.as_ptr(), buffer, size),
            #[cfg(target_os = "macos")]
            Node::Fd(fd) => libc::fgetxattr(fd, name.as_ptr(), buffer, size, 0, 0),
            #[cfg(target_os = "macos")]
            Node::Link(path) => libc::getxattr(
                path.as_ptr(),
                name.as_ptr(),
                buffer,
                size,
                0,
                libc::XATTR_NOFOLLOW,
            ),
        }
    };
    sized >= 0
}

/// Whether `node` is a link.
fn is_link(node: Node<'_>) -> bool {
    matches!(node, Node::Link(_))
}

/// `err`, which `doing` the extended attribute `name` failed with, naming both.
fn failed_on(err: io::Error, doing: &str, name: &CStr) -> io::Error {
    io::Error::new(
        err.kind(),
        format!(
            "{doing} the extended attribute {}: {err}",
            name.to_string_lossy()
        ),
    )
}

/// Reads the names of `node`'s extended attributes into `names`, each ended by a NUL. A file
/// system that holds no extended attributes lists none.
fn list(node: Node<'_>, names: &mut Vec<u8>) -> io::Result<()> {
    read_sized(names, |buffer, size| match node {
        // SAFETY: the descriptor is open, and `buffer` is writable for `size` bytes.
        #[cfg(target_os = "linux")]
        Node::Fd(fd) => unsafe { libc::flistxattr(fd, buffer, size) },
        // SAFETY: the path is NUL-terminated, and `buffer` is writable for `size` bytes.
        #[cfg(target_os = "linux")]
        Node::Link(path) => unsafe { libc::llistxattr(path.as_ptr(), buffer, size) },
        // SAFETY: the descriptor is open, and `buffer` is writable for `size` bytes.
        #[cfg(target_os = "macos")]
        Node::Fd(fd) => unsafe { libc::flistxattr(fd, buffer, size, 0) },
        // SAFETY: the path is NUL-terminated, and `buffer` is writable for `size` bytes.
        #[cfg(target_os = "macos")]
        Node::Link(path) => unsafe {
            libc::listxattr(path.as_ptr(), buffer, size, libc::XATTR_NOFOLLOW)
        },
    })
    .or_else(|err| match err.raw_os_error() {
        Some(libc::ENOTSUP) => {
            names.clear();
            Ok(())
        }
        _ => Err(err),
    })
}

/// The error `getxattr` reports for an attribute that is gone.
#[cfg(target_os = "linux")]
const NO_ATTRIBUTE: i32 = libc::ENODATA;
/// The error `getxattr` reports for an attribute that is gone.
#[cfg(target_os = "macos")]
const NO_ATTRIBUTE: i32 = libc::ENOATTR;

/// Reads the value of `node`'s attribute `name` into `value`: false where it is gone.
fn get(node: Node<'_>, name: &CStr, value: &mut Vec<u8>) -> io::Result<bool> {
    let read = read_sized(value, |buffer, size| match node {
        // SAFETY: the descriptor is open, the name is NUL-terminated, and `buffer` is writable
        // for `size` bytes.
        #[cfg(target_os = "linux")]
        Node::Fd(fd) => unsafe { libc::fgetxattr(fd, name.as_ptr(), buffer.cast(), size) },
        // SAFETY: the path and name are NUL-terminated, and `buffer` is writable for `size`
        // bytes.
        #[cfg(target_os = "linux")]
        Node::Link(path) => unsafe {
            libc::lgetxattr(path.as_ptr(), name.as_ptr(), buffer.cast(), size)
        },
        // SAFETY: the descriptor is open, the name is NUL-terminated, and `buffer` is writable
        // for `size` bytes.
        #[cfg(target_os = "macos")]
        Node::Fd(fd) => unsafe { libc::fgetxattr(fd, name.as_ptr(), buffer.cast(), size, 0, 0) },
        // SAFETY: the path and name are NUL-terminated, and `buffer` is writable for `size`
        // bytes.
        #[cfg(target_os = "macos")]
        Node::Link(path) => unsafe {
            libc::getxattr(
                path.as_ptr(),
                name.as_ptr(),
                buffer.cast(),
                size,
                0,
                libc::XATTR_NOFOLLOW,
            )
        },
    });
    match read {
        Ok(()) => Ok(true),
        Err(err) if err.raw_os_error() == Some(NO_ATTRIBUTE) => Ok(false),
        Err(err) => Err(err),
    }
}

/// Sets `node`'s attribute `name` to `value`.
fn set(node: Node<'_>, name: &CStr, value: &[u8]) -> io::Result<()> {
    let (bytes, len) = (value.as_ptr().cast(), value.len());
    // SAFETY: the descriptor or path is open or NUL-terminated, the name is NUL-terminated, and
    // `value` is readable for its length.
    check(unsafe {
        match node {
            #[cfg(target_os = "linux")]
            Node::Fd(fd) => libc::fsetxattr(fd, name.as_ptr(), bytes, len, 0),
            #[cfg(target_os = "linux")]
            Node::Link(path) => libc::lsetxattr(path.as_ptr(), name.as_ptr(), bytes, len, 0),
            #[cfg(target_os = "macos")]
            Node::Fd(fd) => libc::fsetxattr(fd, name.as_ptr(), bytes, len, 0, 0),
            #[cfg(target_os = "macos")]
            Node::Link(path) => libc::setxattr(
                path.as_ptr(),
                name.as_ptr(),
                bytes,
                len,
                0,
                libc::XATTR_NOFOLLOW,
            ),
        }
    })
}

/// Nothing on Linux, where an ACL is an extended attribute.
#[cfg(target_os = "linux")]
#[allow(clippy::unnecessary_wraps)]
fn copy_acl(_from: Node<'_>, _to: Node<'_>) -> io::Result<()> {
    Ok(())
}

/// Copies `from`'s extended ACL to `to`, where it has one.
#[cfg(target_os = "macos")]
fn copy_acl(from: Node<'_>, to: Node<'_>) -> io::Result<()> {
    let target = |node| match node {
        Node::Fd(fd) => crate::acl::Target::Fd(fd),
        Node::Link(path) => crate::acl::Target::Link(path),
    };
    crate::acl::copy_extended(target(from), target(to))
}

/// Fills `buffer` with what `read` writes, sizing it first with a read of size zero, and again
/// where what is read grew in between, as the attribute calls answer `ERANGE` then, or with more
/// than the buffer holds. What sizes as empty is empty, since a read of size zero only sizes.
#[lore_macro::test_pub]
fn read_sized(
    buffer: &mut Vec<u8>,
    read: impl Fn(*mut libc::c_char, usize) -> isize,
) -> io::Result<()> {
    loop {
        let needed = read(std::ptr::null_mut(), 0);
        let Ok(needed) = usize::try_from(needed) else {
            return Err(io::Error::last_os_error());
        };
        buffer.clear();
        if needed == 0 {
            return Ok(());
        }
        buffer.reserve(needed);
        let done = read(buffer.as_mut_ptr().cast(), buffer.capacity());
        match usize::try_from(done) {
            Ok(done) if done <= buffer.capacity() => {
                // SAFETY: the call wrote `done` bytes, at most the capacity it was given.
                unsafe { buffer.set_len(done) };
                return Ok(());
            }
            Ok(_) => {}
            Err(_) if io::Error::last_os_error().raw_os_error() == Some(libc::ERANGE) => {}
            Err(_) => return Err(io::Error::last_os_error()),
        }
    }
}

/// Copies the data of `input`, of `len` bytes, to `output`, which is empty, range by range of data,
/// so that each hole of `input` is a hole of `output`, and gives `output` that length. The length
/// is set only where `input` ends in a hole, since the copy of the last range ends at it otherwise,
/// and setting it after `copy_file_range` costs about as much again as the rest of a small file's
/// copy on xfs.
fn copy_keeping_holes(input: &File, output: &File, len: u64) -> io::Result<()> {
    let mut cursor = crate::holes::DataCursor::default();
    let mut in_kernel = cfg!(target_os = "linux");
    let mut offset = 0;
    while offset < len {
        let Some(range) = crate::holes::next_data(input, offset, &mut cursor)? else {
            break;
        };
        let end = range.end.min(len);
        if range.start >= end {
            break;
        }
        copy_range(input, output, range.start, end, &mut in_kernel)?;
        offset = end;
    }
    if offset < len {
        output.set_len(len)?;
    }
    Ok(())
}

/// Copies the bytes `start` to `end` of `input` to the same offsets of `output`, with
/// `copy_file_range` while `in_kernel` holds, and through the thread's buffer once it can't copy
/// between the two file systems, which clears `in_kernel`.
fn copy_range(
    input: &File,
    output: &File,
    mut start: u64,
    end: u64,
    in_kernel: &mut bool,
) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    while *in_kernel && start < end {
        let mut from = i64::try_from(start).map_err(io::Error::other)?;
        let mut to = from;
        let wanted = usize::try_from(end - start)
            .unwrap_or(usize::MAX)
            .min(1 << 30);
        // SAFETY: both descriptors are open, and the offsets are writable.
        let copied = unsafe {
            libc::copy_file_range(
                input.as_raw_fd(),
                &raw mut from,
                output.as_raw_fd(),
                &raw mut to,
                wanted,
                0,
            )
        };
        match usize::try_from(copied) {
            Ok(0) => return Ok(()),
            Ok(copied) => start += copied as u64,
            Err(_) => {
                let err = io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::EXDEV | libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP) => {
                        *in_kernel = false;
                    }
                    _ => return Err(err),
                }
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = in_kernel;
    if start >= end {
        return Ok(());
    }
    let (mut data, names, value) = BUFFERS.take();
    data.resize(COPY_BUFFER, 0);
    let copied = (|| {
        while start < end {
            let wanted = usize::try_from(end - start)
                .unwrap_or(usize::MAX)
                .min(data.len());
            let read = input.read_at(&mut data[..wanted], start)?;
            if read == 0 {
                return Ok(());
            }
            output.write_all_at(&data[..read], start)?;
            start += read as u64;
        }
        Ok(())
    })();
    BUFFERS.set((data, names, value));
    copied
}

/// `Ok` where a call answered 0, and its OS error otherwise.
fn check(answer: libc::c_int) -> io::Result<()> {
    if answer == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
