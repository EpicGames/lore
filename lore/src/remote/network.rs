// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_error_set::prelude::*;

// Reexport the unimplemented OS generic module
#[cfg(not(any(target_os = "windows", target_family = "unix")))]
mod stub;

#[cfg(not(any(target_os = "windows", target_family = "unix")))]
mod os_specific {
    pub use super::stub::UdsListener;
    pub use super::stub::UdsStream;
    pub use super::stub::uds_supported;
}

// Reexport the unix specific module
#[cfg(target_family = "unix")]
mod unix;
#[cfg(target_family = "unix")]
mod os_specific {
    pub use super::unix::UdsListener;
    pub use super::unix::UdsStream;
    pub use super::unix::uds_supported;
}

// Reexport the windows specific module
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
mod os_specific {
    pub use super::windows::UdsListener;
    pub use super::windows::UdsStream;
    pub use super::windows::uds_supported;
}

// Reexport everything from the private OS specific networking module
pub use os_specific::*;

#[error_set]
pub enum UdsListenerError {}

#[error_set]
pub enum UdsAcceptError {}

#[error_set]
pub enum UdsConnectionError {}

// The transport is exercised here rather than in the per-OS modules so that
// every backend is covered by the same test.
#[cfg(all(test, any(target_os = "windows", target_family = "unix")))]
mod tests {
    use std::io::Read;
    use std::io::Write;
    use std::sync::mpsc::Sender;
    use std::thread::sleep;
    use std::time::Duration;

    use super::*;

    const TEST_STRING: &str = "ABC";

    fn run_service(ready_signal: Sender<()>) -> String {
        let listener = UdsListener::new().unwrap();

        ready_signal.send(()).unwrap();

        let mut stream = listener.accept().unwrap();

        let mut buf = Vec::new();
        stream.reader().read_to_end(&mut buf).unwrap();
        let result = str::from_utf8(&buf).unwrap();
        println!("RECEIVED: {result}");

        result.to_string()
    }

    fn run_client() {
        let mut conn = UdsStream::connect().unwrap();
        conn.writer().write_all(TEST_STRING.as_bytes()).unwrap();
    }

    fn run_both() -> String {
        let (sender, receiver) = std::sync::mpsc::channel::<()>();
        let service = std::thread::spawn(move || run_service(sender));
        receiver.recv().unwrap();
        sleep(Duration::from_secs(1));
        let client = std::thread::spawn(move || {
            run_client();
        });
        let result = service.join().unwrap();
        client.join().unwrap();
        result
    }

    #[test]
    fn test_both() {
        assert_eq!(run_both(), TEST_STRING.to_string());
    }
}
