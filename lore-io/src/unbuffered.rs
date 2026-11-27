// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Writes that bypass the file cache. On Windows they go through handles opened with
//! `FILE_FLAG_NO_BUFFERING`, from memory, offsets and lengths aligned to
//! [`UNBUFFERED_ALIGNMENT`](crate::UNBUFFERED_ALIGNMENT), so the data goes to the device during the
//! write and the file holds no cache pages afterwards. Elsewhere the same calls write through the
//! file cache.

#[cfg(target_family = "windows")]
use std::fs::File;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// Count of unbuffered writes the process made through the file cache, being refused.
static FALLBACKS: AtomicU64 = AtomicU64::new(0);

/// Whether `err` is the refusal of an unbuffered open or write, as a volume answers whose sector
/// size [`UNBUFFERED_ALIGNMENT`](crate::UNBUFFERED_ALIGNMENT) is not a multiple of, or one whose
/// file system has no unbuffered I/O, such as a network share: `ERROR_INVALID_PARAMETER` or
/// `ERROR_NOT_SUPPORTED` on Windows. No error is one elsewhere.
pub fn unbuffered_refused(err: &std::io::Error) -> bool {
    #[cfg(target_family = "windows")]
    {
        use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
        use windows_sys::Win32::Foundation::ERROR_NOT_SUPPORTED;

        err.raw_os_error().is_some_and(|code| {
            code == ERROR_INVALID_PARAMETER as i32 || code == ERROR_NOT_SUPPORTED as i32
        })
    }
    #[cfg(not(target_family = "windows"))]
    {
        let _ = err;
        false
    }
}

/// How many unbuffered writes of a file the process has made through the file cache, being
/// refused ([`unbuffered_refused`]), since it started.
pub fn unbuffered_fallbacks() -> u64 {
    FALLBACKS.load(Ordering::Relaxed)
}

/// Counts a file whose unbuffered write was refused and goes through the file cache instead, in
/// [`unbuffered_fallbacks`].
pub fn record_unbuffered_fallback() {
    FALLBACKS.fetch_add(1, Ordering::Relaxed);
}

/// The flags an open of `options` adds for unbuffered I/O.
#[cfg(target_family = "windows")]
pub(crate) fn open_flags(options: &crate::OpenOptions) -> u32 {
    if options.is_unbuffered() {
        windows_sys::Win32::Storage::FileSystem::FILE_FLAG_NO_BUFFERING
    } else {
        0
    }
}

/// Writes `data` as the whole contents of `path`, created or truncated, with `durable` syncs it,
/// and answers its metadata, as the cached whole-file write does, and whether it wrote it
/// unbuffered. On Windows the file is opened unbuffered, `data` is copied into this thread's
/// aligned buffer ([`write_padded_at`]) and written with its padding in one write, and the end of
/// the file is then set to the length of `data`. A refused open or write ([`unbuffered_refused`])
/// is counted ([`unbuffered_fallbacks`]) and the file written through the file cache, as it is
/// elsewhere.
pub(crate) fn write_file(
    path: &Path,
    data: &[u8],
    durable: bool,
) -> std::io::Result<(std::fs::Metadata, bool)> {
    #[cfg(target_family = "windows")]
    match write_file_unbuffered(path, data, durable) {
        Err(err) if unbuffered_refused(&err) => record_unbuffered_fallback(),
        written => return written.map(|metadata| (metadata, true)),
    }
    write_file_cached(path, data, durable).map(|metadata| (metadata, false))
}

/// Writes `data` as the whole contents of `path` through the file cache. See [`write_file`].
pub(crate) fn write_file_cached(
    path: &Path,
    data: &[u8],
    durable: bool,
) -> std::io::Result<std::fs::Metadata> {
    use std::io::Write;

    let mut file = crate::file::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .to_std_blocking()
        .open(path)?;
    file.write_all(data)?;
    if durable {
        file.sync_data()?;
    }
    file.metadata()
}

#[cfg(target_family = "windows")]
fn write_file_unbuffered(
    path: &Path,
    data: &[u8],
    durable: bool,
) -> std::io::Result<std::fs::Metadata> {
    #[cfg(feature = "test-util")]
    test_util::refuse(path, test_util::Refusal::Open)?;
    let file = crate::file::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .unbuffered(true)
        .to_std_blocking()
        .open(path)?;
    if !data.is_empty() {
        #[cfg(feature = "test-util")]
        test_util::refuse(path, test_util::Refusal::Write)?;
        write_padded_at(&file, data.len(), std::iter::once(data), 0)?;
        if !data.len().is_multiple_of(crate::UNBUFFERED_ALIGNMENT) {
            file.set_len(data.len() as u64)?;
        }
    }
    if durable {
        file.sync_data()?;
    }
    file.metadata()
}

