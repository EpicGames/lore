// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::types::ZeroHeapAlloc;
use lore_revision::interface::LoreNodeType;
use zerocopy::IntoBytes;

mod cycle_tests;
mod reserved_name_tests;

use lore_revision::node::*;

/// A legacy block keeps its flags, version and 511 entries, and the entry the
/// current format adds is zero.
#[test]
fn a_legacy_file_metadata_block_converts_to_the_current_format() {
    let mut legacy = NodeFileMetadataBlockDataV0::new_from_heap_zeroed();
    legacy.flags = 3;
    legacy.version = 7;
    for (index, entry) in legacy.node.iter_mut().enumerate() {
        entry.node = [index as u32 + 1, 0];
    }

    let current = legacy.to_current();

    assert_eq!((current.flags, current.version), (3, 7));
    for (index, entry) in current.node[..BLOCK_NODE_FILE_METADATA_COUNT_V0]
        .iter()
        .enumerate()
    {
        assert_eq!(entry.node, [index as u32 + 1, 0], "entry {index}");
    }
    assert!(
        current.node[BLOCK_NODE_FILE_METADATA_COUNT_V0..]
            .iter()
            .all(|entry| entry.as_bytes().iter().all(|&byte| byte == 0)),
        "the added entry is zero"
    );
}

fn node_with_flags(flags: u16) -> Node {
    Node {
        flags,
        ..Default::default()
    }
}

#[test]
fn dirty_flag_bit_positions() {
    // V1: Dirty at bit 3
    assert_eq!(NodeFlags::Dirty.bits(), 0b1000);
    // V1: Dirty does not overlap with Staged (bit 4)
    assert_eq!(NodeFlags::Dirty.bits() & NodeFlags::Staged.bits(), 0);
    // V1: Dirty does not overlap with File/Module/ExternalName
    assert_eq!(NodeFlags::Dirty.bits() & NodeFlags::File.bits(), 0);
    assert_eq!(NodeFlags::Dirty.bits() & NodeFlags::Link.bits(), 0);

    // V2: Dirty at bit 15
    assert_eq!(NodeFlagsV2::Dirty.bits(), 1 << 15);
    // V2: Dirty does not overlap with Staged (bit 16)
    assert_eq!(NodeFlagsV2::Dirty.bits() & NodeFlagsV2::Staged.bits(), 0);
}

#[test]
fn dirty_compound_flags_v1() {
    assert_eq!(
        NodeFlags::DirtyModify.bits(),
        NodeFlags::Dirty.bits() | NodeFlags::StagedModify.bits() & NodeFlags::ActionBits.bits()
    );
    assert_eq!(
        NodeFlags::DirtyAdd.bits(),
        NodeFlags::Dirty.bits() | NodeFlags::StagedAdd.bits() & NodeFlags::ActionBits.bits()
    );
    assert_eq!(
        NodeFlags::DirtyDelete.bits(),
        NodeFlags::Dirty.bits() | NodeFlags::StagedDelete.bits() & NodeFlags::ActionBits.bits()
    );
    assert_eq!(
        NodeFlags::DirtyMove.bits(),
        NodeFlags::Dirty.bits() | NodeFlags::StagedMove.bits() & NodeFlags::ActionBits.bits()
    );
    assert_eq!(
        NodeFlags::DirtyCopy.bits(),
        NodeFlags::Dirty.bits() | NodeFlags::StagedCopy.bits() & NodeFlags::ActionBits.bits()
    );
}

#[test]
fn dirty_bits_mask() {
    // DirtyBits = Dirty + ActionBits (bits 3, 5-9)
    let expected = NodeFlags::Dirty.bits() | NodeFlags::ActionBits.bits();
    assert_eq!(NodeFlags::DirtyBits.bits(), expected);
    // DirtyBits does NOT include Staged (bit 4)
    assert_eq!(NodeFlags::DirtyBits.bits() & NodeFlags::Staged.bits(), 0);
    // DirtyBits does NOT include MergeBits
    assert_eq!(NodeFlags::DirtyBits.bits() & NodeFlags::MergeBits.bits(), 0);
}

