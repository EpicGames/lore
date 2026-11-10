// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Quantities as they are shown to a person.

/// A byte count in the largest unit that leaves it above one.
///
/// A raw byte count renders as an integer, a fraction of a byte saying nothing.
/// Only the scaled units carry decimals, the remainder there distinguishing
/// 1.02 MiB from 1.98 MiB.
pub fn format_bytes_to_string(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;

    let (converted, unit) = if bytes > GIB {
        ((bytes / MIB) as f64 / 1024.0, "GiB")
    } else if bytes > MIB {
        ((bytes / KIB) as f64 / 1024.0, "MiB")
    } else if bytes > KIB {
        (bytes as f64 / 1024.0, "KiB")
    } else {
        return format!("{bytes} bytes");
    };

    format!("{converted:.2} {unit}")
}
