// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Each of the model's replay and count rules, on records made by hand over the State made by hand:
//! the legal record applied as the code applies it, and the record each rule refuses caught where
//! it stands.

use lore_base::types::Hash;
use lore_revision::node::BLOCK_NODE_COUNT;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::REATTACH_CHANGES;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::GuardEvent;
use lore_revision::state::node::hooks::log::How;
use lore_revision::state::node::hooks::log::Kind;
use lore_revision::state::node::hooks::log::Name;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;
use lore_storage::hash::hash_string;

use super::A;
use super::B;
use super::F;
use super::FREE;
use super::G;
use super::H;
use super::INSTANCE;
use super::L;
use super::NEXT;
use super::PINNED;
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
use crate::harness::model::BlockSnapshot;
use crate::harness::model::Model;
use crate::harness::model::Snapshot;
use crate::harness::model::Status;
use crate::harness::model::Unchained;

const R: u32 = REATTACH_CHANGES;

fn rename(node: NodeID, parent: NodeID, name_hash: u64, name: &str) -> Effect {
    Effect::Rename {
        node,
        parent,
        name_hash,
        name: Name::new(name.as_bytes()),
        instance: INSTANCE,
    }
}

fn build_add(node: NodeID, parent: NodeID, name: &str) -> Effect {
    Effect::BuildAdd {
        node,
        parent,
        name_hash: hash_string(name),
        name: Name::new(name.as_bytes()),
        kind: Kind::File,
    }
}

/// A build's records from `seq` on: the free slot grabbed as the directory `r` below `a`, the
/// next slot as the file `n` below it, added into it.
fn build(seq: u64) -> Vec<Record> {
    let root = record(A, "r", Kind::Directory);
    let added = record(FREE, "n", Kind::File);
    [
        section(seq, Section::Grab, vec![grab(FREE)]),
        section(
            seq + 1,
            Section::Grab,
            vec![word(FREE, initialized(root), false)],
        ),
        section(seq + 2, Section::Grab, vec![grab(NEXT)]),
        section(
            seq + 3,
            Section::Grab,
            vec![word(NEXT, initialized(added), false)],
        ),
        section(
            seq + 4,
            Section::BuildAdd,
            vec![build_add(NEXT, FREE, "n"), word(NEXT, added, false)],
        ),
    ]
    .concat()
}

fn status(model: &Model, node: NodeID) -> Status {
    model
        .state(STATE)
        .and_then(|state| state.slot(node))
        .map(|slot| slot.status)
        .expect("the model holds the node")
}

fn changes(model: &Model, node: NodeID) -> u32 {
    model
        .state(STATE)
        .and_then(|state| state.slot(node))
        .map(|slot| slot.changes)
        .expect("the model holds the node")
}

fn chain(model: &Model, directory: NodeID) -> Vec<NodeID> {
    model
        .state(STATE)
        .expect("the State's model")
        .chain(directory)
}

/// Block 0's `node_count` and free list, its head first.
fn slots(model: &Model) -> (u32, Vec<u32>) {
    let block = model
        .state(STATE)
        .and_then(|state| state.block(0))
        .expect("block 0's model");
    (block.node_count, block.free.iter().rev().copied().collect())
}

#[test]
fn the_start_reads_chains_and_free_slots() {
    let model = replayed(&snapshot(), &[]);
    clean(&model);
    assert_eq!(chain(&model, ROOT_NODE), vec![L, B, A]);
    assert_eq!(chain(&model, A), vec![G, F]);
    assert_eq!(chain(&model, B), vec![H]);
    assert_eq!(status(&model, FREE), Status::Free);
    assert_eq!(status(&model, NEXT), Status::Free);
    assert_eq!(slots(&model), (NEXT, vec![FREE]));
}

#[test]
fn a_grab_takes_the_free_head_then_the_next_slot() {
    let records = [
        section(1, Section::Grab, vec![grab(FREE)]),
        section(2, Section::Grab, vec![grab(NEXT)]),
    ]
    .concat();
    let model = replayed(&snapshot(), &records);
    clean(&model);
    assert_eq!(slots(&model), (NEXT + 1, vec![]));
    assert_eq!(status(&model, FREE), Status::InFlight);
    assert_eq!(status(&model, NEXT), Status::InFlight);
}

