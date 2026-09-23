// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Where content is read from, and how it is read.
//!
//! Chunking and comparison both read content the same way — a window at an offset — and neither
//! needs to know where it is held. [`ContentSource`] names where; [`ContentHandle`] reads it.

use std::path::Path;

use bytes::Bytes;
use bytes::BytesMut;

use crate::error::StorageError;
use crate::errors::InvalidArguments;

/// The window a read fills, owned by the operation for its whole flight and handed back
/// with it. A single segment: the read lands in `buffer[start..start + want]`, leaving
/// anything in front of it as headroom for bytes the caller carries over itself.
pub struct WindowRead {
    pub(crate) buffer: BytesMut,
    pub(crate) start: usize,
    pub(crate) want: usize,
}

impl WindowRead {
    /// A read of `want` bytes landing at `start`, which `buffer` must already hold room for:
    /// the read fills what it was asked for or fails, and never grows the buffer to do it.
    pub fn new(buffer: BytesMut, start: usize, want: usize) -> Self {
        Self {
            buffer,
            start,
            want,
        }
    }
}

impl lore_io::StableBufListMut for WindowRead {
    fn byte_segments_mut(&mut self) -> impl Iterator<Item = &mut [u8]> {
        std::iter::once(&mut self.buffer[self.start..self.start + self.want])
    }
}

/// Where content a chunking or a comparison reads is held.
///
/// Names the content without opening it: a comparison settled by size alone, or by a hash of the
/// whole content, never opens one. Opening yields a [`ContentHandle`], which is what reads
/// windows.
///
/// Separate from [`crate::write::ContentHashes`], which holds what was computed about content
/// rather than where it was read from, so that one of those serves a comparison whatever holds
/// the content.
///
/// A closed set of content kinds, each named by a type this crate or one below it owns, so every
/// read is dispatched statically and no signature here carries a type parameter for it. Content a
/// library of its own reads rather than the IO driver earns a variant, and that library's read
/// primitive belongs below this crate — a filesystem provider chooses which content, never how it
/// is read.
pub enum ContentSource<'a> {
    /// A file on the host filesystem, read through the IO driver.
    File(std::borrow::Cow<'a, Path>),
    /// Content a virtualizing library holds, named by whatever reads it.
    Virtual(VirtualContent),
}

/// What reads content a virtualizing library holds.
///
/// Uninhabited until such a library's read primitive lives below this crate, so nothing can
/// construct one and every arm reading one is unreachable. The variants that read content are
/// where that primitive plugs in.
#[derive(Clone)]
pub enum VirtualContent {}

impl<'a> ContentSource<'a> {
    /// Content at a path the caller already holds.
    pub fn file(path: &'a Path) -> Self {
        ContentSource::File(std::borrow::Cow::Borrowed(path))
    }

