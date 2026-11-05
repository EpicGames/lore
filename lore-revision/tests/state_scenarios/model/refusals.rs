// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The model's refusal rows, on records made by hand over the State made by hand: a section's
//! `Refused` record read what the model holds at its `seq`, and its reason follows from that read
//! and from what it expected. Each row's refusal as the code makes it is clean; one reading
//! otherwise, or one its read does not explain, is caught.

use lore_revision::node::BLOCK_NODE_COUNT;
use lore_revision::node::INVALID_NODE;
use lore_revision::node::Node;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Expected;
use lore_revision::state::node::hooks::log::Kind;
use lore_revision::state::node::hooks::log::Read;
use lore_revision::state::node::hooks::log::Reason;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;
use lore_revision::state::node::hooks::log::Words;
use lore_storage::hash::hash_string;

use super::A;
use super::B;
use super::F;
use super::FREE;
use super::G;
use super::H;
use super::INSTANCE;
use super::L;
use super::caught;
use super::clean;
use super::detached;
use super::free;
use super::grab;
use super::initialized;
use super::record;
use super::replayed;
use super::section;
use super::snapshot;
use super::word;
use crate::harness::model::Model;
use crate::harness::model::Snapshot;

/// [`snapshot`] with each node's flags and extension flags as `changed` adds them.
fn with(changed: &[(NodeID, u16, u32)]) -> Snapshot {
    let mut made = snapshot();
    for &(node, flags, ext) in changed {
        let held = &mut made.blocks[0].records[node as usize];
        held.flags |= flags;
        held.ext_flags |= ext;
    }
    made
}

/// The word of `node` as `snapshot` holds it, Discarded where `discarded`.
fn read(snapshot: &Snapshot, node: NodeID, discarded: bool) -> Read {
    let mut held = snapshot.blocks[0].records[node as usize];
    if discarded {
        held.flags |= NodeFlags::Discarded.bits();
    }
    Read::Word(Words(held))
}

fn refused(node: NodeID, parent: NodeID, reason: Reason, read: Read, expected: Expected) -> Effect {
    Effect::Refused {
        node,
        parent,
        reason,
        read,
        expected,
    }
}

/// The model of `snapshot` with `before` replayed, then `refusal` in a section of its own; and
/// that section's `seq`.
fn judged(snapshot: &Snapshot, before: Vec<Record>, refusal: Effect) -> (Model, u64) {
    let seq = before.last().map_or(1, |record| record.seq + 1);
    let mut records = before;
    records.extend(section(seq, Section::Detach, vec![refusal]));
    (replayed(snapshot, &records), seq)
}

/// Asserts `refusal`, after `before`, is clean.
fn holds(snapshot: &Snapshot, before: Vec<Record>, refusal: Effect) {
    clean(&judged(snapshot, before, refusal).0);
}

/// Asserts `refusal`, after `before`, is caught saying `what`.
fn wrong(snapshot: &Snapshot, before: Vec<Record>, refusal: Effect, what: &str) {
    let (model, seq) = judged(snapshot, before, refusal);
    caught(&model, seq, what);
}

const STAGED_DELETE: u16 = NodeFlags::StagedDelete.bits();
const STAGED_ADD: u16 = NodeFlags::StagedAdd.bits();
const DIRTY: u16 = NodeFlags::Dirty.bits();
const LOCAL: u32 = NodeExtFlags::LocalContent.bits();

#[test]
fn a_refusal_reads_a_word_as_the_model_holds_it() {
    let dirty = with(&[(F, DIRTY, 0)]);
    holds(
        &dirty,
        vec![],
        refused(
            F,
            F,
            Reason::IfClean,
            read(&dirty, F, false),
            Expected::Nothing,
        ),
    );
    // Read clean, where the model holds it dirty.
    let clean_read = read(&snapshot(), F, false);
    wrong(
        &dirty,
        vec![],
        refused(F, F, Reason::IfClean, clean_read, Expected::Nothing),
        "otherwise than the model",
    );
    // Read below another parent.
    let mut elsewhere = record(B, "f", Kind::File);
    elsewhere.flags |= DIRTY;
    wrong(
        &dirty,
        vec![],
        refused(
            F,
            F,
            Reason::IfClean,
            Read::Word(Words(elsewhere)),
            Expected::Nothing,
        ),
        "otherwise than the model",
    );
    // A parent's word, Discarded as its detach left it.
    holds(
        &snapshot(),
        detached(1, A, 0),
        refused(
            FREE,
            A,
            Reason::NotFound,
            read(&snapshot(), A, true),
            Expected::Nothing,
        ),
    );
    wrong(
        &snapshot(),
        detached(1, A, 0),
        refused(
            FREE,
            A,
            Reason::NotFound,
            read(&snapshot(), A, false),
            Expected::Nothing,
        ),
        "otherwise than the model",
    );
}

