// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_storage::ContentHandle;
use lore_storage::ContentSource;
use lore_storage::WindowRead;
use tokio::io;

use crate::util::encoding::decode_text_for_display;
use crate::util::encoding::is_utf16_bom;

/// The head of `source`, at most `max` bytes of it.
async fn infer_into_buffer(source: &ContentSource<'_>, max: u64) -> io::Result<bytes::Bytes> {
    // TODO(mjansson): Fuse the open and the head read through an `open_read_head` on
    // `ContentSource`, which naming a host path did in one dispatch.
    let (handle, size) = source.open_once().await?;
    handle.read_all(std::cmp::min(max, size) as usize).await
}

pub fn infer_type_by_slice(buffer: &[u8]) -> Option<&str> {
    // Is it containing a magic marker known to the infer crate?
    if let Some(kind) = infer::get(buffer) {
        return Some(kind.mime_type());
    }

    None
}

pub fn infer_is_utf8_by_slice(buffer: &[u8]) -> bool {
    // Is it containing a valid utf8 string?
    std::str::from_utf8(buffer).is_ok()
}

pub fn infer_is_upackage_by_slice(buffer: &[u8]) -> bool {
    // Is it containing an unreal magic marker?
    if buffer.len() >= 4 {
        let package_file_tag = vec![0x9E, 0x2A, 0x83, 0xC1];
        if buffer[..4] == package_file_tag {
            return true;
        }

        let package_file_tag_swapped = vec![0xC1, 0x83, 0x2A, 0x9E];
        if buffer[..4] == package_file_tag_swapped {
            return true;
        }
    }

    false
}

pub fn infer_is_diffable_by_slice(buffer: &[u8]) -> bool {
    // Check if it's an unreal asset.
    if infer_is_upackage_by_slice(buffer) {
        return false;
    }

    // Check if it's a non-diffable mime type.
    if let Some(mime_type) = infer_type_by_slice(buffer)
        && mime_type != "text/html"
        && mime_type != "text/x-shellscript"
        && mime_type != "text/xml"
    {
        return false;
    }

    // Check if it's a utf8 string.
    // Do this on substrings to disregard utf8 truncation at the end of the buffer.
    for n in 0..3 {
        let len = buffer.len() - n;
        if len == 0 {
            return false;
        }

        if infer_is_utf8_by_slice(&buffer[..len]) {
            return true;
        }
    }

    false
}

/// Check if conflict markers are present in line.
///
/// # Arguments
///
/// * `line` - A &str that holds the text to inspect.
///
/// # Return value
///
/// * `true` if there are conflict markers in `line`.
/// * `false` if there are no conflict markers in `line`.
///
fn infer_is_conflicted_by_line(line: &str) -> bool {
    if line.starts_with("||||||| ") {
        return true;
    }
    if line.starts_with("<<<<<<< ") {
        return true;
    }
    if line.starts_with(">>>>>>> ") {
        return true;
    }

    false
}

/// Check if conflict markers are present in text.
///
/// # Arguments
///
/// * `text` - A &str that holds the text to inspect.
///
/// # Return value
///
/// * `true` if there are conflict markers in `text`.
/// * `false` if there are no conflict markers in `text`.
///
pub fn infer_is_conflicted_by_str(text: &str) -> bool {
    for line in text.lines() {
        if infer_is_conflicted_by_line(line) {
            return true;
        }
    }

    false
}

/// What [`infer_is_conflicted`] reads at a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictMarkers {
    /// A file holding conflict markers.
    Present,
    /// A file holding none, or one that cannot be opened for another reason than its absence.
    Absent,
    /// No file.
    NoFile,
}

/// Window size for the streaming line scan in [`infer_is_conflicted`].
#[lore_macro::test_pub]
const SCAN_WINDOW: usize = 64 * 1024;

