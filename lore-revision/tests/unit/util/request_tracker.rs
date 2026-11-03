// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::util::request_tracker::StoreRequestTracker;

mod track_lookup {
    use super::*;

    /// The peak is the most lookups held at once, and every tracked lookup is counted.
    #[test]
    fn the_peak_and_count_follow_the_lookups_held() {
        let tracker = StoreRequestTracker::default();
        {
            let _a = tracker.track_lookup();
            let _b = tracker.track_lookup();
            let _c = tracker.track_lookup();
        }
        let _d = tracker.track_lookup();

        assert_eq!(tracker.peak_lookups_in_flight(), 3);
        assert_eq!(tracker.lookups(), 4);
    }
}

mod track_metadata_read {
    use super::*;

    /// Metadata blob reads keep a peak and count of their own, apart from the fragment lookups.
    #[test]
    fn metadata_reads_are_counted_apart_from_lookups() {
        let tracker = StoreRequestTracker::default();
        let _lookup = tracker.track_lookup();
        {
            let _a = tracker.track_metadata_read();
            let _b = tracker.track_metadata_read();
        }
        let _c = tracker.track_metadata_read();

        assert_eq!(tracker.peak_metadata_reads_in_flight(), 2);
        assert_eq!(tracker.metadata_reads(), 3);
        assert_eq!(tracker.peak_lookups_in_flight(), 1);
        assert_eq!(tracker.lookups(), 1);
    }
}
