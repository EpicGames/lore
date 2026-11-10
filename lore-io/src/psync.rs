// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;

use crate::buffer::StableBuf;
use crate::buffer::StableBufList;
use crate::buffer::StableBufListMut;
use crate::pool::SyscallPool;
use crate::pool::retry_on_interrupt;

/// Segments per vectored syscall, matching the `IOV_MAX` of 1024 on Linux
/// and macOS. Larger segment lists complete through the callers'
/// exact-loops, which resubmit with a byte skip.
#[cfg(target_family = "unix")]
pub(crate) const MAX_IO_SEGMENTS: usize = 1024;

/// Backend implementing the driver operations with positional syscalls
/// executed on the shared syscall pool.
///
/// This is the portable baseline backend and the semantic reference the
/// completion-based backends are conformance-tested against.
pub(crate) struct PsyncDriver;

impl PsyncDriver {
    pub(crate) async fn open(
        &self,
        options: std::fs::OpenOptions,
        path: PathBuf,
    ) -> std::io::Result<File> {
        SyscallPool::global()
            .submit(move || retry_on_interrupt(|| options.open(&path)))
            .await
    }

    /// Reads up to `max_len` bytes, returning what arrived.
    ///
    /// The buffer is allocated here rather than taken from the caller, so the memory the kernel
    /// copies into is owned by the thread receiving the copy. The process allocator caches spans
    /// per thread, which makes that reuse thread-local for free — where a shared pool hands a
    /// thread memory another core last wrote, and the copy then pays for the ownership transfer.
    pub(crate) async fn read_at(
        &self,
        file: Arc<File>,
        max_len: usize,
        offset: u64,
    ) -> std::io::Result<Bytes> {
        SyscallPool::global()
            .submit(move || {
                // SAFETY: truncated to the byte count below, so no uninitialised byte escapes.
                let mut buffer = unsafe { crate::buffer::uninit_buffer(max_len) };
                let read = read_at_impl(&file, &mut buffer, offset)?;
                buffer.truncate(read);
                Ok(buffer.freeze())
            })
            .await
    }

