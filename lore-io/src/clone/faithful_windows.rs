// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The entries of a faithful copy on Windows. Each file and directory is copied with `BackupRead`
//! and `BackupWrite`, on handles opened with `FILE_FLAG_BACKUP_SEMANTICS`, through one buffer of
//! the copying thread's, and each file is flushed with `FlushFileBuffers` before its handle closes.
//!
//! On NTFS the backup stream carries a file's data and its sparse ranges, the alternate data
//! streams of files and directories, and, where asked to, the owner, group, DACL and mandatory
//! label, with each ACE's inheritance flags and the DACL's protection. It puts the security first,
//! and `BackupWrite` opens each alternate data stream through the security it has set, so a DACL
//! that grants the caller no write fails the copy. The copy therefore copies the stream without
//! its security, and sets the owner, group, DACL and label itself once the streams are written.
//! `BackupWrite` marks the copy sparse where the stream says the source is; the copy marks a
//! sparse file sparse itself before its data, so it doesn't rest on that alone.
//!
//! The stream leaves to the caller the attributes, which the copy sets with its times once the
//! data is in place, and a directory's once its last entry is; the creation, access and write
//! times are kept to the 100 ns, read from the source after its data. It doesn't tell a file with
//! more than one link, which the copy refuses by the link count. A reparse point's data comes as a
//! stream of its own, which the copy refuses by the entry's attributes, as it refuses a link or a
//! junction the walk lists.
//!
//! The stream silently loses what the copy can't keep as it is: an encrypted file's stream holds
//! no data, and a compressed file's data is written uncompressed. So the copy refuses an entry
//! with an attribute it can't keep as it is, such as encryption or compression, and an owner the
//! caller may not assign. It leaves out the audit entries of a SACL, which a process reads only
//! with `SeSecurityPrivilege`, and object IDs, which identify a file on its volume rather than
//! what it holds.

use std::cell::Cell;
use std::fs::File;
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
use windows_sys::Win32::Foundation::ERROR_INVALID_OWNER;
use windows_sys::Win32::Foundation::GENERIC_READ;
use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::GROUP_SECURITY_INFORMATION;
use windows_sys::Win32::Security::GetKernelObjectSecurity;
use windows_sys::Win32::Security::GetSecurityDescriptorControl;
use windows_sys::Win32::Security::LABEL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
use windows_sys::Win32::Security::SE_SACL_PRESENT;
use windows_sys::Win32::Security::SetKernelObjectSecurity;
use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
use windows_sys::Win32::Storage::FileSystem::BackupRead;
use windows_sys::Win32::Storage::FileSystem::BackupWrite;
use windows_sys::Win32::Storage::FileSystem::DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_ARCHIVE;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_COMPRESSED;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_ENCRYPTED;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_OFFLINE;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_READONLY;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_SPARSE_FILE;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_SYSTEM;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_TEMPORARY;
use windows_sys::Win32::Storage::FileSystem::FILE_BASIC_INFO;
use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_FLAG_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE;
use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_FLAG_POSIX_SEMANTICS;
use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO;
use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO_EX;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_SEQUENTIAL_SCAN;
use windows_sys::Win32::Storage::FileSystem::FileBasicInfo;
use windows_sys::Win32::Storage::FileSystem::FileDispositionInfo;
use windows_sys::Win32::Storage::FileSystem::FileDispositionInfoEx;
use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;
use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;
use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
use windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle;
use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;
use windows_sys::Win32::Storage::FileSystem::WRITE_OWNER;
use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;

use super::COPY_BUFFER;
use super::TestPoints;
use super::refused;
use super::source::Source;

/// What a source is opened for: its data and streams, and its security descriptor.
const SOURCE_ACCESS: u32 = GENERIC_READ | READ_CONTROL;

/// What a copy is opened for: its data, streams and attributes, and its owner, DACL and label.
const COPY_ACCESS: u32 = GENERIC_WRITE | WRITE_OWNER | WRITE_DAC;

/// The parts of a security descriptor the copy keeps.
const SECURITY: u32 = OWNER_SECURITY_INFORMATION
    | GROUP_SECURITY_INFORMATION
    | DACL_SECURITY_INFORMATION
    | LABEL_SECURITY_INFORMATION;

/// The attributes the copy takes from its source through its basic information.
const SET_ATTRIBUTES: u32 = FILE_ATTRIBUTE_READONLY
    | FILE_ATTRIBUTE_HIDDEN
    | FILE_ATTRIBUTE_SYSTEM
    | FILE_ATTRIBUTE_ARCHIVE
    | FILE_ATTRIBUTE_TEMPORARY
    | FILE_ATTRIBUTE_OFFLINE
    | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;

/// The attributes the copy keeps: those it sets, and the kind and sparseness the stream carries.
const KEPT_ATTRIBUTES: u32 =
    SET_ATTRIBUTES | FILE_ATTRIBUTE_NORMAL | FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_SPARSE_FILE;

