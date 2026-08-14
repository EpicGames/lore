// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Encoding of a [`Fragment`] as S3 object metadata.
//!
//! The fragment describing a payload travels on the S3 object holding that payload, as an
//! `x-amz-meta-*` header, rather than in a separate `DynamoDB` record. Object metadata is part of
//! the object version: it cannot be changed without rewriting the object, and a `GetObject`
//! returns headers and body from the same version. A reader therefore always sees the fragment
//! that was written with the bytes it is reading, which is what makes the two impossible to tear
//! apart — no write protocol is involved, only S3's own object atomicity.
//!
//! Only the representation is carried here. Obliteration state is mutable and lives in
//! `DynamoDB`; see [`crate::store::immutable_store`].

use std::collections::HashMap;

use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;

/// The flags that describe the payload itself, and therefore the only ones the object carries.
///
/// Everything outside this mask falls into one of three groups, none of which belongs on an
/// immutable object:
///
/// - `PayloadObliterating` / `PayloadObliterated` are lifecycle state. They change while the bytes
///   do not, and object metadata cannot be edited without rewriting the object, so they live in
///   `DynamoDB`.
/// - `PayloadStoredDurable` / `PayloadStoredLocal` are facts about *which store* holds the payload,
///   not about the payload. Each store derives its own on read; writing them down lets one store's
///   answer be served for another.
/// - `PayloadLocalCachePriority` is a per-machine caching hint. It says what one host should keep,
///   which is not a property of the content and is not the same answer for every reader.
/// - `PayloadDoNotReplicate` is a request about the transfer in hand, stripped before storage by
///   `sanitise_fragment_behavior_flags`.
///
/// `PayloadRevisionState` does travel here: it says what the payload *is*, which is the same for
/// every reader of those bytes.
pub const PAYLOAD_FLAGS: u32 = FragmentFlags::PayloadFragmented.bits()
    | FragmentFlags::PayloadCompressed.bits()
    | FragmentFlags::PayloadRevisionState.bits();

/// The single object metadata key holding the whole fragment.
///
/// One key rather than one per field: a key name is spelled out in full on every request and every
/// response, so three of them cost several times what the values do. S3 lowercases metadata keys,
/// so this is written lowercase to make the round trip an identity.
const KEY_FRAGMENT: &str = "lore-fragment";

/// Separator between the fragment's fields within [`KEY_FRAGMENT`].
const SEPARATOR: char = ':';

/// Names for the parts of the value, used only to say which one failed to parse.
const FIELD_COUNT: &str = "field count";
const FIELD_FLAGS: &str = "flags";
const FIELD_SIZE_PAYLOAD: &str = "size_payload";
const FIELD_SIZE_CONTENT: &str = "size_content";

/// Why a stored object did not yield a usable fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectMetadataError {
    /// The object carries no lore metadata at all. An object written before fragments moved onto
    /// the object has this shape, so this is the discriminator a migration reads.
    Absent,
    /// The object carries lore metadata that could not be parsed. Names the offending field.
    Malformed(&'static str),
}

impl std::error::Error for ObjectMetadataError {}

impl std::fmt::Display for ObjectMetadataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Absent => write!(f, "object carries no fragment metadata"),
            Self::Malformed(field) => write!(f, "fragment metadata field {field} is malformed"),
        }
    }
}

/// Render a fragment as the object metadata to attach to its object.
///
/// The value is `<flags>:<size_payload>:<size_content>` — flags in hex, because it is a bit field
/// and hex is how bits are read; the sizes in decimal, because they are magnitudes. Plain text
/// rather than a packed encoding, so an object's shape stays legible from `aws s3api head-object`
/// with no lore tooling.
///
/// Flags are reduced to [`PAYLOAD_FLAGS`] on the way out, so the object describes the payload and
/// nothing else.
pub fn to_object_metadata(fragment: &Fragment) -> HashMap<String, String> {
    HashMap::from([(KEY_FRAGMENT.to_owned(), encode(fragment))])
}

/// The value itself. The map is the S3 SDK's currency, not this module's — the fragment is a fixed
/// triplet with no room for anything else, so the format is a string and the map exists only to
/// hand it over.
fn encode(fragment: &Fragment) -> String {
    let flags = fragment.flags & PAYLOAD_FLAGS;

    format!(
        "{flags:x}{SEPARATOR}{}{SEPARATOR}{}",
        fragment.size_payload, fragment.size_content
    )
}

/// Recover the fragment from the object metadata returned by a `GetObject` or `HeadObject`.
pub fn from_object_metadata(
    metadata: Option<&HashMap<String, String>>,
) -> Result<Fragment, ObjectMetadataError> {
    decode(
        metadata
            .and_then(|metadata| metadata.get(KEY_FRAGMENT))
            .ok_or(ObjectMetadataError::Absent)?,
    )
}

/// Parse the value. Split rather than collected: there are exactly three fields and a fourth is an
/// error, so there is nothing to gather.
fn decode(value: &str) -> Result<Fragment, ObjectMetadataError> {
    let mut fields = value.split(SEPARATOR);
    let malformed = || ObjectMetadataError::Malformed(FIELD_COUNT);

    let flags = fields.next().ok_or_else(malformed)?;
    let size_payload = fields.next().ok_or_else(malformed)?;
    let size_content = fields.next().ok_or_else(malformed)?;

    if fields.next().is_some() {
        return Err(malformed());
    }

    Ok(Fragment {
        flags: u32::from_str_radix(flags, 16)
            .map_err(|_parse| ObjectMetadataError::Malformed(FIELD_FLAGS))?
            & PAYLOAD_FLAGS,
        size_payload: size_payload
            .parse()
            .map_err(|_parse| ObjectMetadataError::Malformed(FIELD_SIZE_PAYLOAD))?,
        size_content: size_content
            .parse()
            .map_err(|_parse| ObjectMetadataError::Malformed(FIELD_SIZE_CONTENT))?,
    })
}

