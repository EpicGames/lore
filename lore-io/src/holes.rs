// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Holes: ranges of a file that hold no data.
//!
//! A hole is a range the file system holds no written data for: one never written, which it maps to
//! no storage, or an extent it allocated but never wrote, such as one `fallocate` reserved (an
//! *unwritten* extent on XFS and ext4). A hole reads as zeros, but it is not zeros: zeros written
//! to a file are data where the file system holds them as written, and the callers of this module
//! keep the two apart.
//!
//! On Linux the map of a file comes from the `FIEMAP` ioctl, with the file synced first, so what
//! is a hole depends on what the file system allocated and wrote alone. `SEEK_DATA` and
//! `SEEK_HOLE`, the fallback where a file system does not support `FIEMAP` and the only source on
//! the other platforms that have them, also take an unwritten extent as a hole, but take it as
//! data wherever the page cache holds pages for it, which any read of the extent, or reading
//! ahead into it, leaves behind. A walk that takes `SEEK_DATA` advises random reads of the file on
//! its first lookup ([`advise_random`]), so the reads through its handle read nothing ahead. On
//! Apple platforms, where the file system holds holes, that first lookup also writes the file's
//! data out, as `FIEMAP` does on Linux: APFS allocates a hole of under 16 MiB beside data as zeros
//! when it writes that data out, so otherwise what is a hole would depend on whether APFS had
//! written the data out yet.
//!
//! On Windows, on NTFS, a file's valid data length, from `FSCTL_QUERY_FILE_REGIONS` with the file's
//! data flushed first, ends what was ever written to it, so everything from there on is a hole, as
//! an unwritten extent is on Linux; on any other file system the valid data length is not read,
//! and runs to the end of the file. Below it, the map of a sparse file comes from
//! `FSCTL_QUERY_ALLOCATED_RANGES`, which reports allocation alone, so every allocated range is
//! data, its bytes never written included, and a file that is not sparse is data whole. Where the
//! valid data length cannot be read, the map is that of the allocation alone. Where the file
//! cannot be flushed, or its file system may keep its data elsewhere until it is read, as for a
//! cloud placeholder or an offline file, it is data whole.
//!
//! All are blocking calls, which the syscall pool runs.

use std::fs::File;
use std::ops::Range;

/// What one walk of a file's ranges of data keeps across [`next_data`] calls: the file system's
/// block, read once ([`DataCursor::block`]), and on Linux and Windows the last batch of ranges the
/// file system mapped, so the walk maps the file once for each batch. A walk starts with a new
/// cursor and keeps it to one file.
#[derive(Default)]
pub struct DataCursor {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    map: Option<Box<fiemap::Cursor>>,
    #[cfg(target_family = "windows")]
    map: Option<Box<allocated::Cursor>>,
    /// Set once the walk has advised random reads of the file, on its first `SEEK_DATA` lookup, and
    /// on Apple platforms written its data out where the file system holds holes.
    #[cfg(not(any(target_os = "linux", target_os = "android", target_family = "windows")))]
    advised: bool,
    /// The file system's block, read on the walk's first lookup. On Linux and Windows the part
    /// that maps the file keeps it, so that the cursor stays one pointer.
    #[cfg(not(any(target_os = "linux", target_os = "android", target_family = "windows")))]
    block: Option<u64>,
}

impl DataCursor {
    /// The file system's block, which the walk read on its first lookup: on Linux `fstatfs`'s
    /// `f_bsize`, which each range from `FIEMAP` is widened to, and on Windows the unit a write
    /// allocates in a sparse file: 16 clusters on NTFS, and one on every other file system. `None`
    /// before the first lookup, where the platform reports no holes, and on Windows for a file that
    /// is not sparse, whose data may be held elsewhere or that the walk cannot flush, and where the
    /// cluster cannot be read, which only makes a resumed read write its file afresh more often
    /// than it needs to.
    #[cfg(any(target_os = "linux", target_os = "android", target_family = "windows"))]
    pub fn block(&self) -> Option<u64> {
        self.map.as_ref().and_then(|map| map.block)
    }

    /// The file system's block, `fstatfs`'s `f_bsize`, which the walk read on its first lookup.
    /// `None` before the first lookup, and where the platform reports no holes.
    #[cfg(not(any(target_os = "linux", target_os = "android", target_family = "windows")))]
    pub fn block(&self) -> Option<u64> {
        self.block
    }
}

