// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The orphans, classified from the model by how their detached root was detached, on records made
//! by hand: a served State's are violations but those of the rename window, counted apart; an
//! unserved State's are counted.

use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::node::hooks::log::Check;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::How;
use lore_revision::state::node::hooks::log::Kind;
use lore_revision::state::node::hooks::log::Name;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;

use super::A;
use super::B;
use super::F;
use super::FREE;
use super::G;
use super::H;
use super::L;
use super::NEXT;
use super::STATE;
use super::attach;
use super::caught;
use super::clean;
use super::free;
use super::grab;
use super::initialized;
use super::moved;
use super::publish;
use super::record;
use super::replayed;
use super::section;
use super::served;
use super::snapshot;
use super::unlink;
use super::word;
use crate::harness::model::Model;
use crate::harness::model::Orphans;
use crate::harness::model::Snapshot;

fn orphans(model: &Model) -> Orphans {
    model.state(STATE).expect("the State's model").orphans()
}

/// `snapshot` with the directory `c` in `b`'s chain, in the slot past the others: a publish joins
/// below a detached root through a directory still standing there, as one into the root itself is
/// refused, the root Discarded.
fn with_c(snapshot: Snapshot) -> Snapshot {
    let mut made = snapshot;
    let block = &mut made.blocks[0];
    let mut c = record(B, "c", Kind::Directory);
    c.sibling = block.records[B as usize].child;
    block.records[B as usize].child = NEXT;
    block.records[NEXT as usize] = c;
    block.names[NEXT as usize] = Some(Name::new(b"c"));
    block.node_count = NEXT + 1;
    made
}

/// `b`, which holds `h` and `c`, detached as `how`; then the file `n` published below it, in `c`.
fn detached_then_published(how: How) -> Vec<Record> {
    [
        section(1, Section::Detach, vec![unlink(B, ROOT_NODE, how)]),
        attach(2, NEXT, "n"),
    ]
    .concat()
}

/// A root detached `IfEmpty` holds nothing a publish could join: `IfUnchanged`'s stands for both.
#[test]
fn a_publish_below_a_root_detached_as_it_stood_is_one() {
    let how = How::IfUnchanged;
    let model = replayed(&with_c(snapshot()), &detached_then_published(how));
    clean(&model);
    let found = orphans(&model);
    assert_eq!(
        found.counted,
        vec![FREE],
        "{how:?}: the file published after the detach, not h or c, which stood below it"
    );
    assert!(found.violations.is_empty(), "{:#?}", found.violations);
    let model = replayed(&with_c(served()), &detached_then_published(how));
    clean(&model);
    let found = orphans(&model);
    assert!(found.counted.is_empty());
    assert_eq!(
        found.violations.len(),
        1,
        "{how:?} on a served State: {:#?}",
        found.violations
    );
}

#[test]
fn a_node_published_before_its_roots_detach_is_none() {
    for snapshot in [snapshot(), served()] {
        let records = [
            attach(1, B, "n"),
            section(
                4,
                Section::Detach,
                vec![unlink(B, ROOT_NODE, How::IfUnchanged)],
            ),
        ]
        .concat();
        let model = replayed(&snapshot, &records);
        clean(&model);
        let found = orphans(&model);
        assert!(
            found.counted.is_empty() && found.violations.is_empty() && found.window.is_empty(),
            "served {}: {found:#?}",
            snapshot.served
        );
    }
}

#[test]
fn a_publish_below_a_root_detached_always_or_at_is_none_but_on_a_served_state() {
    for how in [How::Always, How::At] {
        let found = orphans(&replayed(
            &with_c(snapshot()),
            &detached_then_published(how),
        ));
        assert!(
            found.counted.is_empty() && found.violations.is_empty(),
            "{how:?} unserved: {found:#?}"
        );
        let found = orphans(&replayed(&with_c(served()), &detached_then_published(how)));
        assert_eq!(found.violations.len(), 1, "{how:?} served: {found:#?}");
    }
}