/// Attributes of an entry the copy refuses, each with what it names the entry.
const NAMED_REFUSALS: [(u32, &str); 3] = [
    (FILE_ATTRIBUTE_REPARSE_POINT, "a reparse point"),
    (FILE_ATTRIBUTE_ENCRYPTED, "an encrypted entry"),
    (FILE_ATTRIBUTE_COMPRESSED, "a compressed entry"),
];

thread_local! {
    /// The buffers a thread copies backup streams and security descriptors through, kept from
    /// one entry to the next.
    static BUFFERS: Cell<(Vec<u8>, Vec<u8>)> = const { Cell::new((Vec::new(), Vec::new())) };
}

/// Copies the file `source` names to `destination`, which must not exist, with its data, sparse
/// ranges, alternate data streams, security, attributes and times, and flushes the copy before
/// its handle closes. Refuses a file [`refusal`] names, and one whose owner the caller may not
/// assign. A copy that fails is marked deleted.
pub fn file(source: Source<'_>, destination: &Path, points: &TestPoints) -> io::Result<()> {
    let input = std::fs::OpenOptions::new()
        .access_mode(SOURCE_ACCESS)
        .custom_flags(
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_SEQUENTIAL_SCAN,
        )
        .open(&*source)?;
    let info = handle_information(&input)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            "a directory now",
        ));
    }
    refuse(info.dwFileAttributes, info.nNumberOfLinks)?;
    let output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .access_mode(COPY_ACCESS | DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(destination)?;
    let copied = (|| {
        if info.dwFileAttributes & FILE_ATTRIBUTE_SPARSE_FILE != 0 {
            crate::overlapped::control(
                &output,
                FSCTL_SET_SPARSE,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
            )?;
        }
        backup(&input, &output)?;
        #[cfg(feature = "test-util")]
        points.failure_after_data(destination)?;
        finish(&input, &output)?;
        output.sync_all()?;
        #[cfg(feature = "test-util")]
        points
            .flushed_files
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    })();
    #[cfg(not(feature = "test-util"))]
    let _ = points;
    if copied.is_err() {
        discard(&output);
    }
    copied
}

/// Gives the directory `destination` the alternate data streams, security, attributes and times
/// of the directory at `source`, which the walk holds open, once every entry is in place. Refuses
/// a source [`refusal`] names, as one replaced since it was listed may be.
pub fn finish_directory(source: &Path, destination: &Path) -> io::Result<()> {
    let input = std::fs::OpenOptions::new()
        .access_mode(SOURCE_ACCESS)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(source)?;
    refuse(basic_information(&input)?.FileAttributes, 1)?;
    let output = std::fs::OpenOptions::new()
        .access_mode(COPY_ACCESS)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(destination)?;
    backup(&input, &output)?;
    finish(&input, &output)
}

/// What the copy names an entry with `attributes` and `links` that it refuses, or `None` where
/// it keeps the entry: one with more than one link, or with an attribute it can't keep as it is.
#[lore_macro::test_pub]
fn refusal(attributes: u32, links: u32) -> Option<&'static str> {
    if links > 1 {
        return Some("an entry with more than one link");
    }
    let refused = attributes & !KEPT_ATTRIBUTES;
    (refused != 0).then(|| {
        NAMED_REFUSALS
            .iter()
            .find(|(attribute, _)| refused & attribute != 0)
            .map_or("an entry with an attribute", |&(_, what)| what)
    })
}

/// Refuses an entry with `attributes` and `links`, as [`refusal`] says.
pub fn refuse(attributes: u32, links: u32) -> io::Result<()> {
    refusal(attributes, links).map_or(Ok(()), |what| Err(refused(what)))
}

/// Copies the backup stream of `input` to `output` through the thread's buffer, and then the
/// owner, group, DACL and label, as the module describes.
fn backup(input: &File, output: &File) -> io::Result<()> {
    let (mut data, mut security) = BUFFERS.take();
    data.resize(COPY_BUFFER, 0);
    let copied = copy_stream(input, output, &mut data)
        .and_then(|()| copy_security(input, output, &mut security));
    BUFFERS.set((data, security));
    copied
}

/// Copies the backup stream of `input`, without its security, to `output` through `buffer`.
fn copy_stream(input: &File, output: &File, buffer: &mut [u8]) -> io::Result<()> {
    let mut reading = std::ptr::null_mut();
    let mut writing = std::ptr::null_mut();
    let copied = (|| {
        loop {
            let mut read = 0u32;
            // SAFETY: the handle is open, `buffer` is writable for its length, and `reading` is
            // the context of this stream's reads.
            check(unsafe {
                BackupRead(
                    handle(input),
                    buffer.as_mut_ptr(),
                    u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                    &mut read,
                    0,
                    0,
                    &mut reading,
                )
            })?;
            let mut chunk = buffer.get(..read as usize).unwrap_or_default();
            if chunk.is_empty() {
                return Ok(());
            }
            while !chunk.is_empty() {
                let mut written = 0u32;
                // SAFETY: the handle is open, `chunk` is readable for its length, and `writing`
                // is the context of this stream's writes.
                check(unsafe {
                    BackupWrite(
                        handle(output),
                        chunk.as_ptr(),
                        chunk.len() as u32,
                        &mut written,
                        0,
                        0,
                        &mut writing,
                    )
                })?;
                if written == 0 {
                    return Err(io::Error::from(io::ErrorKind::WriteZero));
                }
                chunk = chunk.get(written as usize..).unwrap_or_default();
            }
        }
    })();
    let mut done = 0u32;
    // SAFETY: the handles are open, and a call that aborts frees the context it is given.
    unsafe {
        if !reading.is_null() {
            BackupRead(
                handle(input),
                std::ptr::null_mut(),
                0,
                &mut done,
                1,
                0,
                &mut reading,
            );
        }
        if !writing.is_null() {
            BackupWrite(
                handle(output),
                std::ptr::null_mut(),
                0,
                &mut done,
                1,
                0,
                &mut writing,
            );
        }
    }
    copied
}