#[test]
fn action_bits_mask() {
    // ActionBits = bits 5-9 (shared between Dirty and Staged)
    let expected = (NodeFlags::StagedModify.bits()
        | NodeFlags::StagedAdd.bits()
        | NodeFlags::StagedDelete.bits()
        | NodeFlags::StagedMove.bits()
        | NodeFlags::StagedCopy.bits())
        & !NodeFlags::Staged.bits();
    assert_eq!(NodeFlags::ActionBits.bits(), expected);
}

#[test]
fn node_is_dirty_queries() {
    // Clean node
    let node = Node::default();
    assert!(!node.is_dirty());
    assert!(!node.is_dirty_modify());

    // Dirty only
    let node = node_with_flags(NodeFlags::DirtyModify.bits());
    assert!(node.is_dirty());
    assert!(node.is_dirty_modify());
    assert!(!node.is_dirty_add());
    assert!(!node.is_staged());

    // Dirty+Staged (orthogonal)
    let node = node_with_flags(NodeFlags::Dirty.bits() | NodeFlags::StagedModify.bits());
    assert!(node.is_dirty());
    assert!(node.is_staged());
    assert!(node.is_staged_modify());
    assert!(node.is_dirty_or_staged());
}

#[test]
fn node_has_any_change_flags() {
    assert!(!Node::default().has_any_change_flags());
    assert!(node_with_flags(NodeFlags::Dirty.bits()).has_any_change_flags());
    assert!(node_with_flags(NodeFlags::Staged.bits()).has_any_change_flags());
    assert!(!node_with_flags(NodeFlags::File.bits()).has_any_change_flags());
}

#[test]
fn clear_staged_flags_preserves_dirty() {
    let mut node = node_with_flags(NodeFlags::Dirty.bits() | NodeFlags::StagedModify.bits());
    node.clear_staged_flags();
    assert!(node.is_dirty());
    assert!(!node.is_staged());
    assert_ne!(node.flags & NodeFlags::ActionBits.bits(), 0);
}

#[test]
fn clear_staged_flags_clears_action_when_no_dirty() {
    let mut node = node_with_flags(NodeFlags::StagedModify.bits());
    node.clear_staged_flags();
    assert_eq!(
        node.flags & (NodeFlags::StagedBits.bits() | NodeFlags::Dirty.bits()),
        0
    );
}

#[test]
fn clear_dirty_flags_preserves_staged() {
    let mut node = node_with_flags(NodeFlags::Dirty.bits() | NodeFlags::StagedModify.bits());
    node.clear_dirty_flags();
    assert!(!node.is_dirty());
    assert!(node.is_staged());
    assert!(node.is_staged_modify());
}

#[test]
fn clear_dirty_flags_clears_action_when_no_staged() {
    let mut node = node_with_flags(NodeFlags::DirtyModify.bits());
    node.clear_dirty_flags();
    assert_eq!(
        node.flags & (NodeFlags::DirtyBits.bits() | NodeFlags::StagedBits.bits()),
        0
    );
}

#[test]
fn clear_all_change_flags() {
    let mut node = node_with_flags(
        NodeFlags::File.bits() | NodeFlags::Dirty.bits() | NodeFlags::StagedModify.bits(),
    );
    node.clear_all_change_flags();
    assert!(node.is_file());
    assert!(!node.is_dirty());
    assert!(!node.is_staged());
    assert_eq!(node.flags & NodeFlags::ActionBits.bits(), 0);
}

#[test]
fn action_bits_extraction() {
    let node = node_with_flags(NodeFlags::DirtyMove.bits());
    assert_eq!(
        node.action_bits(),
        NodeFlags::StagedMove.bits() & NodeFlags::ActionBits.bits()
    );
}

#[test]
fn node_type_reads_the_kind_bits_and_ignores_the_rest() {
    assert_eq!(Node::default().node_type(), LoreNodeType::Directory);
    assert_eq!(
        node_with_flags(NodeFlags::File.bits()).node_type(),
        LoreNodeType::File
    );
    assert_eq!(
        node_with_flags(NodeFlags::Link.bits() | NodeFlags::StagedAdd.bits()).node_type(),
        LoreNodeType::Link
    );
    assert_eq!(
        node_with_flags(NodeFlags::DirtyModify.bits()).node_type(),
        LoreNodeType::Directory
    );
}

