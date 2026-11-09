// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What a socket's directory and the socket itself let other accounts do on Windows, where a
//! socket file keeps the access list its directory hands down rather than permission bits.
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use lore_base::test_util::TempDir;
use lore_transport::local_socket;
use lore_transport::local_socket::Claim;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Foundation::GENERIC_ALL;
use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::ACCESS_ALLOWED_ACE;
use windows_sys::Win32::Security::ACE_HEADER;
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::Authorization::GetNamedSecurityInfoW;
use windows_sys::Win32::Security::Authorization::SDDL_REVISION_1;
use windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT;
use windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW;
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::GetAce;
use windows_sys::Win32::Security::GetSecurityDescriptorDacl;
use windows_sys::Win32::Security::INHERIT_ONLY_ACE;
use windows_sys::Win32::Security::IsWellKnownSid;
use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;
use windows_sys::Win32::Security::WELL_KNOWN_SID_TYPE;
use windows_sys::Win32::Security::WinBuiltinUsersSid;
use windows_sys::Win32::Security::WinWorldSid;
use windows_sys::Win32::Storage::FileSystem::DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_ADD_FILE;
use windows_sys::Win32::Storage::FileSystem::FILE_ADD_SUBDIRECTORY;
use windows_sys::Win32::Storage::FileSystem::FILE_DELETE_CHILD;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_DATA;
use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;
use windows_sys::Win32::Storage::FileSystem::WRITE_OWNER;
use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;

/// Rights over a directory that let their holder place, replace or remove an entry in it.
const CHANGE: u32 = FILE_ADD_FILE
    | FILE_ADD_SUBDIRECTORY
    | FILE_DELETE_CHILD
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | GENERIC_WRITE
    | GENERIC_ALL;

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// A security descriptor the system allocated, freed when dropped.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: the system allocated the descriptor with `LocalAlloc`, and nothing else frees
        // it.
        unsafe { LocalFree(self.0) };
    }
}

/// Replaces the access list of `path` with the one `sddl` states, inheriting nothing. A list of
/// `D:NO_ACCESS_CONTROL` leaves `path` with none at all.
fn set_access(path: &Path, sddl: &str) {
    let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut descriptor = ptr::null_mut();
    // SAFETY: `sddl` is NUL terminated, and the descriptor is written to a local.
    let parsed = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    };
    assert_ne!(parsed, 0, "{}", io::Error::last_os_error());
    let descriptor = Descriptor(descriptor);
    let (mut present, mut defaulted, mut dacl) = (0, 0, ptr::null_mut());
    // SAFETY: the descriptor is valid while `descriptor` is, and the outputs are locals.
    let read =
        unsafe { GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted) };
    assert_ne!(read, 0, "{}", io::Error::last_os_error());
    // SAFETY: the path is NUL terminated, and the list, null for none, lives in `descriptor`
    // through the call.
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null(),
        )
    };
    assert_eq!(
        status,
        ERROR_SUCCESS,
        "setting the access list of {}",
        path.display()
    );
}

/// The rights the access list of `path` allows `group` by its entries for that group alone.
fn allowed(path: &Path, group: WELL_KNOWN_SID_TYPE) -> u32 {
    let (mut dacl, mut descriptor) = (ptr::null_mut(), ptr::null_mut());
    // SAFETY: the path is NUL terminated, and the list points into the descriptor.
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    assert_eq!(
        status,
        ERROR_SUCCESS,
        "reading the access list of {}",
        path.display()
    );
    let _descriptor = Descriptor(descriptor);
    assert!(!dacl.is_null(), "{} has no access list", path.display());
    let mut rights = 0;
    // SAFETY: the list lies in the descriptor, and every entry `GetAce` yields within it.
    unsafe {
        for index in 0..u32::from((*dacl).AceCount) {
            let mut entry = ptr::null_mut();
            assert_ne!(
                GetAce(dacl, index, &mut entry),
                0,
                "{}",
                io::Error::last_os_error()
            );
            let header = *entry.cast::<ACE_HEADER>();
            if u32::from(header.AceType) != ACCESS_ALLOWED_ACE_TYPE
                || u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0
            {
                continue;
            }
            let entry = entry.cast::<ACCESS_ALLOWED_ACE>();
            if IsWellKnownSid(ptr::addr_of_mut!((*entry).SidStart).cast(), group) != 0 {
                rights |= (*entry).Mask;
            }
        }
    }
    rights
}

/// The error `prepare_directory` refuses `directory` with.
fn refusal(directory: &Path) -> io::Error {
    local_socket::prepare_directory(directory).expect_err("the directory is refused")
}

/// A directory the server creates for its sockets lets no user place one of their own there, and
/// the server takes it again at its next start.
#[test]
fn a_socket_directory_it_creates_is_closed_to_users_and_taken_again() {
    let dir = TempDir::new("lore-local-socket-");
    let sockets = dir.path().join("sockets");

    local_socket::prepare_directory(&sockets).unwrap();
    let again = local_socket::prepare_directory(&sockets);

    assert!(again.is_ok(), "{again:?}");
    assert_eq!(allowed(&sockets, WinBuiltinUsersSid) & CHANGE, 0, "users");
    assert_eq!(allowed(&sockets, WinWorldSid) & CHANGE, 0, "everyone");
}

/// A directory another account may change could hold that account's socket for clients to find.
#[test]
fn a_socket_directory_users_or_everyone_may_change_is_refused() {
    // Users, then everyone.
    for group in ["S-1-5-32-545", "S-1-1-0"] {
        let dir = TempDir::new("lore-local-socket-");
        set_access(
            dir.path(),
            &format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;OW)(A;;FW;;;{group})"),
        );

        let refused = refusal(dir.path());

        assert_eq!(
            refused.kind(),
            io::ErrorKind::PermissionDenied,
            "{group}: {refused}"
        );
    }
}

/// A directory with no access list at all lets anyone change it.
#[test]
fn a_socket_directory_without_an_access_list_is_refused() {
    let dir = TempDir::new("lore-local-socket-");
    set_access(dir.path(), "D:NO_ACCESS_CONTROL");

    let refused = refusal(dir.path());

    assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{refused}");
}

/// Connecting takes writing the socket. A socket whose mode lets others write it lets the
/// machine's users connect, and one whose mode does not leaves them what its directory hands
/// down, which in a directory the server created is nothing.
#[test]
fn a_socket_open_to_others_lets_users_connect_and_a_closed_one_does_not() {
    let dir = TempDir::new("lore-local-socket-");
    let sockets = dir.path().join("sockets");
    local_socket::prepare_directory(&sockets).unwrap();
    let listen = |name: &str, mode: u32| {
        Claim::acquire(&sockets.join(name))
            .unwrap()
            .expect("the path is unclaimed")
            .listen(mode)
            .unwrap()
    };

    let open = listen("open.sock", 0o666);
    let closed = listen("closed.sock", 0o600);

    assert_eq!(
        allowed(open.path(), WinBuiltinUsersSid) & FILE_GENERIC_WRITE,
        FILE_GENERIC_WRITE,
        "users may write the open socket"
    );
    assert_eq!(
        allowed(closed.path(), WinBuiltinUsersSid) & FILE_WRITE_DATA,
        0,
        "users may not write the closed socket"
    );
}