    /// Reads exactly `len` bytes, looping inside one dispatch.
    ///
    /// A short read costs another syscall rather than another round trip through the pool: the
    /// job already owns the buffer and the thread, so returning to the caller between syscalls
    /// would buy nothing and pay two handoffs. This is the shape the whole-file read below and
    /// the file scan this backend replaces both use.
    pub(crate) async fn read_exact_at(
        &self,
        file: Arc<File>,
        len: usize,
        offset: u64,
    ) -> std::io::Result<Bytes> {
        SyscallPool::global()
            .submit(move || {
                // SAFETY: every byte up to `len` is filled before returning, and a short read
                // returns an error rather than the buffer.
                let mut buffer = unsafe { crate::buffer::uninit_buffer(len) };
                let mut done = 0;
                while done < len {
                    let read = read_at_impl(&file, &mut buffer[done..len], offset + done as u64)?;
                    if read == 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "file ended before the requested read length",
                        ));
                    }
                    done += read;
                }
                Ok(buffer.freeze())
            })
            .await
    }

    /// Writes all `len` bytes, looping inside one dispatch. See [`Self::read_exact_at`].
    pub(crate) async fn write_all_at<B: StableBuf>(
        &self,
        file: Arc<File>,
        buffer: B,
        len: usize,
        offset: u64,
    ) -> std::io::Result<B> {
        SyscallPool::global()
            .submit(move || {
                let mut done = 0;
                while done < len {
                    let written =
                        write_at_impl(&file, &buffer.as_ref()[done..len], offset + done as u64)?;
                    if written == 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::WriteZero,
                            "file refused further writes",
                        ));
                    }
                    done += written;
                }
                Ok(buffer)
            })
            .await
    }

    pub(crate) async fn write_at<B: StableBuf>(
        &self,
        file: Arc<File>,
        buffer: B,
        buffer_offset: usize,
        len: usize,
        offset: u64,
    ) -> std::io::Result<(B, usize)> {
        SyscallPool::global()
            .submit(move || {
                let slice = &buffer.as_ref()[buffer_offset..buffer_offset + len];
                let written = write_at_impl(&file, slice, offset)?;
                Ok((buffer, written))
            })
            .await
    }

    pub(crate) async fn read_file_bytes(&self, path: PathBuf) -> std::io::Result<Bytes> {
        SyscallPool::global()
            .submit(move || {
                let file = crate::file::OpenOptions::new()
                    .read(true)
                    .to_std_blocking()
                    .open(path)?;
                let metadata = file.metadata()?;
                // A directory opens for reading on Unix, and its size is whatever the filesystem
                // reports, zero on some, so reading it by that size could answer with an empty
                // file rather than an error.
                if metadata.is_dir() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::IsADirectory,
                        "is a directory, not a file",
                    ));
                }
                let len = metadata.len() as usize;
                crate::driver::check_whole_file_len(len)?;
                // SAFETY: every byte up to `len` is filled before returning, and a file that
                // shrank returns an error rather than the buffer.
                let mut buffer = unsafe { crate::buffer::uninit_buffer(len) };
                let mut done = 0;
                while done < len {
                    let read = read_at_impl(&file, &mut buffer[done..len], done as u64)?;
                    if read == 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "file shrank while reading",
                        ));
                    }
                    done += read;
                }
                Ok(buffer.freeze())
            })
            .await
    }

    pub(crate) async fn write_file_bytes(
        &self,
        path: PathBuf,
        data: Bytes,
        durable: bool,
    ) -> std::io::Result<std::fs::Metadata> {
        SyscallPool::global()
            .submit(move || {
                use std::io::Write;
                let mut file = crate::file::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .to_std_blocking()
                    .open(path)?;
                file.write_all(data.as_ref())?;
                if durable {
                    file.sync_data()?;
                }
                file.metadata()
            })
            .await
    }

    pub(crate) async fn open_read_head(
        &self,
        options: std::fs::OpenOptions,
        path: PathBuf,
        head_len: usize,
    ) -> std::io::Result<(File, std::fs::Metadata, Bytes)> {
        SyscallPool::global()
            .submit(move || {
                let file = retry_on_interrupt(|| options.open(&path))?;
                let metadata = file.metadata()?;
                let len = std::cmp::min(head_len, metadata.len() as usize);
                // SAFETY: every byte up to `len` is filled before returning, and a file that
                // shrank returns an error rather than the buffer.
                let mut buffer = unsafe { crate::buffer::uninit_buffer(len) };
                let mut done = 0;
                while done < len {
                    let read = read_at_impl(&file, &mut buffer[done..len], done as u64)?;
                    if read == 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "file shrank while reading",
                        ));
                    }
                    done += read;
                }
                Ok((file, metadata, buffer.freeze()))
            })
            .await
    }

    pub(crate) async fn write_file_segments<B: StableBufList, C: Send + 'static>(
        &self,
        options: std::fs::OpenOptions,
        path: PathBuf,
        buffers: B,
        durable: bool,
        completion: C,
    ) -> std::io::Result<()> {
        SyscallPool::global()
            .submit(move || {
                let written = options
                    .open(&path)
                    .and_then(|file| write_segments(file, buffers, durable));
                if written.is_ok() && durable {
                    // Best-effort, as after an atomic write: the directory entry matters only to a
                    // file this write created.
                    let _ = sync_parent_dir(&path);
                }
                drop(completion);
                written
            })
            .await
    }

    pub(crate) async fn write_file_segments_atomic<B: StableBufList, C: Send + 'static>(
        &self,
        options: std::fs::OpenOptions,
        temporary_path: PathBuf,
        final_path: PathBuf,
        buffers: B,
        durable: bool,
        completion: C,
    ) -> std::io::Result<()> {
        SyscallPool::global()
            .submit(move || {
                let file = options.open(&temporary_path)?;
                let written = write_segments(file, buffers, durable)
                    .and_then(|()| std::fs::rename(&temporary_path, &final_path));
                if written.is_err() {
                    let _ = std::fs::remove_file(&temporary_path);
                } else if durable {
                    // Directory sync is best-effort: the rename is already durable in content, and
                    // a lost directory entry after a crash reproduces the pre-write state the
                    // caller already handles.
                    let _ = sync_parent_dir(&final_path);
                }
                drop(completion);
                written
            })
            .await
    }

    pub(crate) async fn read_vectored_at<B: StableBufListMut>(
        &self,
        file: Arc<File>,
        mut buffers: B,
        skip: usize,
        offset: u64,
    ) -> std::io::Result<(B, usize)> {
        SyscallPool::global()
            .submit(move || {
                let read = read_vectored_impl(&file, &mut buffers, skip, offset)?;
                Ok((buffers, read))
            })
            .await
    }

    pub(crate) async fn write_vectored_at<B: StableBufList>(
        &self,
        file: Arc<File>,
        buffers: B,
        skip: usize,
        offset: u64,
    ) -> std::io::Result<(B, usize)> {
        SyscallPool::global()
            .submit(move || {
                let written = write_vectored_impl(&file, &buffers, skip, offset)?;
                Ok((buffers, written))
            })
            .await
    }

    pub(crate) async fn metadata(&self, path: PathBuf) -> std::io::Result<std::fs::Metadata> {
        SyscallPool::global()
            .submit(move || std::fs::metadata(path))
            .await
    }

    pub(crate) async fn stat(&self, path: PathBuf) -> std::io::Result<crate::Stat> {
        SyscallPool::global().submit(move || stat_impl(&path)).await
    }

    pub(crate) async fn symlink_stat(&self, path: PathBuf) -> std::io::Result<crate::Stat> {
        SyscallPool::global()
            .submit(move || symlink_stat_impl(&path))
            .await
    }

    pub(crate) async fn holds_name_exactly(&self, path: PathBuf) -> Option<bool> {
        SyscallPool::global()
            .submit(move || holds_name_exactly_impl(&path))
            .await
    }

    pub(crate) async fn names_one_entry(
        &self,
        from: PathBuf,
        to: PathBuf,
    ) -> std::io::Result<Option<bool>> {
        SyscallPool::global()
            .submit(move || {
                let Some(from) = entry_identity(&from)? else {
                    return Ok(None);
                };
                Ok(Some(entry_identity(&to)? == Some(from)))
            })
            .await
    }

    pub(crate) async fn file_metadata(
        &self,
        file: Arc<File>,
    ) -> std::io::Result<std::fs::Metadata> {
        SyscallPool::global().submit(move || file.metadata()).await
    }

    pub(crate) async fn set_len<C: Send + 'static>(
        &self,
        file: Arc<File>,
        len: u64,
        completion: C,
    ) -> std::io::Result<()> {
        SyscallPool::global()
            .submit(move || {
                let set = retry_on_interrupt(|| file.set_len(len));
                drop(completion);
                set
            })
            .await
    }

    pub(crate) async fn rename(&self, from: PathBuf, to: PathBuf) -> std::io::Result<()> {
        SyscallPool::global()
            .submit(move || std::fs::rename(from, to))
            .await
    }

    pub(crate) async fn remove_file(&self, path: PathBuf) -> std::io::Result<()> {
        SyscallPool::global()
            .submit(move || std::fs::remove_file(path))
            .await
    }

    pub(crate) async fn set_permissions(
        &self,
        path: PathBuf,
        permissions: std::fs::Permissions,
    ) -> std::io::Result<()> {
        SyscallPool::global()
            .submit(move || std::fs::set_permissions(path, permissions))
            .await
    }

    pub(crate) async fn make_owner_writable(&self, path: PathBuf) -> std::io::Result<()> {
        SyscallPool::global()
            .submit(move || make_owner_writable_impl(&path))
            .await
    }

    pub(crate) async fn read_dir(&self, path: PathBuf) -> std::io::Result<crate::DirStream> {
        SyscallPool::global()
            .submit(move || crate::dir::open(path))
            .await
    }

    pub(crate) async fn create_dir_all(&self, path: PathBuf) -> std::io::Result<()> {
        SyscallPool::global()
            .submit(move || forgive_existing_dir(std::fs::create_dir_all(&path), &path))
            .await
    }

    pub(crate) async fn remove_dir(&self, path: PathBuf) -> std::io::Result<()> {
        SyscallPool::global()
            .submit(move || std::fs::remove_dir(path))
            .await
    }

    pub(crate) async fn copy(&self, from: PathBuf, to: PathBuf) -> std::io::Result<u64> {
        SyscallPool::global()
            .submit(move || std::fs::copy(from, to))
            .await
    }

    pub(crate) async fn remove_dir_all(&self, path: PathBuf) -> std::io::Result<()> {
        SyscallPool::global()
            .submit(move || std::fs::remove_dir_all(path))
            .await
    }

    pub(crate) async fn remove_dir_all_keeping(
        &self,
        path: PathBuf,
        boundary: &'static str,
        reserved: impl Fn(&str) -> bool + Send + 'static,
    ) -> std::io::Result<()> {
        SyscallPool::global()
            .submit(move || remove_tree_keeping(path, boundary, &reserved))
            .await
    }

    pub(crate) async fn remove_dir_all_in_background(
        &self,
        path: PathBuf,
        aside: Option<PathBuf>,
        make_writable: bool,
    ) -> (PathBuf, std::io::Result<()>) {
        SyscallPool::global()
            .run_on_own_thread("lore-io-background-removal", move || {
                remove_tree_in_background(
                    path,
                    aside,
                    |from, to| std::fs::rename(from, to),
                    |tree| {
                        if make_writable {
                            remove_tree_making_it_writable(tree)
                        } else {
                            std::fs::remove_dir_all(tree)
                        }
                    },
                )
            })
            .await
    }

    pub(crate) async fn sync(&self, file: Arc<File>, data_only: bool) -> std::io::Result<()> {
        SyscallPool::global()
            .submit(move || {
                if data_only {
                    file.sync_data()
                } else {
                    file.sync_all()
                }
            })
            .await
    }
}

