// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `DynamoDB`'s JSON, written into a request buffer and read out of a response buffer.
//!
//! Writing appends straight into a [`BytesMut`], so a request body is one allocation that grows
//! once or twice — there is no intermediate `Value`, no `HashMap`, and no `String` per attribute
//! name.
//!
//! Reading is where the allocation goal is won. The response is one [`Bytes`]; `serde_json` hands
//! back string slices borrowed from it, and [`Arena`] turns those into [`ByteStr`] by
//! reference-counting the same buffer rather than copying. Binary attributes decode into the
//! [`Blob`]'s own inline bytes, so a hundred-row batch of keys and hashes allocates nothing at all
//! for its blobs.
//!
//! Everything here parses hostile input: a truncated body, a wrong type, bad base64, bytes after
//! the document. All of it is an error, none of it panics.
use std::cell::Cell;
use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::BufMut;
use bytes::Bytes;
use bytes::BytesMut;
use serde::de::DeserializeSeed;
use serde::de::IgnoredAny;
use serde::de::MapAccess;
use serde::de::SeqAccess;
use serde::de::Visitor;

use crate::aws::item::AttributeValue;
use crate::aws::item::BLOB_INLINE;
use crate::aws::item::Blob;
use crate::aws::item::ByteStr;
use crate::aws::item::Item;
use crate::aws::item::NumStr;

/// How many maps and lists a value may sit inside before the parser refuses it.
///
/// Each map or list counts one level. `DynamoDB` accepts 32 levels of nesting, so no row it stores
/// is refused; the bound is what keeps a hostile body from turning into unbounded recursion in the
/// visitor.
#[lore_macro::test_pub]
const MAX_DEPTH: usize = 32;

/// The allocation a run of request bodies is carved from.
///
/// Small because the bodies are. Only `DynamoDB` builds bodies here — an S3 payload is `Bytes` that
/// arrives already built and never passes through this writer — and every single-item operation
/// writes between 64 and 256 bytes.
const CHUNK: usize = 2 * 1024;

/// The most elements a list is sized for up front from a caller's expectation.
///
/// A hundred is the most keys one `BatchGetItem` reads. The expectation only saves regrowth, so a
/// longer list still reads in full.
const MAX_CAPACITY_HINT: usize = 100;

/// A request body under construction.
#[derive(Debug, Default)]
pub struct BodyWriter {
    buffer: BytesMut,
}