/// The first range of `file` at or after `offset` the file system holds data for, ending where the
/// next hole begins: `None` when only holes follow `offset`, or `offset` is at or past the end.
/// An unwritten extent is a hole. A range from `FIEMAP` is widened to whole blocks of the
/// file system, and may end past the end of the file. A file system that refuses `FIEMAP` once is
/// walked with `SEEK_DATA` from then on, and the first `SEEK_DATA` lookup of a walk advises random
/// reads of `file` ([`advise_random`]); on Apple platforms, where the file system holds holes, it
/// then writes the file's data out before it looks. The advice changes no answer, so its failure is
/// ignored.
///
/// On Windows, on NTFS, a range ends at the valid data length, past which the file is a hole.
/// Below it a range is an allocated range of a sparse file, as the file system reports it, and a
/// file that is not sparse, or on a file system that refuses the query, is one range of data. A
/// file whose data may be held elsewhere, or that the walk cannot flush, is one range of data to
/// its end.
///
/// Where the platform reports no holes, everything from `offset` to the end is one range of data.
pub(crate) fn next_data(
    file: &File,
    offset: u64,
    cursor: &mut DataCursor,
) -> std::io::Result<Option<Range<u64>>> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let map = cursor.map.get_or_insert_with(Box::default);
        if !map.unsupported {
            match fiemap::next_data(file, offset, map) {
                Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {
                    map.unsupported = true;
                    let _ = advise_random(file);
                }
                found => return found,
            }
        }
    }
    #[cfg(target_family = "windows")]
    {
        allocated::next_data(file, offset, cursor.map.get_or_insert_with(Box::default))
    }
    #[cfg(not(target_family = "windows"))]
    {
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        if !cursor.advised {
            cursor.advised = true;
            let _ = advise_random(file);
            #[cfg(any(target_os = "freebsd", target_vendor = "apple"))]
            {
                cursor.block = Some(block_size(file)?);
            }
            #[cfg(target_vendor = "apple")]
            if holds_holes(file) {
                fsync(file)?;
            }
        }
        seek_next_data(file, offset)
    }
}

/// Marks `file` sparse, so a range of it never written holds no storage, and answers whether it is
/// sparse. Where the marking fails, as on a file system without sparse files, the file stays as it
/// was, and is not sparse: it reads the same, its unwritten ranges held as zeros. Elsewhere than on
/// Windows a range never written holds no storage already.
pub(crate) fn make_sparse(file: &File) -> bool {
    #[cfg(target_family = "windows")]
    {
        crate::overlapped::control(
            file,
            windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
        )
        .is_ok()
    }

    #[cfg(not(target_family = "windows"))]
    {
        let _ = file;
        true
    }
}

/// Folds `batch`, the allocated ranges of one map of a file in order, into `found`, the range of
/// data a lookup from `offset` builds, each range clipped to `offset..valid`: adjacent and
/// overlapping ranges are one. Answers whether `found` is complete, as a range past a gap after it,
/// or starting at or past `valid`, ends it. Where `batch` runs out first, `found` may go on into
/// the next batch, mapped from where this one ends.
#[cfg(any(target_family = "windows", feature = "test-util"))]
#[lore_macro::test_pub]
#[inline]
pub(crate) fn fold_ranges(
    found: &mut Option<Range<u64>>,
    batch: impl IntoIterator<Item = Range<u64>>,
    offset: u64,
    valid: u64,
) -> bool {
    for range in batch {
        let start = range.start.max(offset);
        if start >= valid {
            return true;
        }
        let end = range.end.min(valid);
        match found {
            None => *found = Some(start..end),
            Some(data) if start <= data.end => data.end = data.end.max(end),
            Some(_) => return true,
        }
    }
    false
}

/// Whether `name`, a file system's name as Windows reports it, ending in a null, names NTFS.
#[cfg(any(target_family = "windows", feature = "test-util"))]
#[lore_macro::test_pub]
pub(crate) fn names_ntfs(name: &[u16]) -> bool {
    name.starts_with(&[b'N', b'T', b'F', b'S', 0].map(u16::from))
}

/// Where what was written to a file of `len` bytes ends, from the regions its file system reports,
/// each a range and whether it holds valid data, in the shape NTFS reports them in: from offset 0
/// to at least `len` without a gap, with every valid region before the first that is not. The end
/// of the last valid region, or zero where none is. `None` for any other answer, no region
/// included, which a walk does not trust.
#[cfg(any(target_family = "windows", feature = "test-util"))]
#[lore_macro::test_pub]
pub(crate) fn valid_end(
    regions: impl IntoIterator<Item = (Range<u64>, bool)>,
    len: u64,
) -> Option<u64> {
    let mut covered = None;
    let mut valid = 0;
    let mut unflagged = false;
    for (region, holds_valid) in regions {
        if region.start != covered.unwrap_or(0) || region.end <= region.start {
            return None;
        }
        if holds_valid {
            if unflagged {
                return None;
            }
            valid = region.end;
        } else {
            unflagged = true;
        }
        covered = Some(region.end);
    }
    covered.filter(|end| *end >= len).map(|_| valid)
}