#[test]
fn a_refusal_reads_a_count_as_the_model_holds_it() {
    let count = |changes: u32, instance: u64| Read::Count { changes, instance };
    let expected = |changes: u32| Expected::Count {
        changes,
        instance: INSTANCE,
    };
    holds(
        &snapshot(),
        vec![],
        refused(A, A, Reason::IfUnchanged, count(0, INSTANCE), expected(5)),
    );
    wrong(
        &snapshot(),
        vec![],
        refused(A, A, Reason::IfUnchanged, count(2, INSTANCE), expected(5)),
        "count",
    );
    wrong(
        &snapshot(),
        vec![],
        refused(
            A,
            A,
            Reason::IfUnchanged,
            count(0, INSTANCE + 1),
            expected(5),
        ),
        "count",
    );
    // The count it expected, read: nothing changed, so nothing refuses.
    wrong(
        &snapshot(),
        vec![],
        refused(A, A, Reason::IfUnchanged, count(0, INSTANCE), expected(0)),
        "does not explain",
    );
}

#[test]
fn an_exists_refusal_names_a_live_holder_of_the_name() {
    // `FREE` grabbed and initialized as `f` below `a`, which `f` holds.
    let attaching = [
        section(1, Section::Grab, vec![grab(FREE)]),
        section(
            2,
            Section::Grab,
            vec![word(FREE, initialized(record(A, "f", Kind::File)), false)],
        ),
    ]
    .concat();
    let holding = |holder: NodeID, name: &str| Read::Holder {
        holder,
        name_hash: hash_string(name),
    };
    let exists = |holder: NodeID, name: &str| {
        refused(
            FREE,
            A,
            Reason::Exists,
            holding(holder, name),
            Expected::Log { seq: 0 },
        )
    };
    holds(&snapshot(), attaching.clone(), exists(F, "f"));
    // Another name's holder, another directory's node, the node itself, another name, and a
    // holder of a name the attach did not look up.
    for (holder, name) in [(G, "f"), (H, "h"), (FREE, "f"), (F, "x"), (G, "g")] {
        wrong(
            &snapshot(),
            attaching.clone(),
            exists(holder, name),
            "holding the name",
        );
    }
    // A holder gone, and one in the chain but Discarded.
    let mut gone = attaching.clone();
    gone.extend(detached(3, F, A));
    wrong(&snapshot(), gone, exists(F, "f"), "holding the name");
    let discarded = with(&[(F, NodeFlags::Discarded.bits(), 0)]);
    wrong(
        &discarded,
        attaching.clone(),
        exists(F, "f"),
        "holding the name",
    );
    // A reattach's: the mover `g` to `b` under the name `h` holds there; a mover is not its own
    // holder, and the name is the one it moves to.
    let reattach = |holder: NodeID, name: &str| {
        refused(
            G,
            B,
            Reason::Exists,
            holding(holder, name),
            Expected::Nothing,
        )
    };
    holds(&snapshot(), vec![], reattach(H, "h"));
    for (holder, name) in [(F, "f"), (H, "x"), (G, "g")] {
        wrong(
            &snapshot(),
            vec![],
            reattach(holder, name),
            "holding the name",
        );
    }
}

#[test]
fn an_if_empty_refusal_reads_the_head_of_the_chain() {
    let head = |node: NodeID| {
        refused(
            A,
            A,
            Reason::IfEmpty,
            Read::Head(node),
            Expected::Head(node),
        )
    };
    holds(&snapshot(), vec![], head(G));
    wrong(&snapshot(), vec![], head(F), "head");
    // `b` emptied: no head to read, and none read refuses nothing.
    wrong(
        &snapshot(),
        detached(1, H, B),
        refused(B, B, Reason::IfEmpty, Read::Head(H), Expected::Head(H)),
        "head",
    );
    wrong(
        &snapshot(),
        detached(1, H, B),
        refused(
            B,
            B,
            Reason::IfEmpty,
            Read::Head(INVALID_NODE),
            Expected::Head(INVALID_NODE),
        ),
        "did not hold",
    );
    // A head read where a free reached the chain of a detached directory: the State's own.
    let mut freed = detached(1, A, 0);
    freed.extend(section(2, Section::Release, vec![free(F)]));
    holds(
        &snapshot(),
        freed,
        refused(
            A,
            A,
            Reason::IfEmpty,
            Read::Head(FREE),
            Expected::Head(FREE),
        ),
    );
}