/// Forgives a `create_dir_all` failure when `path` is already a directory.
///
/// `std::fs::create_dir_all` forgives only `AlreadyExists` (Rust 1.94, rust-lang/rust#148196);
/// a Windows container bind-mount root instead fails `mkdir` with `PermissionDenied` while
/// remaining a directory. This restores the pre-1.94 contract every caller here was written
/// against: success means the directory exists, however that came to be true.
#[lore_macro::test_pub]
fn forgive_existing_dir(
    result: std::io::Result<()>,
    path: &std::path::Path,
) -> std::io::Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(_) if path.is_dir() => Ok(()),
        Err(err) => Err(err),
    }
}

/// Removes the tree at `path` as `std::fs::remove_dir_all` does, but keeps each directory that
/// holds a directory named `boundary`, with all it holds and the directories leading to it, and
/// each entry whose name `reserved` answers for, with the directories leading to it. A path that
/// does not exist is `Ok`, as is an entry removed by someone else during the walk.
fn remove_tree_keeping(
    mut path: PathBuf,
    boundary: &str,
    reserved: &impl Fn(&str) -> bool,
) -> std::io::Result<()> {
    let file_type = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata.file_type(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    forgive_missing(remove_entry_keeping(
        &mut path,
        file_type,
        boundary,
        reserved,
        &mut Vec::new(),
    ))
    .map(drop)
}

/// Removes the entry of `file_type` at `path` as [`remove_tree_keeping`] does, and answers whether
/// it is gone. A link is removed, not followed.
///
/// A directory is listed in full before anything in it is removed, since the boundary can be its
/// last entry. `entries` is one stack every level shares, holding the listings of the directories
/// on the current path, and a call's caller truncates what the call leaves on it. `path` is one
/// buffer each name is pushed onto and popped off.
fn remove_entry_keeping(
    path: &mut PathBuf,
    file_type: std::fs::FileType,
    boundary: &str,
    reserved: &impl Fn(&str) -> bool,
    entries: &mut Vec<(std::ffi::OsString, std::fs::FileType)>,
) -> std::io::Result<bool> {
    if !file_type.is_dir() {
        remove_non_directory(path, file_type)?;
        return Ok(true);
    }
    let start = entries.len();
    let mut removed_all = true;
    for entry in std::fs::read_dir(&*path)? {
        let entry = entry?;
        let name = entry.file_name();
        let file_type = entry.file_type()?;
        if file_type.is_dir() && name == boundary {
            return Ok(false);
        }
        if name.to_str().is_some_and(reserved) {
            removed_all = false;
            continue;
        }
        entries.push((name, file_type));
    }
    while entries.len() > start {
        let index = entries.len() - 1;
        let file_type = entries[index].1;
        path.push(&entries[index].0);
        let removed = remove_entry_keeping(path, file_type, boundary, reserved, entries);
        path.pop();
        entries.truncate(index);
        removed_all &= forgive_missing(removed)?;
    }
    if removed_all {
        std::fs::remove_dir(&*path)?;
    }
    Ok(removed_all)
}

/// Answers an entry that is not there as removed.
fn forgive_missing(removed: std::io::Result<bool>) -> std::io::Result<bool> {
    match removed {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(true),
        removed => removed,
    }
}

/// Removes the file or link at `path`. Windows removes a link to a directory as a directory, and a
/// read-only file once its flag is cleared, as `std::fs::remove_dir_all` does.
fn remove_non_directory(
    path: &std::path::Path,
    file_type: std::fs::FileType,
) -> std::io::Result<()> {
    #[cfg(target_family = "windows")]
    {
        use std::os::windows::fs::FileTypeExt;
        if file_type.is_symlink_dir() {
            return std::fs::remove_dir(path);
        }
        match std::fs::remove_file(path) {
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                let mut permissions = std::fs::symlink_metadata(path)?.permissions();
                if !permissions.readonly() {
                    return Err(err);
                }
                #[allow(clippy::permissions_set_readonly_false)]
                permissions.set_readonly(false);
                std::fs::set_permissions(path, permissions)?;
                std::fs::remove_file(path)
            }
            removed => removed,
        }
    }
    #[cfg(not(target_family = "windows"))]
    {
        let _ = file_type;
        std::fs::remove_file(path)
    }
}

