// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! `detect` and `choose` pick the backend for a pair of directories, `volume_of` names a volume,
//! and `locality_of` says whether its file system is local. The faithful copy of an entry keeps
//! the target's value of a refused `security.*` attribute only, refuses immutable and append-only
//! entries, and on Windows the attributes and links its backup stream can't keep.

use std::io;

use lore_base::test_util::TempDir;
use lore_io::clone::Backend;
use lore_io::clone::choose;
use lore_io::clone::detect;
use lore_io::clone::duplicate_ranges;
use lore_io::clone::volume_of;

#[test]
fn detection_leaves_no_probe_behind() {
    let root = TempDir::new("lore-io-clone-probe-");
    let source = root.join("source");
    let destination = root.join("parent");
    std::fs::create_dir(&source).expect("create the source");
    std::fs::create_dir(&destination).expect("create the parent");

    let detection = detect(&source, &destination).expect("detect the backend");

    assert_eq!(detection.source, detection.destination);
    for directory in [&source, &destination] {
        assert_eq!(
            std::fs::read_dir(directory)
                .expect("read the directory")
                .count(),
            0
        );
    }
}

#[test]
fn different_filesystems_copy_without_a_probe() {
    let backend = choose(false, |backend| panic!("{backend:?} was probed"));

    assert_eq!(backend.expect("a choice"), Backend::Copy);
}

#[test]
fn a_volume_whose_probes_all_fail_copies() {
    let mut probed = Vec::new();

    let backend = choose(true, |backend| {
        probed.push(backend);
        Err(io::Error::from(io::ErrorKind::Unsupported))
    });

    assert_eq!(backend.expect("a choice"), Backend::Copy);
    assert_eq!(probed, Backend::BLOCK_CLONING);
}

#[test]
fn a_backend_whose_probe_clones_is_chosen() {
    let Some(&cloning) = Backend::BLOCK_CLONING.last() else {
        return;
    };

    let backend = choose(true, |backend| {
        if backend == cloning {
            Ok(())
        } else {
            Err(io::Error::from(io::ErrorKind::Unsupported))
        }
    });

    assert_eq!(backend.expect("a choice"), cloning);
}

