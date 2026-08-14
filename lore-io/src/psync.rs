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
                let len = file.metadata()?.len() as usize;
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

    pub(crate) async fn write_file_segments<B: StableBufList>(
        &self,
        options: std::fs::OpenOptions,
        path: PathBuf,
        buffers: B,
        durable: bool,
    ) -> std::io::Result<()> {
        SyscallPool::global()
            .submit(move || {
                let file = options.open(path)?;
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
                // See the atomic twin: the segments are released before the sync, not after it.
                drop(buffers);
                if durable {
                    file.sync_all()?;
                }
                Ok(())
            })
            .await
    }

    pub(crate) async fn write_file_segments_atomic<B: StableBufList>(
        &self,
        options: std::fs::OpenOptions,
        temporary_path: PathBuf,
        final_path: PathBuf,
        buffers: B,
    ) -> std::io::Result<()> {
        SyscallPool::global()
            .submit(move || {
                let file = options.open(&temporary_path)?;
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
                // The kernel has the bytes, so whatever kept the segments stable is free from
                // here: the sync and the rename below touch the page cache and the directory,
                // never the caller's memory.
                drop(buffers);
                file.sync_all()?;
                drop(file);
                std::fs::rename(&temporary_path, &final_path)?;
                // Directory sync is best-effort: the rename is already
                // durable in content, and a lost directory entry after a
                // crash reproduces the pre-write state the caller already
                // handles.
                let _ = sync_parent_dir(&final_path);
                Ok(())
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

    pub(crate) async fn file_metadata(
        &self,
        file: Arc<File>,
    ) -> std::io::Result<std::fs::Metadata> {
        SyscallPool::global().submit(move || file.metadata()).await
    }

    pub(crate) async fn set_len(&self, file: Arc<File>, len: u64) -> std::io::Result<()> {
        SyscallPool::global()
            .submit(move || retry_on_interrupt(|| file.set_len(len)))
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

    pub(crate) async fn read_dir(&self, path: PathBuf) -> std::io::Result<crate::DirStream> {
        SyscallPool::global()
            .submit(move || Ok(crate::DirStream::start(std::fs::read_dir(path)?)))
            .await
    }

    pub(crate) async fn create_dir_all(&self, path: PathBuf) -> std::io::Result<()> {
        SyscallPool::global()
            .submit(move || std::fs::create_dir_all(path))
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