/// A log started 20 sequences before the u32 wrap, with patch `i` naming node `i`.
fn log_across_the_wrap(appends: u32) -> std::sync::Arc<NodeBlock> {
    let block = NodeBlock::new_zeroed();
    block.watch_chain(0, None);
    {
        let mut writer = block.write_for_tests();
        let log = writer.chain_log_mut(0).expect("an installed log");
        log.restart_at_for_tests(u32::MAX - 20);
        for node in 0..appends {
            log.append(ChainPatch {
                name_hash: u64::from(node),
                block: std::sync::Weak::new(),
                node,
                kind: PatchKind::Attached,
            });
        }
    }
    block
}

/// The window's edges: the oldest patch held, the one before it, the newest, and the head.
fn assert_edges(block: &NodeBlock, appends: u32, held: u32, when: &str) {
    let reader = block.read();
    let log = reader.chain_log(0).expect("an installed log");
    let start = u32::MAX - 20;
    let (seq, floor) = (log.seq(), log.floor());
    assert_eq!(seq, start.wrapping_add(appends), "{when}: the head");
    assert_eq!(seq.wrapping_sub(floor), held, "{when}: the patches held");
    let node = |sequence: u32| log.patch(sequence).map(|patch| patch.node);
    assert_eq!(node(floor), Some(appends - held), "{when}: the oldest held");
    assert_eq!(
        node(floor.wrapping_sub(1)),
        None,
        "{when}: before the oldest"
    );
    assert_eq!(
        node(seq.wrapping_sub(1)),
        Some(appends - 1),
        "{when}: the newest"
    );
    assert_eq!(node(seq), None, "{when}: nothing at the head");
    assert!(log.covers(floor), "{when}: the floor is covered");
    assert!(
        !log.covers(floor.wrapping_sub(1)),
        "{when}: before the floor is stale"
    );
    assert!(log.covers(seq), "{when}: the head is covered");
    assert!(
        !log.covers(seq.wrapping_add(1)),
        "{when}: past the head is stale"
    );
}

/// Appends `count` patches to the log of node 0 in `block`, each naming its own sequence as
/// its node, every third a detach, and respells once.
fn fill(block: &NodeBlock, count: usize) {
    let mut writer = block.write_for_tests();
    let log = writer.chain_log_mut(0).expect("an installed log");
    for _ in 0..count {
        let sequence = log.seq();
        log.append(ChainPatch {
            name_hash: u64::from(sequence),
            block: std::sync::Weak::new(),
            node: sequence,
            kind: if sequence.is_multiple_of(3) {
                PatchKind::Detached
            } else {
                PatchKind::Attached
            },
        });
    }
    log.respell();
}

/// The log of node `index` in `block`: head, floor, detaches, respells and capacity.
pub(crate) fn log_state(block: &NodeBlock, index: usize) -> (u32, u32, u32, u32, usize) {
    let reader = block.read();
    let log = reader.chain_log(index).expect("an installed log");
    (
        log.seq(),
        log.floor(),
        log.detaches(),
        log.respells(),
        log.capacity(),
    )
}

/// Asserts every patch of node 0's window sits at its own sequence.
fn assert_every_patch_at_its_sequence(block: &NodeBlock, when: &str) {
    let reader = block.read();
    let log = reader.chain_log(0).expect("an installed log");
    let mut sequence = log.floor();
    while sequence != log.seq() {
        assert_eq!(
            log.patch(sequence).map(|patch| patch.node),
            Some(sequence),
            "{when}: patch {sequence}"
        );
        sequence = sequence.wrapping_add(1);
    }
}