impl BodyWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Room for a body of `capacity` bytes, reusing what the previous body left of its chunk.
    ///
    /// A body that has been handed out still holds a reference to the allocation it was carved
    /// from, so reserving the exact size every time would allocate per call. One chunk of two
    /// kilobytes serves many bodies instead. The exceptions are `BatchGetItem` and
    /// `TransactWriteItems`, which scale with their item count up to roughly 16 KiB.
    ///
    /// Sizing the chunk for those would make every small body pin 16 KiB until it is dropped,
    /// because the carved slice keeps the whole allocation alive; under concurrency that amplifies
    /// resident memory by orders of magnitude over the bytes in use. So a body that needs more than
    /// the chunk gets an allocation of its own, and [`Self::split`] does not leave an allocation
    /// larger than a chunk behind for the bodies after it.
    ///
    /// Anything a panicked write left in the buffer is discarded.
    pub fn reserve_for(&mut self, capacity: usize) {
        self.buffer.clear();

        if self.buffer.capacity() < capacity {
            self.buffer = BytesMut::with_capacity(capacity.max(CHUNK));
        }
    }

    /// Take what has been written, leaving the rest of the chunk for the next body.
    ///
    /// The counterpart to [`Self::reserve_for`]: the written region is handed over with its own
    /// reference to the allocation, and the remaining capacity stays positioned after it. When that
    /// allocation is larger than a chunk — an estimate past the chunk, or growth while writing —
    /// the remainder is released instead, so the next body starts a fresh chunk rather than pinning
    /// this allocation.
    pub fn split(&mut self) -> Bytes {
        let body = self.buffer.split().freeze();

        if body.len() + self.buffer.capacity() > CHUNK {
            self.buffer = BytesMut::new();
        }

        body
    }

    /// Append a literal — a punctuation character, or a fixed key.
    pub fn raw(&mut self, text: &str) {
        self.buffer.put_slice(text.as_bytes());
    }

    /// Append a quoted, escaped JSON string.
    pub fn string(&mut self, text: &str) {
        self.buffer.put_u8(b'"');
        escape_into(&mut self.buffer, text);
        self.buffer.put_u8(b'"');
    }

    /// Append `"name":`.
    pub fn key(&mut self, name: &str) {
        self.string(name);
        self.buffer.put_u8(b':');
    }

    /// Append a `"name": <string>` pair.
    pub fn string_field(&mut self, name: &str, value: &str) {
        self.key(name);
        self.string(value);
    }

    /// Append a `"name": <bool>` pair.
    pub fn bool_field(&mut self, name: &str, value: bool) {
        self.key(name);
        self.raw(if value { "true" } else { "false" });
    }

    /// Append a `"name": <number>` pair.
    pub fn number_field(&mut self, name: &str, value: i64) {
        self.key(name);
        let number = NumStr::from(value);
        self.raw(number.as_str());
    }

    /// Append an item as a JSON object of attribute name to typed value.
    pub fn item(&mut self, item: &Item) {
        self.buffer.put_u8(b'{');
        for (index, (name, value)) in item.iter().enumerate() {
            if index > 0 {
                self.buffer.put_u8(b',');
            }
            self.key(name.as_str());
            self.attribute_value(value);
        }
        self.buffer.put_u8(b'}');
    }

    /// Append one typed attribute value: `{"B": "…"}`, `{"S": "…"}`, and so on.
    pub fn attribute_value(&mut self, value: &AttributeValue) {
        self.buffer.put_u8(b'{');
        self.key(value.type_tag());

        match value {
            AttributeValue::B(bytes) => self.base64(bytes),
            AttributeValue::S(text) => self.string(text.as_str()),
            AttributeValue::N(number) => self.string(number.as_str()),
            AttributeValue::Bool(flag) => self.raw(if *flag { "true" } else { "false" }),
            // The NULL attribute's value is the literal `true`; `{"NULL": false}` is not a thing
            // DynamoDB accepts.
            AttributeValue::Null => self.raw("true"),
            AttributeValue::M(item) => self.item(item),
            AttributeValue::L(list) => {
                self.buffer.put_u8(b'[');
                for (index, element) in list.iter().enumerate() {
                    if index > 0 {
                        self.buffer.put_u8(b',');
                    }
                    self.attribute_value(element);
                }
                self.buffer.put_u8(b']');
            }
            AttributeValue::Bs(set) => {
                self.buffer.put_u8(b'[');
                for (index, element) in set.iter().enumerate() {
                    if index > 0 {
                        self.buffer.put_u8(b',');
                    }
                    self.base64(element);
                }
                self.buffer.put_u8(b']');
            }
            AttributeValue::Ss(set) => {
                self.buffer.put_u8(b'[');
                for (index, element) in set.iter().enumerate() {
                    if index > 0 {
                        self.buffer.put_u8(b',');
                    }
                    self.string(element.as_str());
                }
                self.buffer.put_u8(b']');
            }
            AttributeValue::Ns(set) => {
                self.buffer.put_u8(b'[');
                for (index, element) in set.iter().enumerate() {
                    if index > 0 {
                        self.buffer.put_u8(b',');
                    }
                    self.string(element.as_str());
                }
                self.buffer.put_u8(b']');
            }
        }

        self.buffer.put_u8(b'}');
    }

    /// Append `bytes` as a quoted base64 string, encoded in place at the end of the buffer.
    ///
    /// The region is sized to the padded encoding, which is exactly what the engine writes, so the
    /// encode cannot run short.
    fn base64(&mut self, bytes: &[u8]) {
        let encoded = 4 * bytes.len().div_ceil(3);
        self.buffer.reserve(encoded + 2);
        self.buffer.put_u8(b'"');

        let start = self.buffer.len();
        self.buffer.resize(start + encoded, 0);
        let written = BASE64
            .encode_slice(bytes, &mut self.buffer[start..])
            .unwrap_or_default();
        self.buffer.truncate(start + written);

        self.buffer.put_u8(b'"');
    }
}

