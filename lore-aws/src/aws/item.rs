// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A `DynamoDB` row, held the way our rows actually look.
//!
//! Lore's rows are narrow — two attributes for a fragment association, seven for the widest lock —
//! and their attribute names come from struct definitions, so they are `&'static str`. An [`Item`]
//! is therefore an ordered small vector rather than a `HashMap`: no table allocation, no hashing,
//! no per-name allocation, and an ordering that is a function of its input, which is what makes two
//! request bodies comparable byte for byte.
//!
//! Values avoid allocating in the shapes Lore's rows actually take. A response arrives as one
//! contiguous buffer, so a string attribute is a [`ByteStr`] slice of it — handing an item from the
//! thread that parsed it to the thread that consumes it costs a refcount rather than a cross-thread
//! free per attribute. Numbers and binary values are short enough to live inside the value itself.
use std::borrow::Borrow;
use std::borrow::Cow;
use std::fmt;
use std::hash::Hash;
use std::hash::Hasher;
use std::ops::Deref;
use std::str::Utf8Error;

use bytes::Bytes;
use smallvec::SmallVec;

pub use crate::aws::serde::from_item;
pub use crate::aws::serde::to_attribute_value;
pub use crate::aws::serde::to_item;

/// How many attributes an [`Item`] holds without allocating.
///
/// Sized for the rows this store reads in bulk: `FragmentsEntry` has two attributes,
/// `FragmentStateEntry` two, `LockKey` two, `MutableStoreEntry` three, `FragmentMetadataEntry`
/// four. `LockEntry` at seven spills to one allocation, which is the right trade — locks are orders
/// of magnitude rarer than fragment reads, and a wider inline array would cost every batch of a
/// hundred narrow rows.
const ITEM_INLINE: usize = 4;

/// UTF-8 text backed by [`Bytes`].
///
/// Cloning shares. A literal costs nothing through [`ByteStr::from_static`], and text read off the
/// wire is a slice of the response buffer, so neither case allocates.
#[derive(Clone, Default, Eq)]
pub struct ByteStr(Bytes);

impl ByteStr {
    /// A `ByteStr` over a literal. No allocation, no copy.
    pub const fn from_static(text: &'static str) -> Self {
        Self(Bytes::from_static(text.as_bytes()))
    }

    /// A `ByteStr` owning a copy of `text`. The allocating constructor, named so it reads as one.
    pub fn copy_from_str(text: &str) -> Self {
        Self(Bytes::copy_from_slice(text.as_bytes()))
    }

    /// A `ByteStr` over bytes already held, checking they are text.
    ///
    /// The check is what the type is for: everything downstream may treat this as a `str` without
    /// re-validating, and a `DynamoDB` string attribute is defined to be UTF-8.
    pub fn from_utf8(bytes: Bytes) -> Result<Self, Utf8Error> {
        std::str::from_utf8(&bytes)?;
        Ok(Self(bytes))
    }

    /// A `ByteStr` over bytes already known to be text, without checking them again.
    ///
    /// # Safety
    ///
    /// `bytes` must be valid UTF-8: [`Self::as_str`] hands them out as a `str` unchecked.
    pub(crate) unsafe fn from_utf8_unchecked(bytes: Bytes) -> Self {
        Self(bytes)
    }

    pub fn as_str(&self) -> &str {
        // Checked on construction, and `Bytes` is immutable, so this cannot have changed.
        debug_assert!(std::str::from_utf8(&self.0).is_ok());
        unsafe { std::str::from_utf8_unchecked(&self.0) }
    }

    pub fn as_bytes(&self) -> &Bytes {
        &self.0
    }

