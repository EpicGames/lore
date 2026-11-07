// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use lore_fuse::abi::NotifyCode;
use lore_fuse::abi::NotifyInvalEntryOut;
use lore_fuse::abi::NotifyInvalInodeOut;
use lore_fuse::abi::OutHeader;
use lore_fuse::abi::notify::EXPIRE_ONLY;
use lore_fuse::notify::*;
use lore_fuse::transport::Notifier;
use lore_fuse::transport::TransportError;
use zerocopy::FromBytes;

/// A `Notifier` wired to one end of a `socketpair`, with the other end handed back as a
/// plain `UnixStream` so tests can read exactly what was written.
fn socketpair_notifier() -> (Notifier, UnixStream) {
    let mut fds = [-1i32; 2];
    // SAFETY: `fds` is a valid, appropriately sized out-parameter.
    let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    assert_eq!(ret, 0, "socketpair must succeed in a test");
    // SAFETY: both fds were just created by `socketpair` above and are open and valid.
    let notifier_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: see above.
    let reader_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    (
        Notifier {
            fd: Arc::new(notifier_end),
        },
        UnixStream::from(reader_end),
    )
}

#[test]
fn inval_inode_encodes_range() {
    let (notifier, mut reader) = socketpair_notifier();

    notifier.inval_inode(42, 100, 200).expect("send");

    let mut buf = [0u8; 256];
    let n = std::io::Read::read(&mut reader, &mut buf).expect("read");

    let (header, rest) = OutHeader::ref_from_prefix(&buf[..n]).expect("header decodes");
    assert_eq!(header.error, NotifyCode::InvalInode as i32);
    assert_eq!(header.unique, 0);

    let (body, _) = NotifyInvalInodeOut::ref_from_prefix(rest).expect("body decodes");
    assert_eq!(body.ino, 42);
    assert_eq!(body.off, 100);
    assert_eq!(body.len, 200);
}

#[test]
fn inval_entry_encodes_parent_namelen_flags_and_name() {
    let (notifier, mut reader) = socketpair_notifier();

    notifier.inval_entry(7, b"foo", true).expect("send");

    let mut buf = [0u8; 256];
    let n = std::io::Read::read(&mut reader, &mut buf).expect("read");

    let (header, rest) = OutHeader::ref_from_prefix(&buf[..n]).expect("header decodes");
    assert_eq!(header.error, NotifyCode::InvalEntry as i32);
    assert_eq!(header.unique, 0);

    let (body, rest) = NotifyInvalEntryOut::ref_from_prefix(rest).expect("body decodes");
    assert_eq!(body.parent, 7);
    assert_eq!(body.namelen, 3);
    assert_eq!(body.flags, EXPIRE_ONLY);

    assert_eq!(&rest[..3], b"foo");
    assert_eq!(rest[3], 0, "name must be NUL-terminated");
}

#[test]
fn enoent_from_kernel_is_ok_enotconn_is_disconnected() {
    let enoent = std::io::Error::from_raw_os_error(libc::ENOENT);
    assert!(classify_notify_error(enoent).is_ok());

    let enotconn = std::io::Error::from_raw_os_error(libc::ENOTCONN);
    let error = classify_notify_error(enotconn).expect_err("must be disconnected");
    assert!(matches!(error, TransportError::Disconnected));
}