#[cfg(test)]
mod test {
    use super::*;

    fn fragment() -> Fragment {
        Fragment {
            flags: FragmentFlags::PayloadCompressedZstd.bits(),
            size_payload: 4096,
            size_content: 16384,
        }
    }

    #[test]
    fn round_trips_a_fragment() {
        let encoded = to_object_metadata(&fragment());

        assert_eq!(from_object_metadata(Some(&encoded)), Ok(fragment()));
    }

    #[test]
    fn the_format_round_trips_without_a_map() {
        assert_eq!(decode(&encode(&fragment())), Ok(fragment()));
        assert_eq!(encode(&fragment()), "8:4096:16384");
    }

    #[test]
    fn writes_one_key_holding_hex_flags_and_decimal_sizes() {
        let encoded = to_object_metadata(&fragment());

        assert_eq!(encoded.len(), 1, "one key, not one per field");
        assert_eq!(encoded.get(KEY_FRAGMENT).unwrap(), "8:4096:16384");
    }

    #[test]
    fn round_trips_the_extreme_values() {
        let extreme = Fragment {
            flags: PAYLOAD_FLAGS,
            size_payload: u32::MAX,
            size_content: u64::MAX,
        };
        let encoded = to_object_metadata(&extreme);

        assert_eq!(from_object_metadata(Some(&encoded)), Ok(extreme));
    }

    #[test]
    fn keeps_every_flag_that_describes_the_payload() {
        for flag in [
            FragmentFlags::PayloadFragmented,
            FragmentFlags::PayloadCompressedLZ4,
            FragmentFlags::PayloadCompressedOodle2,
            FragmentFlags::PayloadCompressedZstd,
            FragmentFlags::PayloadRevisionState,
        ] {
            let carried = Fragment {
                flags: flag.bits(),
                ..fragment()
            };

            assert_eq!(
                from_object_metadata(Some(&to_object_metadata(&carried))),
                Ok(carried),
                "{flag:?} describes the payload and must travel on the object"
            );
        }
    }

    #[test]
    fn drops_state_store_location_and_per_machine_flags() {
        for flag in [
            FragmentFlags::PayloadObliterating,
            FragmentFlags::PayloadObliterated,
            FragmentFlags::PayloadStoredDurable,
            FragmentFlags::PayloadStoredLocal,
            FragmentFlags::PayloadLocalCachePriority,
            FragmentFlags::PayloadDoNotReplicate,
        ] {
            let mut tainted = fragment();
            tainted.flags |= flag.bits();

            assert_eq!(
                from_object_metadata(Some(&to_object_metadata(&tainted))),
                Ok(fragment()),
                "{flag:?} is not a property of the payload and must not travel on the object"
            );
        }
    }

    #[test]
    fn masks_flags_that_reached_the_object_some_other_way() {
        let encoded = HashMap::from([(KEY_FRAGMENT.to_owned(), format!("{:x}:1:1", u32::MAX))]);

        let recovered = from_object_metadata(Some(&encoded)).unwrap();

        assert_eq!(recovered.flags, PAYLOAD_FLAGS);
    }

    #[test]
    fn reports_an_object_with_no_metadata_as_absent() {
        assert_eq!(from_object_metadata(None), Err(ObjectMetadataError::Absent));
        assert_eq!(
            from_object_metadata(Some(&HashMap::new())),
            Err(ObjectMetadataError::Absent)
        );
        assert_eq!(
            from_object_metadata(Some(&HashMap::from([(
                "unrelated".to_owned(),
                "8:1:1".to_owned()
            )]))),
            Err(ObjectMetadataError::Absent)
        );
    }

    #[test]
    fn reports_the_wrong_number_of_fields_as_malformed() {
        for value in ["", "8", "8:1", "8:1:1:1"] {
            let encoded = HashMap::from([(KEY_FRAGMENT.to_owned(), value.to_owned())]);

            assert_eq!(
                from_object_metadata(Some(&encoded)),
                Err(ObjectMetadataError::Malformed(FIELD_COUNT)),
                "{value:?} does not hold three fields"
            );
        }
    }

    #[test]
    fn names_the_field_that_failed_to_parse() {
        for (value, field) in [
            ("zz:1:1", FIELD_FLAGS),
            ("8:nope:1", FIELD_SIZE_PAYLOAD),
            ("8:1:nope", FIELD_SIZE_CONTENT),
        ] {
            let encoded = HashMap::from([(KEY_FRAGMENT.to_owned(), value.to_owned())]);

            assert_eq!(
                from_object_metadata(Some(&encoded)),
                Err(ObjectMetadataError::Malformed(field))
            );
        }
    }

    #[test]
    fn reports_an_overflowing_value_as_malformed() {
        let encoded = HashMap::from([(
            KEY_FRAGMENT.to_owned(),
            format!("8:{}:1", u64::from(u32::MAX) + 1),
        )]);

        assert_eq!(
            from_object_metadata(Some(&encoded)),
            Err(ObjectMetadataError::Malformed(FIELD_SIZE_PAYLOAD))
        );
    }

    /// Decimal flags would silently decode as a different bit pattern for any value above 9, so
    /// the base is part of the format rather than an incidental choice.
    #[test]
    fn reads_flags_as_hex() {
        let encoded = HashMap::from([(KEY_FRAGMENT.to_owned(), "a:1:1".to_owned())]);

        assert_eq!(
            from_object_metadata(Some(&encoded)).unwrap().flags,
            0xa & PAYLOAD_FLAGS
        );
    }
}