#[test]
fn a_probe_that_cannot_write_fails_the_choice() {
    if Backend::BLOCK_CLONING.is_empty() {
        return;
    }

    let backend = choose(true, |_| {
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    });

    assert_eq!(
        backend.expect_err("no choice").kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn duplicate_ranges_cover_whole_clusters_in_calls_under_four_gibibytes() {
    let ranges = |length, cluster| duplicate_ranges(length, cluster).collect::<Vec<_>>();
    let most = (1u64 << 32) - 4096;

    assert_eq!(ranges(0, 4096), vec![]);
    assert_eq!(ranges(100, 4096), vec![(0, 4096)]);
    assert_eq!(ranges(8192, 4096), vec![(0, 8192)]);
    assert_eq!(
        ranges(5 << 30, 4096),
        vec![(0, most), (most, (5 << 30) - most)]
    );
    assert_eq!(
        ranges(1 << 32, 65_536),
        vec![(0, (1 << 32) - 65_536), ((1 << 32) - 65_536, 65_536)]
    );
}

/// On Linux, the file systems Lore has verified as local and durable across a power loss are
/// local by their `statfs` magic number, and every other is not, tmpfs included.
#[cfg(target_os = "linux")]
#[test]
fn a_linux_file_system_is_local_by_its_magic_number() {
    use lore_io::clone::magic_is_local;

    for (name, magic) in [
        ("btrfs", 0x9123_683E),
        ("xfs", 0x5846_5342),
        ("bcachefs", 0xCA45_1A4E),
        ("ext2/ext3/ext4", 0xEF53),
        ("zfs", 0x2FC1_2FC1),
    ] {
        assert!(magic_is_local(magic), "{name} is local");
    }
    for (name, magic) in [
        ("tmpfs", 0x0102_1994),
        ("overlayfs", 0x794C_7630),
        ("nfs", 0x6969),
        ("smb2", 0xFE53_4D42),
        ("cifs", 0xFF53_4D42),
        ("fuse", 0x6573_5546),
        ("unknown", 0),
    ] {
        assert!(!magic_is_local(magic), "{name} is not local");
    }
}

/// On macOS, apfs is the local file system by its `f_fstypename`, and every other is not, hfs
/// included.
#[cfg(target_os = "macos")]
#[test]
fn a_macos_file_system_is_local_by_its_name() {
    use lore_io::clone::name_is_local;

    assert!(name_is_local("apfs"));
    for name in [
        "hfs", "smbfs", "nfs", "afpfs", "msdos", "exfat", "webdav", "APFS", "",
    ] {
        assert!(!name_is_local(name), "{name} is not local");
    }
}

/// On Windows, a fixed or removable drive is local, and every other drive type is not.
#[cfg(target_family = "windows")]
#[test]
fn a_windows_drive_is_local_by_its_type() {
    use lore_io::clone::drive_type_is_local;
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_CDROM;
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_NO_ROOT_DIR;
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_RAMDISK;
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOTE;
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOVABLE;
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_UNKNOWN;

    assert!(drive_type_is_local(DRIVE_FIXED));
    assert!(drive_type_is_local(DRIVE_REMOVABLE));
    for drive_type in [
        DRIVE_REMOTE,
        DRIVE_CDROM,
        DRIVE_RAMDISK,
        DRIVE_UNKNOWN,
        DRIVE_NO_ROOT_DIR,
    ] {
        assert!(!drive_type_is_local(drive_type), "{drive_type}");
    }
}

/// On Windows, a volume holds access control lists, alternate data streams and sparse files only
/// with all three of their file system flags.
#[cfg(target_family = "windows")]
#[test]
fn a_windows_volume_holds_what_a_copy_keeps_only_with_every_flag() {
    use windows_sys::Win32::System::SystemServices::FILE_NAMED_STREAMS;
    use windows_sys::Win32::System::SystemServices::FILE_PERSISTENT_ACLS;
    use windows_sys::Win32::System::SystemServices::FILE_SUPPORTS_SPARSE_FILES;

    let all = FILE_PERSISTENT_ACLS | FILE_NAMED_STREAMS | FILE_SUPPORTS_SPARSE_FILES;
    let volume = |flags| lore_io::clone::Locality {
        local: true,
        mount_id: None,
        flags,
    };
    assert!(volume(all).holds_acls_streams_and_sparse_files());
    assert!(volume(all | 1).holds_acls_streams_and_sparse_files());
    for missing in [
        FILE_PERSISTENT_ACLS,
        FILE_NAMED_STREAMS,
        FILE_SUPPORTS_SPARSE_FILES,
    ] {
        assert!(!volume(all & !missing).holds_acls_streams_and_sparse_files());
    }
}

/// Two directories on one mount have one volume and one locality, with the mount ID `statx`
/// names on Linux.
#[test]
fn two_directories_on_one_mount_have_one_volume() {
    let root = TempDir::new("lore-io-clone-volume-");
    let inner = root.join("inner");
    std::fs::create_dir(&inner).expect("create the directory");

    let outer = volume_of(root.path()).expect("the volume of the root");
    assert_eq!(
        volume_of(&inner).expect("the volume of the directory"),
        outer
    );
    let locality = lore_io::clone::locality_of(root.path()).expect("the locality of the root");
    assert_eq!(
        lore_io::clone::locality_of(&inner).expect("the locality of the directory"),
        locality
    );
    #[cfg(target_os = "linux")]
    assert!(locality.mount_id.is_some(), "{locality:?}");
    #[cfg(not(target_os = "linux"))]
    assert_eq!(locality.mount_id, None);
}

/// A `security.*` attribute the system's policy refuses to set keeps the value the target gives
/// it; every other failure, and an attribute the target's file system can't hold, fails the copy.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn only_a_refused_security_attribute_keeps_the_targets_value() {
    use lore_io::clone::target_keeps;

    let failed = io::Error::from_raw_os_error;
    for name in [b"security.selinux".as_slice(), b"security.capability"] {
        assert!(target_keeps(name, &failed(libc::EPERM)));
        assert!(target_keeps(name, &failed(libc::EACCES)));
        assert!(!target_keeps(name, &failed(libc::ENOTSUP)));
        assert!(!target_keeps(name, &failed(libc::ENOSPC)));
    }
    for name in [
        b"user.lore".as_slice(),
        b"trusted.lore",
        b"system.posix_acl_access",
        b"system.posix_acl_default",
    ] {
        for code in [
            libc::EPERM,
            libc::EACCES,
            libc::ENOTSUP,
            libc::ENOSPC,
            libc::EIO,
        ] {
            assert!(!target_keeps(name, &failed(code)), "{code}");
        }
    }
    assert!(!target_keeps(b"security.selinux", &failed(libc::EIO)));
    assert!(!target_keeps(
        b"security.capability",
        &io::Error::other("not an OS error")
    ));
}

/// `flag` as the faithful copy reads flags.
#[cfg(target_os = "linux")]
fn flag_bits(flag: libc::c_int) -> u64 {
    u64::try_from(flag).expect("flags")
}

/// `flag` as the faithful copy reads flags.
#[cfg(target_os = "macos")]
fn flag_bits(flag: libc::c_uint) -> u64 {
    u64::from(flag)
}

/// An entry marked immutable or append-only is refused, and the flags that mark how a file is
/// shown, backed up or stored are left out.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn only_immutable_and_append_only_flags_are_refused() {
    use lore_io::clone::refuses_flags;

    #[cfg(target_os = "linux")]
    let (refused, kept) = (
        [
            libc::STATX_ATTR_IMMUTABLE,
            libc::STATX_ATTR_APPEND,
            libc::STATX_ATTR_IMMUTABLE | libc::STATX_ATTR_APPEND | libc::STATX_ATTR_NODUMP,
        ],
        [0, libc::STATX_ATTR_COMPRESSED, libc::STATX_ATTR_NODUMP],
    );
    #[cfg(target_os = "macos")]
    let (refused, kept) = (
        [
            libc::UF_IMMUTABLE,
            libc::UF_APPEND,
            libc::SF_IMMUTABLE,
            libc::SF_APPEND,
            libc::UF_IMMUTABLE | libc::UF_HIDDEN,
        ],
        [0, libc::UF_HIDDEN, libc::UF_COMPRESSED, libc::UF_NODUMP],
    );
    for flags in refused {
        assert!(refuses_flags(flag_bits(flags)), "{flags:#x}");
    }
    for flags in kept {
        assert!(!refuses_flags(flag_bits(flags)), "{flags:#x}");
    }
}