/// Asserts the refusal of `node` for `reason`, reading the word of `of` as `snapshot` holds it,
/// expecting `expected`, is clean where `founded`, caught as one its read does not explain where
/// not.
fn row(
    snapshot: &Snapshot,
    (node, of): (NodeID, NodeID),
    reason: Reason,
    expected: Expected,
    founded: bool,
) {
    let refusal = refused(node, of, reason, read(snapshot, of, false), expected);
    if founded {
        holds(snapshot, vec![], refusal);
    } else {
        wrong(snapshot, vec![], refusal, "does not explain");
    }
}

#[test]
fn a_refusal_of_a_node_word_follows_from_its_read() {
    let plain = snapshot();
    let nothing = Expected::Nothing;
    // `IfClean`: the node dirty.
    row(&plain, (F, F), Reason::IfClean, nothing, false);
    let at = |parent: NodeID, name: &str| Expected::At {
        parent,
        name_hash: hash_string(name),
    };
    // `IfClean`: on the node's own word.
    row(
        &with(&[(A, DIRTY, 0)]),
        (F, A),
        Reason::IfClean,
        nothing,
        false,
    );
    // `NotLocal`: a directory without local content, moved elsewhere than a respell.
    row(&plain, (A, A), Reason::NotLocal, at(B, "a"), true);
    row(&plain, (A, A), Reason::NotLocal, at(0, "x"), true);
    row(&plain, (A, A), Reason::NotLocal, at(0, "a"), false);
    row(
        &with(&[(A, 0, LOCAL)]),
        (A, A),
        Reason::NotLocal,
        at(B, "a"),
        false,
    );
    row(&plain, (F, F), Reason::NotLocal, at(B, "f"), false);
    // `StagedDelete`: the parent staged for deletion.
    row(
        &with(&[(A, STAGED_DELETE, 0)]),
        (FREE, A),
        Reason::StagedDelete,
        nothing,
        true,
    );
    row(&plain, (FREE, A), Reason::StagedDelete, nothing, false);
    // `Unmovable`: the node staged for deletion, or a parent staged for deletion.
    let staged_f = with(&[(F, STAGED_DELETE, 0)]);
    row(&staged_f, (F, F), Reason::Unmovable, nothing, true);
    row(&plain, (F, F), Reason::Unmovable, nothing, false);
    row(
        &with(&[(A, STAGED_DELETE, 0)]),
        (F, A),
        Reason::Unmovable,
        nothing,
        true,
    );
    row(&plain, (F, A), Reason::Unmovable, nothing, false);
    // `At`: another parent or name hash than expected.
    row(&plain, (F, F), Reason::At, at(B, "f"), true);
    row(&plain, (F, F), Reason::At, at(A, "g"), true);
    row(&plain, (F, F), Reason::At, at(A, "f"), false);
    row(&plain, (F, F), Reason::At, nothing, false);
    // `NotFound`: Discarded, which no live node or parent is, a file's no more than a
    // directory's: no section tests the kind with it.
    row(&plain, (FREE, F), Reason::NotFound, nothing, false);
    row(&plain, (FREE, A), Reason::NotFound, nothing, false);
    row(&plain, (F, F), Reason::NotFound, nothing, false);
    // `NotDirectory`, `NotLink`.
    row(&plain, (FREE, F), Reason::NotDirectory, nothing, true);
    row(&plain, (FREE, A), Reason::NotDirectory, nothing, false);
    row(&plain, (F, F), Reason::NotLink, nothing, true);
    row(&plain, (L, L), Reason::NotLink, nothing, false);
    // `Rewrite`: no live file, or one staged for deletion.
    row(&plain, (A, A), Reason::Rewrite, nothing, true);
    row(&staged_f, (F, F), Reason::Rewrite, nothing, true);
    row(&plain, (F, F), Reason::Rewrite, nothing, false);
    // `ServedRewrite`: no file; a parent Discarded, below.
    row(&plain, (A, A), Reason::ServedRewrite, nothing, true);
    row(&plain, (F, F), Reason::ServedRewrite, nothing, false);
    row(&plain, (F, A), Reason::ServedRewrite, nothing, false);
    // `Restore`: the node not staged for deletion; its parent staged for deletion.
    row(&plain, (F, F), Reason::Restore, nothing, true);
    row(&staged_f, (F, F), Reason::Restore, nothing, false);
    row(
        &with(&[(A, STAGED_DELETE, 0)]),
        (F, A),
        Reason::Restore,
        nothing,
        true,
    );
    row(&plain, (F, A), Reason::Restore, nothing, false);
    // The delete level's `Staged`, once `Other`'s conditions failed, and `Other`.
    let other = |parent: NodeID| at(parent, "f");
    row(&staged_f, (F, F), Reason::DeleteLevelStaged, other(A), true);
    row(&plain, (F, F), Reason::DeleteLevelStaged, other(A), false);
    row(
        &staged_f,
        (F, F),
        Reason::DeleteLevelStaged,
        other(B),
        false,
    );
    row(
        &with(&[(F, STAGED_ADD, 0)]),
        (F, F),
        Reason::DeleteLevelOther,
        other(A),
        true,
    );
    row(&plain, (F, F), Reason::DeleteLevelOther, other(B), true);
    row(&plain, (F, F), Reason::DeleteLevelOther, other(A), false);
    // An in-place or owned change's closure declining is the kind's to judge, once its guard
    // passed: the node live, and for an in-place change at its place.
    row(&plain, (F, F), Reason::InPlace, at(A, "f"), true);
    row(&plain, (F, F), Reason::InPlace, at(B, "f"), false);
    row(&plain, (F, F), Reason::Owned, nothing, true);
    let gone_word = with(&[(F, NodeFlags::Discarded.bits(), 0)]);
    row(&gone_word, (F, F), Reason::InPlace, at(A, "f"), false);
    row(&gone_word, (F, F), Reason::Owned, nothing, false);
    // An in-place change's guard: its node moved, renamed or gone; an owned one's, gone.
    row(&plain, (F, F), Reason::InPlaceAway, at(B, "f"), true);
    row(&plain, (F, F), Reason::InPlaceAway, at(A, "f"), false);
    row(&plain, (F, F), Reason::OwnedGone, nothing, false);
    // `StagedDelete` and `NotLink` of a Discarded word are `NotFound`s.
    let gone_staged = with(&[(A, STAGED_DELETE | NodeFlags::Discarded.bits(), 0)]);
    row(
        &gone_staged,
        (FREE, A),
        Reason::StagedDelete,
        nothing,
        false,
    );
    row(
        &with(&[(F, NodeFlags::Discarded.bits(), 0)]),
        (F, F),
        Reason::NotLink,
        nothing,
        false,
    );
    // A Discarded node staged for deletion is `Other`'s, and a node the delete level finds under
    // its parent is not, whatever its name.
    let gone_f = with(&[(F, STAGED_DELETE | NodeFlags::Discarded.bits(), 0)]);
    row(&gone_f, (F, F), Reason::DeleteLevelStaged, other(A), false);
    row(&plain, (F, F), Reason::DeleteLevelOther, at(A, "x"), false);
    // A live node's rows: `NotLocal` and the node's own `Unmovable` read one.
    let gone_dir = with(&[(A, NodeFlags::Discarded.bits(), 0)]);
    row(&gone_dir, (A, A), Reason::NotLocal, at(B, "a"), false);
    row(&gone_f, (F, F), Reason::Unmovable, nothing, false);
}