#[test]
fn dirty_words_below_a_root_detached_if_clean_are_orphans() {
    let dirty = Node {
        flags: record(A, "f", Kind::File).flags | NodeFlags::Dirty.bits(),
        ..record(A, "f", Kind::File)
    };
    for (snapshot, marked) in [
        (snapshot(), Section::UnservedMark),
        (served(), Section::ServedMark),
    ] {
        let records = [
            section(1, marked, vec![word(F, dirty, false)]),
            section(2, Section::Detach, vec![unlink(A, ROOT_NODE, How::IfClean)]),
        ]
        .concat();
        let model = replayed(&snapshot, &records);
        clean(&model);
        let found = orphans(&model);
        if snapshot.served {
            assert_eq!(found.violations.len(), 1, "f, not g: {found:#?}");
            assert!(found.violations[0].contains(&format!(" {F} ")));
        } else {
            assert_eq!(found.counted, vec![F], "f, not g: {found:#?}");
        }
    }
}

/// A holder that held nothing but its mover at the move has no subtree a publish can join below
/// it, the holder itself Discarded: only one that held nodes leaves an orphan, the window's.
#[test]
fn a_publish_below_a_replaced_holder_is_the_rename_windows_where_it_held_nodes() {
    // `g` moves to the root as `b`, replacing `b`, which holds `h` and `c`; `n` is then published
    // below it, in `c`.
    let window = [
        section(
            1,
            Section::Reattach,
            vec![moved(G, (A, ROOT_NODE), "b", Some(B))],
        ),
        attach(2, NEXT, "n"),
    ]
    .concat();
    let model = replayed(&with_c(served()), &window);
    clean(&model);
    let found = orphans(&model);
    assert_eq!(found.window, vec![FREE], "{found:#?}");
    assert!(found.violations.is_empty(), "{found:#?}");
    // On an unserved State, counted.
    let found = orphans(&replayed(&with_c(snapshot()), &window));
    assert_eq!(found.counted, vec![FREE], "{found:#?}");
}

/// A node joins below a holder that held nothing but its mover, or below a root detached empty,
/// only by a move no check refused: below the holder a served State's violation, not the
/// window's, its reattach caught at its record too; below the root an unserved State's, counted.
#[test]
fn a_move_below_an_emptied_root_is_an_orphan() {
    // `h` moves to the root as `b`, replacing `b`, which held `h` alone; `g` is then reattached
    // into `b`, gone, as a reattach checking no live directory would.
    let mut into_gone = moved(G, (A, B), "g", None);
    if let Effect::Move { check, .. } = &mut into_gone {
        *check = Check::Walk;
    }
    let replaced = [
        section(
            1,
            Section::Reattach,
            vec![moved(H, (B, ROOT_NODE), "b", Some(B))],
        ),
        section(2, Section::Reattach, vec![into_gone]),
    ]
    .concat();
    let model = replayed(&served(), &replaced);
    caught(&model, 2, "no live directory");
    let found = orphans(&model);
    assert!(found.window.is_empty(), "{found:#?}");
    assert_eq!(found.violations.len(), 1, "{found:#?}");
    // `b` emptied and detached `IfEmpty`, then `g` moved into it.
    let emptied = [
        section(1, Section::Detach, vec![unlink(H, B, How::Always)]),
        section(2, Section::Detach, vec![unlink(B, ROOT_NODE, How::IfEmpty)]),
        section(3, Section::MoveNamed, vec![moved(G, (A, B), "g", None)]),
    ]
    .concat();
    let model = replayed(&snapshot(), &emptied);
    clean(&model);
    assert_eq!(orphans(&model).counted, vec![G], "{:#?}", orphans(&model));
}