/// An attribute list or value read as empty is empty, even where the call would answer more had
/// it been asked again, as when an attribute is set between the two calls: the buffer never takes
/// a length larger than it holds.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn an_attribute_read_as_empty_stays_empty() {
    let calls = std::cell::Cell::new(0);
    let mut buffer = Vec::new();

    lore_io::clone::read_sized(&mut buffer, |_buffer, _size| {
        calls.set(calls.get() + 1);
        if calls.get() == 1 { 0 } else { 5 }
    })
    .expect("read");

    assert!(buffer.is_empty());
    assert!(buffer.len() <= buffer.capacity());
}

/// A faithful copy refuses an entry with more than one link, or with an attribute it doesn't keep,
/// such as a reparse point's, an encrypted or a compressed entry's, and keeps every other.
#[cfg(target_family = "windows")]
#[test]
fn a_faithful_copy_refuses_what_backup_writes_cannot_keep() {
    use lore_io::clone::refusal;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_ARCHIVE;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_COMPRESSED;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_ENCRYPTED;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_INTEGRITY_STREAM;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_READONLY;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_SPARSE_FILE;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_SYSTEM;

    let kept = FILE_ATTRIBUTE_ARCHIVE
        | FILE_ATTRIBUTE_HIDDEN
        | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED
        | FILE_ATTRIBUTE_READONLY
        | FILE_ATTRIBUTE_SPARSE_FILE
        | FILE_ATTRIBUTE_SYSTEM;
    assert!(refusal(kept, 1).is_none());
    assert!(refusal(FILE_ATTRIBUTE_DIRECTORY, 1).is_none());
    assert!(refusal(FILE_ATTRIBUTE_NORMAL, 1).is_none());
    assert!(refusal(FILE_ATTRIBUTE_INTEGRITY_STREAM, 1).is_some());
    assert!(refusal(kept, 2).is_some());
    for refused in [
        FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_ATTRIBUTE_ENCRYPTED,
        FILE_ATTRIBUTE_COMPRESSED,
    ] {
        assert!(refusal(kept | refused, 1).is_some(), "{refused:#x}");
        assert!(
            refusal(FILE_ATTRIBUTE_DIRECTORY | refused, 1).is_some(),
            "{refused:#x}"
        );
    }
}
