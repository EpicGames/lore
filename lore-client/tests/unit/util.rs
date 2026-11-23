// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod termination_signal_tests {
    #[cfg(unix)]
    use std::time::Duration;

    #[cfg(unix)]
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

mod rfc2822_display_tests {
    use lore_client::util::rfc2822_display;

    #[test]
    fn a_timestamp_displays_as_rfc2822_in_utc() {
        assert_eq!(
            rfc2822_display(1_795_351_520_303).as_deref(),
            Some("Sun, 22 Nov 2026 12:45:20 +0000")
        );
    }

    /// The timestamp is read as signed, so the largest one is a millisecond before the epoch.
    #[test]
    fn a_timestamp_past_the_signed_range_displays_before_the_epoch() {
        assert_eq!(
            rfc2822_display(u64::MAX).as_deref(),
            Some("Wed, 31 Dec 1969 23:59:59 +0000")
        );
    }

    #[test]
    fn a_timestamp_past_the_year_9999_displays_nothing() {
        assert_eq!(rfc2822_display(253_402_300_800_000), None);
        assert_eq!(rfc2822_display(1 << 63), None);
    }
}