/// Escape `text` into `out` as JSON string content.
///
/// The two-character escapes plus `\u00XX` for the remaining control characters, which is the whole
/// of what RFC 8259 requires. Everything else, including non-ASCII, is valid UTF-8 in a JSON string
/// and passes through — a lock description or an owner id is text, not something to mangle.
fn escape_into(out: &mut BytesMut, text: &str) {
    let bytes = text.as_bytes();
    let mut plain_from = 0usize;

    for (index, byte) in bytes.iter().enumerate() {
        let escape: &[u8] = match byte {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0x08 => b"\\b",
            0x0c => b"\\f",
            0x00..=0x1f => {
                out.put_slice(&bytes[plain_from..index]);
                // The remaining control characters have no short form.
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.put_slice(b"\\u00");
                out.put_u8(HEX[(byte >> 4) as usize]);
                out.put_u8(HEX[(byte & 0x0f) as usize]);
                plain_from = index + 1;
                continue;
            }
            _ => continue,
        };

        out.put_slice(&bytes[plain_from..index]);
        out.put_slice(escape);
        plain_from = index + 1;
    }

    out.put_slice(&bytes[plain_from..]);
}

/// Why a response could not be read.
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum ParseError {
    /// Not JSON, not the shape the operation answers with, or more than one document.
    #[error("malformed response body: {0}")]
    Malformed(String),
    #[error("attribute value has no type tag")]
    UntaggedAttribute,
    #[error("unknown attribute type {0}")]
    UnknownAttributeType(String),
    #[error("binary attribute is not base64")]
    BadBase64,
    #[error("value nests deeper than {MAX_DEPTH} levels")]
    TooDeep,
}

/// The response buffer, so that what is parsed out of it can share it.
///
/// Text borrowed from the response becomes a [`ByteStr`] over the same allocation. Binary
/// attributes do not need the response at all — they decode into the [`Blob`]'s inline bytes.
///
/// It also holds why a parse failed: `serde_json` carries a visitor's error only as text, so the
/// typed reason is kept here for [`parse_response`] to report.
pub struct Arena {
    body: Bytes,
    failure: Cell<Option<ParseError>>,
}

impl Arena {
    pub fn new(body: Bytes) -> Self {
        Self {
            body,
            failure: Cell::new(None),
        }
    }

    /// Text that `serde_json` borrowed from the response, as a slice of the response.
    ///
    /// Falls back to a copy if the slice does not sit inside the body. A network parser must not be
    /// one bounds assumption away from a panic.
    fn borrowed_text(&self, text: &str) -> ByteStr {
        let body = self.body.as_ptr() as usize;
        let start = text.as_ptr() as usize;

        if start >= body && start + text.len() <= body + self.body.len() {
            // SAFETY: the slice holds exactly the bytes of `text`, and a `str` is UTF-8.
            return unsafe { ByteStr::from_utf8_unchecked(self.body.slice_ref(text.as_bytes())) };
        }

        ByteStr::copy_from_str(text)
    }

    /// Record `error` as why the parse failed, and return the parser error that unwinds it.
    fn fail<E: serde::de::Error>(&self, error: ParseError) -> E {
        let unwind = E::custom(&error);
        self.failure.set(Some(error));
        unwind
    }
}

/// Enough stack bytes to decode any value that fits a [`Blob`] inline.
///
/// `decode_slice` requires room for its own upper bound on the decoded length, which for a value of
/// exactly [`BLOB_INLINE`] bytes rounds four past it. Decoding through the stack and copying keeps
/// such a value inline, and copying thirty-two bytes is not a cost worth avoiding.
#[lore_macro::test_pub]
const BLOB_SCRATCH: usize = BLOB_INLINE + 4;

/// Decode a base64 attribute into a [`Blob`].
///
/// Every binary value Lore writes is a hash or a key, which lands in the blob's inline bytes and so
/// allocates nothing. A longer value — a foreign row holding something Lore never writes — spills
/// to a single allocation.
#[lore_macro::test_pub]
fn decode_blob(encoded: &str) -> Result<Blob, ParseError> {
    // Base64 expands by four thirds, and `decode_slice` rejects an output slice shorter than this.
    let upper_bound = encoded.len() / 4 * 3 + 3;

    if upper_bound <= BLOB_SCRATCH {
        let mut scratch = [0u8; BLOB_SCRATCH];
        let written = BASE64
            .decode_slice(encoded.as_bytes(), &mut scratch)
            .map_err(|_error| ParseError::BadBase64)?;

        return Ok(Blob::from_slice(&scratch[..written]));
    }

    let mut blob = Blob::zeroed(upper_bound);
    let written = BASE64
        .decode_slice(encoded.as_bytes(), blob.as_mut_slice())
        // The decode failure names an offset in a value we would not want in a log, and the caller
        // only needs to know the attribute was not base64.
        .map_err(|_error| ParseError::BadBase64)?;

    blob.truncate(written);

    Ok(blob)
}

