// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::util::request_tracker::StoreRequestTracker;

mod track {
    use super::*;

    /// The peak is the most guards held at once, and every track counts as a request.
    #[test]
    fn the_peak_and_request_count_follow_the_guards_held() {
        let tracker = StoreRequestTracker::default();
        {
            let _a = tracker.track();
            let _b = tracker.track();
            let _c = tracker.track();
        }
        let _d = tracker.track();

        assert_eq!(tracker.peak_in_flight(), 3);
        assert_eq!(tracker.requests(), 4);
    }
}
