// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_transport::quic::slow_response::*;

mod report_rate_limit {
    use super::*;

    fn admitted_in(limit: &ReportRateLimit, second: u32, attempts: u32) -> u32 {
        (0..attempts)
            .filter(|_| limit.admit_in_second(second))
            .count() as u32
    }

    #[test]
    fn admits_up_to_the_cap_in_one_second() {
        let limit = ReportRateLimit::new();

        assert_eq!(
            admitted_in(&limit, 1, MAX_REPORTS_PER_SECOND * 3),
            MAX_REPORTS_PER_SECOND
        );
    }

    #[test]
    fn a_later_second_admits_again() {
        let limit = ReportRateLimit::new();
        admitted_in(&limit, 1, MAX_REPORTS_PER_SECOND);

        assert_eq!(
            admitted_in(&limit, 2, MAX_REPORTS_PER_SECOND * 2),
            MAX_REPORTS_PER_SECOND
        );
    }

    #[test]
    fn an_earlier_second_counts_against_the_current_one() {
        let limit = ReportRateLimit::new();
        assert!(limit.admit_in_second(5));

        assert_eq!(
            admitted_in(&limit, 4, MAX_REPORTS_PER_SECOND * 2),
            MAX_REPORTS_PER_SECOND - 1
        );
        assert!(!limit.admit_in_second(5));
    }

    #[test]
    fn admits_now() {
        assert!(ReportRateLimit::new().admit());
    }
}