/// Removes the directory at `path` at background disk priority, as
/// [`IoDriver::remove_dir_all_in_background`](crate::IoDriver::remove_dir_all_in_background)
/// describes, with `rename` renaming a directory and `remove_tree` removing a tree.
#[lore_macro::test_pub]
fn remove_tree_in_background(
    path: PathBuf,
    aside: Option<PathBuf>,
    rename: impl FnOnce(&std::path::Path, &std::path::Path) -> std::io::Result<()>,
    remove_tree: impl Fn(&std::path::Path) -> std::io::Result<()>,
) -> (PathBuf, std::io::Result<()>) {
    let _background = crate::priority::BackgroundIo::begin();
    let Some(aside) = aside else {
        let removed = forgive_missing_tree(remove_tree(&path));
        return (path, removed);
    };
    match is_mount_folder(&path) {
        Ok(false) => {}
        Ok(true) => {
            let volume_root = is_volume_root(&path);
            let removed = remove_contents(&path, &remove_tree, |entry| {
                volume_root && is_volume_system_entry(entry)
            });
            return (path, removed);
        }
        Err(err) => return (path, forgive_missing_tree(Err(err))),
    }
    match rename(&path, &aside) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return (path, Ok(())),
        Err(err) if is_mount_point_refusal(&err) => {
            let removed = remove_in_place(&path, &remove_tree);
            return (path, removed);
        }
        Err(err) => return (path, Err(err)),
    }
    let removed = forgive_missing_tree(remove_tree(&aside));
    (aside, removed)
}

/// Answers a removal of a tree that is not there as done.
fn forgive_missing_tree(removed: std::io::Result<()>) -> std::io::Result<()> {
    match removed {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    }
}

/// Removes what the directory at `path` holds, then the directory. A mount point keeps its
/// directory, which the system refuses to remove while it is mounted. A path that does not exist
/// is `Ok`, as is an entry removed by someone else during the removal.
fn remove_in_place(
    path: &std::path::Path,
    remove_tree: &impl Fn(&std::path::Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    remove_contents(path, remove_tree, |_| false)?;
    match std::fs::remove_dir(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound || is_mount_point_refusal(&err) => {
            Ok(())
        }
        removed => removed,
    }
}

/// Removes what the directory at `path` holds, except each entry `kept` answers for, and keeps the
/// directory. A path that does not exist is `Ok`, as is an entry removed by someone else during
/// the removal.
fn remove_contents(
    path: &std::path::Path,
    remove_tree: &impl Fn(&std::path::Path) -> std::io::Result<()>,
    kept: impl Fn(&std::fs::DirEntry) -> bool,
) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        entries => entries?,
    };
    for entry in entries {
        let entry = entry?;
        if kept(&entry) {
            continue;
        }
        let file_type = entry.file_type()?;
        let child = entry.path();
        let removed = if file_type.is_dir() {
            remove_tree(&child)
        } else {
            remove_non_directory(&child, file_type)
        };
        match removed {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            removed => removed?,
        }
    }
    Ok(())
}

/// Whether the directory at `path` is a mount folder, which a rename would move without the
/// files it leads to. On Windows that is a directory that is a reparse point with the
/// mount-point tag, a volume mounted in a folder or a junction: a rename moves the reparse point
/// itself, and a removal of the renamed name removes only the reparse point. Unix has none; a
/// mount point there refuses the rename.
#[cfg(target_family = "unix")]
#[lore_macro::test_pub]
fn is_mount_folder(_path: &std::path::Path) -> std::io::Result<bool> {
    Ok(false)
}

/// Whether the directory at `path` is a mount folder, as the Unix version describes, read from
/// the name itself through a handle opened without following a reparse point.
#[cfg(target_family = "windows")]
#[lore_macro::test_pub]
fn is_mount_folder(path: &std::path::Path) -> std::io::Result<bool> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_TAG_INFO;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::FileAttributeTagInfo;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;
    use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_MOUNT_POINT;

    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let mut info = FILE_ATTRIBUTE_TAG_INFO {
        FileAttributes: 0,
        ReparseTag: 0,
    };
    // SAFETY: the handle is open for the call, and `info` is a `FILE_ATTRIBUTE_TAG_INFO` of the
    // size passed, which the call writes.
    let queried = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileAttributeTagInfo,
            (&raw mut info).cast(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if queried == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let reparse_directory = FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT;
    Ok(info.FileAttributes & reparse_directory == reparse_directory
        && info.ReparseTag == IO_REPARSE_TAG_MOUNT_POINT)
}

/// Whether the mount folder at `path` leads to the root of a volume, where the system keeps
/// entries of its own that deny access. Unix has no mount folder.
#[cfg(target_family = "unix")]
#[lore_macro::test_pub]
fn is_volume_root(_path: &std::path::Path) -> bool {
    false
}

/// Whether the directory at `path` is the root of a volume, a drive or a volume mounted in a
/// folder, which `GetVolumeNameForVolumeMountPointW` names. A junction to an ordinary directory
/// is not. Where the system cannot answer, the directory is taken as not a volume root.
#[cfg(target_family = "windows")]
#[lore_macro::test_pub]
fn is_volume_root(path: &std::path::Path) -> bool {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW;

    const BACKSLASH: u16 = b'\\' as u16;
    let mut mount_point: Vec<u16> = path.as_os_str().encode_wide().collect();
    if mount_point.last() != Some(&BACKSLASH) {
        mount_point.push(BACKSLASH);
    }
    mount_point.push(0);
    // A volume name is `\\?\Volume{GUID}\`: 49 characters and the terminator.
    let mut volume_name = [0u16; 50];
    // SAFETY: `mount_point` is a terminated wide string, and `volume_name` holds the number of
    // characters passed, which the call writes at most.
    let named = unsafe {
        GetVolumeNameForVolumeMountPointW(
            mount_point.as_ptr(),
            volume_name.as_mut_ptr(),
            volume_name.len() as u32,
        )
    };
    named != 0
}