/// Check if conflict markers are present in content.
///
/// # Arguments
///
/// * `source` - Where the content to inspect is read from.
///
/// # Return value
///
/// * `Ok(ConflictMarkers::Present)` if there are conflict markers in `source`.
/// * `Ok(ConflictMarkers::Absent)` if there are none, or `source` cannot be opened for another
///   reason than its absence.
/// * `Ok(ConflictMarkers::NoFile)` if `source` holds no file to open.
/// * `Error()` if an I/O error occurs.
///
/// # Notes
///
/// Streams line-by-line for UTF-8 (the hot path for large generated text).
/// UTF-16 BOM-prefixed files — which `BufReader::lines` cannot decode — are
/// read whole and routed through [`decode_text_for_display`].
///
/// Reads through the source rather than a host path, so content a provider serves rather than
/// the filesystem is scanned where it is held.
pub async fn infer_is_conflicted(
    source: &ContentSource<'_>,
) -> Result<ConflictMarkers, std::io::Error> {
    // TODO(mjansson): Fuse the open and the first window read through an `open_read_head` on
    // `ContentSource`, which naming a host path did in one dispatch.
    let (handle, file_size) = match source.open_once().await {
        Ok(opened) => opened,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(ConflictMarkers::NoFile),
        Err(_) => return Ok(ConflictMarkers::Absent),
    };
    Ok(if holds_conflict_markers(&handle, file_size).await? {
        ConflictMarkers::Present
    } else {
        ConflictMarkers::Absent
    })
}

/// Whether the `file_size` bytes `handle` reads hold conflict markers.
async fn holds_conflict_markers(handle: &ContentHandle, file_size: u64) -> io::Result<bool> {
    /// Mirrors the previous line reader: a line that is not valid UTF-8
    /// ends the scan as not-conflicted.
    enum LineScan {
        Conflicted,
        Clean,
        NotText,
    }
    fn scan_line(line: &[u8]) -> LineScan {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        match std::str::from_utf8(line) {
            Ok(text) if infer_is_conflicted_by_line(text) => LineScan::Conflicted,
            Ok(_) => LineScan::Clean,
            Err(_) => LineScan::NotText,
        }
    }

    // One buffer for every window: the scan carries its trailing partial line in `carry`, so a
    // window is scanned and refilled rather than held. Sized to the first window, the largest
    // any of them asks for.
    let mut filled = std::cmp::min(SCAN_WINDOW as u64, file_size) as usize;
    // SAFETY: only `buffer[..filled]` is read, which the read before it filled exactly.
    let mut buffer = unsafe { lore_io::uninit_buffer(filled) };
    buffer = handle
        .read_window(WindowRead::new(buffer, 0, filled), 0)
        .await?;

    if filled >= 2 && is_utf16_bom(&buffer[..2]) {
        let bytes = if filled as u64 == file_size {
            buffer.freeze()
        } else {
            handle.read_all(file_size as usize).await?
        };
        return Ok(infer_is_conflicted_by_str(&decode_text_for_display(&bytes)));
    }

    // Stream fixed windows, scanning complete lines and carrying the
    // trailing partial line across window boundaries.
    let mut carry: Vec<u8> = Vec::new();
    let mut offset = 0u64;
    loop {
        let window = &buffer[..filled];
        let mut start = 0usize;
        while let Some(newline) = window[start..].iter().position(|&byte| byte == b'\n') {
            let end = start + newline;
            let result = if carry.is_empty() {
                scan_line(&window[start..end])
            } else {
                carry.extend_from_slice(&window[start..end]);
                let result = scan_line(&carry);
                carry.clear();
                result
            };
            match result {
                LineScan::Conflicted => return Ok(true),
                LineScan::NotText => return Ok(false),
                LineScan::Clean => {}
            }
            start = end + 1;
        }
        carry.extend_from_slice(&window[start..]);
        offset += filled as u64;
        if offset >= file_size {
            break;
        }
        filled = std::cmp::min(SCAN_WINDOW as u64, file_size - offset) as usize;
        buffer = handle
            .read_window(WindowRead::new(buffer, 0, filled), offset)
            .await?;
    }
    Ok(!carry.is_empty() && matches!(scan_line(&carry), LineScan::Conflicted))
}

/// Checks if content contains diffable data.
///
/// # Arguments
///
/// * `source` - Where the content to check is read from.
pub async fn infer_is_diffable(source: &ContentSource<'_>) -> io::Result<bool> {
    // Inspect the first 4 KiB of the content at most.
    let buffer = infer_into_buffer(source, 4 * 1024).await?;
    Ok(infer_is_diffable_by_slice(buffer.as_ref()))
}