/// The largest write [`write_padded_at`] makes from the aligned buffer a pool thread keeps between
/// unbuffered writes; a larger one allocates a buffer of its own. It holds a file of one fragment
/// many times over, and the writes of a fragmented file are merged up to it.
#[cfg(target_family = "windows")]
pub const PADDED_WRITE_RETAINED: usize = 4 * 1024 * 1024;

#[cfg(target_family = "windows")]
thread_local! {
    /// The aligned buffer [`write_padded_at`] copies into, allocated once per pool thread, so an
    /// unbuffered write allocates nothing.
    static SCRATCH: std::cell::RefCell<Option<crate::AlignedBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Writes `segments`, `len` bytes in all, and the zeros that pad them to a multiple of the
/// alignment, in one write at `offset` of `file`, through this thread's aligned buffer. No
/// completion reaches a completion port `file` is bound to.
#[cfg(target_family = "windows")]
pub(crate) fn write_padded_at<'a>(
    file: &File,
    len: usize,
    segments: impl Iterator<Item = &'a [u8]>,
    offset: u64,
) -> std::io::Result<()> {
    let fill = |content: &mut [u8]| {
        let mut at = 0;
        for segment in segments {
            content[at..at + segment.len()].copy_from_slice(segment);
            at += segment.len();
        }
    };
    if len > PADDED_WRITE_RETAINED {
        // SAFETY: `segments` hold `len` bytes, so the copy writes every byte of the content before
        // the write reads it.
        let mut buffer = unsafe { crate::AlignedBuf::uninit(len) };
        fill(buffer.content_mut());
        return crate::psync::write_all_at_impl(file, buffer.as_ref(), offset);
    }
    SCRATCH.with_borrow_mut(|scratch| {
        // SAFETY: as above.
        let scratch = scratch
            .get_or_insert_with(|| unsafe { crate::AlignedBuf::uninit(PADDED_WRITE_RETAINED) });
        // SAFETY: as above.
        unsafe { scratch.set_len(len) };
        fill(scratch.content_mut());
        crate::psync::write_all_at_impl(file, scratch.as_ref(), offset)
    })
}

/// Test points that refuse unbuffered opens and writes of chosen paths, as a volume refuses them.
#[cfg(feature = "test-util")]
pub mod test_util {
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// The step of an unbuffered write a test refuses.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Refusal {
        /// The unbuffered open of the file.
        Open,
        /// The unbuffered writes to the file, its open allowed.
        Write,
    }

    static REFUSED: Mutex<Vec<(PathBuf, Refusal)>> = Mutex::new(Vec::new());

    /// Refuses `refusal` for every file at or below `path` from now on, in this process, with
    /// `ERROR_INVALID_PARAMETER` on Windows.
    pub fn refuse_unbuffered(path: &Path, refusal: Refusal) {
        REFUSED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((path.to_path_buf(), refusal));
    }

    /// Whether `refusal` of `path` is refused.
    #[cfg(target_family = "windows")]
    fn refuses(path: &Path, refusal: Refusal) -> bool {
        REFUSED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|(refused, step)| *step == refusal && path.starts_with(refused))
    }

    /// The error a volume that refuses an unbuffered call answers: `ERROR_INVALID_PARAMETER`.
    #[cfg(target_family = "windows")]
    pub(crate) fn refused() -> std::io::Error {
        std::io::Error::from_raw_os_error(
            windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER as i32,
        )
    }

    /// Fails as a volume that refuses an unbuffered call fails ([`refused`]) where `refusal` of
    /// `path` is refused.
    #[cfg(target_family = "windows")]
    pub(crate) fn refuse(path: &Path, refusal: Refusal) -> std::io::Result<()> {
        if refuses(path, refusal) {
            return Err(refused());
        }
        Ok(())
    }

    /// Fails an unbuffered open of `path` with `options` as [`refuse`] does where
    /// [`Refusal::Open`] of it is refused, and answers whether to refuse the padded writes of the
    /// file it opens ([`crate::IoFile::write_padded_at`]): where [`Refusal::Write`] of it is.
    #[cfg(target_family = "windows")]
    pub(crate) fn refused_open(path: &Path, options: &crate::OpenOptions) -> std::io::Result<bool> {
        if !options.is_unbuffered() {
            return Ok(false);
        }
        refuse(path, Refusal::Open)?;
        Ok(refuses(path, Refusal::Write))
    }
}