#[test]
fn a_grab_past_the_free_head_is_caught() {
    let model = replayed(&snapshot(), &section(1, Section::Grab, vec![grab(NEXT)]));
    caught(&model, 1, "where the block gives slot 7");
}

#[test]
fn a_grab_of_a_used_slot_is_caught() {
    let model = replayed(&snapshot(), &section(1, Section::Grab, vec![grab(F)]));
    caught(&model, 1, "which is not free");
}

#[test]
fn an_attach_publishes_at_the_head_and_counts_one() {
    let model = replayed(&snapshot(), &attach(1, A, "n"));
    clean(&model);
    assert_eq!(chain(&model, A), vec![FREE, G, F]);
    assert_eq!(changes(&model, A), 1);
    assert_eq!(status(&model, FREE), Status::Chained);
    let slot = model
        .state(STATE)
        .and_then(|state| state.slot(FREE))
        .expect("the slot");
    assert_eq!(slot.name.as_ref().map(Name::as_bytes), Some(&b"n"[..]));
    assert!(slot.published);
}

#[test]
fn a_publish_of_a_node_not_in_flight_is_caught() {
    let model = replayed(
        &snapshot(),
        &section(1, Section::Publish, vec![publish(F, B, "f", Kind::File)]),
    );
    caught(&model, 1, "not in flight");
}

#[test]
fn a_publish_below_another_parent_than_its_initialization_is_caught() {
    let mut records = attach(1, A, "n");
    let Effect::Publish { parent, .. } = &mut records[2].effect else {
        panic!("the attach's publish");
    };
    *parent = B;
    let model = replayed(&snapshot(), &records);
    caught(&model, 3, "initialized below");
}

#[test]
fn a_directorys_publish_tells_what_no_word_of_its_initialization_did() {
    let records = [
        section(1, Section::Grab, vec![grab(FREE)]),
        section(
            2,
            Section::Publish,
            vec![publish(FREE, A, "r", Kind::Directory)],
        ),
    ]
    .concat();
    let model = replayed(&snapshot(), &records);
    clean(&model);
    let published = super::record_of(&model, FREE);
    assert_eq!(
        (published.parent, published.name_hash),
        (A, hash_string("r"))
    );
    assert!(
        model
            .state(STATE)
            .and_then(|state| state.slot(FREE))
            .is_some_and(|slot| slot.placed),
        "the publish told its place"
    );
    let file = [
        section(1, Section::Grab, vec![grab(FREE)]),
        section(2, Section::Publish, vec![publish(FREE, A, "n", Kind::File)]),
    ]
    .concat();
    caught(&replayed(&snapshot(), &file), 2, "initialized below");
}

#[test]
fn a_free_pushes_its_slot_on_the_free_list() {
    let records = [
        section(1, Section::Detach, vec![unlink(F, A, How::Always)]),
        section(2, Section::DiscardFree, vec![free(F)]),
    ]
    .concat();
    let model = replayed(&snapshot(), &records);
    clean(&model);
    assert_eq!(slots(&model), (NEXT, vec![F, FREE]));
    let freed = super::record_of(&model, F);
    assert_eq!(freed.flags, NodeFlags::Discarded.bits());
}

#[test]
fn the_last_free_of_a_block_resets_it() {
    let mut block = BlockSnapshot::empty(INSTANCE, 2);
    block.records[0].child = 1;
    block.records[1] = record(ROOT_NODE, "x", Kind::File);
    let lone = Snapshot {
        blocks: vec![block],
        ..Snapshot::default()
    };
    let records = [
        section(1, Section::Detach, vec![unlink(1, ROOT_NODE, How::Always)]),
        section(2, Section::DiscardFree, vec![free(1)]),
    ]
    .concat();
    let model = replayed(&lone, &records);
    clean(&model);
    assert_eq!(slots(&model), (1, vec![]), "block 0 keeps its root alone");
}

#[test]
fn a_free_of_a_live_node_is_caught() {
    let model = replayed(
        &snapshot(),
        &section(1, Section::DiscardFree, vec![free(F)]),
    );
    caught(&model, 1, "a free of live 2");
}

#[test]
fn a_free_of_a_free_slot_is_caught() {
    let model = replayed(&snapshot(), &section(1, Section::Release, vec![free(FREE)]));
    caught(&model, 1, "which is free");
}