    /// Content at a path the caller built, which outlives no borrow.
    pub fn owned_file(path: std::path::PathBuf) -> ContentSource<'static> {
        ContentSource::File(std::borrow::Cow::Owned(path))
    }

    /// How large the content is, which settles a comparison before any of it is read.
    pub(crate) async fn size(&self) -> Result<u64, StorageError> {
        let path = self.host_path();
        lore_io::IoDriver::global()
            .metadata(path)
            .await
            .map(|metadata| metadata.len())
            .map_err(|err| {
                StorageError::internal_with_context(
                    err,
                    &format!("failed to query file metadata: {}", path.display()),
                )
            })
    }

    /// The whole content at once, naming it rather than opening it, which reads it in one
    /// dispatch. A caller already holding a handle reads through [`ContentHandle::read_all`]
    /// instead of naming the content twice.
    ///
    /// The caller budgets for holding the content resident.
    pub async fn read_all(&self) -> Result<Bytes, StorageError> {
        let path = self.host_path();
        lore_io::IoDriver::global()
            .read_file_bytes(path)
            .await
            .map_err(|err| {
                StorageError::internal_with_context(err, &format!("read file: {}", path.display()))
            })
    }

    /// The content as a handle windows are read from, and its size, retrying a transient failure
    /// but not content the caller named wrong.
    ///
    /// Content that does not exist, or that does not name a regular file, will not open on any
    /// attempt, so spending the back-off on it costs the caller ten seconds and reports an
    /// internal fault for what is an argument error. Both are `InvalidArguments` on the first
    /// attempt. Everything else keeps the back-off, which is there for a reader holding the file
    /// open on Windows.
    pub(crate) async fn open(&self) -> Result<(ContentHandle, u64), StorageError> {
        let path = self.host_path();
        let mut retry = crate::retry(10, 10_000, 10);
        loop {
            match self.open_once().await {
                Ok(opened) => return Ok(opened),
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput
                    ) =>
                {
                    return Err(StorageError::from(InvalidArguments {
                        reason: format!("open file: {}: {err}", path.display()),
                    }));
                }
                Err(err) => {
                    if !retry.wait().await {
                        return Err(StorageError::internal_with_context(
                            err,
                            &format!("open file: {}", path.display()),
                        ));
                    }
                }
            }
        }
    }

    /// One attempt at what [`open`](Self::open) retries, for a caller whose answer to a failed
    /// open is to stop rather than to wait.
    ///
    /// A scan reporting content it cannot read as clean spends nothing on a back-off it will
    /// discard the outcome of, and one running per path in a walk would spend it per path.
    pub async fn open_once(&self) -> std::io::Result<(ContentHandle, u64)> {
        let (file, size) = open_read(self.host_path()).await?;
        Ok((ContentHandle::File(file), size))
    }

    /// The host path the content is read from, where it is read from one.
    pub fn file_path(&self) -> Option<&Path> {
        match self {
            ContentSource::File(path) => Some(path),
            ContentSource::Virtual(content) => match *content {},
        }
    }

    /// The host path a file's content is read from.
    ///
    /// Every reader here is the file variant's, so the match is where a second kind of content
    /// stops sharing them.
    fn host_path(&self) -> &Path {
        match self {
            ContentSource::File(path) => path,
            ContentSource::Virtual(content) => match *content {},
        }
    }
}

impl std::fmt::Display for ContentSource<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContentSource::File(path) => write!(formatter, "{}", path.display()),
            ContentSource::Virtual(content) => match *content {},
        }
    }
}

/// Open content, read a window at a time.
///
/// A chunker issues the next read while the current window is being cut, so it reads through a
/// handle of its own: cloning one is cheap and a read borrows nothing.
///
/// Produced by [`ContentSource::open`] or [`ContentSource::open_once`]. Read by chunking and
/// comparison here, and by a caller elsewhere scanning content it must not name a host path
/// for.
#[derive(Clone)]
pub enum ContentHandle {
    /// A file on the host, read through the IO driver.
    File(lore_io::IoFile),
    /// Content a virtualizing library holds, open. Unreachable until one is named.
    #[allow(dead_code)]
    Virtual(VirtualContent),
}

impl ContentHandle {
    /// The whole content, for a caller holding an open handle to it and hashing it in one go.
    ///
    /// Reads through the handle rather than naming the content again, so opening it is what the
    /// open already did.
    pub async fn read_all(&self, size: usize) -> std::io::Result<Bytes> {
        // SAFETY: the read fills the buffer whole before anything reads a byte of it.
        let buffer = unsafe { lore_io::uninit_buffer(size) };
        Ok(self
            .read_window(WindowRead::new(buffer, 0, size), 0)
            .await?
            .freeze())
    }

    /// Fills `window.buffer[window.start .. window.start + window.want]` from `offset` and hands
    /// the buffer back.
    ///
    /// Exact: a read either fills what was asked for or fails, so no caller reasons about a short
    /// one. The buffer is the caller's, so a walk reuses it rather than allocating per window.
    pub async fn read_window(&self, window: WindowRead, offset: u64) -> std::io::Result<BytesMut> {
        match self {
            ContentHandle::File(file) => {
                Ok(file.read_exact_vectored_at(window, offset).await?.buffer)
            }
            ContentHandle::Virtual(content) => match *content {},
        }
    }
}

/// Open `path` for reading, returning the shared handle and its size.
///
/// The size comes off the open handle rather than the path, so it describes the bytes about to be
/// read rather than what a separate stat of the path once saw. The same stat carries the file type,
/// so refusing anything but a regular file costs nothing beyond it — and has to happen here:
/// opening a directory read-only succeeds, and the size it reports is whatever the filesystem
/// chooses.
async fn open_read(path: &Path) -> std::io::Result<(lore_io::IoFile, u64)> {
    let file = lore_io::IoDriver::global()
        .open(path, &lore_io::OpenOptions::new().read(true))
        .await?;
    let metadata = file.metadata().await?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("not a regular file: {}", path.display()),
        ));
    }
    Ok((file, metadata.len()))
}
