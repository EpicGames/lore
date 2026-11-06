// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::fs::swfs::callback_counters::CallbackCounters;

#[test]
fn a_new_counter_has_seen_nothing() {
    let counters = CallbackCounters::default();

    let delta = counters.since(counters.mark());

    assert_eq!(delta.read_file, 0);
    assert_eq!(delta.directory_evaluations, 0);
}

#[test]
fn the_delta_counts_only_what_came_after_the_mark() {
    let counters = CallbackCounters::default();
    counters.record_read_file();
    counters.record_directory_evaluation();

    let mark = counters.mark();
    counters.record_read_file();
    counters.record_read_file();
    counters.record_directory_evaluation();

    let delta = counters.since(mark);

    assert_eq!(delta.read_file, 2);
    assert_eq!(delta.directory_evaluations, 1);
}

/// The smoke tests read this line from a command's debug output, so its wording is a contract.
#[test]
fn the_delta_reads_as_the_line_smoke_tests_parse() {
    let counters = CallbackCounters::default();
    let mark = counters.mark();
    counters.record_read_file();
    counters.record_read_file();
    counters.record_directory_evaluation();

    assert_eq!(
        counters.since(mark).to_string(),
        "read_file callbacks +2, directory evaluations +1"
    );
}

#[test]
fn counting_is_safe_from_many_threads() {
    let counters = std::sync::Arc::new(CallbackCounters::default());
    let mark = counters.mark();
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let counters = counters.clone();
            std::thread::spawn(move || {
                for _ in 0..1000 {
                    counters.record_read_file();
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }

    assert_eq!(counters.since(mark).read_file, 8000);
}
