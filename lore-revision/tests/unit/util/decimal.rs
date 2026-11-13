// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::util::to_decimal_str;

/// The digits are those `to_string` writes, at both sides of every power of ten up to the 20 digits
/// of `u64::MAX`, in a buffer an earlier, longer number filled.
#[test]
fn to_decimal_str_writes_the_digits_to_string_writes() {
    let mut digits = [0u8; 20];
    let values = (0..20)
        .map(|exponent| 10u64.pow(exponent))
        .flat_map(|power| [power - 1, power])
        .chain([u64::MAX, 1300, 0]);

    for value in values {
        assert_eq!(to_decimal_str(value, &mut digits), value.to_string());
    }
}
