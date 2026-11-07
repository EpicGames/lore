// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_fuse::reply::*;

#[test]
fn write_path_errnos_carry_the_libc_values() {
    for (errno, raw) in [
        (Errno::EEXIST, libc::EEXIST),
        (Errno::ENOTEMPTY, libc::ENOTEMPTY),
        (Errno::EACCES, libc::EACCES),
        (Errno::EPERM, libc::EPERM),
        (Errno::EXDEV, libc::EXDEV),
        (Errno::ENOSPC, libc::ENOSPC),
        (Errno::EFBIG, libc::EFBIG),
        (Errno::ENODATA, libc::ENODATA),
        (Errno::ELOOP, libc::ELOOP),
        (Errno::EBADF, libc::EBADF),
    ] {
        assert_eq!(errno, Errno(raw));
    }
}