/// Whether `entry` is one the system keeps at the root of a volume. Unix has none.
#[cfg(target_family = "unix")]
fn is_volume_system_entry(_entry: &std::fs::DirEntry) -> bool {
    false
}

/// Whether `entry` is one the system keeps at the root of a volume, such as
/// `System Volume Information` and `$RECYCLE.BIN`, which deny access to a removal: an entry both
/// hidden and system.
#[cfg(target_family = "windows")]
fn is_volume_system_entry(entry: &std::fs::DirEntry) -> bool {
    use std::os::windows::fs::MetadataExt;

    entry
        .metadata()
        .is_ok_and(|metadata| is_hidden_system(metadata.file_attributes()))
}

/// Whether `attributes` mark an entry both hidden and system.
#[cfg(target_family = "windows")]
#[lore_macro::test_pub]
fn is_hidden_system(attributes: u32) -> bool {
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_SYSTEM;

    let hidden_system = FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM;
    attributes & hidden_system == hidden_system
}

/// Whether a rename or removal of a directory failed because the directory is a mount point:
/// `EXDEV` or `EBUSY` from a rename out of it or a removal of it.
#[cfg(target_family = "unix")]
#[lore_macro::test_pub]
fn is_mount_point_refusal(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(libc::EXDEV | libc::EBUSY))
}

/// Whether a rename of a directory failed because the directory is a mount point:
/// `ERROR_NOT_SAME_DEVICE`, which a rename of a volume's mount point gives.
#[cfg(target_family = "windows")]
#[lore_macro::test_pub]
fn is_mount_point_refusal(err: &std::io::Error) -> bool {
    err.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_NOT_SAME_DEVICE as i32)
}

// A case-sensitive filesystem holds the name it was given, so a path that
// resolves is a path spelled the way the filesystem holds it, and one that does
// not is a name it does not hold - either way an answer. Following links matches
// what a caller asking about a name would do with the path next; a dangling one
// answers `Some(false)` and is left to the directory read.
#[cfg(target_os = "linux")]
fn holds_name_exactly_impl(path: &std::path::Path) -> Option<bool> {
    Some(std::fs::metadata(path).is_ok())
}

/// Characters the Win32 lookup reads as a pattern rather than as themselves:
/// the two wildcards, and the three legacy DOS ones the NT matcher still takes
/// (`<` for `DOS_STAR`, `>` for `DOS_QM`, `"` for `DOS_DOT`; `<` and `"` do
/// match `Rock.mesh` when passed through). None is legal in a Windows path, so
/// a path holding one names nothing.
///
/// UTF-16 gives no non-ASCII character an ASCII unit, so this reads the same
/// before or after the conversion.
///
/// Skipping those is an optimisation, not the safeguard: what a pattern matched
/// would be some other name and so fails the comparison below regardless. It
/// saves the two syscalls spent finding that out.
#[cfg(target_family = "windows")]
fn is_pattern_unit(unit: u16) -> bool {
    u8::try_from(unit).is_ok_and(|byte| matches!(byte, b'*' | b'?' | b'<' | b'>' | b'"'))
}

/// Units of path the Win32 path calls here are willing to build.
///
/// A plain path is capped at `MAX_PATH` by the calls themselves: past that they
/// want the `\\?\` verbatim prefix, which is a rewrite of the path rather than a
/// longer buffer, which [`crate::win_path::to_extended_wide`] makes and these
/// calls do not. A path that arrives already verbatim may be longer, so
/// there is room for one, and anything past even that is left to what `std`
/// answers, since `std` prefixes on its own.
#[cfg(target_family = "windows")]
pub(crate) const PATH_UNITS: usize = 512;

/// The path length a plain path call is defined for, terminator included. Past it
/// the call fails on a path that is perfectly well there, which is a fact about
/// the call and not about the path - so a longer path is declined rather than
/// answered, and only a verbatim one is carried through.
#[cfg(target_family = "windows")]
const MAX_PATH_UNITS: usize = 260;

/// Why a path cannot be handed to a Win32 path call as it stands.
#[cfg(target_family = "windows")]
pub(crate) enum Unencodable {
    /// The path holds a null, which would end the path the call sees early and make it
    /// answer about a different one. No path a filesystem holds carries one.
    Null,
    /// The path holds [`PATH_UNITS`] units or more, or [`MAX_PATH_UNITS`] or more without the
    /// verbatim prefix that lifts that limit.
    Long,
}

/// Writes `path` followed by `suffix` into `wide` as the null-terminated UTF-16 a Win32 path
/// call takes.
///
/// A path that arrives with the verbatim prefix is written as it is and is not held to
/// `MAX_PATH`; one without it is, and rewriting it into a verbatim one is not this
/// function's to do. The limits hold for the whole of what is written, `suffix` included.
#[cfg(target_family = "windows")]
pub(crate) fn encode_wide_path(
    path: &std::path::Path,
    suffix: &[u16],
    wide: &mut [u16; PATH_UNITS],
) -> Result<(), Unencodable> {
    use std::os::windows::ffi::OsStrExt;

    let mut length = 0;
    for unit in path.as_os_str().encode_wide().chain(suffix.iter().copied()) {
        if length == wide.len() - 1 {
            return Err(Unencodable::Long);
        }
        if unit == 0 {
            return Err(Unencodable::Null);
        }
        wide[length] = unit;
        length += 1;
    }
    wide[length] = 0;

    let verbatim = matches!(
        path.components().next(),
        Some(std::path::Component::Prefix(prefix)) if prefix.kind().is_verbatim()
    );
    if !verbatim && length >= MAX_PATH_UNITS {
        return Err(Unencodable::Long);
    }
    Ok(())
}

