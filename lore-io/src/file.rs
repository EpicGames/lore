// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fs::File;
use std::sync::Arc;

use bytes::Bytes;

use crate::buffer::StableBuf;
use crate::buffer::StableBufList;
use crate::buffer::StableBufListMut;
use crate::driver::IoDriver;

/// Options for opening a file, mirroring `std::fs::OpenOptions`.
#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    create: bool,
    create_new: bool,
    truncate: bool,
    #[cfg(target_family = "windows")]
    share_mode: Option<u32>,
}

impl OpenOptions {
    pub fn new() -> OpenOptions {
        OpenOptions::default()
    }

    pub fn read(mut self, read: bool) -> OpenOptions {
        self.read = read;
        self
    }

    pub fn write(mut self, write: bool) -> OpenOptions {
        self.write = write;
        self
    }

    pub fn create(mut self, create: bool) -> OpenOptions {
        self.create = create;
        self
    }

    pub fn create_new(mut self, create_new: bool) -> OpenOptions {
        self.create_new = create_new;
        self
    }

    pub fn truncate(mut self, truncate: bool) -> OpenOptions {
        self.truncate = truncate;
        self
    }

    /// Restricts the Windows share mode (`FILE_SHARE_*` flags), mirroring
    /// `std::os::windows::fs::OpenOptionsExt::share_mode`. Unset, a handle shares read, write and
    /// deletion, as `std` does; a file the store owns names the narrower mode it wants here.
    #[cfg(target_family = "windows")]
    pub fn share_mode(mut self, share_mode: u32) -> OpenOptions {
        self.share_mode = Some(share_mode);
        self
    }

    /// The options for a handle the data path drives with its own `OVERLAPPED`.
    ///
    /// A synchronous Windows handle serializes every operation issued on it, whatever the caller
    /// does, and this API exists to run many at once on one shared handle. So the handle is
    /// overlapped and the data path issues its own `OVERLAPPED`; `crate::overlapped` records why
    /// `std`'s positional calls cannot be used on such a handle.
    pub(crate) fn to_std(&self) -> std::fs::OpenOptions {
        #[cfg(not(target_family = "windows"))]
        return self.to_std_blocking();

        #[cfg(target_family = "windows")]
        {
            use std::os::windows::fs::OpenOptionsExt;

            let mut options = self.to_std_blocking();
            options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED);
            options
        }
    }

    /// The options for a handle a pool thread drives with ordinary synchronous calls, which an
    /// overlapped handle cannot serve. Carries the same share modes as [`to_std`](Self::to_std),
    /// so a one-shot whole-file operation guards the file exactly as a long-lived handle does.
    pub(crate) fn to_std_blocking(&self) -> std::fs::OpenOptions {
        let mut options = std::fs::OpenOptions::new();
        options
            .read(self.read)
            .write(self.write)
            .create(self.create)
            .create_new(self.create_new)
            .truncate(self.truncate);
        #[cfg(target_family = "windows")]
        {
            use std::os::windows::fs::OpenOptionsExt;

            use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
            use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
            use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;

            // `CreateFileW` opens exclusive by default; `std` shares read, write and deletion on
            // every handle, and so does this. Most files Lore opens are files it does not own —
            // working-tree files an editor, an engine or a scanner holds at the same moment — and
            // a handle refusing them fails the operation over something Lore has no say in.
            // Sharing is checked both ways, so a mode omitting a right also refuses to coexist
            // with a handle already granted it, which makes a narrow default fail opens that have
            // nothing to do with the file's contents. A file the store owns states the narrower
            // mode at its own call site, where the exclusion it needs is a local claim rather
            // than a rule imposed on every path.
            options.share_mode(
                self.share_mode
                    .unwrap_or(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE),
            );
        }
        options
    }
}

/// An open file handle with positional, owned-buffer operations.
///
/// All operations are positional: there is no file cursor, and concurrent
/// operations on one handle at disjoint offsets are safe and unordered.
/// Cloning is cheap and clones share the underlying handle.
#[derive(Clone)]
pub struct IoFile {
    driver: IoDriver,
    file: Arc<File>,
}

impl std::fmt::Debug for IoFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IoFile")
            .field("backend", &self.driver.backend_name())
            .finish()
    }
}

