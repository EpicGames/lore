// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::borrow::Cow;

use crate::errors::InvalidArguments;

/// The order a UTF-16 buffer pairs its bytes in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Utf16ByteOrder {
    Little,
    Big,
}

impl Utf16ByteOrder {
    /// The name the byte order goes by in a message a person reads.
    fn label(self) -> &'static str {
        match self {
            Self::Little => "LE",
            Self::Big => "BE",
        }
    }
}

/// An encoding a byte-order mark names.
enum Encoding {
    Utf8,
    Utf16(Utf16ByteOrder),
    /// Recognised only so that it can be refused: Lore does not decode UTF-32.
    Utf32,
}

/// Splits a leading byte-order mark off `bytes`, naming the encoding it marks
/// and returning the bytes after it.
///
/// The marks are `FF FE 00 00` for UTF-32 LE, `00 00 FE FF` for UTF-32 BE,
/// `FF FE` for UTF-16 LE, `FE FF` for UTF-16 BE and `EF BB BF` for UTF-8. UTF-32
/// LE is matched before UTF-16 LE because it opens with the same two bytes, and
/// a UTF-32 file read as UTF-16 decodes to its text with a NUL beside every
/// character rather than to anything a reader would question.
///
/// A mark is the one case where the encoding does not have to be inferred from
/// the text, so every decode consults this first.
fn split_bom(bytes: &[u8]) -> Option<(Encoding, &[u8])> {
    if let Some(payload) = bytes.strip_prefix(&[0xFF, 0xFE, 0x00, 0x00]) {
        return Some((Encoding::Utf32, payload));
    }
    if let Some(payload) = bytes.strip_prefix(&[0x00, 0x00, 0xFE, 0xFF]) {
        return Some((Encoding::Utf32, payload));
    }
    if let Some(payload) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return Some((Encoding::Utf16(Utf16ByteOrder::Little), payload));
    }
    if let Some(payload) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return Some((Encoding::Utf16(Utf16ByteOrder::Big), payload));
    }
    if let Some(payload) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return Some((Encoding::Utf8, payload));
    }
    None
}

/// The code units of an already-paired UTF-16 payload, read in `order`.
fn utf16_units(pairs: &[[u8; 2]], order: Utf16ByteOrder) -> impl Iterator<Item = u16> + '_ {
    pairs.iter().map(move |pair| match order {
        Utf16ByteOrder::Little => u16::from_le_bytes(*pair),
        Utf16ByteOrder::Big => u16::from_be_bytes(*pair),
    })
}

/// Decodes a UTF-16 payload, with any byte-order mark already removed,
/// substituting U+FFFD for an unpaired surrogate as `String::from_utf8_lossy`
/// does for a malformed sequence.
///
/// A trailing odd byte cannot complete a code unit and is dropped.
fn decode_utf16_lossy(payload: &[u8], order: Utf16ByteOrder) -> String {
    char::decode_utf16(utf16_units(payload.as_chunks::<2>().0, order))
        .map(|unit| unit.unwrap_or('\u{FFFD}'))
        .collect()
}

/// Decodes a UTF-16 payload, with any byte-order mark already removed, refusing
/// a trailing byte that cannot complete a code unit and any unpaired surrogate.
fn decode_utf16(payload: &[u8], order: Utf16ByteOrder) -> Result<String, InvalidArguments> {
    let (pairs, remainder) = payload.as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(InvalidArguments {
            reason: format!("UTF-16 {} ends mid code unit", order.label()),
        });
    }
    char::decode_utf16(utf16_units(pairs, order))
        .collect::<Result<String, _>>()
        .map_err(|error| InvalidArguments {
            reason: format!("not valid UTF-16 {}: {error}", order.label()),
        })
}

/// The refusal UTF-32 meets wherever it is recognised, by its mark or by its
/// shape: Lore does not decode it.
fn refuse_utf32() -> InvalidArguments {
    InvalidArguments {
        reason: "UTF-32 is not a supported encoding".to_string(),
    }
}

