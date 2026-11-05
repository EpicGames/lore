// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The has-children reads held to the model (`node_has_dirty_children` and its peers): each reads
//! its directory's listing, from its head read, then each child's words, so a child found carrying
//! the flags was a node of the directory at some read from the listing's head read and carried them
//! at that read or a later one; and answering none is a violation where a child carried them in the
//! directory throughout from there, but one a move within the directory repositioned since, which
//! the listing may pass over.

use lore_revision::node::NodeFlags;
use lore_revision::state::REATTACH_CHANGES;
use lore_revision::state::node::hooks::log::Kind;
use lore_revision::state::node::hooks::log::Section;

use super::A;
use super::B;
use super::FREE;
use super::base_call;
use super::by;
use super::caught;
use super::clean;
use super::flagging;
use super::head;
use super::moved;
use super::moved_out;
use super::published;
use super::record;
use super::replayed_with;
use super::repositioned;
use super::rewritten_of;
use super::section;
use super::snapshot;
use crate::harness::catalogue;
use crate::harness::catalogue::call::Answer;
use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Outcome;

/// A has-children read of `a/` of `kind` over `[begin, end)`, answering `has`.
fn has(kind: &'static catalogue::Kind, span: (u64, u64), has: bool) -> Call {
    Call {
        target: Some(A),
        ..base_call(kind, span, Outcome::Answer(Answer::Has(has)))
    }
}

const CARRIED: &str = "no node of the directory carrying them at any seq from its listing's head";
const UNCARRIED: &str = "one carrying them there throughout from its listing's head read";

/// A child found carrying the flags was a node of the directory at some read from the listing's
/// head read, carrying them at that read or a later one: dirtied before the read or within it, once
/// it moved out after the listing could have held it, or joined after the head read; not where no
/// node of the directory carried them, nor where it left before the listing's head read, nor where
/// only a node a discard marked carried them, nor where the flags carried are not the ones asked
/// for.
#[test]
fn a_child_found_carrying_its_flags_was_in_the_directory_and_carried_them() {
    let made = snapshot();
    let dirty = &catalogue::HAS_DIRTY_CHILDREN;
    let flag = |seq, flags| flagging(&made, seq, flags);
    caught(
        &replayed_with(&made, &head(1, 0), vec![has(dirty, (1, 3), true)]),
        3,
        CARRIED,
    );
    clean(&replayed_with(
        &made,
        &[flag(1, NodeFlags::Dirty), head(2, 0)].concat(),
        vec![has(dirty, (2, 4), true)],
    ));
    clean(&replayed_with(
        &made,
        &[head(2, 0), flag(3, NodeFlags::Dirty)].concat(),
        vec![has(dirty, (2, 5), true)],
    ));
    clean(&replayed_with(
        &made,
        &[head(1, 0), moved_out(2), flag(3, NodeFlags::Dirty)].concat(),
        vec![has(dirty, (1, 5), true)],
    ));
    // Moved out before the head read: the listing never held it, its count the move's.
    let left = [
        moved_out(2),
        head(3, REATTACH_CHANGES),
        flag(4, NodeFlags::Dirty),
    ]
    .concat();
    caught(
        &replayed_with(&made, &left, vec![has(dirty, (1, 6), true)]),
        6,
        CARRIED,
    );
    clean(&replayed_with(
        &made,
        &left,
        vec![has(dirty, (1, 6), false)],
    ));
    // A node that joined after the head read, which a restart, or a reposition of the node the
    // walk stood on, can lead the walk to: dirty once it moved out, as it is read after.
    let mut joined = record(A, "n", Kind::File);
    joined.flags |= NodeFlags::Dirty.bits();
    let gone = by(
        1,
        section(5, Section::MoveNamed, vec![moved(FREE, (A, B), "n", None)]),
    );
    clean(&replayed_with(
        &made,
        &[
            head(1, 0),
            published(2),
            gone,
            rewritten_of(FREE, 6, joined),
        ]
        .concat(),
        vec![has(dirty, (1, 8), true)],
    ));
    caught(
        &replayed_with(
            &made,
            &[flag(1, NodeFlags::Dirty | NodeFlags::Discarded), head(2, 0)].concat(),
            vec![has(dirty, (2, 4), true)],
        ),
        4,
        CARRIED,
    );
    // A dirty child read for staged ones, and a staged one read for dirty ones, carries no flag
    // it asks for.
    for (flags, kind) in [
        (NodeFlags::Dirty, &catalogue::HAS_STAGED_CHILDREN),
        (NodeFlags::Staged, &catalogue::HAS_DIRTY_CHILDREN),
    ] {
        caught(
            &replayed_with(
                &made,
                &[flag(1, flags), head(2, 0)].concat(),
                vec![has(kind, (2, 4), true)],
            ),
            4,
            CARRIED,
        );
    }
    for (flags, kind) in [
        (NodeFlags::Staged, &catalogue::HAS_STAGED_CHILDREN),
        (NodeFlags::Staged, &catalogue::HAS_STAGED_OR_DIRTY_CHILDREN),
        (NodeFlags::Dirty, &catalogue::HAS_STAGED_OR_DIRTY_CHILDREN),
    ] {
        clean(&replayed_with(
            &made,
            &[flag(1, flags), head(2, 0)].concat(),
            vec![has(kind, (2, 4), true)],
        ));
    }
}