/// Empties `file` and sizes it to `len`, sparse, so every range of it is a hole that reads as
/// zeros ([`make_sparse`]).
///
/// On Windows the whole file is deallocated with `FSCTL_SET_ZERO_DATA` before it is truncated.
/// Observed on NTFS, not documented: while a handle to the file stays open, truncating it can keep
/// its allocation, so sizing it again brings back ranges it held, as allocated zeros, unless the
/// file was flushed first, as a walk of its ranges of data flushes it. Measured on version 3.14 of
/// the Resilient File System, a sparse file truncated before its data is flushed brings back the
/// ranges it held past the truncation the same way once it grows, through any handle and however
/// long after; a deallocation before the truncation leaves none. The deallocation keeps an
/// emptying from depending on a flush. Its regression tests are
/// `a_hole_of_the_temporary_file_holding_data_empties_it`, which fails without it only where a
/// walk does not flush the file either, and
/// `a_direct_write_over_a_sparse_file_not_yet_written_out_leaves_its_holes`, which fails without
/// it on the Resilient File System. A file that cannot be made sparse ([`make_sparse`]) is
/// emptied by the truncation alone, as zeroing it would write zeros over it, and so is one whose
/// deallocation fails: the truncation empties its content either way. Elsewhere truncating
/// frees the file's storage, and this is a truncation, then a resize where `len` is not zero.
pub(crate) fn empty(file: &File, len: u64) -> std::io::Result<()> {
    #[cfg(target_family = "windows")]
    if make_sparse(file) {
        let whole = windows_sys::Win32::System::Ioctl::FILE_ZERO_DATA_INFORMATION {
            FileOffset: 0,
            BeyondFinalZero: i64::MAX,
        };
        let _ = crate::overlapped::control(
            file,
            windows_sys::Win32::System::Ioctl::FSCTL_SET_ZERO_DATA,
            (&raw const whole).cast(),
            size_of_val(&whole) as u32,
            std::ptr::null_mut(),
            0,
        );
    }
    crate::pool::retry_on_interrupt(|| file.set_len(0))?;
    if len == 0 {
        return Ok(());
    }
    crate::pool::retry_on_interrupt(|| file.set_len(len))
}