/// Each row reads the word its section reads: the node's own, or the directory it changes, its
/// parent where it is one. One meeting its condition on another node's word is caught.
#[test]
fn a_refusal_reads_the_word_its_section_reads() {
    let at = |parent: NodeID, name: &str| Expected::At {
        parent,
        name_hash: hash_string(name),
    };
    let nothing = Expected::Nothing;
    let staged_a = with(&[(A, STAGED_DELETE, 0)]);
    let staged_add_a = with(&[(A, STAGED_ADD, 0)]);
    // The node's own word, read of its parent.
    row(&snapshot(), (F, A), Reason::At, at(B, "a"), false);
    row(&snapshot(), (F, A), Reason::NotLocal, at(B, "a"), false);
    row(&snapshot(), (L, A), Reason::NotLink, nothing, false);
    row(&snapshot(), (F, A), Reason::Rewrite, nothing, false);
    row(
        &staged_add_a,
        (F, A),
        Reason::DeleteLevelOther,
        at(0, "a"),
        false,
    );
    row(
        &staged_a,
        (F, A),
        Reason::DeleteLevelStaged,
        at(0, "a"),
        false,
    );
    row(&snapshot(), (F, A), Reason::InPlaceAway, at(B, "a"), false);
    wrong(
        &snapshot(),
        vec![],
        refused(
            F,
            A,
            Reason::IfUnchanged,
            Read::Count {
                changes: 0,
                instance: INSTANCE,
            },
            Expected::Count {
                changes: 5,
                instance: INSTANCE,
            },
        ),
        "does not explain",
    );
    wrong(
        &snapshot(),
        vec![],
        refused(F, A, Reason::IfEmpty, Read::Head(G), Expected::Head(G)),
        "does not explain",
    );
    // The directory's word, read of the node itself.
    row(&staged_a, (A, A), Reason::StagedDelete, nothing, false);
    row(&snapshot(), (F, F), Reason::NotDirectory, nothing, false);
    // A parent's word, of a directory the node does not hang from.
    let staged_b = with(&[(B, STAGED_DELETE, 0)]);
    wrong(
        &staged_b,
        vec![],
        refused(F, B, Reason::Restore, read(&staged_b, B, false), nothing),
        "below",
    );
    wrong(
        &snapshot(),
        detached(1, B, 0),
        refused(F, B, Reason::NotFound, read(&snapshot(), B, true), nothing),
        "below",
    );
}

