// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fs::File;
use std::path::PathBuf;
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
    #[cfg(not(target_family = "windows"))]
    custom_flags: i32,
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

    /// Adds `flags` to the flags of the `open(2)` call, mirroring
    /// `std::os::unix::fs::OpenOptionsExt::custom_flags`. For a caller that must not follow a
    /// link at the path, or wait on a FIFO there, while it holds other locks.
    #[cfg(not(target_family = "windows"))]
    pub fn custom_flags(mut self, flags: i32) -> OpenOptions {
        self.custom_flags |= flags;
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
    #[lore_macro::test_pub]
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
        #[cfg(not(target_family = "windows"))]
        {
            use std::os::unix::fs::OpenOptionsExt;

            options.custom_flags(self.custom_flags);
        }
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

/// What an attempt at the exclusive lock of a file came to ([`IoFile::try_lock`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockAttempt {
    /// The handle holds the lock.
    Acquired,
    /// Another handle holds the lock.
    Busy,
    /// The file system refuses advisory locks.
    Unsupported,
}

/// Whether an attempt at a lock failed because the file system refuses advisory locks, as one
/// without lock support or a network mount without a lock service answers: `ENOLCK`, `ENOTSUP`,
/// `EOPNOTSUPP` or `ENOSYS` on Unix, `ERROR_NOT_SUPPORTED` or `ERROR_INVALID_FUNCTION` on Windows,
/// or [`std::io::ErrorKind::Unsupported`]. Another holder having the lock is not one.
pub fn lock_refused(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(target_family = "windows")]
    let refusals = [
        windows_sys::Win32::Foundation::ERROR_NOT_SUPPORTED as i32,
        windows_sys::Win32::Foundation::ERROR_INVALID_FUNCTION as i32,
    ];
    #[cfg(not(target_family = "windows"))]
    let refusals = [libc::ENOLCK, libc::ENOTSUP, libc::EOPNOTSUPP, libc::ENOSYS];
    err.raw_os_error()
        .is_some_and(|code| refusals.contains(&code))
}

/// One attempt at the exclusive lock of `file`: `true` where it took the lock, `false` where
/// another handle holds it.
#[cfg(not(target_family = "windows"))]
fn try_lock_file(file: &File) -> std::io::Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(err)) => Err(err),
    }
}

/// One attempt at the exclusive lock of `file`, an overlapped handle: `true` where it took the
/// lock, `false` where another handle holds it. See [`IoFile::try_lock`].
#[cfg(target_family = "windows")]
fn try_lock_file(file: &File) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Foundation::ERROR_IO_PENDING;
    use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::LOCKFILE_EXCLUSIVE_LOCK;
    use windows_sys::Win32::Storage::FileSystem::LOCKFILE_FAIL_IMMEDIATELY;
    use windows_sys::Win32::Storage::FileSystem::LockFileEx;
    use windows_sys::Win32::System::IO::GetOverlappedResult;
    use windows_sys::Win32::System::IO::OVERLAPPED;
    use windows_sys::Win32::System::Threading::CreateEventW;

    /// The event an attempt waits on, closed once the attempt is done with it.
    struct Event(HANDLE);

    impl Drop for Event {
        fn drop(&mut self) {
            // SAFETY: the handle is the event this attempt created, and nothing uses it after.
            unsafe { CloseHandle(self.0) };
        }
    }

    let refused = |err: std::io::Error| {
        if err.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
            Ok(false)
        } else {
            Err(err)
        }
    };

    // SAFETY: creates an unnamed, manual-reset event, not signalled, with default security.
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if event.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let event = Event(event);
    let handle = file.as_raw_handle();
    // SAFETY: an all-zero `OVERLAPPED` is valid, and its zero offset starts the lock at the file's
    // first byte.
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.hEvent = event.0.map_addr(|address| address | 1);
    // SAFETY: the handle is open, and `overlapped` and the event outlive the call, which the wait
    // below ends before either goes for a call left pending.
    let locked = unsafe {
        LockFileEx(
            handle,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if locked != 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
        return refused(err);
    }
    let mut transferred = 0u32;
    // SAFETY: waits for the call pending on `overlapped`, which lives until it returns.
    if unsafe { GetOverlappedResult(handle, &overlapped, &mut transferred, 1) } != 0 {
        return Ok(true);
    }
    refused(std::io::Error::last_os_error())
}

