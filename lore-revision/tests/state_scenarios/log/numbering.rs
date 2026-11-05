// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The log's numbering and its guards (`hooks::log`), on a recording scheduler of no task: a
//! section's `(seq, sub)`, a read outside every section, a label, a section of several locks, and
//! the panics of a read or a point inside a section.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use futures::executor::block_on;
use lore_base::error::NodeNotFound;
use lore_revision::state::StateError;
use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::Point;
use lore_revision::state::node::hooks::Resource;
use lore_revision::state::node::hooks::Scheduler;
use lore_revision::state::node::hooks::StateHooks;
use lore_revision::state::node::hooks::install;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Name;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;
use lore_revision::state::node::hooks::log::build;
use lore_revision::state::node::hooks::log::head;
use lore_revision::state::node::hooks::log::hop;
use lore_revision::state::node::hooks::log::idle;
use lore_revision::state::node::hooks::log::in_multi;
use lore_revision::state::node::hooks::log::label;
use lore_revision::state::node::hooks::log::multi;
use lore_revision::state::node::hooks::log::named;
use lore_revision::state::node::hooks::log::open;
use lore_revision::state::node::hooks::log::parent;
use lore_revision::state::node::hooks::log::record;
use lore_revision::state::node::hooks::log::recording;

/// Takes every record while `on`.
struct Recorder {
    on: AtomicBool,
    records: Mutex<Vec<Record>>,
}

impl Scheduler for Recorder {
    fn reached(&self, _point: Point) -> Option<Pin<Box<dyn Future<Output = ()> + Send>>> {
        None
    }

    fn touched(&self, _resource: Resource, _write: bool) {}

