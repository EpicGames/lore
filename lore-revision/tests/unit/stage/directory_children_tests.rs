// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::node::NodeID;
use lore_revision::stage::*;

/// The children in sibling order, as `stage_directory` indexes them.
fn children(entries: &[(NodeID, u64)]) -> DirectoryChildren {
    DirectoryChildren::new(entries.to_vec(), None)
}

#[test]
fn a_claim_finds_the_child_with_that_name() {
    let mut children = children(&[(10, 0xAA), (11, 0xBB), (12, 0xCC)]);
    assert_eq!(children.claim(0xBB), Some(11));
    assert_eq!(children.claim(0xAA), Some(10));
    assert_eq!(children.claim(0xCC), Some(12));
    assert_eq!(
        children.unclaimed().collect::<Vec<_>>(),
        Vec::<NodeID>::new()
    );
}

#[test]
fn a_name_the_directory_does_not_hold_claims_nothing() {
    let mut children = children(&[(10, 0xAA)]);
    assert_eq!(children.claim(0xBB), None);
    assert_eq!(children.unclaimed().collect::<Vec<_>>(), vec![10]);
}

/// Two names differing only in case hash the same, and a claim takes the
/// first child not already claimed. Sibling order is what makes that choice
/// reproducible, so it has to survive the sort.
#[test]
fn equal_hashes_are_claimed_in_sibling_order() {
    let mut children = children(&[(10, 0xAA), (11, 0xAA), (12, 0xAA)]);
    assert_eq!(children.claim(0xAA), Some(10));
    assert_eq!(children.claim(0xAA), Some(11));
    assert_eq!(children.claim(0xAA), Some(12));
    assert_eq!(children.claim(0xAA), None);
}

/// A run of children sharing a hash, wide enough that a sort keyed on the
/// hash alone reorders it.
fn wide_runs_of_equal_hashes() -> Vec<(NodeID, u64)> {
    (0..64u64)
        .map(|index| (index as NodeID + 100, index % 3))
        .collect()
}

/// The index keys on the sibling position as well as the hash, so a run of
/// equal hashes carries no ties for the sort to order as it likes.
#[test]
fn the_index_leaves_no_ties_among_equal_hashes() {
    let children = children(&wide_runs_of_equal_hashes());
    assert!(
        children
            .by_name_hash
            .windows(2)
            .all(|pair| pair[0] < pair[1]),
        "the index must be strictly ordered: {:?}",
        children.by_name_hash
    );
}

/// A claim takes the child out of what the listing offers, not out of what
/// it holds: a search of the chain still reaches it, so the index still
/// answers for it.
#[test]
fn a_claimed_child_is_still_held() {
    let mut children = children(&[(10, 0xAA), (11, 0xAA), (12, 0xBB)]);
    assert_eq!(children.claim(0xAA), Some(10));
    assert_eq!(children.holds(0xAA), Some(10));
    assert_eq!(children.claim(0xAA), Some(11));
    assert_eq!(children.claim(0xAA), None, "the listing offers no more");
    assert_eq!(children.holds(0xAA), Some(10), "the listing still holds it");
    assert_eq!(children.holds(0xCC), None, "a name it never held");
}

#[test]
fn what_nothing_claimed_comes_back_in_sibling_order() {
    let mut children = children(&[(10, 0xAA), (11, 0xBB), (12, 0xAA), (13, 0xCC)]);
    assert_eq!(children.claim(0xAA), Some(10));
    assert_eq!(children.claim(0xCC), Some(13));
    assert_eq!(children.unclaimed().collect::<Vec<_>>(), vec![11, 12]);
}

#[test]
fn an_empty_directory_claims_nothing_and_deletes_nothing() {
    let mut children = children(&[]);
    assert_eq!(children.claim(0xAA), None);
    assert_eq!(children.unclaimed().count(), 0);
}

/// The first child carrying `name_hash` that nothing has taken, taken, found
/// by scanning `scanned` in sibling order.
fn scan_claim(scanned: &mut [Option<(NodeID, u64)>], name_hash: u64) -> Option<NodeID> {
    let position = scanned
        .iter()
        .position(|entry| entry.is_some_and(|(_, hash)| hash == name_hash))?;
    scanned[position].take().map(|(node, _)| node)
}

/// The index has to agree with a scan of the same children on every input,
/// not just the ones written out above: same children, same sequence of
/// claims, same answers and same leftovers.
///
/// The claims cover every hash the children hold and two they do not, each
/// asked for more times than the children can answer, so duplicates, misses
/// and exhausted runs all occur.
#[test]
fn the_index_answers_exactly_as_a_scan_of_the_same_children_would() {
    let entries = wide_runs_of_equal_hashes();
    let mut indexed = children(&entries);
    let mut scanned: Vec<Option<(NodeID, u64)>> = entries.iter().copied().map(Some).collect();

    for step in 0..160u64 {
        let name_hash = (step * 7) % 5;
        assert_eq!(
            indexed.claim(name_hash),
            scan_claim(&mut scanned, name_hash),
            "claim({name_hash}) at step {step}"
        );
    }
    assert_eq!(
        indexed.unclaimed().collect::<Vec<_>>(),
        scanned
            .iter()
            .filter_map(|entry| entry.map(|(node, _)| node))
            .collect::<Vec<_>>()
    );
}
