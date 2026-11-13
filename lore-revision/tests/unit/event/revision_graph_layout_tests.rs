// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::mem::align_of;
use std::mem::size_of;

use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_revision::commit::LoreRevisionCommitStatsEventData;
use lore_revision::event::LoreErrorDetail;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_graph::LoreRevisionGraphBranchEventData;
use lore_revision::event::revision_graph::LoreRevisionGraphInstanceEventData;
use lore_revision::event::revision_graph::LoreRevisionGraphLocationCompleteEventData;
use lore_revision::event::revision_graph::LoreRevisionGraphRevisionEventData;
use lore_revision::instance::InstanceId;
use lore_revision::instance::LoreRepositoryInstanceCloneCompleteEventData;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreString;

/// The largest payload among the variants ahead of the revision graph ones, as
/// `-Zprint-type-sizes` reports `LoreEvent`.
type LargestExistingPayload = LoreRevisionCommitStatsEventData;

/// The tag of `RepositoryInstanceCloneComplete`, the last variant ahead of the revision graph
/// ones: `LORE_EVENT_REPOSITORY_INSTANCE_CLONE_COMPLETE` in `enum lore_event_id_t`.
const LAST_EXISTING_TAG: u32 = 245;

/// Under `repr(C, u32)` a payload follows the tag at the alignment of the most aligned variant,
/// so a variant larger than the largest payload, or aligned past it, grows `LoreEvent`.
#[test]
fn lore_event_is_sized_by_an_existing_payload() {
    let event = size_of::<LoreEvent>();
    let tagged = size_of::<LargestExistingPayload>() + align_of::<LargestExistingPayload>();
    assert!(
        event <= tagged,
        "LoreEvent holds {event} bytes, the largest existing payload with its tag {tagged}; the \
         graph event data's (size, alignment): branch {:?}, instance {:?}, revision {:?}, \
         location complete {:?}",
        layout::<LoreRevisionGraphBranchEventData>(),
        layout::<LoreRevisionGraphInstanceEventData>(),
        layout::<LoreRevisionGraphRevisionEventData>(),
        layout::<LoreRevisionGraphLocationCompleteEventData>(),
    );
}

/// The size and alignment of `T`.
fn layout<T>() -> (usize, usize) {
    (size_of::<T>(), align_of::<T>())
}

#[test]
fn graph_variants_are_appended_after_the_last_existing_variant() {
    let last =
        LoreEvent::RepositoryInstanceCloneComplete(LoreRepositoryInstanceCloneCompleteEventData {
            instance_id: InstanceId::default(),
            target_path: LoreString::default(),
            cloned_directory_count: 0,
            cloned_entry_count: 0,
            copied_file_count: 0,
        })
        .discriminant();
    assert_eq!(last, LAST_EXISTING_TAG);

    let graph = [
        LoreEvent::RevisionGraphBranch(branch()).discriminant(),
        LoreEvent::RevisionGraphInstance(instance()).discriminant(),
        LoreEvent::RevisionGraphRevision(revision()).discriminant(),
        LoreEvent::RevisionGraphLocationComplete(location_complete()).discriminant(),
    ];
    assert_eq!(graph, [last + 1, last + 2, last + 3, last + 4]);
}

fn branch() -> LoreRevisionGraphBranchEventData {
    LoreRevisionGraphBranchEventData {
        id: 0,
        location: LoreBranchLocation::Local,
        branch: BranchId::default(),
        name: LoreString::default(),
        category: LoreString::default(),
        latest: Hash::default(),
        stack: LoreArray::default(),
        creator: LoreString::default(),
        created: 0,
        protect: 0,
        archived: 0,
        divergent: 0,
    }
}

fn instance() -> LoreRevisionGraphInstanceEventData {
    LoreRevisionGraphInstanceEventData {
        id: 0,
        location: LoreBranchLocation::Local,
        instance_id: InstanceId::default(),
        path: LoreString::default(),
        branch: BranchId::default(),
        revision: Hash::default(),
        stale: 0,
    }
}

fn revision() -> LoreRevisionGraphRevisionEventData {
    LoreRevisionGraphRevisionEventData {
        id: 0,
        location: LoreBranchLocation::Local,
        revision_hash: Hash::default(),
        revision_number: 0,
        branch: BranchId::default(),
    }
}

fn location_complete() -> LoreRevisionGraphLocationCompleteEventData {
    LoreRevisionGraphLocationCompleteEventData {
        id: 0,
        location: LoreBranchLocation::Local,
        records: 0,
        error: LoreErrorDetail::default(),
    }
}