// `FindFirstFileExW` resolves the path and reports the name as the filesystem
// holds it, which is compared in place: the caller wants a verdict, and building
// an `OsString` to compare and drop is the allocation this exists to avoid.
// `FindExInfoBasic` leaves the 8.3 name it would otherwise fill in unqueried. A
// path holding a null names nothing, and one the call cannot take as it stands
// says nothing about the name.
#[cfg(target_family = "windows")]
fn holds_name_exactly_impl(path: &std::path::Path) -> Option<bool> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::FindClose;
    use windows_sys::Win32::Storage::FileSystem::FindExInfoBasic;
    use windows_sys::Win32::Storage::FileSystem::FindExSearchNameMatch;
    use windows_sys::Win32::Storage::FileSystem::FindFirstFileExW;
    use windows_sys::Win32::Storage::FileSystem::WIN32_FIND_DATAW;

    // A path ending in a root or in `..` names no child, so there is no spelling
    // to hold.
    let Some(name) = path.file_name() else {
        return Some(false);
    };

    // Only the last component is matched as a pattern - the rest of the path is
    // resolved literally, and a pattern character in it simply resolves to
    // nothing - so only the last component is checked, which is also the
    // shorter scan by far. Checking the whole path would refuse every verbatim
    // `\\?\` one over the `?` in its prefix.
    if name.encode_wide().any(is_pattern_unit) {
        return Some(false);
    }

    let mut wide = [0u16; PATH_UNITS];
    match encode_wide_path(path, &[], &mut wide) {
        Ok(()) => {}
        Err(Unencodable::Null) => return Some(false),
        Err(Unencodable::Long) => return None,
    }

    let mut found = std::mem::MaybeUninit::<WIN32_FIND_DATAW>::uninit();
    // SAFETY: the path is null-terminated and the structure is writable for the
    // size the call fills in. The search takes no filter, which is what
    // `FindExSearchNameMatch` with a null filter means.
    let handle = unsafe {
        FindFirstFileExW(
            wide.as_ptr(),
            FindExInfoBasic,
            found.as_mut_ptr().cast(),
            FindExSearchNameMatch,
            std::ptr::null(),
            0,
        )
    };
    // The lookup ran and matched nothing, which is an answer about the spelling:
    // whatever the reason - no such name, no such directory, no access to it -
    // the caller reads the directory next and gets the reason from there.
    if handle == INVALID_HANDLE_VALUE {
        return Some(false);
    }
    // SAFETY: the call succeeded, so it initialized the structure, and the
    // handle it returned is closed once and not used again.
    let found = unsafe {
        let found = found.assume_init();
        FindClose(handle);
        found
    };

    Some(
        name.encode_wide()
            .eq(crate::find::record_name(&found).iter().copied()),
    )
}

// macOS holds one spelling and answers lookups in any other, as Windows does,
// but without a call that is cheaper than reading the directory the caller
// would otherwise read. There is nothing to answer with that is not that read,
// so it does not answer.
#[cfg(not(any(target_os = "linux", target_family = "windows")))]
fn holds_name_exactly_impl(_path: &std::path::Path) -> Option<bool> {
    None
}

/// Removes the tree at `path`, as `std::fs::remove_dir_all` does. Where a directory in it denies
/// the removal, as one without its owner's write permission does on Unix, the owner is given read,
/// write and search permission on every directory in the tree, and the removal is tried once more.
#[lore_macro::test_pub]
fn remove_tree_making_it_writable(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            make_directories_owner_writable(path)?;
            std::fs::remove_dir_all(path)
        }
        removed => removed,
    }
}

/// Gives the owner read, write and search permission on the directory at `path` and on every
/// directory below it. A link listed in the tree is not followed, and a directory gone meanwhile is
/// left out. A directory replaced by a link between its stat and its change would be followed;
/// the tree is one that a removal has renamed aside, which no other process writes.
#[cfg(target_family = "unix")]
fn make_directories_owner_writable(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    const OWNER_ALL: u32 = 0o700;
    let mut waiting = vec![path.to_path_buf()];
    while let Some(directory) = waiting.pop() {
        let mode = match std::fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() => metadata.permissions().mode(),
            Ok(_) => continue,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        if mode & OWNER_ALL != OWNER_ALL {
            std::fs::set_permissions(
                &directory,
                std::fs::Permissions::from_mode((mode & 0o7777) | OWNER_ALL),
            )?;
        }
        let entries = match std::fs::read_dir(&directory) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            entries => entries?,
        };
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                waiting.push(entry.path());
            }
        }
    }
    Ok(())
}

/// Nothing to do: Windows removes an entry whatever the attributes of the directory that holds
/// it, and `std::fs::remove_dir_all` clears a read-only file's attribute itself.
#[cfg(target_family = "windows")]
#[allow(clippy::unnecessary_wraps)]
fn make_directories_owner_writable(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

/// Whether `err`, from reaching what a path names, says it names nothing: no entry is there, or a
/// file stands where a directory leads to it.
fn names_nothing(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// The device and inode numbers of the file `path` names, symbolic links followed, and `None`
/// where it names nothing.
#[cfg(target_family = "unix")]
fn entry_identity(path: &std::path::Path) -> std::io::Result<Option<(u64, u64)>> {
    use std::os::unix::fs::MetadataExt;

    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some((metadata.dev(), metadata.ino()))),
        Err(err) if names_nothing(&err) => Ok(None),
        Err(err) => Err(err),
    }
}