/// Decodes UTF-8, with any byte-order mark already removed, refusing a malformed
/// sequence.
fn decode_utf8(bytes: &[u8]) -> Result<&str, InvalidArguments> {
    std::str::from_utf8(bytes).map_err(|error| InvalidArguments {
        reason: format!("not valid UTF-8: {error}"),
    })
}

/// Refuses decoded text holding a NUL, whatever encoding produced it.
///
/// No platform admits a NUL in a path, so a rule holding one matches nothing,
/// which is the silent failure this decode exists to prevent. A mark settles
/// what the bytes are, not what a rule may hold, so `EF BB BF *.log\0\n` is
/// refused like any other.
///
/// On mark-less bytes a NUL also marks an encoding that went unnamed: UTF-16 too
/// far outside ASCII for [`detect_bomless_utf16`] still leaves the NULs of its
/// line endings. UTF-16 LE `你好\n` is `60 4F 7D 59 0A 00`, valid UTF-8 spelling
/// `` `O}Y `` and a NUL.
fn refuse_nul(text: &str) -> Result<(), InvalidArguments> {
    match text.find('\0') {
        Some(offset) => Err(InvalidArguments {
            reason: format!(
                "NUL at byte {offset} of the decoded text: no rule can hold one, \
                 and UTF-16 or UTF-32 needs a byte-order mark to be decoded as such"
            ),
        }),
        None => Ok(()),
    }
}

/// Whether the side more NULs fall on holds enough of a majority to name the
/// encoding they are evidence of.
///
/// The winning side must cover at least half the code units, so a file merely
/// containing a NUL is not taken for a wider encoding, and it must leave the
/// other side well behind. The other side need not be empty: a character of the
/// form U+xx00, U+4E00 among them, puts a NUL on it.
fn nul_majority_holds(winner: usize, loser: usize, units: usize) -> bool {
    winner * 2 >= units && winner > loser * 4
}

/// Whether `bytes` holds UTF-32 that carries no byte-order mark.
///
/// A character in the Basic Multilingual Plane, which a filter file is made of,
/// encodes to UTF-32 as two bytes beside two NULs: the last two under LE, the
/// first two under BE. [`nul_majority_holds`] decides it, over quads rather than
/// the pairs UTF-16 is counted in.
///
/// The winning order is not reported and a trailing remainder is not weighed,
/// since UTF-32 is refused either way.
///
/// Asked before UTF-16, as [`split_bom`] matches the UTF-32 marks first. Read as
/// UTF-8, a UTF-32 buffer decodes to its text with three NULs beside every
/// character, all of them valid UTF-8.
fn is_bomless_utf32(bytes: &[u8]) -> bool {
    let units = bytes.as_chunks::<4>().0;
    if units.is_empty() {
        return false;
    }

    let (little, big) = units.iter().fold((0usize, 0usize), |(little, big), quad| {
        (
            little + usize::from(quad[2] == 0 && quad[3] == 0),
            big + usize::from(quad[0] == 0 && quad[1] == 0),
        )
    });

    nul_majority_holds(little.max(big), little.min(big), units.len())
}

/// Names the byte order of a buffer holding UTF-16 that carries no byte-order
/// mark, or `None` when the buffer is not UTF-16.
///
/// The evidence is NUL bytes: an ASCII character encodes to UTF-16 as its own
/// byte beside a NUL — at the odd offset under LE, the even one under BE — while
/// UTF-8 text has no reason to hold a NUL at all. The side the NULs fall on
/// therefore names the byte order, once [`nul_majority_holds`] is satisfied
/// there are enough of them to go on.
///
/// A trailing byte that pairs with nothing does not rule UTF-16 out; it says the
/// buffer is cut mid code unit. The order is named for it too, so that
/// [`decode_utf16`] refuses it as truncated rather than leaving ASCII beside
/// NULs to pass as UTF-8. An odd length is evidence against UTF-16 as well, so a
/// lone code unit's NUL does not settle it.
///
/// Detection needs ASCII to find. A mark-less UTF-16 file whose text lies mostly
/// outside ASCII leaves too little alternation to name an order by, and
/// [`refuse_nul`] then refuses it on the NUL its line endings leave. Only a
/// buffer with no character below U+0100 in it at all, line ending included,
/// leaves no evidence either way, and that one has to carry a mark.
fn detect_bomless_utf16(bytes: &[u8]) -> Option<Utf16ByteOrder> {
    let (units, remainder) = bytes.as_chunks::<2>();
    let minimum = if remainder.is_empty() { 1 } else { 2 };
    if units.len() < minimum {
        return None;
    }

    let (little, big) = units
        .iter()
        .fold((0usize, 0usize), |(little, big), [even, odd]| {
            (
                little + usize::from(*odd == 0),
                big + usize::from(*even == 0),
            )
        });

    let (order, winner, loser) = if little >= big {
        (Utf16ByteOrder::Little, little, big)
    } else {
        (Utf16ByteOrder::Big, big, little)
    };
    nul_majority_holds(winner, loser, units.len()).then_some(order)
}

