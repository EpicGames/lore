// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The backend for the Resilient File System: `FSCTL_DUPLICATE_EXTENTS_TO_FILE`, its block
//! cloning, which Dev Drives use as well. The call reports whether it cloned, and fails on a
//! volume that cannot, so a clone never turns into a copy unannounced.

use std::io;
use std::path::Path;

use super::source::Source;

/// The ranges, as offset and length, that cloning `length` bytes in clusters of `cluster` bytes
/// takes: the length rounded up to whole clusters, in calls of less than 4 GiB each, each a whole
/// number of clusters.
#[cfg(any(target_family = "windows", feature = "test-util"))]
#[lore_macro::test_pub]
pub(super) fn duplicate_ranges(length: u64, cluster: u32) -> impl Iterator<Item = (u64, u64)> {
    let cluster = u64::from(cluster.max(1));
    let end = length.div_ceil(cluster) * cluster;
    let most = ((1u64 << 32) - 1) / cluster * cluster;
    let mut offset = 0;
    std::iter::from_fn(move || {
        if offset >= end {
            return None;
        }
        let count = most.min(end - offset);
        let range = (offset, count);
        offset += count;
        Some(range)
    })
}

/// Clones the file `source` names to `destination`, which must not exist, from one open handle to
/// the other, and then gives the clone the times and read-only attribute that handle reports. A
/// link at `source` is copied as a link. [`io::ErrorKind::Unsupported`] where the volume cannot
/// clone the file, with nothing left at `destination`.
#[cfg(target_family = "windows")]
pub(super) fn clone_file(source: Source<'_>, destination: &Path) -> io::Result<()> {
    use super::entry;
    use super::entry::OpenedSource;

    let input = match entry::open_source(source)? {
        OpenedSource::File(input) => input,
        OpenedSource::Link => return entry::link(source, destination),
    };
    let output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let cloned = windows::duplicate_extents(&input, &output)
        .and_then(|()| entry::finish_file(&input, &output));
    if cloned.is_err() {
        drop(output);
        let _ = std::fs::remove_file(destination);
    }
    cloned
}

/// Block cloning on the Resilient File System exists on Windows only.
#[cfg(not(target_family = "windows"))]
pub(super) fn clone_file(_source: Source<'_>, _destination: &Path) -> io::Result<()> {
    Err(super::entry::unsupported("FSCTL_DUPLICATE_EXTENTS_TO_FILE"))
}

#[cfg(target_family = "windows")]
mod windows {
    use std::ffi::c_void;
    use std::fs::File;
    use std::io;
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::ERROR_INVALID_FUNCTION;
    use windows_sys::Win32::Foundation::ERROR_NOT_SAME_DEVICE;
    use windows_sys::Win32::Foundation::ERROR_NOT_SUPPORTED;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_SPARSE_FILE;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::DUPLICATE_EXTENTS_DATA;
    use windows_sys::Win32::System::Ioctl::FSCTL_DUPLICATE_EXTENTS_TO_FILE;
    use windows_sys::Win32::System::Ioctl::FSCTL_GET_INTEGRITY_INFORMATION;
    use windows_sys::Win32::System::Ioctl::FSCTL_GET_INTEGRITY_INFORMATION_BUFFER;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_INTEGRITY_INFORMATION;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_INTEGRITY_INFORMATION_BUFFER;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;

    /// Makes `output`, a new empty file, share every block of `input`.
    ///
    /// The destination takes the source's integrity settings and sparseness first, as block
    /// cloning requires, and its length. The ranges are those [`super::duplicate_ranges`] gives.
    pub(super) fn duplicate_extents(input: &File, output: &File) -> io::Result<()> {
        let metadata = input.metadata()?;
        let mut integrity = FSCTL_GET_INTEGRITY_INFORMATION_BUFFER::default();
        control(
            input,
            FSCTL_GET_INTEGRITY_INFORMATION,
            std::ptr::null(),
            0,
            (&raw mut integrity).cast(),
            size_of_u32::<FSCTL_GET_INTEGRITY_INFORMATION_BUFFER>(),
        )?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE != 0 {
            control(
                output,
                FSCTL_SET_SPARSE,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
            )?;
        }
        let settings = FSCTL_SET_INTEGRITY_INFORMATION_BUFFER {
            ChecksumAlgorithm: integrity.ChecksumAlgorithm,
            Reserved: 0,
            Flags: integrity.Flags,
        };
        control(
            output,
            FSCTL_SET_INTEGRITY_INFORMATION,
            (&raw const settings).cast(),
            size_of_u32::<FSCTL_SET_INTEGRITY_INFORMATION_BUFFER>(),
            std::ptr::null_mut(),
            0,
        )?;
        let length = metadata.len();
        output.set_len(length)?;
        for (offset, count) in super::duplicate_ranges(length, integrity.ClusterSizeInBytes) {
            let range = DUPLICATE_EXTENTS_DATA {
                FileHandle: input.as_raw_handle() as HANDLE,
                SourceFileOffset: offset as i64,
                TargetFileOffset: offset as i64,
                ByteCount: count as i64,
            };
            control(
                output,
                FSCTL_DUPLICATE_EXTENTS_TO_FILE,
                (&raw const range).cast(),
                size_of_u32::<DUPLICATE_EXTENTS_DATA>(),
                std::ptr::null_mut(),
                0,
            )?;
        }
        Ok(())
    }

    fn size_of_u32<T>() -> u32 {
        u32::try_from(size_of::<T>()).unwrap_or(u32::MAX)
    }

    /// Issues `code` on `file`. A volume that cannot clone answers
    /// [`io::ErrorKind::Unsupported`].
    fn control(
        file: &File,
        code: u32,
        input: *const c_void,
        input_size: u32,
        output: *mut c_void,
        output_size: u32,
    ) -> io::Result<()> {
        let mut returned = 0u32;
        // SAFETY: the handle is open for the call, and each buffer is valid for the size given.
        let done = unsafe {
            DeviceIoControl(
                file.as_raw_handle() as HANDLE,
                code,
                input,
                input_size,
                output,
                output_size,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if done != 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error().and_then(|code| u32::try_from(code).ok()) {
            Some(ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED | ERROR_NOT_SAME_DEVICE) => Err(
                super::super::entry::unsupported("FSCTL_DUPLICATE_EXTENTS_TO_FILE"),
            ),
            _ => Err(err),
        }
    }
}
