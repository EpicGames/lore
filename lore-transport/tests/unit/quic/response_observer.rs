// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;
use std::time::Instant;

use lore_transport::quic::response_observer::*;

mod observed_response_new {
    use super::*;

    #[test]
    fn splits_the_wait_at_the_write_and_the_header() {
        let queued = Instant::now();
        let written = queued + Duration::from_millis(5);
        let header = written + Duration::from_millis(30);
        let last_byte = header + Duration::from_millis(12);

        let response =
            ObservedResponse::new(4, None, 60_000, 2, queued, written, header, last_byte);

        assert_eq!(response.queue_to_written, Duration::from_millis(5));
        assert_eq!(response.to_header, Duration::from_millis(30));
        assert_eq!(response.header_to_last_byte, Duration::from_millis(12));
    }

    #[test]
    fn a_header_dated_before_the_write_returned_counts_as_no_wait() {
        let header = Instant::now();
        let written = header + Duration::from_millis(1);

        let response = ObservedResponse::new(4, Some(7), 0, 0, written, written, header, written);

        assert_eq!(response.to_header, Duration::ZERO);
        assert_eq!(response.error_status, Some(7));
    }
}