#[test]
fn action_bits_without_dirt_below_a_root_detached_clean_are_none() {
    let with = |flags: u16| Node {
        flags: record(A, "f", Kind::File).flags | flags,
        ..record(A, "f", Kind::File)
    };
    let action = NodeFlags::DirtyModify.bits() & !NodeFlags::Dirty.bits();
    for (snapshot, marked, flags) in [
        (
            snapshot(),
            Section::StagedModify,
            NodeFlags::StagedModify.bits(),
        ),
        (served(), Section::ServedMark, action),
    ] {
        let records = [
            section(1, marked, vec![word(F, with(flags), false)]),
            section(2, Section::Detach, vec![unlink(A, ROOT_NODE, How::IfClean)]),
        ]
        .concat();
        let model = replayed(&snapshot, &records);
        clean(&model);
        let found = orphans(&model);
        assert!(
            found.counted.is_empty() && found.violations.is_empty(),
            "served {}: {found:#?}",
            snapshot.served
        );
    }
}

#[test]
fn a_node_moved_below_a_detached_root_after_the_detach_is_one() {
    for (snapshot, moving) in [
        (snapshot(), Section::MoveNamed),
        (served(), Section::Reattach),
    ] {
        let records = [
            section(
                1,
                Section::Detach,
                vec![unlink(B, ROOT_NODE, How::IfUnchanged)],
            ),
            section(2, moving, vec![moved(F, (A, B), "f", None)]),
        ]
        .concat();
        let model = replayed(&snapshot, &records);
        clean(&model);
        let found = orphans(&model);
        if snapshot.served {
            assert_eq!(found.violations.len(), 1, "{found:#?}");
        } else {
            assert_eq!(found.counted, vec![F], "{found:#?}");
        }
    }
}

/// `snapshot` with `b` out of the root's chain, `Discarded`: detached before the model starts.
fn detached_at_start(snapshot: Snapshot) -> Snapshot {
    let mut detached = snapshot;
    let block = &mut detached.blocks[0];
    block.records[L as usize].sibling = A;
    block.records[B as usize].flags |= NodeFlags::Discarded.bits();
    detached
}

#[test]
fn a_node_joining_below_a_root_detached_before_the_start_is_one_on_a_served_state() {
    let records = attach(1, NEXT, "n");
    let model = replayed(&detached_at_start(with_c(served())), &records);
    clean(&model);
    let found = orphans(&model);
    assert_eq!(found.violations.len(), 1, "{found:#?}");
    let found = orphans(&replayed(&detached_at_start(with_c(snapshot())), &records));
    assert!(
        found.counted.is_empty() && found.violations.is_empty(),
        "how it was detached is not known: {found:#?}"
    );
}

#[test]
fn what_stands_below_a_root_a_discard_left_partly_freed_is_counted() {
    let records = section(
        1,
        Section::DiscardMarks,
        vec![unlink(A, ROOT_NODE, How::Discard), free(F)],
    );
    let model = replayed(&snapshot(), &records);
    clean(&model);
    let found = orphans(&model);
    assert_eq!(found.counted, vec![G], "{found:#?}");
}

#[test]
fn nothing_below_a_pending_root_is_an_orphan() {
    // `u`, a directory below `a`; `a` deleted through the C API, detached and queued, `f` and `g`
    // marked; then `n` published below `u`, which the marks have not reached, as a staged add may.
    let records = [
        section(1, Section::Grab, vec![grab(FREE)]),
        section(
            2,
            Section::Publish,
            vec![publish(FREE, A, "u", Kind::Directory)],
        ),
        section(
            3,
            Section::DiscardMarks,
            vec![
                unlink(A, ROOT_NODE, How::At),
                Effect::Pend { root: A },
                Effect::Discarding { node: F },
                Effect::Discarding { node: G },
            ],
        ),
        section(4, Section::Grab, vec![grab(NEXT)]),
        section(
            5,
            Section::Grab,
            vec![word(
                NEXT,
                initialized(record(FREE, "n", Kind::File)),
                false,
            )],
        ),
        section(
            6,
            Section::Publish,
            vec![
                publish(NEXT, FREE, "n", Kind::File),
                word(NEXT, record(FREE, "n", Kind::File), false),
            ],
        ),
    ]
    .concat();
    let model = replayed(&snapshot(), &records);
    clean(&model);
    let found = orphans(&model);
    assert!(
        found.counted.is_empty() && found.violations.is_empty(),
        "{found:#?}"
    );
}