impl IoFile {
    pub(crate) fn new(driver: IoDriver, file: Arc<File>) -> IoFile {
        IoFile { driver, file }
    }

    pub fn driver(&self) -> &IoDriver {
        &self.driver
    }

    /// Reads up to `max_len` bytes at `offset`, returning what arrived. Fewer than `max_len` means
    /// the file ended; empty means the offset was already at or past the end.
    ///
    /// Reads allocate their own memory rather than filling a caller's buffer, and do it on the
    /// thread that receives the copy. The process allocator caches per thread, so the memory a
    /// read lands in is memory that thread already owns.
    pub async fn read_at(&self, max_len: usize, offset: u64) -> std::io::Result<Bytes> {
        self.driver
            .read_at_raw(Arc::clone(&self.file), max_len, offset)
            .await
    }

    /// Reads exactly `len` bytes at `offset`, failing with `UnexpectedEof` if the file ends first.
    pub async fn read_exact_at(&self, len: usize, offset: u64) -> std::io::Result<Bytes> {
        self.driver
            .read_exact_at_raw(Arc::clone(&self.file), len, offset)
            .await
    }

    /// Writes the buffer contents at `offset`. Returns the buffer and the number of bytes
    /// written.
    ///
    /// The length is the buffer's own: writes carry data the caller already has, so unlike a read
    /// there is nothing to allocate and no length to pass.
    pub async fn write_at<B: StableBuf>(
        &self,
        buffer: B,
        offset: u64,
    ) -> std::io::Result<(B, usize)> {
        let len = buffer.as_ref().len();
        self.driver
            .write_at_raw(Arc::clone(&self.file), buffer, 0, len, offset)
            .await
    }

    /// Writes the complete buffer contents at `offset`, looping until they are all written.
    pub async fn write_all_at<B: StableBuf>(&self, buffer: B, offset: u64) -> std::io::Result<B> {
        let len = buffer.as_ref().len();
        self.driver
            .write_all_at_raw(Arc::clone(&self.file), buffer, len, offset)
            .await
    }

    /// Reads exactly the combined byte length of all segments at `offset`,
    /// scattering directly into the segments with no intermediate buffer.
    /// Fails with `UnexpectedEof` if the file ends first.
    pub async fn read_exact_vectored_at<B: StableBufListMut>(
        &self,
        buffers: B,
        offset: u64,
    ) -> std::io::Result<B> {
        let mut buffers = buffers;
        let total: usize = buffers
            .byte_segments_mut()
            .map(|segment| segment.len())
            .sum();
        let mut done = 0;
        while done < total {
            let (returned, read) = self
                .driver
                .read_vectored_at_raw(Arc::clone(&self.file), buffers, done, offset + done as u64)
                .await?;
            buffers = returned;
            if read == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "file ended before the requested read length",
                ));
            }
            done += read;
        }
        Ok(buffers)
    }

    /// Writes the combined contents of all segments at `offset`, gathering
    /// directly from the segments with no intermediate buffer.
    pub async fn write_all_vectored_at<B: StableBufList>(
        &self,
        buffers: B,
        offset: u64,
    ) -> std::io::Result<B> {
        let mut buffers = buffers;
        let total: usize = buffers.byte_segments().map(|segment| segment.len()).sum();
        let mut done = 0;
        while done < total {
            let (returned, written) = self
                .driver
                .write_vectored_at_raw(Arc::clone(&self.file), buffers, done, offset + done as u64)
                .await?;
            buffers = returned;
            if written == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "file refused further writes",
                ));
            }
            done += written;
        }
        Ok(buffers)
    }

    /// Syncs file data (not necessarily metadata) to disk.
    pub async fn sync_data(&self) -> std::io::Result<()> {
        self.driver.sync_raw(Arc::clone(&self.file), true).await
    }

    /// Syncs file data and metadata to disk.
    pub async fn sync_all(&self) -> std::io::Result<()> {
        self.driver.sync_raw(Arc::clone(&self.file), false).await
    }

    pub async fn metadata(&self) -> std::io::Result<std::fs::Metadata> {
        self.driver.file_metadata_raw(Arc::clone(&self.file)).await
    }

    /// Sets the file length, extending it with a hole that reads as zeros or truncating it.
    pub async fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.driver.set_len_raw(Arc::clone(&self.file), len).await
    }
}