/// Decodes raw file bytes for **display or read-only inspection**: diff
/// rendering, conflict-marker scanning, log output, and similar paths where the
/// caller never writes the result back to disk.
///
/// Strips a UTF-8 BOM (`EF BB BF`) and converts UTF-16 LE / BE BOM input to
/// UTF-8. Both transformations are lossy from a round-trip-to-disk
/// perspective: the BOM bytes are gone, and the UTF-16 byte order is
/// gone. Never feed this function's output into a writer that persists to
/// disk — use `String::from_utf8_lossy` (which is a lossless passthrough
/// for valid UTF-8, including UTF-8 BOM) for that case.
///
/// Borrows valid UTF-8, marked or not, and allocates only to transcode UTF-16 or
/// to stand U+FFFD in for what it cannot read. Malformed input is rendered
/// rather than refused, because something unreadable on screen is more use to a
/// person than nothing at all. Use [`decode_text_for_parsing`] where the result
/// is read by code, which needs the opposite.
///
/// Acts on a byte-order mark only, so mark-less UTF-16 is not recognised, and
/// UTF-32 is left as the bytes it is rather than read as the UTF-16 its mark
/// opens like.
pub fn decode_text_for_display(bytes: &[u8]) -> Cow<'_, str> {
    match split_bom(bytes) {
        Some((Encoding::Utf16(order), payload)) => Cow::Owned(decode_utf16_lossy(payload, order)),
        Some((Encoding::Utf8, payload)) => String::from_utf8_lossy(payload),
        Some((Encoding::Utf32, _)) | None => String::from_utf8_lossy(bytes),
    }
}

/// Decodes the bytes of a text file Lore **parses** — a filter file, and
/// anything else read for its lines rather than shown to a person.
///
/// Accepts the six shapes such a file arrives in: UTF-8, UTF-16 LE and UTF-16
/// BE, each with or without a byte-order mark. The mark is consumed rather than
/// decoded, so the first line parses as itself rather than as itself behind an
/// invisible U+FEFF. Where [`decode_text_for_display`] acts on a mark alone,
/// this reads the bytes themselves as well, through [`is_bomless_utf32`] and
/// [`detect_bomless_utf16`]: a parser has nothing downstream that would notice
/// text arriving as characters alternating with NULs, and would take the NULs as
/// part of the rules.
///
/// Mark-less UTF-16 is inferred rather than declared, so it carries a bound.
/// Rules mostly of ASCII are detected and decoded; rules mostly outside ASCII
/// are refused on the NUL their line endings leave, rather than decoded as the
/// unrelated UTF-8 text the same bytes spell; a buffer with no character below
/// U+0100 in it at all has to carry a mark.
///
/// No decode yields a NUL, whatever named its encoding. [`refuse_nul`] runs on
/// every result, a mark settling what the bytes are and not what a rule may hold.
///
/// Malformed input is refused rather than repaired, and UTF-32 is refused
/// outright, mark or no mark. A U+FFFD standing in for bytes that could not be
/// read is silent by the time it reaches a parser, which carries it into a rule
/// that then matches something other than what the file names; an error leaves
/// the caller able to name the file it cannot honour.
///
/// Borrows valid UTF-8, marked or not. Allocating is confined to transcoding
/// UTF-16.
///
/// Like [`decode_text_for_display`], the result does not round-trip to disk: the
/// mark and the UTF-16 byte order are both gone from it, so a caller that writes
/// the file back writes UTF-8 without a mark.
pub fn decode_text_for_parsing(bytes: &[u8]) -> Result<Cow<'_, str>, InvalidArguments> {
    let text = match split_bom(bytes) {
        Some((Encoding::Utf32, _)) => Err(refuse_utf32()),
        Some((Encoding::Utf16(order), payload)) => decode_utf16(payload, order).map(Cow::Owned),
        Some((Encoding::Utf8, payload)) => decode_utf8(payload).map(Cow::Borrowed),
        None if is_bomless_utf32(bytes) => Err(refuse_utf32()),
        None => match detect_bomless_utf16(bytes) {
            Some(order) => decode_utf16(bytes, order).map(Cow::Owned),
            None => decode_utf8(bytes).map(Cow::Borrowed),
        },
    }?;
    refuse_nul(&text)?;
    Ok(text)
}

