// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod termination_signal_tests {
    use std::time::Duration;

    use lore_client::util::TerminationSignals;

    /// What registering ahead of the socket relies on: the handler is what holds
    /// a signal, so one delivered between registering and waiting reaches the
    /// wait rather than ending the process.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_signal_delivered_before_the_wait_still_ends_it() {
        let mut signals = TerminationSignals::register().expect("the handlers must register");

        // Safety: raises a signal in this process, which the handler registered
        // above now holds rather than the default disposition that would end it.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);

        tokio::time::timeout(Duration::from_secs(5), signals.recv())
            .await
            .expect("a signal delivered before the wait must still end it");
    }
}
