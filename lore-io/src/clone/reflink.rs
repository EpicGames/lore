// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The reflink backend: the `FICLONE` ioctl, which makes a file share every extent of another and
//! fails with an error, not a copy, when the filesystem cannot. Btrfs, XFS and bcachefs support
//! it.

use std::io;
use std::path::Path;

use super::source::Source;

/// Clones the file `source` names to `destination`, which must not exist, from one open handle to
/// the other, and then gives the clone the permissions and times that handle reports. A link at
/// `source` is copied as a link. [`io::ErrorKind::Unsupported`] where the filesystem cannot
/// clone the file, with nothing left at `destination`.
#[cfg(target_os = "linux")]
pub(super) fn clone_file(source: Source<'_>, destination: &Path) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    use super::entry;
    use super::entry::OpenedSource;

    let input = match entry::open_source(source)? {
        OpenedSource::File(input) => input,
        OpenedSource::Link => return entry::link(source, destination),
    };
    let output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)?;
    // SAFETY: both descriptors are open; FICLONE takes the source descriptor as argument.
    let cloned =
        if unsafe { libc::ioctl(output.as_raw_fd(), libc::FICLONE, input.as_raw_fd()) } == 0 {
            entry::finish_file(&input, &output)
        } else {
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EOPNOTSUPP | libc::EXDEV | libc::EINVAL) => {
                    Err(entry::unsupported("FICLONE"))
                }
                _ => Err(err),
            }
        };
    if cloned.is_err() {
        drop(output);
        let _ = std::fs::remove_file(destination);
    }
    cloned
}

/// `FICLONE` exists on Linux only.
#[cfg(not(target_os = "linux"))]
pub(super) fn clone_file(_source: Source<'_>, _destination: &Path) -> io::Result<()> {
    Err(super::entry::unsupported("FICLONE"))
}