/// Answering no child carrying the flags is a violation where a child carried them in the
/// directory throughout from the listing's head read: not where it took them within the read,
/// moved out within it, a move within the directory repositioned it since the head read, or a
/// discard marked it; nor with no head read, which a listing of no directory makes.
#[test]
fn no_child_found_carrying_its_flags_is_a_violation_where_one_carried_them_throughout() {
    let made = snapshot();
    let dirty = &catalogue::HAS_DIRTY_CHILDREN;
    let flag = |seq, flags| flagging(&made, seq, flags);
    let before = flag(1, NodeFlags::Dirty);
    clean(&replayed_with(
        &made,
        &head(1, 0),
        vec![has(dirty, (1, 3), false)],
    ));
    caught(
        &replayed_with(
            &made,
            &[before.clone(), head(2, 0)].concat(),
            vec![has(dirty, (2, 4), false)],
        ),
        4,
        UNCARRIED,
    );
    clean(&replayed_with(
        &made,
        &[head(2, 0), flag(3, NodeFlags::Dirty)].concat(),
        vec![has(dirty, (2, 5), false)],
    ));
    clean(&replayed_with(
        &made,
        &[before.clone(), head(2, 0), moved_out(3)].concat(),
        vec![has(dirty, (2, 5), false)],
    ));
    clean(&replayed_with(
        &made,
        &[before.clone(), head(2, 0), repositioned(3, "f")].concat(),
        vec![has(dirty, (2, 5), false)],
    ));
    // Repositioned before the head read, before the call began or within it: the walk starts
    // after, so meets it; its count the reposition's.
    let ahead = [
        before.clone(),
        repositioned(2, "f"),
        head(3, REATTACH_CHANGES + 1),
    ]
    .concat();
    for begin in [3, 2] {
        caught(
            &replayed_with(&made, &ahead, vec![has(dirty, (begin, 5), false)]),
            5,
            UNCARRIED,
        );
        clean(&replayed_with(
            &made,
            &ahead,
            vec![has(dirty, (begin, 5), true)],
        ));
    }
    clean(&replayed_with(
        &made,
        &[flag(1, NodeFlags::Dirty | NodeFlags::Discarded), head(2, 0)].concat(),
        vec![has(dirty, (2, 4), false)],
    ));
    clean(&replayed_with(
        &made,
        &before,
        vec![has(dirty, (2, 4), false)],
    ));
    caught(
        &replayed_with(
            &made,
            &[flag(1, NodeFlags::Staged), head(2, 0)].concat(),
            vec![has(&catalogue::HAS_STAGED_OR_DIRTY_CHILDREN, (2, 4), false)],
        ),
        4,
        UNCARRIED,
    );
    clean(&replayed_with(
        &made,
        &[before.clone(), head(2, 0)].concat(),
        vec![has(&catalogue::HAS_STAGED_CHILDREN, (2, 4), false)],
    ));
}