impl IoFile {
    pub(crate) fn new(driver: IoDriver, file: Arc<File>) -> IoFile {
        IoFile { driver, file }
    }

    /// Wraps `file`, which the caller opened, so that `driver` runs its positional operations and
    /// syncs as it runs those of a file [`IoDriver::open`] opened. On Windows, `file` must be
    /// opened with `FILE_FLAG_OVERLAPPED`. The `iocp` backend binds it to the driver's completion
    /// port, which fails for a handle already bound to one.
    ///
    /// # Safety
    ///
    /// On the `iocp` backend, from this call on and whether or not it succeeds, the file takes I/O
    /// only through `IoFile`s of `driver`, both through `file` and through any handle sharing its
    /// file object. The driver takes every completion its port receives for one of its own
    /// operations. The other backends require nothing.
    pub unsafe fn from_std(driver: &IoDriver, file: Arc<File>) -> std::io::Result<IoFile> {
        driver.register(&file)?;
        Ok(IoFile::new(driver.clone(), file))
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

    /// Whether `path` names the file this handle has open, as
    /// [`IoDriver::names_one_entry`](crate::IoDriver::names_one_entry) tells one entry: `false`
    /// where it names another file, the file having been renamed or replaced, or nothing.
    pub async fn is_at(&self, path: impl Into<PathBuf>) -> std::io::Result<bool> {
        self.driver
            .is_at_raw(Arc::clone(&self.file), path.into())
            .await
    }

    /// Takes the exclusive lock of the file this handle has open where no other handle holds it,
    /// without waiting: [`LockAttempt::Busy`] while another handle does, and
    /// [`LockAttempt::Unsupported`] where the file system refuses advisory locks ([`lock_refused`]).
    /// The lock is held until the last clone of this handle is dropped and every operation issued
    /// through one has ended. It is advisory on Unix, and on Windows covers the whole file, so
    /// other handles can neither read nor write it. The attempt runs on the calling thread, so on a
    /// network file system it may block that thread for as long as the server takes to answer.
    ///
    /// On Windows the attempt is a `LockFileEx` of its own rather than `std`'s, which hands an
    /// overlapped handle a stack `OVERLAPPED` it may return before the kernel is done with: it
    /// waits on an event of its own whose low-order bit is set, so that no completion of it reaches
    /// an `iocp` port, and waits for an attempt left pending.
    pub fn try_lock(&self) -> std::io::Result<LockAttempt> {
        match try_lock_file(&self.file) {
            Ok(true) => Ok(LockAttempt::Acquired),
            Ok(false) => Ok(LockAttempt::Busy),
            Err(err) if lock_refused(&err) => Ok(LockAttempt::Unsupported),
            Err(err) => Err(err),
        }
    }

    /// Sets the file length, extending it with a hole that reads as zeros or truncating it.
    ///
    /// `completion` is dropped once the length is set, whether or not the call is still awaited,
    /// as [`crate::IoDriver::write_file_segments_atomic`] drops its own.
    pub async fn set_len<C: Send + 'static>(&self, len: u64, completion: C) -> std::io::Result<()> {
        self.driver
            .set_len_raw(Arc::clone(&self.file), len, false, completion)
            .await
    }

    /// Sets the file length as [`Self::set_len`] does, with the file made sparse first, so a range
    /// of it never written holds no storage: on Windows `FSCTL_SET_SPARSE`. Where the marking
    /// fails, as on a file system without sparse files, the file is sized all the same and is not
    /// sparse, its unwritten ranges held as zeros, which read the same. Elsewhere a range never
    /// written holds no storage already. Both run in one job on the syscall pool.
    pub async fn set_len_sparse(&self, len: u64) -> std::io::Result<()> {
        self.driver
            .set_len_raw(Arc::clone(&self.file), len, true, ())
            .await
    }