/// Gives `output` the owner, group, DACL and label of `input`, read into `buffer`, the label only
/// where `input` has one, so a copy without one is given no empty SACL. Refuses an owner the
/// caller may not assign.
fn copy_security(input: &File, output: &File, buffer: &mut Vec<u8>) -> io::Result<()> {
    loop {
        let mut needed = 0u32;
        // SAFETY: the handle is open, and `buffer` is writable for its length.
        let read = unsafe {
            GetKernelObjectSecurity(
                handle(input),
                SECURITY,
                buffer.as_mut_ptr().cast(),
                u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                &mut needed,
            )
        };
        if read != 0 {
            break;
        }
        let failure = io::Error::last_os_error();
        if failure.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(failure);
        }
        buffer.resize(needed as usize, 0);
    }
    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: `buffer` holds the self-relative descriptor just read, and the outputs are writable.
    check(unsafe {
        GetSecurityDescriptorControl(buffer.as_mut_ptr().cast(), &mut control, &mut revision)
    })?;
    let parts = if control & SE_SACL_PRESENT == 0 {
        SECURITY & !LABEL_SECURITY_INFORMATION
    } else {
        SECURITY
    };
    // SAFETY: the handle is open, and `buffer` holds the self-relative descriptor just read.
    match check(unsafe {
        SetKernelObjectSecurity(handle(output), parts, buffer.as_mut_ptr().cast())
    }) {
        Err(err) if err.raw_os_error() == Some(ERROR_INVALID_OWNER as i32) => {
            Err(refused("an owner this process may not assign"))
        }
        set => set,
    }
}

/// Gives `output` the attributes and the creation, access and write times of `input`, read once
/// its data is in place.
fn finish(input: &File, output: &File) -> io::Result<()> {
    let mut basic = basic_information(input)?;
    basic.ChangeTime = 0;
    basic.FileAttributes = match basic.FileAttributes & SET_ATTRIBUTES {
        0 => FILE_ATTRIBUTE_NORMAL,
        kept => kept,
    };
    set_basic_information(output, &basic)
}

/// Marks the copy `output` deleted, so it is removed once its handle closes, whatever its
/// security says: with POSIX semantics past its read-only attribute where the file system has
/// them, and otherwise once the attribute is cleared.
fn discard(output: &File) {
    let disposition = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    // SAFETY: the handle is open, and `disposition` is readable for the size given.
    let marked = unsafe {
        SetFileInformationByHandle(
            handle(output),
            FileDispositionInfoEx,
            (&raw const disposition).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if marked != 0 {
        return;
    }
    let cleared = FILE_BASIC_INFO {
        FileAttributes: FILE_ATTRIBUTE_NORMAL,
        ..FILE_BASIC_INFO::default()
    };
    let _ = set_basic_information(output, &cleared);
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the handle is open, and `disposition` is readable for the size given.
    unsafe {
        SetFileInformationByHandle(
            handle(output),
            FileDispositionInfo,
            (&raw const disposition).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        );
    }
}

/// The attributes, times and link count of the entry `file` was opened on.
fn handle_information(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    // SAFETY: zeroed is a valid BY_HANDLE_FILE_INFORMATION, which the call fills.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is open, and `info` is writable.
    check(unsafe { GetFileInformationByHandle(handle(file), &raw mut info) })?;
    Ok(info)
}

/// The basic information of the entry `file` was opened on: its times and attributes.
fn basic_information(file: &File) -> io::Result<FILE_BASIC_INFO> {
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: the handle is open, and `basic` is writable for the size given.
    check(unsafe {
        GetFileInformationByHandleEx(
            handle(file),
            FileBasicInfo,
            (&raw mut basic).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    })?;
    Ok(basic)
}

/// Sets the basic information of the entry `file` is open on to `basic`: its times, where they
/// are not zero, and its attributes.
fn set_basic_information(file: &File, basic: &FILE_BASIC_INFO) -> io::Result<()> {
    // SAFETY: the handle is open, and `basic` is readable for the size given.
    check(unsafe {
        SetFileInformationByHandle(
            handle(file),
            FileBasicInfo,
            std::ptr::from_ref(basic).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    })
}

/// The raw handle of `file`.
fn handle(file: &File) -> HANDLE {
    file.as_raw_handle() as HANDLE
}

/// `Ok` where a call answered nonzero, and its OS error otherwise.
fn check(answer: i32) -> io::Result<()> {
    if answer == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