/// On a served State only a sweep frees a published slot (C3): a free of one in another section
/// is caught; in the drain's and the sweeps' section, which only a sweep writes there, it is not.
#[test]
fn a_served_free_of_a_published_node_is_caught() {
    let records = [
        section(1, Section::Detach, vec![unlink(F, A, How::Always)]),
        section(2, Section::Release, vec![free(F)]),
    ]
    .concat();
    caught(
        &replayed(&served(), &records),
        2,
        "a free of published 2 on a served State",
    );
    let swept = [
        section(1, Section::Detach, vec![unlink(F, A, How::Always)]),
        section(2, Section::DiscardFree, vec![free(F)]),
    ]
    .concat();
    clean(&replayed(&served(), &swept));
    let released = [
        section(1, Section::Grab, vec![grab(FREE)]),
        section(2, Section::Release, vec![free(FREE)]),
    ]
    .concat();
    clean(&replayed(&served(), &released));
}

#[test]
fn a_record_of_a_replaced_instance_is_caught() {
    let records = [
        section(
            1,
            Section::Unlabelled,
            vec![Effect::Loaded {
                block: 0,
                instance: 9,
                zeroed: false,
            }],
        ),
        section(2, Section::Grab, vec![grab(FREE)]),
    ]
    .concat();
    caught(
        &replayed(&snapshot(), &records),
        2,
        "instance 3, where its instance is 9",
    );
}

/// A reader's head read is held to its directory's count and its block's instance as the model
/// has them at its `seq`: the count before an add at the head, then after it; not the count from
/// before, nor another instance.
#[test]
fn a_readers_head_read_reads_the_count_at_its_seq() {
    let head = |seq, changes, instance| {
        section(
            seq,
            Section::Unlabelled,
            vec![Effect::Head {
                node: A,
                changes,
                instance,
            }],
        )
    };
    let made = snapshot();
    clean(&replayed(
        &made,
        &[
            head(1, 0, INSTANCE),
            attach(2, A, "n"),
            head(5, 1, INSTANCE),
        ]
        .concat(),
    ));
    caught(
        &replayed(&made, &[attach(1, A, "n"), head(4, 0, INSTANCE)].concat()),
        4,
        "a reader's head read on 1's count 0 in instance 3, where the model holds Some(1) in \
         Some(3)",
    );
    caught(
        &replayed(&made, &head(1, 0, 9)),
        1,
        "count 0 in instance 9, where the model holds Some(0) in Some(3)",
    );
}

#[test]
fn a_load_keeps_the_block_and_restarts_its_counts_and_bits() {
    let mut started = snapshot();
    started.blocks[0].changes[A as usize] = 5;
    started.blocks[0].clear_pending[A as usize] = true;
    let records = section(
        1,
        Section::Unlabelled,
        vec![Effect::Loaded {
            block: 0,
            instance: 9,
            zeroed: false,
        }],
    );
    let model = replayed(&started, &records);
    clean(&model);
    let slot = model
        .state(STATE)
        .and_then(|state| state.slot(A))
        .expect("the slot");
    assert_eq!((slot.changes, slot.clear_pending), (0, false));
    assert_eq!(chain(&model, A), vec![G, F]);
    assert_eq!(slots(&model), (NEXT, vec![FREE]));
}

#[test]
fn a_zeroed_load_empties_the_block() {
    let records = section(
        1,
        Section::Unlabelled,
        vec![Effect::Loaded {
            block: 0,
            instance: 9,
            zeroed: true,
        }],
    );
    let model = replayed(&snapshot(), &records);
    clean(&model);
    assert_eq!(slots(&model), (1, vec![]));
    assert_eq!(status(&model, F), Status::Free);
}

#[test]
fn a_build_publishes_with_its_root() {
    let mut records = build(1);
    records.extend(section(
        6,
        Section::Publish,
        vec![publish(FREE, A, "r", Kind::Directory)],
    ));
    let model = replayed(&snapshot(), &records);
    clean(&model);
    assert_eq!(chain(&model, FREE), vec![NEXT]);
    assert_eq!(chain(&model, A), vec![FREE, G, F]);
    assert_eq!(changes(&model, A), 1, "the build's add counts nothing");
    let added = model
        .state(STATE)
        .and_then(|state| state.slot(NEXT))
        .expect("the slot");
    assert!(added.published && added.build.is_none());
}

