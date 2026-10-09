// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::interface::LoreBinary;

/// `LoreBinary` owns its payload, so a clone survives the original being
/// dropped. Before it owned anything, the clone was a copy of a pointer and
/// this read freed memory.
#[test]
fn a_clone_outlives_the_value_it_came_from() {
    let clone = {
        let original = LoreBinary::from_bytes(&[0xde, 0xad, 0xbe, 0xef]);
        original.clone()
    };
    assert_eq!(clone.as_bytes(), &[0xde, 0xad, 0xbe, 0xef]);
}

#[test]
fn an_empty_block_is_a_null_pointer_of_zero_length() {
    let empty = LoreBinary::from_bytes(&[]);
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);
    assert!(empty.payload.is_null());
    assert_eq!(empty.as_bytes(), &[] as &[u8]);
    assert_eq!(empty, LoreBinary::default());
}

/// Equality is by content, not by length or by pointer identity: two blocks
/// of the same size holding different bytes are different values.
#[test]
fn blocks_of_equal_length_compare_by_content() {
    let block = LoreBinary::from_bytes(&[1, 2, 3, 4]);
    assert_eq!(block, LoreBinary::from_bytes(&[1, 2, 3, 4]));
    assert_ne!(block, LoreBinary::from_bytes(&[1, 2, 3, 5]));
    assert_ne!(block, LoreBinary::from_bytes(&[1, 2, 3]));
}

/// An empty block is the one input where the text encoding carries no characters at all.
#[test]
fn an_empty_block_serializes_as_empty_text() {
    let json = serde_json::to_string(&LoreBinary::from_bytes(&[])).expect("json serialize");
    assert_eq!(json, r#""""#);
}
