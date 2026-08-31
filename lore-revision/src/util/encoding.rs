// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
/// Returns `true` when `bytes` starts with a UTF-16 LE (FF FE) or BE (FE FF) byte-order mark.
pub fn is_utf16_bom(bytes: &[u8]) -> bool {
    bytes.len() >= 2
        && ((bytes[0] == 0xFF && bytes[1] == 0xFE) || (bytes[0] == 0xFE && bytes[1] == 0xFF))
}

/// Decode raw file bytes to a UTF-8 `String` suitable for **display or
/// read-only inspection**: diff rendering, conflict-marker scanning, log
/// output, and similar paths where the caller never writes the result back
/// to disk.
///
/// Strips a UTF-8 BOM (`EF BB BF`) and converts UTF-16 LE / BE BOM input to
/// UTF-8. Both transformations are lossy from a round-trip-to-disk
/// perspective: the BOM bytes are gone, and the UTF-16 byte order is
/// gone. Never feed this function's output into a writer that persists to
/// disk — use `String::from_utf8_lossy` (which is a lossless passthrough
/// for valid UTF-8, including UTF-8 BOM) for that case.
pub fn decode_text_for_display(bytes: &[u8]) -> String {
    // UTF-16 LE BOM (FF FE) — common for files created by Windows PowerShell
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let payload = &bytes[2..];
        let u16_iter = payload
            .as_chunks::<2>()
            .0
            .iter()
            .map(|chunk| u16::from_le_bytes(*chunk));
        return char::decode_utf16(u16_iter)
            .map(|r| r.unwrap_or('\u{FFFD}'))
            .collect();
    }

    // UTF-16 BE BOM (FE FF)
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let payload = &bytes[2..];
        let u16_iter = payload
            .as_chunks::<2>()
            .0
            .iter()
            .map(|chunk| u16::from_be_bytes(*chunk));
        return char::decode_utf16(u16_iter)
            .map(|r| r.unwrap_or('\u{FFFD}'))
            .collect();
    }

    // UTF-8 BOM (EF BB BF) — strip BOM, decode as UTF-8
    if bytes.len() >= 3 && bytes[0] == 0xEF && bytes[1] == 0xBB && bytes[2] == 0xBF {
        return String::from_utf8_lossy(&bytes[3..]).into_owned();
    }

    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_utf8_passthrough() {
        let input = b"Hello World\nSecond line\n";
        assert_eq!(decode_text_for_display(input), "Hello World\nSecond line\n");
    }

    #[test]
    fn decode_utf16_le_with_bom() {
        let content = "Hello World\nLine two\n";
        let mut bytes = vec![0xFF, 0xFE]; // UTF-16 LE BOM
        bytes.extend(content.encode_utf16().flat_map(|c| c.to_le_bytes()));
        assert_eq!(decode_text_for_display(&bytes), content);
    }

    #[test]
    fn decode_utf16_be_with_bom() {
        let content = "Hello World\nLine two\n";
        let mut bytes = vec![0xFE, 0xFF]; // UTF-16 BE BOM
        bytes.extend(content.encode_utf16().flat_map(|c| c.to_be_bytes()));
        assert_eq!(decode_text_for_display(&bytes), content);
    }

    #[test]
    fn decode_utf8_with_bom() {
        let content = "Hello World\n";
        let mut bytes = vec![0xEF, 0xBB, 0xBF]; // UTF-8 BOM
        bytes.extend(content.as_bytes());
        assert_eq!(decode_text_for_display(&bytes), content);
    }

    #[test]
    fn decode_utf16_le_with_crlf() {
        let content = "Line one\r\nLine two\r\n";
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(content.encode_utf16().flat_map(|c| c.to_le_bytes()));
        assert_eq!(decode_text_for_display(&bytes), content);
    }

    #[test]
    fn decode_empty_input() {
        assert_eq!(decode_text_for_display(b""), "");
    }

    #[test]
    fn is_utf16_bom_detects_le() {
        assert!(is_utf16_bom(&[0xFF, 0xFE, 0x00]));
    }

    #[test]
    fn is_utf16_bom_detects_be() {
        assert!(is_utf16_bom(&[0xFE, 0xFF, 0x00]));
    }

    #[test]
    fn is_utf16_bom_rejects_short_or_other() {
        assert!(!is_utf16_bom(b""));
        assert!(!is_utf16_bom(&[0xFF]));
        assert!(!is_utf16_bom(&[0xEF, 0xBB, 0xBF]));
        assert!(!is_utf16_bom(b"hello"));
    }
}