#[test]
fn a_build_add_into_a_published_tree_is_caught() {
    let made = record(A, "n", Kind::File);
    let records = [
        section(1, Section::Grab, vec![grab(FREE)]),
        section(2, Section::Grab, vec![word(FREE, initialized(made), false)]),
        section(3, Section::BuildAdd, vec![build_add(FREE, A, "n")]),
    ]
    .concat();
    caught(
        &replayed(&snapshot(), &records),
        3,
        "which is in no unpublished build",
    );
}

#[test]
fn an_abandon_cuts_its_build_loose_for_its_frees() {
    let mut records = build(1);
    records.extend(
        [
            section(6, Section::Abandon, vec![Effect::Abandon { root: FREE }]),
            section(7, Section::Release, vec![free(NEXT)]),
            section(8, Section::Release, vec![free(FREE)]),
        ]
        .concat(),
    );
    let model = replayed(&snapshot(), &records);
    clean(&model);
    assert_eq!(status(&model, FREE), Status::Free);
    assert_eq!(status(&model, NEXT), Status::Free);
    let abandoned = section(1, Section::Abandon, vec![Effect::Abandon { root: A }]);
    caught(&replayed(&snapshot(), &abandoned), 1, "no build's root");
}

#[test]
fn a_free_in_a_detached_directorys_chain_marks_the_chain_freed() {
    let records = section(
        1,
        Section::DiscardMarks,
        vec![unlink(A, ROOT_NODE, How::Discard), free(F)],
    );
    let model = replayed(&snapshot(), &records);
    clean(&model);
    let freed = |node| {
        model
            .state(STATE)
            .and_then(|state| state.slot(node))
            .is_some_and(|slot| slot.chain_freed)
    };
    assert!(freed(A), "the State's chain of a still points at f's slot");
    assert!(!freed(B));
}

#[test]
fn the_start_gives_live_chains_their_nodes_before_a_detached_directorys_stale_chain() {
    // `a` out of the root's chain, detached, its chain left stale into `h`, which `b` holds.
    let mut stale = snapshot();
    let block = &mut stale.blocks[0];
    block.records[B as usize].sibling = 0;
    block.records[A as usize].flags |= NodeFlags::Discarded.bits();
    block.records[A as usize].child = H;
    let model = replayed(&stale, &[]);
    assert_eq!(chain(&model, B), vec![H]);
    assert_eq!(status(&model, H), Status::Chained);
    let freed = |node| {
        model
            .state(STATE)
            .and_then(|state| state.slot(node))
            .is_some_and(|slot| slot.chain_freed)
    };
    assert!(freed(A) && !freed(B));
}

#[test]
fn a_chain_the_start_finds_looping_ends_and_is_marked_freed() {
    // `a`'s chain `g`, `f`, `g` again.
    let mut looping = snapshot();
    looping.blocks[0].records[F as usize].sibling = G;
    let model = replayed(&looping, &[]);
    assert_eq!(chain(&model, A), vec![G, F]);
    assert!(
        model
            .state(STATE)
            .and_then(|state| state.slot(A))
            .is_some_and(|slot| slot.chain_freed)
    );
}

#[test]
fn a_chain_the_start_finds_stale_is_marked_freed() {
    // `a`'s chain runs into the free slot, as a discard failing part way leaves it.
    let mut stale = snapshot();
    let block = &mut stale.blocks[0];
    block.records[G as usize].sibling = FREE;
    let model = replayed(&stale, &[]);
    let freed = |node| {
        model
            .state(STATE)
            .and_then(|state| state.slot(node))
            .is_some_and(|slot| slot.chain_freed)
    };
    assert!(freed(A));
    assert!(!freed(B));
}

#[test]
fn a_directory_abandoned_before_its_publish_keeps_its_place_unknown() {
    let added = record(FREE, "n", Kind::File);
    let records = [
        section(1, Section::Grab, vec![grab(FREE)]),
        section(2, Section::Grab, vec![grab(NEXT)]),
        section(
            3,
            Section::Grab,
            vec![word(NEXT, initialized(added), false)],
        ),
        section(
            4,
            Section::BuildAdd,
            vec![build_add(NEXT, FREE, "n"), word(NEXT, added, false)],
        ),
        section(5, Section::Abandon, vec![Effect::Abandon { root: FREE }]),
    ]
    .concat();
    let model = replayed(&snapshot(), &records);
    clean(&model);
    let slot = |node| {
        model
            .state(STATE)
            .and_then(|state| state.slot(node))
            .expect("the slot")
    };
    assert_eq!(slot(FREE).status, Status::Unchained(Unchained::Abandoned));
    assert!(
        !slot(FREE).placed,
        "no record told the root's parent and name: the compare leaves them to the State"
    );
    assert!(slot(NEXT).placed, "its add told the file's");
}

