// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Holes: ranges of a file that hold no data.
//!
//! A hole is a range the file system holds no written data for: one never written, which it maps to
//! no storage, or an extent it allocated but never wrote, such as one `fallocate` reserved (an
//! *unwritten* extent on XFS and ext4). A hole reads as zeros, but it is not zeros: zeros written
//! to a file are data, and the callers of this module keep the two apart.
//!
//! On Linux the map of a file comes from the `FIEMAP` ioctl, with the file synced first, so what
//! is a hole depends on what the file system allocated and wrote alone. `SEEK_DATA` and
//! `SEEK_HOLE`, the fallback where a file system does not support `FIEMAP` and the only source on
//! the other platforms that have them, also take an unwritten extent as a hole, but take it as
//! data wherever the page cache holds pages for it, which any read of the extent, or reading
//! ahead into it, leaves behind. A walk that takes `SEEK_DATA` advises random reads of the file on
//! its first lookup ([`advise_random`]), so the reads through its handle read nothing ahead.
//!
//! All are blocking calls, which the syscall pool runs.

use std::fs::File;
use std::ops::Range;

/// What one walk of a file's ranges of data keeps across [`next_data`] calls: the file system's
/// block, read once ([`DataCursor::block`]), and on Linux the last batch of extents `FIEMAP`
/// mapped, so the walk maps the file, and syncs it, once for each batch. A walk starts with a new
/// cursor and keeps it to one file.
#[derive(Default)]
pub struct DataCursor {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    map: Option<Box<fiemap::Cursor>>,
    /// Set once the walk has advised random reads of the file, on its first `SEEK_DATA` lookup.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    advised: bool,
    /// The file system's block, read on the walk's first lookup. On Linux the `FIEMAP` part keeps
    /// it, so that the cursor stays one pointer.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    block: Option<u64>,
}

impl DataCursor {
    /// The file system's block, `fstatfs`'s `f_bsize`, which the walk read on its first lookup and
    /// widens each range from `FIEMAP` to. `None` before the first lookup, and where the platform
    /// reports no holes.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn block(&self) -> Option<u64> {
        self.map.as_ref().and_then(|map| map.block)
    }

    /// The file system's block, `fstatfs`'s `f_bsize`, which the walk read on its first lookup.
    /// `None` before the first lookup, and where the platform reports no holes.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub fn block(&self) -> Option<u64> {
        self.block
    }
}

/// The first range of `file` at or after `offset` the file system holds data for, ending where the
/// next hole begins: `None` when only holes follow `offset`, or `offset` is at or past the end.
/// An unwritten extent is a hole. A range from `FIEMAP` is widened to whole blocks of the
/// file system, and may end past the end of the file. A file system that refuses `FIEMAP` once is
/// walked with `SEEK_DATA` from then on, and the first `SEEK_DATA` lookup of a walk advises random
/// reads of `file` ([`advise_random`]). The advice changes no answer, so its failure is ignored.
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
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    if !cursor.advised {
        cursor.advised = true;
        let _ = advise_random(file);
        #[cfg(any(target_os = "freebsd", target_vendor = "apple"))]
        {
            cursor.block = Some(block_size(file)?);
        }
    }
    seek_next_data(file, offset)
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

/// [`next_data`] from `SEEK_DATA` and `SEEK_HOLE`, where the platform has them.
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

/// Tells the platform the reads of `file` through this handle are made at random, so it reads
/// nothing ahead of them. Reading ahead past the end of a range of data would leave pages of the
/// extent after it in the page cache, which `SEEK_DATA` would then report as data. Other handles
/// to the same file read as before. Nothing to do where the platform reads ahead on no advice.
/// Advice alone: it changes what is cached, never what a read returns.
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