    /// Empties the file and sizes it to `len`, made sparse as [`Self::set_len_sparse`] makes it, so
    /// every range of it is a hole that reads as zeros, whatever it held. On Windows the file is
    /// deallocated whole first, as NTFS is observed, not documented, to keep a file's allocation
    /// through a truncation while a handle to it is open, and version 3.14 of the Resilient File
    /// System to record the ranges a sparse file held past a truncation as allocated again once it
    /// grows, through any handle, unless its data was written out first. All of it runs in one job
    /// on the syscall pool, which starts when this is called, as for [`Self::next_data`].
    pub fn empty(&self, len: u64) -> impl Future<Output = std::io::Result<()>> + use<> {
        IoDriver::empty_raw(Arc::clone(&self.file), len)
    }

    /// The first range at or after `offset` the file system holds data for, ending where the next
    /// hole begins: `None` when only holes follow `offset`, or `offset` is at or past the end. A
    /// hole is a range holding no written data: mapped to no storage, or allocated and never
    /// written (an unwritten extent). It reads as zeros, but zeros written to the file are data
    /// where the file system holds them as written.
    /// On Linux the map comes from `FIEMAP`, with the file synced first, and a range is widened to
    /// whole blocks of the file system, so it may end past the end of the file. On the other
    /// platforms but Windows, and on a Linux file system without `FIEMAP`, it comes from
    /// `SEEK_DATA`, which takes an unwritten extent the page cache holds pages for as data. On
    /// Apple platforms, where the file system holds holes, the walk writes the file's data out
    /// before its first lookup, as APFS allocates the small holes beside data it writes out. On
    /// Windows, on NTFS, everything at or past the file's valid data length, from
    /// `FSCTL_QUERY_FILE_REGIONS` with the file's data flushed first, was never written and is a
    /// hole; on any other file system the allocation alone decides. Below it a range is an
    /// allocated range of a sparse file, from `FSCTL_QUERY_ALLOCATED_RANGES`, and a file that is
    /// not sparse is data whole. Where the valid data length cannot be read, the allocation alone
    /// decides. Where the file cannot be flushed, or its data may be held elsewhere until it is
    /// read, as for a cloud placeholder or an offline file, it is data whole. Where the platform
    /// reports no holes, everything from `offset` to the end is one range of data.
    ///
    /// `cursor` is what one walk of the file keeps between calls ([`crate::DataCursor`]): a new one
    /// for the first call of a walk, then the one each call hands back with its answer.
    ///
    /// The lookup starts when this is called, and is not an `async fn`, so a future awaiting it
    /// holds the pool's task alone.
    pub fn next_data(
        &self,
        offset: u64,
        cursor: crate::DataCursor,
    ) -> impl Future<
        Output = (
            std::io::Result<Option<std::ops::Range<u64>>>,
            crate::DataCursor,
        ),
    > + use<> {
        IoDriver::next_data_raw(Arc::clone(&self.file), offset, cursor)
    }

    /// Writes the file's data out, then deallocates the whole blocks of the file system in each of
    /// `holes`, so that each holds no storage and reads as zeros; APFS keeps a small hole beside
    /// data only where it is deallocated after the data is written out. A part of a hole in a block
    /// it shares with data stays as it is, and a hole that ends the file takes its last block
    /// whole. Does nothing where the file system holds no holes ([`Self::holds_holes`]), and fails
    /// where it holds them and refuses to deallocate one.
    ///
    /// The call starts when this is called, as [`Self::next_data`] does.
    #[cfg(target_vendor = "apple")]
    pub fn punch_holes(
        &self,
        holes: Vec<std::ops::Range<u64>>,
    ) -> impl Future<Output = std::io::Result<()>> + use<> {
        IoDriver::punch_holes_raw(Arc::clone(&self.file), holes)
    }

    /// Whether the file system of this file holds holes: its smallest hole, `_PC_MIN_HOLE_SIZE`, is
    /// above zero. [`Self::punch_holes`] deallocates nothing where it is not.
    ///
    /// The call starts when this is called, as [`Self::next_data`] does.
    #[cfg(target_vendor = "apple")]
    pub fn holds_holes(&self) -> impl Future<Output = bool> + use<> {
        IoDriver::holds_holes_raw(Arc::clone(&self.file))
    }
}