#[test]
fn a_move_counts_r_from_and_one_to() {
    let model = replayed(
        &snapshot(),
        &section(1, Section::MoveNamed, vec![moved(F, (A, B), "f", None)]),
    );
    clean(&model);
    assert_eq!(chain(&model, A), vec![G]);
    assert_eq!(chain(&model, B), vec![F, H]);
    assert_eq!((changes(&model, A), changes(&model, B)), (R, 1));
    assert_eq!(super::record_of(&model, F).parent, B);
}

#[test]
fn a_replacing_move_counts_r_on_to_across_directories_and_none_within() {
    let across = replayed(
        &snapshot(),
        &section(1, Section::Reattach, vec![moved(F, (A, B), "h", Some(H))]),
    );
    clean(&across);
    assert_eq!(chain(&across, B), vec![F]);
    assert_eq!(status(&across, H), Status::Unchained(Unchained::Replaced));
    assert!(super::record_of(&across, H).flags & NodeFlags::Discarded.bits() != 0);
    assert_eq!((changes(&across, A), changes(&across, B)), (R, R + 1));
    let within = replayed(
        &snapshot(),
        &section(1, Section::Reattach, vec![moved(F, (A, A), "g", Some(G))]),
    );
    clean(&within);
    assert_eq!(chain(&within, A), vec![F]);
    assert_eq!(changes(&within, A), R + 1);
}

#[test]
fn a_move_of_a_node_in_no_chain_is_caught() {
    let records = [
        section(1, Section::Detach, vec![unlink(F, A, How::Always)]),
        section(2, Section::MoveNamed, vec![moved(F, (A, B), "f", None)]),
    ]
    .concat();
    caught(
        &replayed(&snapshot(), &records),
        2,
        "whose chain does not hold it",
    );
}

#[test]
fn a_move_into_its_own_subtree_is_caught() {
    caught(
        &replayed(
            &snapshot(),
            &section(
                1,
                Section::MoveNamed,
                vec![moved(A, (ROOT_NODE, A), "a", None)],
            ),
        ),
        1,
        "into its own subtree",
    );
}

#[test]
fn a_rename_counts_r_and_a_respell_none() {
    let records = [
        section(
            1,
            Section::RenameInPlace,
            vec![rename(F, A, hash_string("x"), "x")],
        ),
        section(
            2,
            Section::RenameInPlace,
            vec![rename(F, A, hash_string("x"), "X")],
        ),
    ]
    .concat();
    let model = replayed(&snapshot(), &records);
    clean(&model);
    assert_eq!(changes(&model, A), R);
    assert_eq!(chain(&model, A), vec![G, F], "a rename keeps its place");
}

#[test]
fn an_unlink_counts_one_and_a_discards_none() {
    let records = [
        section(1, Section::Detach, vec![unlink(F, A, How::Always)]),
        section(2, Section::DiscardMarks, vec![unlink(G, A, How::Discard)]),
    ]
    .concat();
    let model = replayed(&snapshot(), &records);
    clean(&model);
    assert_eq!(changes(&model, A), 1);
    assert!(chain(&model, A).is_empty());
    assert_eq!(
        status(&model, F),
        Status::Unchained(Unchained::Detached(How::Always))
    );
}

#[test]
fn an_unlink_from_another_parent_is_caught() {
    caught(
        &replayed(
            &snapshot(),
            &section(1, Section::Detach, vec![unlink(F, B, How::Always)]),
        ),
        1,
        "whose chain does not hold it",
    );
}

#[test]
fn served_names_are_unique_by_hash() {
    caught(
        &replayed(&served(), &attach(1, A, "g")),
        3,
        "holds its name (C8)",
    );
    clean(&replayed(&snapshot(), &attach(1, A, "g")));
    let onto = section(1, Section::Reattach, vec![moved(H, (B, A), "f", None)]);
    caught(&replayed(&served(), &onto), 1, "holds its name (C8)");
    let replacing = section(1, Section::Reattach, vec![moved(H, (B, A), "f", Some(F))]);
    clean(&replayed(&served(), &replacing));
}