    pub fn into_bytes(self) -> Bytes {
        self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl Deref for ByteStr {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for ByteStr {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for ByteStr {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for ByteStr {
    fn from(text: &str) -> Self {
        Self::copy_from_str(text)
    }
}

impl From<String> for ByteStr {
    fn from(text: String) -> Self {
        Self(Bytes::from(text.into_bytes()))
    }
}

/// Borrowed text is copied once; owned text is taken without a copy.
impl From<Cow<'_, str>> for ByteStr {
    fn from(text: Cow<'_, str>) -> Self {
        match text {
            Cow::Borrowed(text) => Self::copy_from_str(text),
            Cow::Owned(text) => Self::from(text),
        }
    }
}

impl From<ByteStr> for String {
    fn from(text: ByteStr) -> Self {
        text.as_str().to_owned()
    }
}

impl PartialEq for ByteStr {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl PartialEq<str> for ByteStr {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for ByteStr {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl Hash for ByteStr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl fmt::Debug for ByteStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for ByteStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), f)
    }
}

/// A `DynamoDB` number, which travels as decimal text.
///
/// Inline, because every number Lore writes is an integer of at most twenty digits and the point is
/// not to allocate for it. Longer values — a foreign row carrying `DynamoDB`'s full 38 significant
/// digits — spill rather than truncate.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct NumStr(SmallVec<[u8; 24]>);

impl NumStr {
    pub fn from_ascii(text: &str) -> Self {
        Self(SmallVec::from_slice(text.as_bytes()))
    }

    /// The text `value` displays as, formatted straight into the inline bytes.
    pub(crate) fn from_display(value: impl fmt::Display) -> Self {
        let mut digits = SmallVec::new();
        let _ = fmt::write(&mut DigitWriter(&mut digits), format_args!("{value}"));
        Self(digits)
    }

    pub fn as_str(&self) -> &str {
        // Every constructor takes text, and the wire form of a number is ASCII.
        std::str::from_utf8(&self.0).unwrap_or_default()
    }

    pub fn parse<T: std::str::FromStr>(&self) -> Result<T, T::Err> {
        self.as_str().parse()
    }
}

/// Appends formatted text to a [`NumStr`]'s bytes, and never fails.
struct DigitWriter<'a>(&'a mut SmallVec<[u8; 24]>);

impl fmt::Write for DigitWriter<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.0.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

macro_rules! num_str_from_integer {
    ($($ty:ty),+) => {
        $(
            impl From<$ty> for NumStr {
                fn from(value: $ty) -> Self {
                    Self::from_display(value)
                }
            }
        )+
    };
}

num_str_from_integer!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);

impl fmt::Debug for NumStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for NumStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), f)
    }
}

/// How many bytes a binary attribute holds without allocating.
///
/// Every binary value Lore stores is a `Hash` or a `Context` — 32 bytes at most, and the two
/// together in `repository_context` are exactly 32. Sized to hold all of them.
pub const BLOB_INLINE: usize = 32;

/// A binary attribute's bytes, inline while they are small.
///
/// Mirrors [`NumStr`]: the sizes that actually occur live in the value, and anything longer — a
/// foreign row carrying something Lore never writes — spills to the heap rather than being refused.
///
/// This is deliberately not [`bytes::Bytes`], which has no inline representation, so every 32-byte
/// hash would cost an allocation to build and a second one the first time it was cloned. It is also
/// what lets a response made entirely of key-sized blobs decode without allocating at all.
///
/// The trade is that a spilled blob copies when cloned where a `Bytes` would have shared. That only
/// reaches values above [`BLOB_INLINE`], which no Lore table writes.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct Blob(SmallVec<[u8; BLOB_INLINE]>);

impl Blob {
    pub fn from_slice(bytes: &[u8]) -> Self {
        Self(SmallVec::from_slice(bytes))
    }

    /// `len` zeroed bytes, for a decoder to write over.
    ///
    /// Paired with [`Self::truncate`]: base64 decoding knows only an upper bound on the output
    /// until it has run, so the buffer is sized to that and cut down to what was written.
    pub fn zeroed(len: usize) -> Self {
        Self(smallvec::smallvec![0u8; len])
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.0
    }

    pub fn truncate(&mut self, len: usize) {
        self.0.truncate(len);
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }

    /// Whether the bytes sit inline rather than on the heap.
    #[cfg(feature = "test-util")]
    pub fn is_inline(&self) -> bool {
        !self.0.spilled()
    }
}

impl std::ops::Deref for Blob {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsRef<[u8]> for Blob {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl From<&[u8]> for Blob {
    fn from(bytes: &[u8]) -> Self {
        Self::from_slice(bytes)
    }
}

impl From<Vec<u8>> for Blob {
    fn from(bytes: Vec<u8>) -> Self {
        Self::from_slice(&bytes)
    }
}

impl From<Bytes> for Blob {
    fn from(bytes: Bytes) -> Self {
        Self::from_slice(&bytes)
    }
}

/// Hex, because a binary attribute is a hash or a context and hex is how both are read.
impl fmt::Debug for Blob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.as_slice() {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// One `DynamoDB` attribute value.
///
/// The set variants exist so an unknown row round-trips; no Lore table uses a set. `M` is boxed to
/// keep the enum's size off the recursion.
#[derive(Clone, Debug, PartialEq)]
pub enum AttributeValue {
    /// Binary.
    B(Blob),
    /// String.
    S(ByteStr),
    /// Number, as decimal text.
    N(NumStr),
    /// Boolean.
    Bool(bool),
    /// The null attribute — present, with no value.
    Null,
    /// Map.
    M(Box<Item>),
    /// List.
    L(Vec<AttributeValue>),
    /// Binary set.
    Bs(Vec<Blob>),
    /// String set.
    Ss(Vec<ByteStr>),
    /// Number set.
    Ns(Vec<NumStr>),
}

impl AttributeValue {
    /// A binary attribute owning a copy of `bytes`.
    pub fn blob(bytes: impl AsRef<[u8]>) -> Self {
        Self::B(Blob::from_slice(bytes.as_ref()))
    }

