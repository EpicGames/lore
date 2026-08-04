// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fs::File;
use std::sync::Arc;

use bytes::Bytes;

use crate::buffer::StableBuf;
use crate::driver::IoDriver;

/// Options for opening a file, mirroring `std::fs::OpenOptions`.
#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    create: bool,
    create_new: bool,
    truncate: bool,
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

    pub(crate) fn to_std(&self) -> std::fs::OpenOptions {
        let mut options = std::fs::OpenOptions::new();
        options
            .read(self.read)
            .write(self.write)
            .create(self.create)
            .create_new(self.create_new)
            .truncate(self.truncate);
        // A synchronous Windows handle serializes every operation issued on it, whatever the
        // caller does, and this API exists to run many at once on one shared handle. So the handle
        // is overlapped and the data path issues its own `OVERLAPPED`; `crate::overlapped` records
        // why `std`'s positional calls cannot be used on such a handle.
        #[cfg(target_family = "windows")]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED);
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