/// A number attribute's text, kept as written.
fn read_number(text: &str) -> Result<NumStr, ParseError> {
    Ok(NumStr::from_ascii(text))
}

/// An attribute value's type tag, which is also its only key.
enum Tag {
    B,
    S,
    N,
    Bool,
    Null,
    M,
    L,
    Bs,
    Ss,
    Ns,
}

impl Tag {
    fn parse(tag: &str) -> Result<Self, ParseError> {
        Ok(match tag {
            "B" => Self::B,
            "S" => Self::S,
            "N" => Self::N,
            "BOOL" => Self::Bool,
            "NULL" => Self::Null,
            "M" => Self::M,
            "L" => Self::L,
            "BS" => Self::Bs,
            "SS" => Self::Ss,
            "NS" => Self::Ns,
            unknown => return Err(ParseError::UnknownAttributeType(unknown.to_owned())),
        })
    }
}

/// A JSON string handed to `read`, whether `serde_json` lends it from the response or had to
/// unescape it first. A failure `read` reports is the parse's failure.
#[derive(Clone, Copy)]
struct StrSeed<'a, F> {
    arena: &'a Arena,
    read: F,
}

impl<'de, T, F: FnOnce(&str) -> Result<T, ParseError>> DeserializeSeed<'de> for StrSeed<'_, F> {
    type Value = T;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<T, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl<'de, T, F: FnOnce(&str) -> Result<T, ParseError>> Visitor<'de> for StrSeed<'_, F> {
    type Value = T;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "a string")
    }

    fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<T, E> {
        (self.read)(text).map_err(|error| self.arena.fail(error))
    }
}

/// Deserialize one attribute value.
#[lore_macro::test_pub]
#[derive(Clone, Copy)]
struct AttributeValueSeed<'a> {
    arena: &'a Arena,
    /// How many maps and lists enclose this value.
    depth: usize,
}

impl AttributeValueSeed<'_> {
    /// The depth of a value inside this one, refused past [`MAX_DEPTH`].
    fn nested<E: serde::de::Error>(&self) -> Result<usize, E> {
        if self.depth >= MAX_DEPTH {
            return Err(self.arena.fail(ParseError::TooDeep));
        }

        Ok(self.depth + 1)
    }
}

impl<'de> DeserializeSeed<'de> for AttributeValueSeed<'_> {
    type Value = AttributeValue;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for AttributeValueSeed<'_> {
    type Value = AttributeValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "a DynamoDB attribute value")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let arena = self.arena;
        let blob = StrSeed {
            arena,
            read: decode_blob,
        };
        let number = StrSeed {
            arena,
            read: read_number,
        };

        let tag = map
            .next_key_seed(StrSeed {
                arena,
                read: Tag::parse,
            })?
            .ok_or_else(|| arena.fail::<A::Error>(ParseError::UntaggedAttribute))?;

        let value = match tag {
            Tag::B => AttributeValue::B(map.next_value_seed(blob)?),
            Tag::S => AttributeValue::S(map.next_value_seed(TextSeed(arena))?),
            Tag::N => AttributeValue::N(map.next_value_seed(number)?),
            Tag::Bool => AttributeValue::Bool(map.next_value()?),
            Tag::Null => {
                map.next_value::<bool>()?;
                AttributeValue::Null
            }
            Tag::M => AttributeValue::M(Box::new(map.next_value_seed(ItemSeed {
                arena,
                depth: self.nested::<A::Error>()?,
            })?)),
            Tag::L => AttributeValue::L(map.next_value_seed(SeqSeed::new(AttributeValueSeed {
                arena,
                depth: self.nested::<A::Error>()?,
            }))?),
            Tag::Bs => AttributeValue::Bs(map.next_value_seed(SeqSeed::new(blob))?),
            Tag::Ss => AttributeValue::Ss(map.next_value_seed(SeqSeed::new(TextSeed(arena)))?),
            Tag::Ns => AttributeValue::Ns(map.next_value_seed(SeqSeed::new(number))?),
        };

        // An attribute value carries exactly one tag. Draining any surplus keeps a well-formed but
        // over-specified object from failing, and matches what the service would accept.
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}

        Ok(value)
    }
}

/// Deserialize text as a slice of the response, or as its own copy when `serde_json` had to
/// unescape it.
#[derive(Clone, Copy)]
pub(crate) struct TextSeed<'a>(pub(crate) &'a Arena);

