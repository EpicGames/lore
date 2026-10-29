// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_server::quic::initial_cwnd::InitialCwndBytes;

mod initial_cwnd_bytes_try_from {
    use super::*;

    #[test]
    fn minimum_is_accepted() {
        let initial_cwnd =
            InitialCwndBytes::try_from(InitialCwndBytes::MIN).expect("the minimum is valid");

        assert_eq!(initial_cwnd.get(), InitialCwndBytes::MIN);
    }

    #[test]
    fn below_minimum_is_rejected() {
        InitialCwndBytes::try_from(InitialCwndBytes::MIN - 1).expect_err("below the minimum");
    }
}