#[test]
fn a_refusal_reads_what_its_reason_decides_on() {
    // A read of another kind than its reason's.
    wrong(
        &snapshot(),
        vec![],
        refused(
            F,
            F,
            Reason::IfClean,
            Read::Count {
                changes: 0,
                instance: INSTANCE,
            },
            Expected::Nothing,
        ),
        "another kind",
    );
    wrong(
        &snapshot(),
        vec![],
        refused(
            F,
            F,
            Reason::Exists,
            read(&snapshot(), F, false),
            Expected::Nothing,
        ),
        "another kind",
    );
    wrong(
        &snapshot(),
        vec![],
        refused(
            A,
            A,
            Reason::IfUnchanged,
            Read::Count {
                changes: 0,
                instance: INSTANCE,
            },
            Expected::Nothing,
        ),
        "another kind",
    );
    // A full name table, read as such, under its own reason alone.
    holds(
        &snapshot(),
        vec![],
        refused(
            FREE,
            FREE,
            Reason::NameTableFull,
            Read::NameTableFull,
            Expected::Nothing,
        ),
    );
    wrong(
        &snapshot(),
        vec![],
        refused(
            F,
            F,
            Reason::IfClean,
            Read::NameTableFull,
            Expected::Nothing,
        ),
        "another kind",
    );
    // A word of a block the model has no image of.
    let elsewhere = (BLOCK_NODE_COUNT + 1) as NodeID;
    wrong(
        &snapshot(),
        vec![],
        refused(
            elsewhere,
            elsewhere,
            Reason::NotFound,
            read(&snapshot(), F, true),
            Expected::Nothing,
        ),
        "no image",
    );
    // A slot in flight no `Word` placed: its parent and name hash are the State's alone.
    let unplaced = Node {
        flags: NodeFlags::Discarded.bits(),
        parent: A,
        name_hash: hash_string("x"),
        ..Node::default()
    };
    holds(
        &snapshot(),
        section(1, Section::Grab, vec![grab(FREE)]),
        refused(
            FREE,
            FREE,
            Reason::NotFound,
            Read::Word(Words(unplaced)),
            Expected::Nothing,
        ),
    );
}

#[test]
fn a_refusal_of_a_word_gone_follows_from_its_read() {
    let plain = snapshot();
    let gone = |reason: Reason, node: NodeID, of: NodeID, parent: NodeID| {
        holds(
            &plain,
            detached(1, of, parent),
            refused(node, of, reason, read(&plain, of, true), Expected::Nothing),
        );
    };
    gone(Reason::NotFound, F, F, A);
    gone(Reason::NotFound, FREE, A, 0);
    gone(Reason::Unmovable, F, A, 0);
    gone(Reason::Rewrite, F, F, A);
    gone(Reason::ServedRewrite, F, F, A);
    gone(Reason::ServedRewrite, F, A, 0);
    gone(Reason::Restore, F, F, A);
    gone(Reason::Restore, F, A, 0);
    gone(Reason::OwnedGone, F, F, A);
    gone(Reason::InPlaceAway, F, F, A);
    holds(
        &plain,
        detached(1, F, A),
        refused(
            F,
            F,
            Reason::DeleteLevelOther,
            read(&plain, F, true),
            Expected::At {
                parent: A,
                name_hash: hash_string("f"),
            },
        ),
    );
}