#[test]
fn a_word_of_a_free_slot_is_caught() {
    let freed = Node {
        flags: NodeFlags::Discarded.bits() | NodeFlags::File.bits(),
        ..Node::default()
    };
    caught(
        &replayed(
            &snapshot(),
            &section(1, Section::InPlace, vec![word(FREE, freed, false)]),
        ),
        1,
        "a free slot",
    );
}

#[test]
fn a_discards_mark_reaches_no_live_node() {
    caught(
        &replayed(
            &snapshot(),
            &section(
                1,
                Section::DiscardMarks,
                vec![Effect::Discarding { node: F }],
            ),
        ),
        1,
        "a discard's mark of live 2",
    );
    let records = [
        section(
            1,
            Section::Detach,
            vec![unlink(A, ROOT_NODE, How::Always), Effect::Pend { root: A }],
        ),
        section(
            2,
            Section::DiscardMarks,
            vec![Effect::Discarding { node: F }],
        ),
    ]
    .concat();
    let model = replayed(&snapshot(), &records);
    clean(&model);
    assert!(super::record_of(&model, F).flags & NodeFlags::Discarded.bits() != 0);
    assert_eq!(
        status(&model, A),
        Status::Unchained(Unchained::Detached(How::Always))
    );
}

#[test]
fn a_pend_of_a_node_not_detached_is_caught() {
    caught(
        &replayed(
            &snapshot(),
            &section(1, Section::Detach, vec![Effect::Pend { root: A }]),
        ),
        1,
        "not detached",
    );
}

#[test]
fn a_guard_finished_with_none_armed_is_caught() {
    let guard = |seq, event| section(seq, Section::Unlabelled, vec![Effect::Guard { event }]);
    caught(
        &replayed(&snapshot(), &guard(1, GuardEvent::Finished)),
        1,
        "with none armed",
    );
    let records = [
        guard(1, GuardEvent::Armed),
        guard(2, GuardEvent::DroppedArmed),
    ]
    .concat();
    clean(&replayed(&snapshot(), &records));
    // A guard is its task's: one armed in another task leaves this one's finish unmatched.
    let mut finished = guard(2, GuardEvent::Finished);
    finished[0].task = Some(1);
    let records = [guard(1, GuardEvent::Armed), finished].concat();
    caught(
        &replayed(&snapshot(), &records),
        2,
        "with none armed in its task",
    );
}

#[test]
fn records_of_a_state_never_started_are_caught() {
    let mut records = section(1, Section::Grab, vec![grab(FREE)]);
    records[0].state = 99;
    let model = replayed(&snapshot(), &records);
    assert!(
        model
            .violations()
            .iter()
            .any(|violation| violation.state == 99 && violation.what.contains("never started")),
        "{:#?}",
        model.violations()
    );
}

#[test]
fn a_state_made_empty_starts_from_its_zeroed_block() {
    let mut records = [
        section(0, Section::Unlabelled, vec![Effect::Created { from: None }]),
        section(
            1,
            Section::Unlabelled,
            vec![Effect::Loaded {
                block: 0,
                instance: 1,
                zeroed: true,
            }],
        ),
        section(
            2,
            Section::Grab,
            vec![Effect::Grab {
                node: 1,
                instance: 1,
            }],
        ),
    ]
    .concat();
    for record in &mut records {
        record.state = 99;
    }
    let mut model = Model::new();
    model.replay(&records);
    clean(&model);
    let block = model
        .state(99)
        .and_then(|state| state.block(0))
        .expect("block 0 of the State made");
    assert_eq!(block.node_count, 2);
}

#[test]
fn a_state_loaded_with_no_snapshot_is_caught() {
    let mut model = Model::new();
    model.replay(&section(
        0,
        Section::Unlabelled,
        vec![Effect::Created {
            from: Some((1, Hash::default())),
        }],
    ));
    caught(&model, 0, "which no snapshot was provided of");
}

#[test]
fn a_repin_of_the_revision_alone_keeps_its_child() {
    let repin = |child, whole| {
        section(
            1,
            Section::Repin,
            vec![Effect::Repin {
                node: L,
                child,
                whole,
            }],
        )
    };
    caught(
        &replayed(&snapshot(), &repin(PINNED + 1, false)),
        1,
        "moved its child",
    );
    clean(&replayed(&snapshot(), &repin(PINNED, false)));
    let model = replayed(&snapshot(), &repin(PINNED + 1, true));
    clean(&model);
    assert_eq!(super::record_of(&model, L).child, PINNED + 1);
    caught(
        &replayed(
            &snapshot(),
            &section(
                1,
                Section::Repin,
                vec![Effect::Repin {
                    node: F,
                    child: PINNED,
                    whole: true,
                }],
            ),
        ),
        1,
        "no live link",
    );
}

