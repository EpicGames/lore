// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Windows path helpers for direct Win32 file API calls.

#![cfg(target_family = "windows")]

use std::path::Path;

/// Convert a Windows path into a null-terminated UTF-16 buffer suitable for
/// passing to a Win32 file API (e.g. `MoveFileExW`).
///
/// Paths shorter than `MAX_PATH` are returned as a plain wide-encoded copy —
/// Windows accepts them as-is without the verbatim prefix, and forward
/// slashes are handled by kernel path normalisation. Anything that would
/// otherwise exceed `MAX_PATH` is rewritten with the `\\?\` verbatim prefix
/// so it bypasses the 260-character limit regardless of the longPathAware
/// manifest or the `HKLM\...\FileSystem\LongPathsEnabled` registry key. The
/// verbatim form requires canonical `\` separators, so `/` is rewritten to
/// `\` on that path.
///
/// - Drive-letter absolute paths (`X:\...`) become `\\?\X:\...`.
/// - UNC paths (`\\server\share\...`) become `\\?\UNC\server\share\...`.
/// - Paths already prefixed with `\\?\` or `\\.\` pass through unchanged.
/// - Anything else (relative paths, oddly shaped inputs) is encoded as-is
///   and null-terminated; the OS reports the error.
///
/// `std::fs` and `tokio::fs` wrappers already apply this prefix internally
/// and do not need this helper.
pub fn to_extended_wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    const BACKSLASH: u16 = b'\\' as u16;
    const FORWARD_SLASH: u16 = b'/' as u16;
    const QUESTION: u16 = b'?' as u16;
    const DOT: u16 = b'.' as u16;
    const COLON: u16 = b':' as u16;
    const PREFIX_LOCAL: [u16; 4] = [BACKSLASH, BACKSLASH, QUESTION, BACKSLASH];
    const PREFIX_UNC: [u16; 8] = [
        BACKSLASH,
        BACKSLASH,
        QUESTION,
        BACKSLASH,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        BACKSLASH,
    ];
    // MoveFileExW accepts a non-prefixed path up to 259 characters plus the
    // null terminator. Below that, no prefix is required.
    const MAX_PATH: usize = 260;
    // The longest prefix we inject is `\\?\UNC\` (8 wide chars), replacing
    // a leading `\\` (2 wide chars) — a net +6 wide chars. Padding the
    // buffer up front keeps the prefix branch to a single allocation.
    const MAX_PREFIX_OVERHEAD: usize = PREFIX_UNC.len() - 2;

    let os_str = path.as_os_str();
    let byte_len = os_str.len();

    // Fast path: a path whose WTF-8 byte count fits inside `MAX_PATH` also
    // fits as UTF-16 (multi-byte sequences shrink during decoding), so we can
    // skip prefix detection, skip the `/` to `\` rewrite, and just return a
    // null-terminated wide copy.
    if byte_len < MAX_PATH {
        let mut out: Vec<u16> = Vec::with_capacity(byte_len + 1);
        out.extend(os_str.encode_wide());
        out.push(0);
        return out;
    }

    // Inspect the leading bytes to decide the prefix shape. We are past the
    // `byte_len < MAX_PATH` fast path, so the path has at least 260 bytes and
    // direct indexing of the first four is safe. All characters we care
    // about (`\`, `?`, `.`, `:`, `/`) are ASCII, and on Windows `OsStr` is
    // WTF-8 encoded, so ASCII byte values match the corresponding wide
    // chars one-to-one. Forward slashes are normalised here so paths like
    // `Z:/foo` are recognised as drive-absolute.
    let bytes = os_str.as_encoded_bytes();
    let normalise = |b: u8| -> u16 {
        let c = b as u16;
        if c == FORWARD_SLASH { BACKSLASH } else { c }
    };
    let b0 = normalise(bytes[0]);
    let b1 = normalise(bytes[1]);
    let b2 = normalise(bytes[2]);
    let b3 = normalise(bytes[3]);

    let already_extended =
        b0 == BACKSLASH && b1 == BACKSLASH && (b2 == QUESTION || b2 == DOT) && b3 == BACKSLASH;
    let is_unc = !already_extended && b0 == BACKSLASH && b1 == BACKSLASH;
    let is_drive_absolute = !already_extended && !is_unc && b1 == COLON && b2 == BACKSLASH;

    let mut out: Vec<u16> = Vec::with_capacity(byte_len + MAX_PREFIX_OVERHEAD + 1);

    // Emit the prefix (if any). `skip` counts how many of the original wide
    // chars are consumed by the prefix replacement, so the subsequent
    // `encode_wide` pass starts after them.
    let skip = if is_unc {
        out.extend_from_slice(&PREFIX_UNC);
        2
    } else if is_drive_absolute {
        out.extend_from_slice(&PREFIX_LOCAL);
        0
    } else {
        0
    };

    out.extend(
        os_str
            .encode_wide()
            .skip(skip)
            .map(|c| if c == FORWARD_SLASH { BACKSLASH } else { c }),
    );
    out.push(0);
    out
}
