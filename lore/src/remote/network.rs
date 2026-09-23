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
    use std::time::Duration;
    use std::time::Instant;

    use super::*;

    const TEST_STRING: &str = "ABC";

    /// A socket name unique to one test and one run of this binary.
    ///
    /// Kept short because the name joins the temporary directory into an address
    /// that holds 108 bytes. The process suffix keeps two concurrent runs off a
    /// single path, which the Windows listener unlinks without checking for a
    /// live peer.
    fn socket_name(test: &str) -> String {
        format!("{test}-{}", std::process::id())
    }

    fn run_service(name: &str, ready_signal: Sender<()>) -> String {
        let listener = UdsListener::new(name).unwrap();

        ready_signal.send(()).unwrap();

        let mut stream = listener.accept().unwrap();

        let mut buf = Vec::new();
        stream.reader().read_to_end(&mut buf).unwrap();
        let result = str::from_utf8(&buf).unwrap();
        println!("RECEIVED: {result}");

        result.to_string()
    }

    fn run_client(name: &str) {
        let mut conn = UdsStream::connect(name).unwrap();
        conn.writer().write_all(TEST_STRING.as_bytes()).unwrap();
    }

    fn run_both(name: &str) -> String {
        let (sender, receiver) = std::sync::mpsc::channel::<()>();
        // Scoped threads, so both borrow `name` rather than requiring it to be
        // `'static` as `std::thread::spawn` would.
        std::thread::scope(|scope| {
            let service = scope.spawn(move || run_service(name, sender));
            // The signal arrives after the listener is bound and listening, so
            // the backlog already holds the connect below. Nothing to wait for.
            receiver.recv().unwrap();
            let client = scope.spawn(move || {
                run_client(name);
            });
            let result = service.join().unwrap();
            client.join().unwrap();
            result
        })
    }

    #[test]
    fn test_both() {
        assert_eq!(run_both(&socket_name("uds-both")), TEST_STRING.to_string());
    }

    /// A connect with nothing listening reports it rather than waiting.
    ///
    /// The start and stop waits give up after ten seconds and retry around this
    /// call. A connect that waits for a service itself would use one of those
    /// whole waits on a single attempt.
    #[test]
    fn connecting_with_nothing_listening_reports_it_rather_than_waiting() {
        let name = socket_name("uds-unlistened");
        let started = Instant::now();

        let result = UdsStream::connect(&name);

        assert!(result.is_err(), "nothing is listening on {name}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the connect took {:?}, so it waited rather than reporting",
            started.elapsed()
        );
    }
}