/// A node of block 5, which the State made by hand has no image of.
const UNKNOWN: NodeID = (5 * BLOCK_NODE_COUNT + 1) as NodeID;

/// Each record the model cannot apply, or whose rule it breaks, caught where it stands: every
/// rule above's own refusals apart.
#[test]
fn each_record_the_model_cannot_apply_is_caught() {
    let made = |parent| initialized(record(parent, "n", Kind::File));
    let file_word = |label| {
        let mut changed = super::record_of(&replayed(&snapshot(), &[]), F);
        changed.flags |= NodeFlags::DirtyModify.bits();
        changed.size = 9;
        section(1, label, vec![word(F, changed, false)])
    };
    let rows: Vec<(Vec<Record>, u64, &str)> = vec![
        (
            section(1, Section::Unlabelled, vec![Effect::Created { from: None }]),
            1,
            "a State made twice",
        ),
        (
            section(1, Section::Grab, vec![grab(UNKNOWN)]),
            1,
            "a record of block 5, which the model has no image of",
        ),
        (
            section(
                1,
                Section::Unlabelled,
                vec![Effect::Allocated {
                    block: 0,
                    instance: 9,
                }],
            ),
            1,
            "block 0 allocated, which the model holds",
        ),
        (
            section(
                1,
                Section::Unlabelled,
                vec![Effect::Loaded {
                    block: 5,
                    instance: 9,
                    zeroed: false,
                }],
            ),
            1,
            "block 5 loaded from the store, which the model has no image of",
        ),
        (
            [
                section(1, Section::Grab, vec![grab(FREE)]),
                section(2, Section::Grab, vec![word(FREE, made(NEXT), false)]),
                section(
                    3,
                    Section::Publish,
                    vec![publish(FREE, NEXT, "n", Kind::File)],
                ),
            ]
            .concat(),
            3,
            "published below 8, a free slot",
        ),
        (
            section(1, Section::Reattach, vec![moved(F, (A, B), "g", Some(G))]),
            1,
            "3 replaced in 4, whose chain does not hold it",
        ),
        (
            section(
                1,
                Section::RenameInPlace,
                vec![rename(F, B, hash_string("x"), "x")],
            ),
            1,
            "a rename of 2 in 4, whose chain does not hold it",
        ),
        (
            section(
                1,
                Section::Repin,
                vec![Effect::Repin {
                    node: UNKNOWN,
                    child: PINNED,
                    whole: true,
                }],
            ),
            1,
            "which the model has no image of",
        ),
        (
            section(1, Section::InPlace, vec![word(UNKNOWN, made(A), false)]),
            1,
            "a word of 2561, which the model has no image of",
        ),
        (
            file_word(Section::Grab),
            1,
            "an initialization of 2, not in flight",
        ),
        (
            file_word(Section::SettlePhase1),
            1,
            "a settle wrote 2, no directory",
        ),
        (file_word(Section::Save), 1, "a save wrote 2, not the root"),
        (
            [build(1), section(6, Section::Release, vec![free(NEXT)])].concat(),
            6,
            "in the unpublished build of 7, not cut loose",
        ),
        (
            [build(1), section(6, Section::Release, vec![free(FREE)])].concat(),
            6,
            "a build's root holding its build, not cut loose",
        ),
        (
            [
                section(1, Section::Grab, vec![grab(FREE)]),
                section(
                    2,
                    Section::Grab,
                    vec![word(
                        FREE,
                        initialized(record(A, "r", Kind::Directory)),
                        false,
                    )],
                ),
                section(3, Section::BuildAdd, vec![build_add(FREE, FREE, "r")]),
            ]
            .concat(),
            3,
            "added into its own build",
        ),
        (
            section(
                1,
                Section::DiscardMarks,
                vec![Effect::Discarding { node: FREE }],
            ),
            1,
            "a discard's mark of 7, a free slot",
        ),
    ];
    for (records, seq, what) in rows {
        caught(&replayed(&snapshot(), &records), seq, what);
    }
}
