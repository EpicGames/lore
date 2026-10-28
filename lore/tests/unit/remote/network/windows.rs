// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore::remote::network::windows::*;
use windows_sys::Win32::Networking::WinSock;
use windows_sys::Win32::Networking::WinSock::SOCKADDR_UN;

fn written_path(addr: &SOCKADDR_UN) -> String {
    let bytes: Vec<u8> = addr
        .sun_path
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    String::from_utf8(bytes).expect("the path was written as UTF-8")
}

#[test]
fn a_path_that_fits_is_written_with_its_terminator() {
    let path = r"C:\Temp\lore_service";

    let addr = sockaddr_for_path(path).expect("a short path must fit");

    assert_eq!(addr.sun_family, WinSock::AF_UNIX);
    assert_eq!(written_path(&addr), path);
    assert_eq!(addr.sun_path[path.len()], 0, "the path must be terminated");
}

/// A temporary directory or configured socket name long enough to overrun
/// `sun_path` is reported rather than overrunning the copy.
#[test]
fn a_path_longer_than_the_address_is_refused() {
    let path = format!(r"C:\Temp\{}", "n".repeat(200));

    // `SOCKADDR_UN` has no `Debug`, so the success arm is unwrapped by hand
    // rather than through `expect_err`.
    let Err(error) = sockaddr_for_path(&path) else {
        panic!("an overlong path must be refused");
    };

    assert!(error.contains("more than"), "{error}");
    assert!(
        error.contains(&path),
        "the failure must name the path: {error}"
    );
}

/// The terminator needs a byte of its own, so the longest usable path is one
/// short of the array rather than exactly its length.
#[test]
fn a_path_that_exactly_fills_the_address_is_refused() {
    let exact = "a".repeat(SUN_PATH_CAPACITY);

    assert!(
        sockaddr_for_path(&exact).is_err(),
        "a path filling every byte leaves no room to terminate it"
    );
    assert!(
        sockaddr_for_path(&exact[1..]).is_ok(),
        "one byte shorter must fit"
    );
}

/// A socket a service answers on refuses a claim even with no lock held, as on Unix: binding
/// would delete the file it listens on and leave it running unreachable.
#[test]
fn a_socket_served_without_a_claim_refuses_a_claim() {
    let name = super::socket_name("uds-unclaimed");
    let socket = uds_sock_file(&name);
    let addr = sockaddr_for_path(socket.to_str().expect("the path is text"))
        .expect("the path fits the address");

    // Safety: Winsock calls with an address built above and a socket closed below.
    let served = unsafe {
        let mut data = WinSock::WSADATA::default();
        assert_eq!(
            WinSock::WSAStartup(2 << 8 | 2, &mut data),
            0,
            "winsock starts"
        );
        let served = WinSock::socket(WinSock::AF_UNIX as i32, WinSock::SOCK_STREAM, 0);
        assert_ne!(served, WinSock::INVALID_SOCKET, "a socket is created");
        assert_eq!(
            WinSock::bind(
                served,
                (&raw const addr).cast::<WinSock::SOCKADDR>(),
                std::mem::size_of_val(&addr) as i32,
            ),
            0,
            "binds"
        );
        assert_eq!(WinSock::listen(served, 1), 0, "listens");
        served
    };

    let refused = UdsListener::claim(&name)
        .expect("a claim is made")
        .is_none();
    // Safety: the socket created above, closed once.
    unsafe { WinSock::closesocket(served) };
    let _ = std::fs::remove_file(&socket);
    super::remove_claim_lock(&name);
    assert!(refused, "a served socket refuses a claim");
}