impl<'de> DeserializeSeed<'de> for TextSeed<'_> {
    type Value = ByteStr;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl<'de> Visitor<'de> for TextSeed<'_> {
    type Value = ByteStr;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "a string")
    }

    /// Borrowed straight from the response: no copy.
    fn visit_borrowed_str<E>(self, text: &'de str) -> Result<ByteStr, E> {
        Ok(self.0.borrowed_text(text))
    }

    /// `serde_json` had to unescape, so the text is its own. One small copy, only for a string that
    /// actually carried an escape.
    fn visit_str<E>(self, text: &str) -> Result<ByteStr, E> {
        Ok(ByteStr::copy_from_str(text))
    }

    fn visit_string<E>(self, text: String) -> Result<ByteStr, E> {
        Ok(ByteStr::from(text))
    }
}

/// A JSON array, each element read through a copy of `element`.
#[derive(Clone, Copy)]
pub(crate) struct SeqSeed<S> {
    element: S,
    /// The capacity to start from, since `serde_json` cannot say how long an array is.
    capacity: usize,
    /// The most elements the array may hold.
    limit: usize,
}

impl<S> SeqSeed<S> {
    pub(crate) fn new(element: S) -> Self {
        Self {
            element,
            capacity: 0,
            limit: usize::MAX,
        }
    }

    /// An array the caller expects `capacity` elements of, sized for at most [`MAX_CAPACITY_HINT`]
    /// of them up front.
    pub(crate) fn with_capacity(element: S, capacity: usize) -> Self {
        Self {
            element,
            capacity: capacity.min(MAX_CAPACITY_HINT),
            limit: usize::MAX,
        }
    }

    /// Refuse an array of more than `limit` elements, on the element past it.
    ///
    /// For an answer whose length the request already fixes: a response holding more is not that
    /// answer, and decoding all of it would let the body choose how much this process allocates.
    pub(crate) fn at_most(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

impl<'de, S: DeserializeSeed<'de> + Clone> DeserializeSeed<'de> for SeqSeed<S> {
    type Value = Vec<S::Value>;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de, S: DeserializeSeed<'de> + Clone> Visitor<'de> for SeqSeed<S> {
    type Value = Vec<S::Value>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "a list")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut values = Vec::with_capacity(seq.size_hint().unwrap_or(self.capacity));
        while let Some(value) = seq.next_element_seed(self.element.clone())? {
            if values.len() == self.limit {
                return Err(serde::de::Error::custom(format_args!(
                    "more than the {} entries the request allows",
                    self.limit
                )));
            }
            values.push(value);
        }
        Ok(values)
    }
}

/// Deserialize an item: attribute names to typed values.
#[derive(Clone, Copy)]
pub struct ItemSeed<'a> {
    pub arena: &'a Arena,
    /// How many maps and lists enclose the item's values.
    pub depth: usize,
}

impl<'a> ItemSeed<'a> {
    /// A top-level item: a row, a key, or a cursor.
    pub fn new(arena: &'a Arena) -> Self {
        Self { arena, depth: 0 }
    }
}

impl<'de> DeserializeSeed<'de> for ItemSeed<'_> {
    type Value = Item;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for ItemSeed<'_> {
    type Value = Item;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "a DynamoDB item")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut item = Item::with_capacity(map.size_hint().unwrap_or(0));

        while let Some(name) = map.next_key_seed(TextSeed(self.arena))? {
            let value = map.next_value_seed(AttributeValueSeed {
                arena: self.arena,
                depth: self.depth,
            })?;
            // A wire item cannot repeat an attribute name, so appending avoids the scan `insert`
            // does.
            item.push(name, value);
        }

        Ok(item)
    }
}

/// Parse a whole response body with `parse` over its top-level value.
///
/// The one entry point, so every operation's parser gets the same arena, the same borrowing, and
/// the same error mapping. The body must be exactly one JSON document: an empty body and bytes
/// after the document are both malformed.
pub fn parse_response<T, F>(body: Bytes, parse: F) -> Result<T, ParseError>
where
    F: for<'de> FnOnce(
        &mut serde_json::Deserializer<serde_json::de::SliceRead<'de>>,
        &Arena,
    ) -> Result<T, serde_json::Error>,
{
    let arena = Arena::new(body);
    let mut deserializer = serde_json::Deserializer::from_slice(&arena.body);

    parse(&mut deserializer, &arena)
        .and_then(|parsed| deserializer.end().map(|()| parsed))
        .map_err(|error| {
            arena
                .failure
                .take()
                .unwrap_or_else(|| ParseError::Malformed(error.to_string()))
        })
}