/// A change logged without its node's block takes a sequence and leaves the log covering no
/// earlier position, so every listing built before it finds the log stale; a detach is counted.
#[test]
fn a_skip_takes_a_sequence_and_uncovers_every_earlier_position() {
    let block = NodeBlock::new_zeroed();
    block.watch_chain(0, None);
    fill(&block, 3);
    let mut writer = block.write_for_tests();
    let log = writer.chain_log_mut(0).expect("an installed log");
    let (seq, detaches) = (log.seq(), log.detaches());
    assert!(log.covers(seq - 1), "the last patch is in the window");
    log.skip(PatchKind::Detached);
    assert_eq!(log.seq(), seq + 1, "the skip takes a sequence");
    assert_eq!(log.detaches(), detaches + 1, "a skipped detach is counted");
    assert!(log.holds_no_patch(), "the window is empty");
    assert!(log.covers(seq + 1), "a reader at the head is covered");
    assert!(!log.covers(seq), "a reader before the skip is not");
}

/// A watch of a full ring grows it fourfold, keeping each patch at its sequence and the
/// counts, until it holds [`CHAIN_LOG_MAX`], where it stays.
#[test]
fn a_full_log_grows_fourfold_at_each_watch_up_to_1024() {
    let block = NodeBlock::new_zeroed();
    block.watch_chain(0, None);
    for (capacity, grown) in [(16, 64), (64, 256), (256, 1024), (1024, 1024)] {
        let (_, _, _, _, held) = log_state(&block, 0);
        assert_eq!(held, capacity, "before the watch at {capacity}");
        assert_eq!(
            block.chain_log_bytes(),
            16 + 32 + 24 * capacity,
            "the slot, the header and the ring at {capacity}"
        );
        fill(&block, capacity + 3);
        let (seq, floor, detaches, respells, _) = log_state(&block, 0);
        assert_eq!(
            seq.wrapping_sub(floor) as usize,
            capacity,
            "full at {capacity}"
        );

        let (mark, _) = block.watch_chain(0, None);

        assert_eq!(
            log_state(&block, 0),
            (seq, floor, detaches, respells, grown),
            "the watch at {capacity} keeps head, floor and counts"
        );
        assert_eq!(
            (mark.seq, mark.detaches, mark.respells),
            (seq, detaches, respells)
        );
        assert_every_patch_at_its_sequence(&block, &format!("grown from {capacity}"));
    }
}

/// Two grows from one size: the one that finds the log already grown past the size it
/// sized its ring from drops its ring and starts over, so no patch is lost.
#[test]
fn a_grow_finding_the_log_grown_since_it_sized_its_ring_starts_over() {
    let block = NodeBlock::new_zeroed();
    block.watch_chain(0, None);
    fill(&block, 16);
    let fired = std::cell::Cell::new(0);
    let hook = || {
        fired.set(fired.get() + 1);
        if fired.get() > 1 {
            return;
        }
        // Grows twice past the size the outer watch read, filling each ring.
        block.watch_chain(0, None);
        fill(&block, 64);
        block.watch_chain(0, None);
        fill(&block, 256 + 5);
    };
    let (seq, floor, detaches, respells) = {
        let (mark, _) = block.watch_chain(0, Some(&hook));
        let (seq, floor, detaches, respells, _) = log_state(&block, 0);
        assert_eq!(mark.seq, seq);
        (seq, floor, detaches, respells)
    };

    assert_eq!(
        log_state(&block, 0),
        (seq, floor, detaches, respells, 1024),
        "the retry grew the log from the size it found"
    );
    assert_eq!(seq.wrapping_sub(floor), 256, "the window the hook left");
    assert_every_patch_at_its_sequence(&block, "after the race");
}

/// `seq` wraps at 2^32: the slots, the window and a grow carry on across it, so a reader
/// outside the window is stale and one inside finds every patch. With 30 appends the window
/// itself spans the wrap when the log grows.
#[test]
fn a_chain_log_keeps_its_window_across_the_u32_wrap() {
    for appends in [40, 30] {
        let block = log_across_the_wrap(appends);
        let ring = CHAIN_LOG_INITIAL as u32;
        assert_edges(&block, appends, ring, &format!("{appends} appends"));

        block.watch_chain(0, None);

        assert_eq!(
            block.read().chain_log(0).map(ChainLog::capacity),
            Some(CHAIN_LOG_GROWN),
            "the overflowed log grew"
        );
        assert_edges(&block, appends, ring, &format!("{appends} appends, grown"));
    }
}
