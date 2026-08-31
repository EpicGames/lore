// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;

use tokio::io;

use crate::util::encoding::decode_text_for_display;
use crate::util::encoding::is_utf16_bom;

async fn infer_into_buffer(path: &Path, max: u64) -> io::Result<bytes::Bytes> {
    // One backend dispatch: open + stat + first `max` bytes.
    let (_file, _metadata, head) = lore_io::IoDriver::global()
        .open_read_head(path, &lore_io::OpenOptions::new().read(true), max as usize)
        .await?;
    Ok(head)
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
        if buffer[..3] == package_file_tag {
            return true;
        }

        let package_file_tag_swapped = vec![0xC1, 0x83, 0x2A, 0x9E];
        if buffer[..3] == package_file_tag_swapped {
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

/// Window size for the streaming line scan in [`infer_is_conflicted_by_path`].
const SCAN_WINDOW: usize = 64 * 1024;

/// Check if conflict markers are present in file.
///
/// # Arguments
///
/// * `path` - A &Path that holds the path to inspect.
///
/// # Return value
///
/// * `Ok(true)` if there are conflict markers in `path`.
/// * `Ok(false)` if there are no conflict markers in `path`.
/// * `Ok(false)` if `path` does not exist.
/// * `Error()` if an I/O error occurs.
///
/// # Notes
///
/// Streams line-by-line for UTF-8 (the hot path for large generated text).
/// UTF-16 BOM-prefixed files — which `BufReader::lines` cannot decode — are
/// read whole and routed through [`decode_text_for_display`].
pub async fn infer_is_conflicted_by_path(path: &Path) -> Result<bool, std::io::Error> {
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

    let (file, metadata, head) = match lore_io::IoDriver::global()
        .open_read_head(path, &lore_io::OpenOptions::new().read(true), SCAN_WINDOW)
        .await
    {
        Ok(parts) => parts,
        Err(_) => return Ok(false),
    };
    let file_size = metadata.len();

    if head.len() >= 2 && is_utf16_bom(&head[..2]) {
        let bytes = if head.len() as u64 == file_size {
            head
        } else {
            file.read_exact_at(file_size as usize, 0).await?
        };
        return Ok(infer_is_conflicted_by_str(&decode_text_for_display(&bytes)));
    }

    // Stream fixed windows, scanning complete lines and carrying the
    // trailing partial line across window boundaries.
    let mut carry: Vec<u8> = Vec::new();
    let mut offset = 0u64;
    let mut window = head;
    loop {
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
        offset += window.len() as u64;
        if offset >= file_size {
            break;
        }
        let length = std::cmp::min(SCAN_WINDOW as u64, file_size - offset) as usize;
        window = file.read_exact_at(length, offset).await?;
    }
    Ok(!carry.is_empty() && matches!(scan_line(&carry), LineScan::Conflicted))
}

/// Checks if a file contains diffable data.
///
/// # Arguments
///
/// * `path` - An absolute path to the file to check.
pub async fn infer_is_diffable_by_path(path: &Path) -> io::Result<bool> {
    // Inspect the first 4 KiB of the file at most.
    let buffer = infer_into_buffer(path, 4 * 1024).await?;
    Ok(infer_is_diffable_by_slice(buffer.as_ref()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::SCAN_WINDOW;
    use super::infer_is_conflicted_by_path;
    use super::infer_is_diffable_by_slice;
    use super::infer_is_utf8_by_slice;

    /// A file holding `contents`, alive for as long as the returned directory is.
    #[allow(clippy::disallowed_methods)] // A test fixture in its own temporary directory.
    fn file_holding(contents: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("scanned");
        std::fs::write(&path, contents).expect("write scanned file");
        (dir, path)
    }

    #[test]
    fn is_utf8() {
        let one_sparkle_heart = vec![240, 159, 146, 150];
        assert!(
            infer_is_utf8_by_slice(&one_sparkle_heart),
            "One sparkle heart is UTF-8"
        );

        let two_sparkle_hearts = vec![240, 159, 146, 150, 240, 159, 146, 150];
        assert!(
            infer_is_utf8_by_slice(&two_sparkle_hearts),
            "Two sparkle hearts are UTF-8"
        );

        let two_sparkle_hearts_truncated = vec![240, 159, 146, 150, 240, 159, 146];
        assert!(
            !infer_is_utf8_by_slice(&two_sparkle_hearts_truncated),
            "Two sparkle hearts with the last one truncated does not count as UTF-8"
        );

        let three_sparkle_heart_invalid = vec![240, 159, 146, 150, 240, 159, 240, 159, 146, 150];
        assert!(
            !infer_is_utf8_by_slice(&three_sparkle_heart_invalid),
            "Three sparkle hearts with the middle one being invalid does not count as UTF-8"
        );
    }

    #[test]
    fn non_diffable_utf16_le_bom() {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend("Hello\nWorld\n".encode_utf16().flat_map(u16::to_le_bytes));
        assert!(
            !infer_is_diffable_by_slice(&bytes),
            "UTF-16 LE BOM must be non-diffable so merge falls into the binary-conflict path that preserves bytes"
        );
    }

    #[test]
    fn non_diffable_utf16_be_bom() {
        let mut bytes = vec![0xFE, 0xFF];
        bytes.extend("Hello\nWorld\n".encode_utf16().flat_map(u16::to_be_bytes));
        assert!(
            !infer_is_diffable_by_slice(&bytes),
            "UTF-16 BE BOM must be non-diffable so merge falls into the binary-conflict path that preserves bytes"
        );
    }

    #[tokio::test]
    async fn marker_within_one_window_is_conflicted() {
        let (_dir, path) = file_holding(b"clean line\n<<<<<<< ours\nmore\n");
        assert!(infer_is_conflicted_by_path(&path).await.unwrap());
    }

    /// The marker's own bytes straddle the window boundary: three of them end the first window
    /// and the rest open the second, so only the carry joining them sees a marker at all.
    #[tokio::test]
    async fn marker_split_across_a_window_boundary_is_conflicted() {
        let mut contents = vec![b'a'; SCAN_WINDOW - 4];
        contents.push(b'\n');
        contents.extend_from_slice(b"<<<<<<< ours\n");

        let (_dir, path) = file_holding(&contents);
        assert!(infer_is_conflicted_by_path(&path).await.unwrap());
    }

    /// A marker line ending `\r\n` with the carriage return the last byte of one window and the
    /// newline the first of the next. The line is only stripped after the carry joins them, so
    /// this is what says the strip happens on the joined line rather than per window.
    #[tokio::test]
    async fn crlf_split_across_a_window_boundary_is_conflicted() {
        let marker = b"<<<<<<< ours";
        let mut contents = marker.to_vec();
        contents.extend(std::iter::repeat_n(b' ', SCAN_WINDOW - 1 - marker.len()));
        contents.extend_from_slice(b"\r\n");
        assert_eq!(contents[SCAN_WINDOW - 1], b'\r');
        assert_eq!(contents[SCAN_WINDOW], b'\n');

        let (_dir, path) = file_holding(&contents);
        assert!(infer_is_conflicted_by_path(&path).await.unwrap());
    }

    /// The last line of a file with no trailing newline never reaches the in-window scan, only
    /// the carry left over once the windows run out.
    #[tokio::test]
    async fn marker_on_an_unterminated_final_line_is_conflicted() {
        let (_dir, path) = file_holding(b"clean line\n>>>>>>> theirs");
        assert!(infer_is_conflicted_by_path(&path).await.unwrap());
    }

    #[tokio::test]
    async fn unterminated_final_line_without_a_marker_is_clean() {
        let (_dir, path) = file_holding(b"clean line\nalso clean");
        assert!(!infer_is_conflicted_by_path(&path).await.unwrap());
    }

    /// A UTF-16 file is read whole rather than scanned in windows, so one larger than a window
    /// exercises the second read the head does not cover.
    #[tokio::test]
    async fn utf16_larger_than_one_window_is_conflicted() {
        let text = format!("{}\n<<<<<<< ours\n", "a".repeat(SCAN_WINDOW));
        let mut contents = vec![0xFF, 0xFE];
        contents.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        assert!(contents.len() > SCAN_WINDOW);

        let (_dir, path) = file_holding(&contents);
        assert!(infer_is_conflicted_by_path(&path).await.unwrap());
    }

    /// An empty file has no window to scan and no carry to check, and must end rather than wait
    /// for a read that never comes.
    #[tokio::test]
    async fn empty_file_is_clean() {
        let (_dir, path) = file_holding(b"");
        assert!(!infer_is_conflicted_by_path(&path).await.unwrap());
    }

    /// A line that is not text ends the scan, so a marker after one is never reached. This is
    /// the line reader's behaviour that the windowed scan replaced, kept deliberately.
    #[tokio::test]
    async fn a_line_that_is_not_utf8_ends_the_scan() {
        let (_dir, path) = file_holding(b"clean line\n\xC3\x28 broken\n<<<<<<< ours\n");
        assert!(!infer_is_conflicted_by_path(&path).await.unwrap());
    }

    #[tokio::test]
    async fn a_missing_file_is_clean() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("absent");
        assert!(!infer_is_conflicted_by_path(&path).await.unwrap());
    }
}