/// The volume serial number and file ID of the file `path` names, symbolic links followed, and
/// `None` where it names nothing, read through a handle opened for its attributes alone.
///
/// The 128-bit ID is the one `FileIdInfo` answers with, and where the volume does not answer it,
/// the 64-bit file index `GetFileInformationByHandle` answers with, which `std` and the `same-file`
/// crate read a file's identity from.
#[cfg(target_family = "windows")]
fn entry_identity(path: &std::path::Path) -> std::io::Result<Option<(u64, [u8; 16])>> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    use windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::FileIdInfo;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;

    let file = match std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
    {
        Ok(file) => file,
        Err(err) if names_nothing(&err) => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut info = FILE_ID_INFO::default();
    // SAFETY: the handle is open for the call, and `info` is a `FILE_ID_INFO` of the size passed,
    // which the call writes.
    let queried = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&raw mut info).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if queried != 0 {
        return Ok(Some((info.VolumeSerialNumber, info.FileId.Identifier)));
    }
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is open for the call, and `info` is the structure the call writes.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut info) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    let mut identifier = [0u8; 16];
    identifier[..8].copy_from_slice(&index.to_le_bytes());
    Ok(Some((u64::from(info.dwVolumeSerialNumber), identifier)))
}

/// Adds the owner's write permission to the directory `path` names itself, through a handle that
/// `O_NOFOLLOW` keeps from opening a symbolic link and `O_DIRECTORY` from opening anything but a
/// directory.
#[cfg(target_family = "unix")]
fn make_owner_writable_impl(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::fs::PermissionsExt;

    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    let mode = directory.metadata()?.permissions().mode();
    if mode & 0o200 != 0 {
        return Ok(());
    }
    directory.set_permissions(std::fs::Permissions::from_mode((mode & 0o7777) | 0o200))
}

/// Clears the read-only attribute of the name `path` through a handle opened without following a
/// reparse point, as `std` sets permissions on a name it does not follow.
#[cfg(target_family = "windows")]
fn make_owner_writable_impl(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES;

    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let mut permissions = file.metadata()?.permissions();
    if !permissions.readonly() {
        return Ok(());
    }
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    file.set_permissions(permissions)
}

/// What `std::fs::metadata` answers for `path`.
pub(crate) fn std_stat(path: &std::path::Path) -> std::io::Result<crate::Stat> {
    std::fs::metadata(path).map(|metadata| crate::Stat::from(&metadata))
}

#[cfg(not(target_family = "windows"))]
fn stat_impl(path: &std::path::Path) -> std::io::Result<crate::Stat> {
    std_stat(path)
}

/// What [`attribute_stat`] answers for `path`, following a reparse point, and what [`std_stat`]
/// answers where it declines.
#[cfg(target_family = "windows")]
fn stat_impl(path: &std::path::Path) -> std::io::Result<crate::Stat> {
    attribute_stat(path, true).unwrap_or_else(|| std_stat(path))
}

/// What `std::fs::symlink_metadata` answers for `path`, the name itself.
#[cfg(not(target_family = "windows"))]
fn symlink_stat_impl(path: &std::path::Path) -> std::io::Result<crate::Stat> {
    std::fs::symlink_metadata(path).map(|metadata| crate::Stat::from(&metadata))
}

/// What [`attribute_stat`] answers for `path`, the name itself, and where it declines what
/// `std::fs::symlink_metadata` answers, with the kind the name's attributes give.
#[cfg(target_family = "windows")]
fn symlink_stat_impl(path: &std::path::Path) -> std::io::Result<crate::Stat> {
    use std::os::windows::fs::MetadataExt;

    attribute_stat(path, false).unwrap_or_else(|| {
        std::fs::symlink_metadata(path).map(|metadata| crate::Stat {
            kind: crate::stat::attribute_kind(metadata.file_attributes()),
            ..crate::Stat::from(&metadata)
        })
    })
}

/// What one `GetFileAttributesExW` answers for `path`, which it queries without a handle and
/// without following a reparse point, or `None` where the answer could differ from what `std`
/// gives: for a relative path, which `std` resolves first; a path the call cannot take as it
/// stands; a reparse point where `follow` asks for what it stands for; a time `SystemTime` cannot
/// hold; and every failure but the name not being there, which the two report with the same code.
#[lore_macro::test_pub]
#[cfg(target_family = "windows")]
fn attribute_stat(path: &std::path::Path, follow: bool) -> Option<std::io::Result<crate::Stat>> {
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_sys::Win32::Foundation::ERROR_PATH_NOT_FOUND;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::GetFileAttributesExW;
    use windows_sys::Win32::Storage::FileSystem::GetFileExInfoStandard;
    use windows_sys::Win32::Storage::FileSystem::WIN32_FILE_ATTRIBUTE_DATA;

    if !path.is_absolute() {
        return None;
    }
    let mut wide = [0u16; PATH_UNITS];
    encode_wide_path(path, &[], &mut wide).ok()?;

    let mut data = std::mem::MaybeUninit::<WIN32_FILE_ATTRIBUTE_DATA>::uninit();
    // SAFETY: the path is null-terminated, and the structure is writable for the size the
    // standard information level fills in.
    let queried = unsafe {
        GetFileAttributesExW(
            wide.as_ptr(),
            GetFileExInfoStandard,
            data.as_mut_ptr().cast(),
        )
    };
    if queried == 0 {
        let error = std::io::Error::last_os_error();
        let absent = matches!(
            error.raw_os_error(),
            Some(code) if code == ERROR_FILE_NOT_FOUND as i32 || code == ERROR_PATH_NOT_FOUND as i32
        );
        return absent.then_some(Err(error));
    }
    // SAFETY: the call succeeded, so it filled the structure in.
    let data = unsafe { data.assume_init() };
    if follow && data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return None;
    }
    crate::stat::from_attribute_data(&data).map(Ok)
}

#[cfg(target_family = "unix")]
fn read_at_impl(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    retry_on_interrupt(|| file.read_at(buffer, offset))
}

#[cfg(target_family = "unix")]
fn write_at_impl(file: &File, buffer: &[u8], offset: u64) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    retry_on_interrupt(|| file.write_at(buffer, offset))
}

