// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Directory searches on Windows, which query the directory with a larger buffer.
//!
//! `std::fs::read_dir` searches with `FindFirstFileExW` and no flags. A search opened with
//! `FIND_FIRST_EX_LARGE_FETCH` has the system query the directory with a larger buffer, so a wide
//! directory takes fewer queries into the file system, and through each filter driver on the
//! volume, for the same names.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::Path;
use std::path::PathBuf;

use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows_sys::Win32::Foundation::ERROR_NO_MORE_FILES;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FIND_FIRST_EX_LARGE_FETCH;
use windows_sys::Win32::Storage::FileSystem::FindClose;
use windows_sys::Win32::Storage::FileSystem::FindExInfoBasic;
use windows_sys::Win32::Storage::FileSystem::FindExSearchNameMatch;
use windows_sys::Win32::Storage::FileSystem::FindFirstFileExW;
use windows_sys::Win32::Storage::FileSystem::FindNextFileW;
use windows_sys::Win32::Storage::FileSystem::WIN32_FIND_DATAW;

use crate::DirEntry;

/// The tag bit `IsReparseTagNameSurrogate` tests: a reparse point whose tag carries it stands for
/// another name, as a symbolic link and a junction do. windows-sys has no constant for it.
const NAME_SURROGATE: u32 = 0x2000_0000;

const DOT: u16 = b'.' as u16;

const STAR: u16 = b'*' as u16;

const SEPARATOR: u16 = b'\\' as u16;

/// An open directory search, closed when dropped.
#[lore_macro::test_pub]
pub(crate) struct Search {
    handle: HANDLE,
    /// The directory searched, which a name is joined to for a stat by path.
    root: PathBuf,
}

// SAFETY: a find handle is not tied to the thread that opened it. `Search` owns its handle and
// advances it through `&mut self`, so one thread uses it at a time, as `std::fs::ReadDir` does.
unsafe impl Send for Search {}

// SAFETY: only `&mut self` and `drop` reach the handle, so a shared `&Search` gives no thread a
// way to use it. `std`'s find handle is `Sync` for the same reason.
unsafe impl Sync for Search {}

impl Drop for Search {
    fn drop(&mut self) {
        // SAFETY: the handle came from a `FindFirstFileExW` that succeeded, and is closed once.
        unsafe { FindClose(self.handle) };
    }
}

/// What opening a search of a directory answers.
#[lore_macro::test_pub]
pub(crate) enum Opened {
    /// The search is open, and the record the caller passed holds its first name.
    Search(Search),
    /// The search matched no name, which it reports as `ERROR_FILE_NOT_FOUND` and `std` lists as
    /// an empty directory.
    Empty,
    /// The path is handed back unsearched, since the search could answer for it differently than
    /// `std::fs::read_dir`: a relative path, which `std` resolves first, and a path whose search
    /// pattern `FindFirstFileExW` can't take as it stands, which `std` prefixes or refuses.
    Declined(PathBuf),
}

/// Opens a search of the directory `root` that queries it with a larger buffer, writing the first
/// record it lists into `first`.
///
/// A failure is the OS error `std::fs::read_dir` reports for the same path.
#[lore_macro::test_pub]
pub(crate) fn open(root: PathBuf, first: &mut WIN32_FIND_DATAW) -> std::io::Result<Opened> {
    if !root.is_absolute() {
        return Ok(Opened::Declined(root));
    }
    let separated = matches!(
        root.as_os_str().as_encoded_bytes().last(),
        Some(b'\\' | b'/')
    );
    let suffix: &[u16] = if separated {
        &[STAR]
    } else {
        &[SEPARATOR, STAR]
    };
    let mut pattern = [0u16; crate::psync::PATH_UNITS];
    if crate::psync::encode_wide_path(&root, suffix, &mut pattern).is_err() {
        return Ok(Opened::Declined(root));
    }
    // SAFETY: the pattern is null-terminated, and `first` is writable for the record the basic
    // information level fills in. The search takes no filter, which is what
    // `FindExSearchNameMatch` with a null filter means.
    let handle = unsafe {
        FindFirstFileExW(
            pattern.as_ptr(),
            FindExInfoBasic,
            std::ptr::from_mut(first).cast(),
            FindExSearchNameMatch,
            std::ptr::null(),
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    if handle != INVALID_HANDLE_VALUE {
        return Ok(Opened::Search(Search { handle, root }));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) {
        return Ok(Opened::Empty);
    }
    Err(error)
}

impl Search {
    /// Appends what the search lists to `chunk` until it holds `limit` entries, starting with
    /// `first` where the search has just opened. Answers whether the search has more to list.
    ///
    /// A record the search cannot read is appended as its OS error, and ends the search.
    pub(crate) fn fill(
        &mut self,
        first: Option<&WIN32_FIND_DATAW>,
        chunk: &mut Vec<std::io::Result<DirEntry>>,
        limit: usize,
    ) -> bool {
        if let Some(entry) = first.and_then(|first| entry(&self.root, first)) {
            chunk.push(Ok(entry));
        }
        let mut record = WIN32_FIND_DATAW::default();
        while chunk.len() < limit {
            // SAFETY: the handle is open, and `record` is writable for the record the call fills
            // in.
            if unsafe { FindNextFileW(self.handle, &mut record) } == 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(ERROR_NO_MORE_FILES as i32) {
                    chunk.push(Err(error));
                }
                return false;
            }
            if let Some(entry) = entry(&self.root, &record) {
                chunk.push(Ok(entry));
            }
        }
        true
    }
}

/// The name `record` lists, without the terminator and the unused units after it.
pub(crate) fn record_name(record: &WIN32_FIND_DATAW) -> &[u16] {
    let length = record
        .cFileName
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(record.cFileName.len());
    &record.cFileName[..length]
}

/// The entry `record` lists in the directory `root`, or `None` for `.` and `..`.
///
/// The record describes the name it lists, which is what the entry carries unless the name is a
/// symbolic link or a junction, or holds a time of the last write `SystemTime` can't hold. Those
/// are stat'd by path, which describes what a symbolic link stands for.
#[lore_macro::test_pub]
pub(crate) fn entry(root: &Path, record: &WIN32_FIND_DATAW) -> Option<DirEntry> {
    let name = record_name(record);
    if name == [DOT] || name == [DOT, DOT] {
        return None;
    }
    let file_name = OsString::from_wide(name);
    let is_symlink = is_symbolic_link(record);
    let described = if is_symlink {
        None
    } else {
        crate::stat::from_attributes(
            record.dwFileAttributes,
            record.nFileSizeHigh,
            record.nFileSizeLow,
            record.ftLastWriteTime,
        )
    };
    let metadata = described.or_else(|| crate::psync::std_stat(&root.join(&file_name)).ok());
    Some(DirEntry {
        file_name,
        metadata,
        is_symlink,
    })
}

/// Whether `record` lists a symbolic link or a junction: a reparse point whose tag says it stands
/// for another name, which is what `std` takes for a symbolic link.
#[lore_macro::test_pub]
fn is_symbolic_link(record: &WIN32_FIND_DATAW) -> bool {
    record.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        && record.dwReserved0 & NAME_SURROGATE != 0
}