/// The file system's block, `fstatfs`'s `f_bsize`, which every extent but an inline or tail one is
/// aligned to.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_vendor = "apple"
))]
fn block_size(file: &File) -> std::io::Result<u64> {
    use std::os::fd::AsRawFd;

    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: Calling OS functions. `fstatfs` fills `stats`, which outlives the call.
    if unsafe { libc::fstatfs(file.as_raw_fd(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fstatfs` succeeded, so it filled `stats`.
    let stats = unsafe { stats.assume_init() };
    // `f_bsize` is signed on some platforms and unsigned on others.
    #[allow(clippy::unnecessary_fallible_conversions, clippy::useless_conversion)]
    let block = u64::try_from(stats.f_bsize).unwrap_or(0);
    Ok(block.max(1))
}

/// Deallocates the whole blocks of the file system in each of `holes`, ranges of `file` that hold
/// no data of its content, so that each holds no storage and reads as zeros, once the file's data
/// is written out (see the module doc). A part of a hole in a block it shares with data stays as it
/// is, and a hole that ends the file takes its last block whole, as `F_PUNCHHOLE` takes only whole
/// blocks and takes them past the end. Does nothing where the file system holds no holes
/// ([`holds_holes`]). Where it holds them, it fails if one cannot be deallocated, so on success the
/// whole blocks of every hole are unallocated.
#[cfg(target_vendor = "apple")]
pub(crate) fn punch_holes(file: &File, holes: &[Range<u64>]) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    if !holds_holes(file) {
        return Ok(());
    }
    fsync(file)?;
    let fd = file.as_raw_fd();
    let block = block_size(file)?;
    let len = file.metadata()?.len();
    for hole in holes {
        let start = hole.start.div_ceil(block) * block;
        let end = if hole.end >= len {
            hole.end.div_ceil(block) * block
        } else {
            hole.end / block * block
        };
        if start >= end {
            continue;
        }
        let (Ok(offset), Ok(length)) = (
            libc::off_t::try_from(start),
            libc::off_t::try_from(end - start),
        ) else {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
        };
        let punch = libc::fpunchhole_t {
            fp_flags: 0,
            reserved: 0,
            fp_offset: offset,
            fp_length: length,
        };
        // SAFETY: Calling OS functions. `punch` outlives the call, which only reads it.
        if unsafe { libc::fcntl(fd, libc::F_PUNCHHOLE, &punch) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Writes out the data of `file` with `fsync`, rather than `F_FULLFSYNC`, which also flushes the
/// drive's cache.
#[cfg(target_vendor = "apple")]
fn fsync(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    crate::pool::retry_on_interrupt(|| {
        // SAFETY: Calling OS functions. `fsync` reads no memory of the caller's.
        if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    })
}

/// Whether the file system of `file` holds holes: its smallest hole, `_PC_MIN_HOLE_SIZE`, is above
/// zero, as APFS reports its block. A file system that reports none, or that `fpathconf` gives no
/// answer for, holds none.
#[cfg(target_vendor = "apple")]
pub(crate) fn holds_holes(file: &File) -> bool {
    use std::os::fd::AsRawFd;

    // SAFETY: Calling OS functions. `fpathconf` reads no memory of the caller's.
    unsafe { libc::fpathconf(file.as_raw_fd(), libc::_PC_MIN_HOLE_SIZE) > 0 }
}

/// [`next_data`] from `SEEK_DATA` and `SEEK_HOLE`, where the platform has them.
#[cfg(not(target_family = "windows"))]
fn seek_next_data(file: &File, offset: u64) -> std::io::Result<Option<Range<u64>>> {
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_vendor = "apple"
    ))]
    {
        use std::os::fd::AsRawFd;

        let Ok(at) = libc::off_t::try_from(offset) else {
            return Ok(None);
        };
        let fd = file.as_raw_fd();
        // SAFETY: Calling OS functions. `lseek` reads no memory of the caller's, and the shared
        // file position it moves is never used: every read and write of this crate is positional.
        let start = unsafe { libc::lseek(fd, at, libc::SEEK_DATA) };
        if start < 0 {
            let error = std::io::Error::last_os_error();
            return match error.raw_os_error() {
                // No data at or after `offset`, which includes an offset at or past the end.
                Some(libc::ENXIO) => Ok(None),
                // A file system that cannot say. Everything from `offset` is data.
                Some(libc::EINVAL) => whole_data(file, offset),
                _ => Err(error),
            };
        }
        // SAFETY: as above.
        let end = unsafe { libc::lseek(fd, start, libc::SEEK_HOLE) };
        if end < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Some(start as u64..end as u64))
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_vendor = "apple"
    )))]
    {
        whole_data(file, offset)
    }
}

/// Everything from `offset` to the end of `file` as one range of data, or `None` past the end.
#[cfg(not(target_family = "windows"))]
fn whole_data(file: &File, offset: u64) -> std::io::Result<Option<Range<u64>>> {
    let len = file.metadata()?.len();
    Ok((offset < len).then_some(offset..len))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod fiemap {
    //! The extent map of a file from the `FIEMAP` ioctl, read a batch of extents at a time.

    use std::fs::File;
    use std::ops::Range;
    use std::os::fd::AsRawFd;

    /// `_IOWR('f', 11, struct fiemap)`.
    const FS_IOC_FIEMAP: u64 = 0xC020_660B;
    /// Sync the file before mapping it, so data still in the page cache is mapped as written.
    const FIEMAP_FLAG_SYNC: u32 = 0x1;
    /// The last extent of the file.
    const FIEMAP_EXTENT_LAST: u32 = 0x1;
    /// Allocated and never written, which reads as zeros.
    const FIEMAP_EXTENT_UNWRITTEN: u32 = 0x800;

    /// The extents one call maps.
    const BATCH: usize = 64;

    /// `struct fiemap_extent`. The kernel writes the fields this code never reads.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    #[allow(dead_code)]
    struct Extent {
        logical: u64,
        physical: u64,
        length: u64,
        reserved64: [u64; 2],
        flags: u32,
        reserved: [u32; 3],
    }

    /// `struct fiemap`, with room for [`BATCH`] extents after it. The kernel reads the fields this
    /// code only writes.
    #[repr(C)]
    #[allow(dead_code)]
    struct Map {
        start: u64,
        length: u64,
        flags: u32,
        mapped_extents: u32,
        extent_count: u32,
        reserved: u32,
        extents: [Extent; BATCH],
    }

    /// The part of a [`super::DataCursor`] `FIEMAP` keeps: the block, the last batch of extents,
    /// and the range of the file that batch maps.
    pub(super) struct Cursor {
        /// The file system's block, once read ([`super::DataCursor::block`]).
        pub(super) block: Option<u64>,
        /// Set once the file system refused to map the file.
        pub(super) unsupported: bool,
        /// The last batch, as the kernel wrote it.
        map: Map,
        /// How many extents of `map` the kernel wrote.
        count: usize,
        /// The range the batch maps: from where it was mapped to the end of its last extent, or
        /// to the end of any file where it holds the file's last extent, or fewer than a batch.
        mapped: Range<u64>,
    }

    impl Default for Cursor {
        fn default() -> Self {
            Self {
                block: None,
                unsupported: false,
                map: Map {
                    start: 0,
                    length: 0,
                    flags: 0,
                    mapped_extents: 0,
                    extent_count: 0,
                    reserved: 0,
                    extents: [Extent::default(); BATCH],
                },
                count: 0,
                mapped: 0..0,
            }
        }
    }

    impl Cursor {
        /// Maps the batch of extents of `file` from `from`, with the file synced first.
        /// `Unsupported` where the file system has no extent map to give.
        fn map_from(&mut self, file: &File, from: u64) -> std::io::Result<()> {
            self.count = 0;
            self.mapped = 0..0;
            let map = &mut self.map;
            map.start = from;
            map.length = u64::MAX - from;
            map.flags = FIEMAP_FLAG_SYNC;
            map.mapped_extents = 0;
            map.extent_count = BATCH as u32;
            map.reserved = 0;
            let mapped = crate::pool::retry_on_interrupt(|| {
                // SAFETY: Calling OS functions. `map` is a `struct fiemap` with room for the
                // `extent_count` extents the kernel may write after it, and outlives the call.
                let result = unsafe {
                    libc::ioctl(
                        file.as_raw_fd(),
                        FS_IOC_FIEMAP as _,
                        std::ptr::from_mut(map),
                    )
                };
                if result == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
            match mapped {
                Ok(()) => {}
                // A file system with no extent map (tmpfs), or none that takes these flags.
                Err(error)
                    if matches!(
                        error.raw_os_error(),
                        Some(libc::EOPNOTSUPP | libc::ENOTTY | libc::EBADR | libc::EINVAL)
                    ) =>
                {
                    return Err(std::io::Error::from(std::io::ErrorKind::Unsupported));
                }
                Err(error) => return Err(error),
            }
            self.count = (self.map.mapped_extents as usize).min(BATCH);
            let end = match self.map.extents[..self.count].last() {
                Some(last) if self.count == BATCH && last.flags & FIEMAP_EXTENT_LAST == 0 => {
                    last.logical.saturating_add(last.length)
                }
                _ => u64::MAX,
            };
            self.mapped = from..if end > from { end } else { u64::MAX };
            Ok(())
        }
    }

    /// [`super::next_data`] from the extent map: written extents are data, and an unwritten extent
    /// ends a range as a gap between extents does. Adjacent written extents are one range, also
    /// across two batches. The batch `cursor` holds answers where it maps `offset`.
    /// `Unsupported` where the file system has no extent map to give.
    pub(super) fn next_data(
        file: &File,
        offset: u64,
        cursor: &mut Cursor,
    ) -> std::io::Result<Option<Range<u64>>> {
        let block = match cursor.block {
            Some(block) => block,
            None => *cursor.block.insert(super::block_size(file)?),
        };
        if !cursor.mapped.contains(&offset) {
            cursor.map_from(file, offset)?;
        }
        let mut found: Option<Range<u64>> = None;
        let mut first = cursor.map.extents[..cursor.count]
            .iter()
            .position(|extent| extent.logical.saturating_add(extent.length) > offset)
            .unwrap_or(cursor.count);
        loop {
            for extent in &cursor.map.extents[first..cursor.count] {
                let end = extent
                    .logical
                    .saturating_add(extent.length)
                    .div_ceil(block)
                    .saturating_mul(block);
                let start = (extent.logical - extent.logical % block).max(offset);
                if extent.flags & FIEMAP_EXTENT_UNWRITTEN != 0 || start >= end {
                    if found.is_some() {
                        return Ok(found);
                    }
                } else {
                    match &mut found {
                        None => found = Some(start..end),
                        Some(range) if start <= range.end => range.end = range.end.max(end),
                        Some(_) => return Ok(found),
                    }
                }
                if extent.flags & FIEMAP_EXTENT_LAST != 0 {
                    return Ok(found);
                }
            }
            if cursor.mapped.end == u64::MAX {
                return Ok(found);
            }
            let next = cursor.mapped.end;
            cursor.map_from(file, next)?;
            first = 0;
        }
    }
}

#[cfg(target_family = "windows")]
mod allocated {
    //! The valid data length of a file from `FSCTL_QUERY_FILE_REGIONS`, read once, and the
    //! allocated ranges of a sparse file from `FSCTL_QUERY_ALLOCATED_RANGES`, read a batch of
    //! ranges at a time.

    use std::fs::File;
    use std::ops::Range;
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Wdk::Storage::FileSystem::FileFsSizeInformation;
    use windows_sys::Wdk::Storage::FileSystem::NtQueryVolumeInformationFile;
    use windows_sys::Wdk::System::SystemServices::FILE_FS_SIZE_INFORMATION;
    use windows_sys::Win32::Foundation::ERROR_INVALID_FUNCTION;
    use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
    use windows_sys::Win32::Foundation::ERROR_NOT_SUPPORTED;
    use windows_sys::Win32::Foundation::MAX_PATH;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_OFFLINE;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_RECALL_ON_OPEN;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_SPARSE_FILE;
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationByHandleW;
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
    use windows_sys::Win32::System::Ioctl::FILE_ALLOCATED_RANGE_BUFFER;
    use windows_sys::Win32::System::Ioctl::FILE_REGION_INFO;
    use windows_sys::Win32::System::Ioctl::FILE_REGION_INPUT;
    use windows_sys::Win32::System::Ioctl::FILE_REGION_USAGE_VALID_CACHED_DATA;
    use windows_sys::Win32::System::Ioctl::FSCTL_QUERY_ALLOCATED_RANGES;
    use windows_sys::Win32::System::Ioctl::FSCTL_QUERY_FILE_REGIONS;

    /// The ranges one query maps.
    const BATCH: usize = 64;

    /// The regions one query of the valid data reads: NTFS reports one valid region and the one
    /// past it.
    const REGIONS: usize = 4;

    /// `FILE_REGION_OUTPUT`, with room for [`REGIONS`] regions. The file system writes the fields
    /// this code never reads.
    #[repr(C)]
    #[allow(dead_code)]
    #[derive(Default)]
    struct Regions {
        flags: u32,
        total: u32,
        count: u32,
        reserved: u32,
        regions: [FILE_REGION_INFO; REGIONS],
    }

    /// The clusters NTFS allocates at once where a write lands in a sparse file.
    const NTFS_CLUSTERS: u64 = 16;

    /// The attributes of a file whose data its file system may keep elsewhere until it is read,
    /// as a cloud placeholder, an offline or tiered file, or one behind a reparse point does: its
    /// allocation and valid data length need not hold its data, so a walk takes it as data whole.
    const HELD_ELSEWHERE: u32 = FILE_ATTRIBUTE_REPARSE_POINT
        | FILE_ATTRIBUTE_OFFLINE
        | FILE_ATTRIBUTE_RECALL_ON_OPEN
        | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;

    /// The part of a [`super::DataCursor`] the queries keep: the block, whether the file is sparse,
    /// its valid data length, the last batch of ranges, and the range of the file that batch maps.
    pub(super) struct Cursor {
        /// The unit a write allocates, read on the first lookup of a sparse file the walk flushes,
        /// and unknown where it cannot be read ([`super::DataCursor::block`]).
        pub(super) block: Option<u64>,
        /// Whether the file has holes to report below its valid data length, read on the first
        /// lookup: it is sparse, flushed ([`next_data`]), its data is not held elsewhere
        /// ([`HELD_ELSEWHERE`]), and its file system answers the query.
        sparse: Option<bool>,
        /// Where what was ever written to the file ends ([`valid_data_length`]), read on the first
        /// lookup, and at most the file's length then, which it is where it cannot be read.
        valid: u64,
        /// The last batch, as the file system wrote it.
        ranges: [FILE_ALLOCATED_RANGE_BUFFER; BATCH],
        /// How many ranges of `ranges` the file system wrote.
        count: usize,
        /// The range the batch maps: from where it was mapped to the end of its last range, or to
        /// the end of any file where the batch holds every range from there.
        mapped: Range<u64>,
    }

    impl Default for Cursor {
        fn default() -> Self {
            Self {
                block: None,
                sparse: None,
                valid: u64::MAX,
                ranges: [FILE_ALLOCATED_RANGE_BUFFER::default(); BATCH],
                count: 0,
                mapped: 0..0,
            }
        }
    }

    impl Cursor {
        /// Maps the batch of allocated ranges of `file` from `from`. `Unsupported` where the
        /// file system does not answer the query.
        fn map_from(&mut self, file: &File, from: u64) -> std::io::Result<()> {
            self.count = 0;
            self.mapped = 0..0;
            let start = i64::try_from(from).unwrap_or(i64::MAX);
            let query = FILE_ALLOCATED_RANGE_BUFFER {
                FileOffset: start,
                Length: i64::MAX - start,
            };
            let full = match crate::overlapped::control(
                file,
                FSCTL_QUERY_ALLOCATED_RANGES,
                (&raw const query).cast(),
                size_of::<FILE_ALLOCATED_RANGE_BUFFER>() as u32,
                self.ranges.as_mut_ptr().cast(),
                size_of_val(&self.ranges) as u32,
            ) {
                Ok(written) => {
                    self.count = (written / size_of::<FILE_ALLOCATED_RANGE_BUFFER>()).min(BATCH);
                    false
                }
                Err(error) if error.raw_os_error() == Some(ERROR_MORE_DATA as i32) => {
                    self.count = BATCH;
                    true
                }
                Err(error) if refused(&error) => {
                    return Err(std::io::Error::from(std::io::ErrorKind::Unsupported));
                }
                Err(error) => return Err(error),
            };
            let end = match self.ranges[..self.count].last() {
                Some(last) if full => end_of(last),
                _ => u64::MAX,
            };
            self.mapped = from..if end > from { end } else { u64::MAX };
            Ok(())
        }
    }

    /// Where `range` ends.
    fn end_of(range: &FILE_ALLOCATED_RANGE_BUFFER) -> u64 {
        (range.FileOffset as u64).saturating_add(range.Length as u64)
    }

    /// Whether `error` is a file system refusing a sparse-file control, as one without sparse
    /// files does.
    fn refused(error: &std::io::Error) -> bool {
        matches!(
            error
                .raw_os_error()
                .and_then(|code| u32::try_from(code).ok()),
            Some(ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED)
        )
    }

    /// Whether the file system `file` is on is NTFS ([`super::names_ntfs`]), from its name, read
    /// from the handle; `false` where the name cannot be read.
    fn on_ntfs(file: &File) -> bool {
        let mut name = [0u16; MAX_PATH as usize + 1];
        // SAFETY: Calling OS functions. `name` outlives the call, which writes at most its length.
        let named = unsafe {
            GetVolumeInformationByHandleW(
                file.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                name.as_mut_ptr(),
                name.len() as u32,
            )
        };
        named != 0 && super::names_ntfs(&name)
    }

    /// The unit a write allocates whole in a sparse file on the volume of `file`: the cluster,
    /// read from the handle, and where `ntfs` [`NTFS_CLUSTERS`] of them, as NTFS allocates a
    /// sparse file in compression units of 16 clusters and reports its allocated ranges in them;
    /// `None` where the cluster cannot be read. Where the file system's name cannot be read, so
    /// `ntfs` is not set, the unit is the
    /// cluster, which only makes a resumed read write its file afresh more often than it needs to.
    fn unit(file: &File, ntfs: bool) -> Option<u64> {
        let handle = file.as_raw_handle();
        let mut status = IO_STATUS_BLOCK::default();
        let mut size = FILE_FS_SIZE_INFORMATION::default();
        // SAFETY: Calling OS functions. `status` and `size` outlive the call, which writes at most
        // the size given into `size`.
        let queried = unsafe {
            NtQueryVolumeInformationFile(
                handle,
                &mut status,
                (&raw mut size).cast(),
                size_of::<FILE_FS_SIZE_INFORMATION>() as u32,
                FileFsSizeInformation,
            )
        };
        let cluster = u64::from(size.SectorsPerAllocationUnit) * u64::from(size.BytesPerSector);
        (queried == 0).then(|| cluster.max(1) * if ntfs { NTFS_CLUSTERS } else { 1 })
    }

    /// Flushes the data of `file` through a second handle to it with write access, which
    /// `FlushFileBuffers` needs and the walk's handle may lack: whether it did. Data written
    /// through a memory mapping and not yet flushed is not within the valid data length; the Linux
    /// walk syncs its file before mapping it for the same reason.
    fn flushed(file: &File) -> bool {
        use std::os::windows::io::FromRawHandle;
        use std::os::windows::io::OwnedHandle;

        use windows_sys::Win32::Foundation::GENERIC_WRITE;
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
        use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;
        use windows_sys::Win32::Storage::FileSystem::ReOpenFile;

        // SAFETY: Calling OS functions. The handle is open for the call.
        let writer = unsafe {
            ReOpenFile(
                file.as_raw_handle(),
                GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                0,
            )
        };
        if writer == INVALID_HANDLE_VALUE || writer.is_null() {
            return false;
        }
        // SAFETY: `ReOpenFile` returned a new handle, which this value owns and closes.
        let writer = unsafe { OwnedHandle::from_raw_handle(writer) };
        // SAFETY: Calling OS functions. `writer` is open for the call.
        unsafe { FlushFileBuffers(writer.as_raw_handle()) != 0 }
    }

    /// Where what was ever written to `file`, of `len` bytes and flushed ([`flushed`]), ends: its
    /// valid data length on NTFS, from the regions the file system reports as
    /// [`super::valid_end`] reads them. `None` where the file system refuses the query, reports
    /// more regions than one query reads, or answers in a shape [`super::valid_end`] does not
    /// trust, so that the walk maps the allocation alone.
    fn valid_data_length(file: &File, len: u64) -> Option<u64> {
        let query = FILE_REGION_INPUT {
            FileOffset: 0,
            Length: i64::MAX,
            DesiredUsage: FILE_REGION_USAGE_VALID_CACHED_DATA,
        };
        let mut regions = Regions::default();
        crate::overlapped::control(
            file,
            FSCTL_QUERY_FILE_REGIONS,
            (&raw const query).cast(),
            size_of::<FILE_REGION_INPUT>() as u32,
            (&raw mut regions).cast(),
            size_of::<Regions>() as u32,
        )
        .ok()?;
        let count = (regions.count as usize).min(REGIONS);
        if regions.total as usize > count {
            return None;
        }
        super::valid_end(
            regions.regions[..count].iter().map(|region| {
                let start = region.FileOffset as u64;
                (
                    start..start.saturating_add(region.Length as u64),
                    region.Usage & FILE_REGION_USAGE_VALID_CACHED_DATA != 0,
                )
            }),
            len,
        )
    }

    /// [`super::next_data`] from the valid data length and the allocated ranges of a file:
    /// everything at or past the valid data length is a hole. Below it each allocated range of a
    /// sparse file is data, and adjacent ranges are one, also across two batches. The batch
    /// `cursor` holds answers where it maps `offset`. A file that is not sparse is one range of
    /// data up to its valid data length, and its walk reads no block. So is a sparse file whose
    /// file system refuses the query, but its walk keeps the block it read.
    ///
    /// The first lookup flushes the file ([`flushed`]) before it reads the valid data length,
    /// unless the file is neither sparse nor on NTFS, whose answer the flush cannot change.
    /// Where the flush fails, data written through a memory mapping may be in neither the valid
    /// data length nor the allocation, so the file is one range of data to its length, holes
    /// included. A read-only attribute does not change that: a handle opened to write before the
    /// attribute was set still writes. The valid data length is read on NTFS alone ([`on_ntfs`]),
    /// the one file system measured to report it as the walk reads it; on any other the allocation
    /// alone decides. A file whose data may be held elsewhere ([`HELD_ELSEWHERE`]), by the
    /// attributes its handle reports, is one range of data to its length, unflushed and unmapped.
    pub(super) fn next_data(
        file: &File,
        offset: u64,
        cursor: &mut Cursor,
    ) -> std::io::Result<Option<Range<u64>>> {
        let sparse = if let Some(sparse) = cursor.sparse {
            sparse
        } else {
            let metadata = file.metadata()?;
            let len = metadata.len();
            let attributes = metadata.file_attributes();
            let mut sparse = attributes & FILE_ATTRIBUTE_SPARSE_FILE != 0;
            let ntfs = on_ntfs(file);
            cursor.valid = len;
            if attributes & HELD_ELSEWHERE != 0 || ((sparse || ntfs) && !flushed(file)) {
                sparse = false;
            } else if ntfs {
                cursor.valid = valid_data_length(file, len).map_or(len, |valid| valid.min(len));
            }
            if sparse {
                cursor.block = unit(file, ntfs);
            }
            *cursor.sparse.insert(sparse)
        };
        let valid = cursor.valid;
        if offset >= valid {
            return Ok(None);
        }
        if !sparse {
            return Ok(Some(offset..valid));
        }
        if !cursor.mapped.contains(&offset) {
            match cursor.map_from(file, offset) {
                Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {
                    cursor.sparse = Some(false);
                    return Ok(Some(offset..valid));
                }
                mapped => mapped?,
            }
        }
        let mut found: Option<Range<u64>> = None;
        let mut first = cursor.ranges[..cursor.count]
            .iter()
            .position(|range| end_of(range) > offset)
            .unwrap_or(cursor.count);
        loop {
            let batch = cursor.ranges[first..cursor.count]
                .iter()
                .map(|range| range.FileOffset as u64..end_of(range));
            if super::fold_ranges(&mut found, batch, offset, valid) || cursor.mapped.end == u64::MAX
            {
                return Ok(found);
            }
            let next = cursor.mapped.end;
            cursor.map_from(file, next)?;
            first = 0;
        }
    }
}

/// Tells the platform the reads of `file` through this handle are made at random, so it reads
/// nothing ahead of them. Reading ahead past the end of a range of data would leave pages of the
/// extent after it in the page cache, which `SEEK_DATA` would then report as data. Other handles
/// to the same file read as before. Nothing to do where the platform reads ahead on no advice.
/// Advice alone: it changes what is cached, never what a read returns.
#[cfg(not(target_family = "windows"))]
pub(crate) fn advise_random(file: &File) -> std::io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    {
        use std::os::fd::AsRawFd;

        // SAFETY: Calling OS functions. `posix_fadvise` reads no memory of the caller's.
        let error = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_RANDOM) };
        if error == 0 {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(error))
        }
    }

    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;

        // SAFETY: Calling OS functions. `fcntl` with `F_RDAHEAD` reads no memory of the caller's.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_RDAHEAD, 0) } == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_vendor = "apple"
    )))]
    {
        let _ = file;
        Ok(())
    }
}