    /// A string attribute owning a copy of `text`.
    pub fn string(text: impl AsRef<str>) -> Self {
        Self::S(ByteStr::copy_from_str(text.as_ref()))
    }

    /// A number attribute.
    pub fn number(value: impl Into<NumStr>) -> Self {
        Self::N(value.into())
    }

    pub fn as_blob(&self) -> Option<&[u8]> {
        match self {
            Self::B(bytes) => Some(bytes.as_slice()),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::S(text) => Some(text.as_str()),
            _ => None,
        }
    }

    /// The wire tag for this value's type, which is also its JSON key.
    pub fn type_tag(&self) -> &'static str {
        match self {
            Self::B(_) => "B",
            Self::S(_) => "S",
            Self::N(_) => "N",
            Self::Bool(_) => "BOOL",
            Self::Null => "NULL",
            Self::M(_) => "M",
            Self::L(_) => "L",
            Self::Bs(_) => "BS",
            Self::Ss(_) => "SS",
            Self::Ns(_) => "NS",
        }
    }
}

/// A `DynamoDB` item: attribute names in insertion order, each with its value.
#[derive(Clone, Default)]
pub struct Item(SmallVec<[(ByteStr, AttributeValue); ITEM_INLINE]>);

/// Two items are equal when they hold the same attributes in any order: as many entries each, and
/// every entry of either one is also an entry of the other.
///
/// An item is a map on the wire, so attribute order is not part of its identity.
impl PartialEq for Item {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.holds_all(other) && other.holds_all(self)
    }
}

impl Eq for Item {}

impl Item {
    pub fn new() -> Self {
        Self(SmallVec::new())
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self(SmallVec::with_capacity(capacity))
    }

    /// Set `name` to `value`, replacing any existing value for that name.
    ///
    /// Returns the value that was there. Linear, which at four to seven attributes beats hashing
    /// and costs nothing to keep ordered.
    pub fn insert(
        &mut self,
        name: impl Into<ByteStr>,
        value: AttributeValue,
    ) -> Option<AttributeValue> {
        let name = name.into();
        for (existing, slot) in &mut self.0 {
            if existing == &name {
                return Some(std::mem::replace(slot, value));
            }
        }
        self.0.push((name, value));
        None
    }

    /// Append `name` without checking whether it is already present.
    ///
    /// For a parser filling an item it has just created, where the wire cannot repeat a name and
    /// the scan `insert` does would make decoding quadratic in attribute count.
    pub(crate) fn push(&mut self, name: ByteStr, value: AttributeValue) {
        self.0.push((name, value));
    }

    pub fn get(&self, name: &str) -> Option<&AttributeValue> {
        self.0
            .iter()
            .find(|(existing, _)| existing.as_str() == name)
            .map(|(_, value)| value)
    }

    /// Take `name` out of the item, preserving the order of what remains.
    pub fn remove(&mut self, name: &str) -> Option<AttributeValue> {
        let index = self
            .0
            .iter()
            .position(|(existing, _)| existing.as_str() == name)?;

        Some(self.0.remove(index).1)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&ByteStr, &AttributeValue)> {
        self.0.iter().map(|(name, value)| (name, value))
    }

    /// Whether the item still fits in its inline storage. Test-only, to hold the sizing honest.
    #[cfg(feature = "test-util")]
    pub fn spilled(&self) -> bool {
        self.0.spilled()
    }

    /// Whether every entry of `other` is also an entry of this item.
    fn holds_all(&self, other: &Self) -> bool {
        other.0.iter().all(|entry| self.0.contains(entry))
    }
}

impl fmt::Debug for Item {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.0.iter().map(|(name, value)| (name.as_str(), value)))
            .finish()
    }
}

impl IntoIterator for Item {
    type Item = (ByteStr, AttributeValue);
    type IntoIter = smallvec::IntoIter<[(ByteStr, AttributeValue); ITEM_INLINE]>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a Item {
    type Item = (&'a ByteStr, &'a AttributeValue);
    type IntoIter = std::iter::Map<
        std::slice::Iter<'a, (ByteStr, AttributeValue)>,
        fn(&'a (ByteStr, AttributeValue)) -> (&'a ByteStr, &'a AttributeValue),
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter().map(|(name, value)| (name, value))
    }
}

impl FromIterator<(ByteStr, AttributeValue)> for Item {
    fn from_iter<T: IntoIterator<Item = (ByteStr, AttributeValue)>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl<const N: usize> From<[(ByteStr, AttributeValue); N]> for Item {
    fn from(entries: [(ByteStr, AttributeValue); N]) -> Self {
        Self(SmallVec::from_iter(entries))
    }
}

impl From<Item> for AttributeValue {
    fn from(item: Item) -> Self {
        Self::M(Box::new(item))
    }
}
