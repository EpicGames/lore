// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use lore_revision::util::request_tracker::CollectStep;
use lore_revision::util::request_tracker::StepState;
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

mod collect_steps {
    use super::*;

    /// A tracker no collection has started reports no steps at all.
    #[test]
    fn no_collection_reports_none() {
        let tracker = StoreRequestTracker::default();

        assert!(tracker.collect_steps().is_none());
    }

    /// A step no collection has reached is waiting, one a collection has started and not finished
    /// is running.
    #[test]
    fn steps_not_finished_are_waiting_or_running() {
        let tracker = StoreRequestTracker::default();
        tracker.start_collection();
        tracker.start_step(CollectStep::NewFiles);

        let steps = tracker.collect_steps().expect("a collection started");
        assert_eq!(steps.state(CollectStep::NewFiles), StepState::Running);
        assert_eq!(steps.state(CollectStep::NewNames), StepState::Waiting);
        assert_eq!(steps.finished(CollectStep::NewFiles), None);
    }

    /// With two collections, a step one has finished and the other has not reached is waiting
    /// rather than finished.
    #[test]
    fn a_step_one_collection_has_not_reached_is_waiting() {
        let tracker = StoreRequestTracker::default();
        tracker.start_collection();
        tracker.start_collection();
        tracker.start_step(CollectStep::NewNames);
        tracker.finish_step(CollectStep::NewNames, Instant::now());

        let steps = tracker.collect_steps().expect("a collection started");
        assert_eq!(steps.state(CollectStep::NewNames), StepState::Waiting);
    }
}

mod expect_collections {
    use super::*;

    /// A step the collection that started has finished is waiting while a declared collection has
    /// not started.
    #[test]
    fn a_declared_collection_not_yet_started_keeps_steps_waiting() {
        let tracker = StoreRequestTracker::default();
        tracker.expect_collections(2);
        tracker.start_collection();
        tracker.start_step(CollectStep::FromNodeBlocks);
        tracker.finish_step(CollectStep::FromNodeBlocks, Instant::now());

        let steps = tracker.collect_steps().expect("a collection started");
        assert_eq!(steps.state(CollectStep::FromNodeBlocks), StepState::Waiting);

        tracker.start_collection();
        tracker.start_step(CollectStep::FromNodeBlocks);
        tracker.finish_step(CollectStep::FromNodeBlocks, Instant::now());
        let steps = tracker.collect_steps().expect("a collection started");
        let finished = steps
            .finished(CollectStep::FromNodeBlocks)
            .expect("both collections finished the step");
        assert!(finished < Duration::from_secs(60), "{finished:?}");
    }
}

mod finish_step {
    use super::*;

    /// A step reports the time from the start of the collection to its end.
    #[test]
    fn a_finished_step_reports_its_time_from_the_start() {
        let tracker = StoreRequestTracker::default();
        tracker.start_collection();
        let started = Instant::now();
        tracker.start_step(CollectStep::NewFiles);
        tracker.finish_step(CollectStep::NewFiles, started + Duration::from_millis(250));

        let steps = tracker.collect_steps().expect("a collection started");
        let new_files = steps
            .finished(CollectStep::NewFiles)
            .expect("the step finished");
        assert!(
            new_files >= Duration::from_millis(250),
            "{new_files:?} counts from the start"
        );
    }

    /// A step two collections run is running until both have finished it, and then finished at
    /// the later of their ends.
    #[test]
    fn a_step_two_collections_run_finishes_at_the_later_end() {
        let tracker = StoreRequestTracker::default();
        tracker.start_collection();
        tracker.start_collection();
        let started = Instant::now();
        tracker.start_step(CollectStep::ToState);
        tracker.start_step(CollectStep::ToState);
        tracker.finish_step(CollectStep::ToState, started + Duration::from_secs(9));

        let steps = tracker.collect_steps().expect("a collection started");
        assert_eq!(steps.state(CollectStep::ToState), StepState::Running);

        tracker.finish_step(CollectStep::ToState, started + Duration::from_secs(2));
        let steps = tracker.collect_steps().expect("a collection started");
        let to_state = steps
            .finished(CollectStep::ToState)
            .expect("both collections finished the step");
        assert!(
            to_state >= Duration::from_secs(9),
            "{to_state:?} is the later end"
        );
        assert!(
            to_state < Duration::from_secs(10),
            "{to_state:?} is the later end"
        );
    }
}

mod start_tasks {
    use super::*;

    /// The step runs until the spawner and every task added are done, whatever their order.
    #[test]
    fn the_step_finishes_with_its_last_task() {
        let tracker = Arc::new(StoreRequestTracker::default());
        tracker.start_collection();
        let tasks = tracker.start_tasks(CollectStep::FileMetadataBlobs);
        tasks.add();
        tasks.add();

        tasks.done();
        tasks.done();
        let steps = tracker.collect_steps().expect("a collection started");
        assert_eq!(
            steps.state(CollectStep::FileMetadataBlobs),
            StepState::Running
        );

        tasks.done();
        let steps = tracker.collect_steps().expect("a collection started");
        let finished = steps
            .finished(CollectStep::FileMetadataBlobs)
            .expect("every task is done");
        assert!(finished < Duration::from_secs(60), "{finished:?}");
    }
}

mod fmt {
    use super::*;

    /// The display names every step, with its milliseconds once finished and its state otherwise.
    #[test]
    fn the_display_names_every_step() {
        let tracker = StoreRequestTracker::default();
        tracker.start_collection();
        tracker.start_step(CollectStep::FromState);
        tracker.finish_step(CollectStep::FromState, Instant::now());
        tracker.start_step(CollectStep::ToState);

        let shown = tracker
            .collect_steps()
            .expect("a collection started")
            .to_string();
        let names: Vec<&str> = shown
            .split(' ')
            .map(|part| part.split_once('=').expect("name=value").0)
            .collect();
        let labels: Vec<&str> = CollectStep::ALL.iter().map(|step| step.label()).collect();
        assert_eq!(names, labels);
        let from_state = shown.split(' ').next().expect("the first step");
        let millis: u64 = from_state
            .strip_prefix("from_state=")
            .expect("from_state comes first")
            .parse()
            .expect("a finished step shows its milliseconds");
        assert!(millis < 60_000, "{shown}");
        assert!(shown.contains(" to_state=running "), "{shown}");
        assert!(shown.contains(" new_files=waiting "), "{shown}");
    }
}

mod time_step {
    use super::*;

    /// The step runs while its work runs, and is finished once it returns.
    #[tokio::test]
    async fn the_step_runs_with_its_work() {
        let tracker = StoreRequestTracker::default();
        tracker.start_collection();

        let output = tracker
            .time_step(CollectStep::NewNodeBlocks, async {
                let steps = tracker.collect_steps().expect("a collection started");
                assert_eq!(steps.state(CollectStep::NewNodeBlocks), StepState::Running);
                7
            })
            .await;

        assert_eq!(output, 7);
        let steps = tracker.collect_steps().expect("a collection started");
        let finished = steps
            .finished(CollectStep::NewNodeBlocks)
            .expect("the step finished with its work");
        assert!(finished < Duration::from_secs(60), "{finished:?}");
    }
}