/// Returns `true` when `bytes` starts with a UTF-16 LE (FF FE) or BE (FE FF) byte-order mark.
pub fn is_utf16_bom(bytes: &[u8]) -> bool {
    matches!(split_bom(bytes), Some((Encoding::Utf16(_), _)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text of a filter file, in the shape that tells the encodings apart:
    /// ASCII rules on several lines.
    const RULES: &str = "# comment\n*.log\n/Intermediate\n!keep.txt\n";

    const ORDERS: [Utf16ByteOrder; 2] = [Utf16ByteOrder::Little, Utf16ByteOrder::Big];

    /// `text` as UTF-8, behind the byte-order mark when `bom`.
    fn as_utf8(text: &str, bom: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        if bom {
            bytes.extend([0xEF, 0xBB, 0xBF]);
        }
        bytes.extend(text.as_bytes());
        bytes
    }

    /// `text` as UTF-16 in `order`, behind the matching byte-order mark when
    /// `bom`.
    fn as_utf16(text: &str, order: Utf16ByteOrder, bom: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        if bom {
            bytes.extend(match order {
                Utf16ByteOrder::Little => [0xFF, 0xFE],
                Utf16ByteOrder::Big => [0xFE, 0xFF],
            });
        }
        bytes.extend(text.encode_utf16().flat_map(|unit| match order {
            Utf16ByteOrder::Little => unit.to_le_bytes(),
            Utf16ByteOrder::Big => unit.to_be_bytes(),
        }));
        bytes
    }

    /// `text` as UTF-32 in `order`, behind the matching byte-order mark when
    /// `bom`. The byte order is the same idea UTF-16 has, so it is named with
    /// the same type.
    fn as_utf32(text: &str, order: Utf16ByteOrder, bom: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        if bom {
            bytes.extend(match order {
                Utf16ByteOrder::Little => [0xFF, 0xFE, 0x00, 0x00],
                Utf16ByteOrder::Big => [0x00, 0x00, 0xFE, 0xFF],
            });
        }
        bytes.extend(text.chars().flat_map(|character| match order {
            Utf16ByteOrder::Little => u32::from(character).to_le_bytes(),
            Utf16ByteOrder::Big => u32::from(character).to_be_bytes(),
        }));
        bytes
    }

    #[test]
    fn decode_utf8_passthrough() {
        assert_eq!(decode_text_for_display(&as_utf8(RULES, false)), RULES);
    }

    #[test]
    fn decode_utf8_with_bom() {
        assert_eq!(decode_text_for_display(&as_utf8(RULES, true)), RULES);
    }

    #[test]
    fn decode_utf16_with_bom() {
        for order in ORDERS {
            assert_eq!(
                decode_text_for_display(&as_utf16(RULES, order, true)),
                RULES,
                "{order:?}"
            );
        }
    }

    #[test]
    fn decode_utf16_le_with_crlf() {
        let content = "Line one\r\nLine two\r\n";
        let bytes = as_utf16(content, Utf16ByteOrder::Little, true);
        assert_eq!(decode_text_for_display(&bytes), content);
    }

    /// [`decode_text_for_display`] acts on a mark alone, so mark-less UTF-16
    /// reaches it as the bytes it is.
    #[test]
    fn decode_leaves_bomless_utf16_undecoded() {
        for order in ORDERS {
            let bytes = as_utf16(RULES, order, false);
            assert_ne!(decode_text_for_display(&bytes), RULES, "{order:?}");
        }
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

    #[test]
    fn parsing_decodes_every_encoding() {
        for bom in [false, true] {
            assert_eq!(
                decode_text_for_parsing(&as_utf8(RULES, bom)).expect("UTF-8"),
                RULES,
                "UTF-8, mark: {bom}"
            );
            for order in ORDERS {
                assert_eq!(
                    decode_text_for_parsing(&as_utf16(RULES, order, bom)).expect("UTF-16"),
                    RULES,
                    "UTF-16 {order:?}, mark: {bom}"
                );
            }
        }
    }

    /// UTF-8 needs no transcoding, so the decode hands back the input.
    #[test]
    fn parsing_borrows_utf8() {
        for bom in [false, true] {
            let bytes = as_utf8(RULES, bom);
            assert!(
                matches!(decode_text_for_parsing(&bytes), Ok(Cow::Borrowed(_))),
                "UTF-8 was copied, mark: {bom}"
            );
        }
    }

    /// CRLF is what an editor that writes UTF-16 also tends to write, and the
    /// `\r` has to survive to where the parser trims it.
    #[test]
    fn parsing_decodes_utf16_crlf_without_bom() {
        let text = "*.log\r\n/Intermediate\r\n";
        let bytes = as_utf16(text, Utf16ByteOrder::Little, false);
        assert_eq!(decode_text_for_parsing(&bytes).expect("UTF-16 LE"), text);
    }

    #[test]
    fn parsing_decodes_empty_input() {
        assert_eq!(decode_text_for_parsing(b"").expect("empty"), "");
    }

    /// A NUL is refused wherever it decodes from, and a mark does not excuse it:
    /// no rule can hold one, so a rule that does matches nothing. A stray NUL is
    /// also not the alternation UTF-16 leaves, so the file is not taken for
    /// UTF-16 on the way there.
    #[test]
    fn parsing_refuses_a_nul_from_every_encoding() {
        let mut marked_utf16 = as_utf16(RULES, Utf16ByteOrder::Little, true);
        marked_utf16.extend([0x00, 0x00]);

        let cases: [(&str, Vec<u8>); 4] = [
            ("mark-less UTF-8", {
                let mut bytes = as_utf8(RULES, false);
                bytes.extend([0x00, b'x']);
                bytes
            }),
            ("UTF-8 behind a mark", {
                let mut bytes = as_utf8(RULES, true);
                bytes.extend([0x00, b'x']);
                bytes
            }),
            ("the reviewed case", b"\xEF\xBB\xBF*.log\0\n".to_vec()),
            ("U+0000 out of a UTF-16 code unit", marked_utf16),
        ];

        for (name, bytes) in cases {
            let error = decode_text_for_parsing(&bytes)
                .expect_err(&format!("{name} decoded instead of being refused"));
            assert!(
                error.reason.contains("NUL"),
                "{name}: the refusal must name the NUL, got {:?}",
                error.reason
            );
        }

        let mut stray = as_utf8(RULES, false);
        stray.extend([0x00, b'x']);
        assert_eq!(
            detect_bomless_utf16(&stray),
            None,
            "a stray NUL must not turn a UTF-8 file into UTF-16"
        );
    }

    /// A mark-less UTF-16 file whose rules lie outside ASCII leaves too little
    /// alternation to name a byte order by, and what it leaves is valid UTF-8 of
    /// unrelated text: UTF-16 LE `你好\n` is `` `O}Y `` and a NUL. It is refused
    /// on that NUL rather than filtered on a rule it never held.
    #[test]
    fn parsing_refuses_bomless_utf16_of_non_ascii_rules() {
        let text = "你好\n";
        assert_eq!(
            as_utf16(text, Utf16ByteOrder::Little, false),
            [0x60, 0x4F, 0x7D, 0x59, 0x0A, 0x00],
            "the case rests on these bytes"
        );

        for order in ORDERS {
            let bytes = as_utf16(text, order, false);
            assert_eq!(
                detect_bomless_utf16(&bytes),
                None,
                "{order:?} must be the case detection cannot name"
            );
            assert!(
                std::str::from_utf8(&bytes).is_ok(),
                "{order:?} must be the case where a UTF-8 decode would succeed"
            );
            assert!(
                decode_text_for_parsing(&bytes).is_err(),
                "mark-less UTF-16 {order:?} of non-ASCII rules was decoded, as {:?}",
                String::from_utf8_lossy(&bytes)
            );
        }
    }

    /// The bound on that, stated so it is not mistaken for support. With no
    /// character below U+0100 in it at all, a mark-less UTF-16 buffer leaves no
    /// NUL either, and nothing tells it from the UTF-8 text its bytes spell. It
    /// reads as that text, and such a file has to carry a mark.
    #[test]
    fn parsing_reads_bomless_utf16_with_no_ascii_at_all_as_utf8() {
        for (order, spelled) in [
            (Utf16ByteOrder::Little, "`O}Y"),
            (Utf16ByteOrder::Big, "O`Y}"),
        ] {
            let bytes = as_utf16("你好", order, false);
            assert_eq!(
                decode_text_for_parsing(&bytes).expect("valid UTF-8"),
                spelled,
                "{order:?}"
            );
        }
    }

    /// A mark-less UTF-16 file cut mid code unit is left holding ASCII beside
    /// NULs, which is valid UTF-8. It is refused as the truncated UTF-16 it is.
    #[test]
    fn parsing_refuses_truncated_bomless_utf16() {
        for order in ORDERS {
            let mut bytes = as_utf16(RULES, order, false);
            bytes.pop();
            let error = decode_text_for_parsing(&bytes)
                .expect_err(&format!("truncated UTF-16 {order:?} was decoded"));
            assert!(
                error.reason.contains("UTF-16"),
                "truncation must be reported against UTF-16, got {:?}",
                error.reason
            );
        }
    }

    /// Bytes no encoding accounts for are refused, so a parser is never handed a
    /// U+FFFD where the file had something it could not read.
    #[test]
    fn parsing_refuses_malformed_input() {
        let cases: [(&str, Vec<u8>); 10] = [
            ("invalid UTF-8", vec![b'*', 0xC3, 0x28, b'\n']),
            ("invalid UTF-8 behind a mark", {
                let mut bytes = vec![0xEF, 0xBB, 0xBF];
                bytes.extend([b'*', 0xC3, 0x28, b'\n']);
                bytes
            }),
            ("truncated UTF-16 code unit", {
                let mut bytes = as_utf16(RULES, Utf16ByteOrder::Little, true);
                bytes.pop();
                bytes
            }),
            ("truncated mark-less UTF-16 LE", {
                let mut bytes = as_utf16(RULES, Utf16ByteOrder::Little, false);
                bytes.pop();
                bytes
            }),
            ("truncated mark-less UTF-16 BE", {
                let mut bytes = as_utf16(RULES, Utf16ByteOrder::Big, false);
                bytes.pop();
                bytes
            }),
            ("unpaired UTF-16 surrogate", {
                let mut bytes = as_utf16(RULES, Utf16ByteOrder::Little, true);
                bytes.splice(2..2, [0x00, 0xD8]);
                bytes
            }),
            ("UTF-32 LE", as_utf32(RULES, Utf16ByteOrder::Little, true)),
            ("UTF-32 BE", as_utf32(RULES, Utf16ByteOrder::Big, true)),
            (
                "mark-less UTF-32 LE",
                as_utf32(RULES, Utf16ByteOrder::Little, false),
            ),
            (
                "mark-less UTF-32 BE",
                as_utf32(RULES, Utf16ByteOrder::Big, false),
            ),
        ];

        for (name, bytes) in cases {
            assert!(
                decode_text_for_parsing(&bytes).is_err(),
                "{name} was decoded instead of refused"
            );
        }
    }

    /// A mark-less UTF-32 file is ASCII interleaved with NULs, which is valid
    /// UTF-8. The shape has to be recognised on its own and refused as UTF-32.
    #[test]
    fn parsing_refuses_bomless_utf32() {
        for order in ORDERS {
            let bytes = as_utf32(RULES, order, false);
            assert!(
                std::str::from_utf8(&bytes).is_ok(),
                "{order:?} must be the case where a UTF-8 decode would succeed"
            );

            let error = decode_text_for_parsing(&bytes)
                .expect_err(&format!("mark-less UTF-32 {order:?} was decoded"));
            assert!(
                error.reason.contains("UTF-32"),
                "the refusal must name UTF-32, got {:?}",
                error.reason
            );
        }
    }

    /// Mark-less UTF-16 must not answer to the UTF-32 shape. That shape is
    /// checked first, so a false positive would refuse a file this module
    /// decodes.
    #[test]
    fn detect_tells_utf32_from_utf16_and_utf8() {
        for order in ORDERS {
            assert!(
                is_bomless_utf32(&as_utf32(RULES, order, false)),
                "{order:?}"
            );
            assert!(
                !is_bomless_utf32(&as_utf16(RULES, order, false)),
                "UTF-16 {order:?} was taken for UTF-32"
            );
        }
        assert!(!is_bomless_utf32(b""));
        assert!(!is_bomless_utf32(RULES.as_bytes()));
    }

    /// UTF-32 LE opens with the UTF-16 LE mark, and must not be taken for it:
    /// parsing refuses it, and neither [`is_utf16_bom`] nor
    /// [`decode_text_for_display`] reads it as UTF-16.
    #[test]
    fn utf32_is_not_taken_for_utf16() {
        let bytes = as_utf32(RULES, Utf16ByteOrder::Little, true);

        assert!(
            decode_text_for_parsing(&bytes).is_err(),
            "UTF-32 must be refused"
        );
        assert!(!is_utf16_bom(&bytes), "UTF-32 must report no UTF-16 mark");
        assert_ne!(
            decode_text_for_display(&bytes),
            decode_utf16_lossy(&bytes[2..], Utf16ByteOrder::Little),
            "UTF-32 must not be rendered as UTF-16"
        );
    }

    /// UTF-8, and a single code unit ahead of a trailing byte: too little to
    /// name an order over.
    #[test]
    fn detect_rejects_utf8_and_a_lone_code_unit() {
        assert_eq!(detect_bomless_utf16(b""), None);
        assert_eq!(detect_bomless_utf16(RULES.as_bytes()), None);
        assert_eq!(detect_bomless_utf16(&[b'a', 0x00, b'b']), None);
    }

    /// NULs at both offsets are not the every-other-byte pattern UTF-16 leaves,
    /// so neither order wins.
    #[test]
    fn detect_rejects_nuls_at_both_offsets() {
        assert_eq!(detect_bomless_utf16(&[0x00; 8]), None);
    }

    #[test]
    fn detect_names_the_byte_order() {
        for order in ORDERS {
            let bytes = as_utf16(RULES, order, false);
            assert_eq!(detect_bomless_utf16(&bytes), Some(order));
        }
    }

    /// A trailing byte does not hide the pattern. Naming the order for a
    /// truncated buffer is what carries it to the UTF-16 decode that refuses it.
    #[test]
    fn detect_names_the_byte_order_of_truncated_utf16() {
        for order in ORDERS {
            let mut bytes = as_utf16(RULES, order, false);
            bytes.pop();
            assert_eq!(detect_bomless_utf16(&bytes), Some(order), "{order:?}");
        }
    }
}
