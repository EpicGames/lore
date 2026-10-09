// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::interface::LoreString;

#[test]
fn lore_string_serializes_as_its_text() {
    let null_string = LoreString {
        string: std::ptr::null(),
        length: 0,
    };
    for (value, text) in [
        (LoreString::from("abc"), "abc"),
        (LoreString::from("ab\nc"), "ab\nc"),
        (LoreString::from(""), ""),
        (null_string, ""),
    ] {
        assert_eq!(serde_json::to_value(&value).unwrap(), text);
    }
}