    fn recording(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    fn effect(&self, record: Record) {
        self.records.lock().unwrap().push(record);
    }
}

fn recorder(on: bool) -> Arc<Recorder> {
    Arc::new(Recorder {
        on: AtomicBool::new(on),
        records: Mutex::new(Vec::new()),
    })
}

/// Each record's `(seq, sub)`, section and effect.
fn taken(recorder: &Recorder) -> Vec<(u64, u32, Section, Effect)> {
    recorder
        .records
        .lock()
        .unwrap()
        .iter()
        .map(|record| {
            (
                record.seq,
                record.sub,
                record.section,
                record.effect.clone(),
            )
        })
        .collect()
}

#[test]
fn a_section_numbers_its_records_in_order_and_one_outside_takes_its_own_seq() {
    let recorder = recorder(true);
    let _installed = install(recorder.clone());
    let state = StateHooks::new();
    {
        let _section = open(&state);
        record(&state, Effect::Permit);
        record(&state, Effect::Unpermit);
    }
    record(&state, Effect::Served);
    assert_eq!(
        taken(&recorder),
        [
            (0, 0, Section::Unlabelled, Effect::Permit),
            (0, 1, Section::Unlabelled, Effect::Unpermit),
            (1, 0, Section::Unlabelled, Effect::Served),
        ]
    );
    assert_eq!(state.seq_now(), 2);
}

#[test]
fn a_nested_section_of_another_state_takes_only_that_states_records() {
    let recorder = recorder(true);
    let _installed = install(recorder.clone());
    let state = StateHooks::new();
    let other = StateHooks::new();
    let third = StateHooks::new();
    {
        let _outer = open(&state);
        record(&state, Effect::Permit);
        {
            let _inner = open(&other);
            record(&state, Effect::Served);
            record(&other, Effect::Served);
            record(&other, Effect::Permit);
            // A State with no section open takes an order of its own.
            record(&third, Effect::Served);
        }
        record(&state, Effect::Unpermit);
    }
    let records = recorder.records.lock().unwrap().clone();
    let numbered: Vec<_> = records
        .iter()
        .map(|record| (record.state, record.seq, record.sub))
        .collect();
    assert_eq!(
        numbered,
        [
            (state.id(), 0, 0),
            (state.id(), 0, 1),
            (other.id(), 0, 0),
            (other.id(), 0, 1),
            (third.id(), 0, 0),
            (state.id(), 0, 2),
        ]
    );
}

#[test]
#[should_panic(expected = "two sections of one State open on a thread")]
fn two_sections_of_one_state_at_once_panic() {
    let recorder = recorder(true);
    let _installed = install(recorder.clone());
    let state = StateHooks::new();
    let _outer = open(&state);
    let _inner = open(&state);
}

#[test]
#[should_panic(expected = "head inside a section of its State")]
fn a_head_read_inside_a_section_of_its_state_panics() {
    let _installed = install(recorder(true));
    let state = StateHooks::new();
    let _section = open(&state);
    head(&state, 1, 0, 1);
}

#[test]
#[should_panic(expected = "parent inside a section of its State")]
fn a_parent_read_inside_a_section_of_its_state_panics() {
    let _installed = install(recorder(true));
    let state = StateHooks::new();
    let _section = open(&state);
    parent(&state, 1, 0);
}

#[test]
#[should_panic(expected = "name inside a section of its State")]
fn a_name_read_inside_a_section_of_its_state_panics() {
    let _installed = install(recorder(true));
    let state = StateHooks::new();
    let _section = open(&state);
    named(&state, 1, "f");
}

#[test]
#[should_panic(expected = "child inside a section of its State")]
fn a_hop_inside_a_section_of_its_state_panics() {
    let _installed = install(recorder(true));
    let state = StateHooks::new();
    let _section = open(&state);
    hop(&state, (1, 7), &Ok(2));
}

#[test]
fn a_hop_records_its_find_or_its_miss_and_no_other_failure() {
    let recorder = recorder(true);
    let _installed = install(recorder.clone());
    let state = StateHooks::new();
    hop(&state, (1, 7), &Ok(2));
    hop(&state, (2, 8), &Err(StateError::from(NodeNotFound)));
    hop(&state, (2, 9), &Err(StateError::internal("a read failed")));
    let found = |dir, name, found| Effect::Hop { dir, name, found };
    assert_eq!(
        taken(&recorder),
        [
            (0, 0, Section::Unlabelled, found(1, 7, Some(2))),
            (1, 0, Section::Unlabelled, found(2, 8, None)),
        ]
    );
}

#[test]
fn a_parent_or_a_name_read_outside_every_section_takes_its_own_seq() {
    let recorder = recorder(true);
    let _installed = install(recorder.clone());
    let state = StateHooks::new();
    parent(&state, 2, 1);
    named(&state, 2, "f");
    assert_eq!(
        taken(&recorder),
        [
            (
                0,
                0,
                Section::Unlabelled,
                Effect::Parent { node: 2, parent: 1 }
            ),
            (
                1,
                0,
                Section::Unlabelled,
                Effect::Named {
                    node: 2,
                    name: Name::new(b"f")
                }
            ),
        ]
    );
}

#[test]
#[should_panic(expected = "inside a labelled or open section")]
fn a_point_reached_under_a_label_panics() {
    let _installed = install(recorder(false));
    let _label = label(Section::Detach);
    block_on(hooks::reached(Point::Named("x")));
}

#[test]
#[should_panic(expected = "inside a labelled or open section")]
fn a_point_reached_in_an_open_section_panics() {
    let _installed = install(recorder(true));
    let state = StateHooks::new();
    let _section = open(&state);
    block_on(hooks::reached(Point::Named("x")));
}

#[test]
#[should_panic(expected = "inside a labelled or open section")]
fn a_point_reached_while_a_section_takes_its_locks_panics() {
    let _installed = install(recorder(false));
    let _multi = multi();
    block_on(hooks::reached(Point::Named("x")));
}

/// A future that suspends once, waking itself, then ends.
async fn suspend_once() {
    let mut suspended = false;
    std::future::poll_fn(move |context| {
        if suspended {
            return std::task::Poll::Ready(());
        }
        suspended = true;
        context.waker().wake_by_ref();
        std::task::Poll::Pending
    })
    .await;
}

#[test]
#[should_panic(expected = "a task suspended inside a labelled or open section")]
fn a_task_suspended_under_a_label_panics() {
    block_on(hooks::as_task(0, async {
        let _label = label(Section::Detach);
        suspend_once().await;
    }));
}

#[test]
fn a_task_suspended_outside_every_section_goes_on() {
    block_on(hooks::as_task(0, async {
        {
            let _label = label(Section::Detach);
        }
        suspend_once().await;
    }));
}

#[test]
#[should_panic(expected = "a task suspended inside a labelled or open section")]
// The task answers the spawned future unpolled, as a spawn site makes it inside its task.
#[allow(clippy::async_yields_async)]
fn a_spawned_task_suspended_under_a_label_panics() {
    let _installed = install(recorder(false));
    // Made inside a task, as a spawn site makes it, and polled outside it.
    let spawned = block_on(hooks::as_task(0, async {
        hooks::inherit(async {
            let _label = label(Section::Detach);
            suspend_once().await;
        })
    }));
    block_on(spawned);
}

/// A future of a kilobyte.
fn large() -> impl Future<Output = ()> {
    let bytes = [7u8; 1024];
    async move {
        std::hint::black_box(&bytes);
    }
}

#[test]
fn a_task_and_a_spawned_one_hold_their_future_once() {
    let size = std::mem::size_of_val(&large());
    let task = hooks::as_task(0, large());
    let spawned = hooks::inherit(large());
    assert!(
        std::mem::size_of_val(&task) < size + 128,
        "a task's future of {} bytes for one of {size}",
        std::mem::size_of_val(&task)
    );
    assert!(
        std::mem::size_of_val(&spawned) < size + 128,
        "a spawned future of {} bytes for one of {size}",
        std::mem::size_of_val(&spawned)
    );
}

#[test]
#[should_panic(expected = "a task suspended inside a labelled or open section")]
// The task answers the spawned future unpolled, as a spawn site makes it inside its task.
#[allow(clippy::async_yields_async)]
fn a_task_spawned_from_a_thread_with_no_scheduler_is_checked() {
    let spawned = block_on(hooks::as_task(0, async {
        hooks::inherit(async {
            let _label = label(Section::Detach);
            suspend_once().await;
        })
    }));
    block_on(spawned);
}

#[test]
// The task answers the spawned future unpolled, as a spawn site makes it inside its task.
#[allow(clippy::async_yields_async)]
fn a_task_spawned_from_a_thread_with_no_scheduler_keeps_its_task() {
    let spawned = block_on(hooks::as_task(3, async {
        hooks::inherit(async { hooks::task() })
    }));
    assert_eq!(block_on(spawned), Some(3));
}

#[test]
fn a_builds_publish_is_not_idle() {
    let building = build();
    assert!(!idle());
    drop(building);
    assert!(idle());
}

#[test]
fn a_section_of_several_locks_takes_one_seq_after_its_last_lock() {
    let recorder = recorder(true);
    let _installed = install(recorder.clone());
    let state = StateHooks::new();
    let multi = multi();
    assert!(in_multi());
    // A record before the last lock takes a seq of its own; the section's comes after it.
    record(&state, Effect::Served);
    let section = multi.opened(Some(&state));
    assert!(!in_multi());
    record(&state, Effect::Permit);
    record(&state, Effect::Unpermit);
    drop(section);
    assert_eq!(
        taken(&recorder),
        [
            (0, 0, Section::Unlabelled, Effect::Served),
            (1, 0, Section::Unlabelled, Effect::Permit),
            (1, 1, Section::Unlabelled, Effect::Unpermit),
        ]
    );
}

#[test]
fn a_section_of_several_locks_dropped_before_its_last_lock_leaves_none_pending() {
    let multi = multi();
    assert!(in_multi());
    drop(multi);
    assert!(!in_multi());
    assert!(idle());
}

#[test]
fn a_label_names_the_records_and_restores_the_one_before() {
    let recorder = recorder(true);
    let _installed = install(recorder.clone());
    let state = StateHooks::new();
    {
        let _outer = label(Section::Detach);
        assert!(!idle());
        {
            let _inner = label(Section::Publish);
            record(&state, Effect::Permit);
        }
        record(&state, Effect::Unpermit);
    }
    assert!(idle());
    record(&state, Effect::Served);
    let sections: Vec<_> = taken(&recorder)
        .into_iter()
        .map(|(_, _, section, _)| section)
        .collect();
    assert_eq!(
        sections,
        [Section::Publish, Section::Detach, Section::Unlabelled]
    );
}

#[test]
fn nothing_is_numbered_or_recorded_while_the_scheduler_does_not_record() {
    let recorder = recorder(false);
    let _installed = install(recorder.clone());
    let state = StateHooks::new();
    assert!(!recording());
    assert!(open(&state).is_none());
    record(&state, Effect::Served);
    assert!(recorder.records.lock().unwrap().is_empty());
    assert_eq!(state.seq_now(), 0);
    // And from the moment it records, the order starts.
    recorder.on.store(true, Ordering::Relaxed);
    assert!(open(&state).is_some());
    assert_eq!(state.seq_now(), 1);
}

#[test]
fn nothing_is_recorded_with_no_scheduler_installed() {
    let state = StateHooks::new();
    assert!(open(&state).is_none());
    record(&state, Effect::Served);
    assert_eq!(state.seq_now(), 0);
}

#[test]
fn a_section_closed_out_of_order_leaves_the_others_open() {
    let recorder = recorder(true);
    let _installed = install(recorder.clone());
    let state = StateHooks::new();
    let other = StateHooks::new();
    let first = open(&state);
    let second = open(&other);
    drop(first);
    // The State's record now takes a seq of its own: its section closed.
    record(&state, Effect::Served);
    // The other's section, opened after it, still numbers its records.
    record(&other, Effect::Permit);
    record(&other, Effect::Unpermit);
    drop(second);
    assert!(idle());
    assert_eq!(
        taken(&recorder),
        [
            (1, 0, Section::Unlabelled, Effect::Served),
            (0, 0, Section::Unlabelled, Effect::Permit),
            (0, 1, Section::Unlabelled, Effect::Unpermit),
        ]
    );
}