// Windows handles opened through this crate are overlapped, and `std`'s positional calls are only
// defined for synchronous ones — see `overlapped.rs`. These work on either kind, which they have to:
// the whole-file operations above open their own synchronous handle.
#[cfg(target_family = "windows")]
fn read_at_impl(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    crate::overlapped::read_at(file, buffer, offset)
}

#[cfg(target_family = "windows")]
fn write_at_impl(file: &File, buffer: &[u8], offset: u64) -> std::io::Result<usize> {
    crate::overlapped::write_at(file, buffer, offset)
}

/// Writes `buffers` to `file` from its start and, with `durable`, syncs it. The segments are
/// released once the kernel has the bytes, before the sync, which touches the page cache rather
/// than the caller's memory, and `file` is closed by the time it returns, so that the caller can
/// rename or remove it then.
fn write_segments<B: StableBufList>(file: File, buffers: B, durable: bool) -> std::io::Result<()> {
    let total: usize = buffers.byte_segments().map(|segment| segment.len()).sum();
    let mut done = 0;
    while done < total {
        let written = write_vectored_impl(&file, &buffers, done, done as u64)?;
        if written == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "file refused further writes",
            ));
        }
        done += written;
    }
    drop(buffers);
    if durable {
        file.sync_all()?;
    }
    Ok(())
}

#[cfg(target_family = "unix")]
fn sync_parent_dir(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(parent)?;
    // Safety: Calling OS functions
    let result = unsafe { libc::fsync(dir.as_raw_fd()) };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

// Windows has no API to flush a directory.
#[cfg(target_family = "windows")]
fn sync_parent_dir(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(target_family = "unix")]
fn read_vectored_impl<B: StableBufListMut>(
    file: &File,
    buffers: &mut B,
    skip: usize,
    offset: u64,
) -> std::io::Result<usize> {
    use std::os::fd::AsRawFd;
    let mut iovecs: Vec<libc::iovec> = Vec::new();
    let mut remaining_skip = skip;
    for segment in buffers.byte_segments_mut() {
        if iovecs.len() == MAX_IO_SEGMENTS {
            break;
        }
        let len = segment.len();
        if remaining_skip >= len {
            remaining_skip -= len;
            continue;
        }
        iovecs.push(libc::iovec {
            // Safety: remaining_skip is within the segment bounds
            iov_base: unsafe { segment.as_mut_ptr().add(remaining_skip) }.cast(),
            iov_len: len - remaining_skip,
        });
        remaining_skip = 0;
    }
    if iovecs.is_empty() {
        return Ok(0);
    }
    // Safety: the iovecs point into live segments borrowed above
    let read = unsafe {
        libc::preadv(
            file.as_raw_fd(),
            iovecs.as_ptr(),
            iovecs.len() as libc::c_int,
            offset as libc::off_t,
        )
    };
    if read < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(read as usize)
}

#[cfg(target_family = "unix")]
fn write_vectored_impl<B: StableBufList>(
    file: &File,
    buffers: &B,
    skip: usize,
    offset: u64,
) -> std::io::Result<usize> {
    use std::os::fd::AsRawFd;
    let mut iovecs: Vec<libc::iovec> = Vec::new();
    let mut remaining_skip = skip;
    for segment in buffers.byte_segments() {
        if iovecs.len() == MAX_IO_SEGMENTS {
            break;
        }
        let len = segment.len();
        if remaining_skip >= len {
            remaining_skip -= len;
            continue;
        }
        iovecs.push(libc::iovec {
            // Safety: remaining_skip is within the segment bounds; writes
            // only read through the pointer, so the const-to-mut cast that
            // the iovec layout requires never mutates the segment
            iov_base: unsafe { segment.as_ptr().add(remaining_skip) }
                .cast_mut()
                .cast(),
            iov_len: len - remaining_skip,
        });
        remaining_skip = 0;
    }
    if iovecs.is_empty() {
        return Ok(0);
    }
    // Safety: the iovecs point into live segments borrowed above
    let written = unsafe {
        libc::pwritev(
            file.as_raw_fd(),
            iovecs.as_ptr(),
            iovecs.len() as libc::c_int,
            offset as libc::off_t,
        )
    };
    if written < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(written as usize)
}

// Windows has no positional scatter/gather syscall for ordinary handles
// (ReadFileScatter requires page-aligned, sector-sized segments), so the
// portable backend walks the segments with positional reads/writes inside
// the single pool dispatch.
#[cfg(target_family = "windows")]
fn read_vectored_impl<B: StableBufListMut>(
    file: &File,
    buffers: &mut B,
    skip: usize,
    offset: u64,
) -> std::io::Result<usize> {
    let mut remaining_skip = skip;
    let mut offset = offset;
    let mut total = 0;
    for segment in buffers.byte_segments_mut() {
        let len = segment.len();
        if remaining_skip >= len {
            remaining_skip -= len;
            continue;
        }
        let target = &mut segment[remaining_skip..];
        remaining_skip = 0;
        let mut done = 0;
        while done < target.len() {
            let read = read_at_impl(file, &mut target[done..], offset + done as u64)?;
            if read == 0 {
                return Ok(total + done);
            }
            done += read;
        }
        total += done;
        offset += done as u64;
    }
    Ok(total)
}

#[cfg(target_family = "windows")]
fn write_vectored_impl<B: StableBufList>(
    file: &File,
    buffers: &B,
    skip: usize,
    offset: u64,
) -> std::io::Result<usize> {
    let mut remaining_skip = skip;
    let mut offset = offset;
    let mut total = 0;
    for segment in buffers.byte_segments() {
        let len = segment.len();
        if remaining_skip >= len {
            remaining_skip -= len;
            continue;
        }
        let source = &segment[remaining_skip..];
        remaining_skip = 0;
        let mut done = 0;
        while done < source.len() {
            let written = write_at_impl(file, &source[done..], offset + done as u64)?;
            if written == 0 {
                return Ok(total + done);
            }
            done += written;
        }
        total += done;
        offset += done as u64;
    }
    Ok(total)
}
