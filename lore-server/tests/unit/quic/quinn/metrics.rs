// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_server::quic::quinn::metrics::*;

mod packet_tally {
    use super::*;

    #[test]
    fn counts_only_what_was_not_counted_before() {
        let tally = PacketTally::default();

        assert_eq!(tally.advance(2, 100), (2, 100));
        assert_eq!(tally.advance(5, 150), (3, 50));
    }

    #[test]
    fn older_totals_count_nothing() {
        let tally = PacketTally::default();
        tally.advance(5, 150);

        assert_eq!(tally.advance(4, 120), (0, 0));
        assert_eq!(tally.advance(6, 160), (1, 10));
    }
}
