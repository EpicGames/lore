// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod branch_types;
pub mod fragment_flags;
pub mod lock_types;
pub mod store_types;
pub mod typed_bytes;
pub mod verify_types;

use std::fmt::Debug;
use std::fmt::Display;
use std::str::FromStr;

pub use branch_types::*;
use bytes::Bytes;
pub use fragment_flags::*;
pub use lock_types::*;
use rand::Rng;
use rand::distr::Distribution;
use rand::distr::StandardUniform;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use serde::de;
pub use store_types::*;
pub use typed_bytes::*;
pub use verify_types::*;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

use crate::error::AddressNotFound;
use crate::error::PayloadNotFound;

/// Alias: a repository is identified by a `Partition`.
pub type RepositoryId = Partition;

/// Alias: a branch is identified by a `Context`.
pub type BranchId = Context;

/// Expected fragment payload size (64 KiB). Used for query batch sizing.
pub const FRAGMENT_SIZE_EXPECTED: usize = 64 * 1024;

/// Fragment size threshold (256 KiB) above which compression is applied.
pub const FRAGMENT_SIZE_THRESHOLD: usize = 256 * 1024;

pub fn serialize_hex<S>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if !serializer.is_human_readable() {
        return serializer.serialize_bytes(value);
    }

    let type_name = std::any::type_name::<S>();
    if type_name.starts_with("serde_dynamo::") {
        return serializer.serialize_bytes(value);
    }

    serializer.serialize_str(hex::encode(value).as_str())
}

struct HexOrBytesVisitor<const N: usize>;

impl<const N: usize> HexOrBytesVisitor<N> {
    const LEN: usize = N;
}

impl<'de, const N: usize> de::Visitor<'de> for HexOrBytesVisitor<N> {
    type Value = [u8; N];

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "a hex string or byte buffer")
    }

    fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(v)
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(&v)
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        let mut out = [0u8; N];
        hex::decode_to_slice(v, &mut out).map_err(serde::de::Error::custom)?;
        Ok(out)
    }

    fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        v.try_into().map_err(|err| {
            serde::de::Error::custom(format!(
                "expecting buffer of length {}, got {}, err: {}",
                Self::LEN,
                v.len(),
                err
            ))
        })
    }

    fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_bytes(v.as_slice())
    }

    fn visit_borrowed_bytes<E>(self, v: &'de [u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_bytes(v)
    }
}

/// Read an `N`-byte identifier written by [`serialize_hex`].
///
/// A self-describing format is asked what it actually holds, so hex text and raw
/// bytes both read back — `DynamoDB` stores these as either, and reads have to
/// take both. A format that cannot answer that question is read as the bytes
/// [`serialize_hex`] wrote for it: asking bitcode to self-describe fails the read
/// outright, which is why every value carrying one of these identifiers used to
/// be unreadable under it.
fn deserialize_raw<'de, D, const N: usize>(deserializer: D) -> Result<[u8; N], D::Error>
where
    D: Deserializer<'de>,
{
    if deserializer.is_human_readable() {
        deserializer.deserialize_any(HexOrBytesVisitor::<N>)
    } else {
        deserializer.deserialize_bytes(HexOrBytesVisitor::<N>)
    }
}

pub fn deserialize_context<'de, D>(deserializer: D) -> Result<[u8; 16], D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_raw::<D, 16>(deserializer)
}

pub fn deserialize_hash<'de, D>(deserializer: D) -> Result<[u8; 32], D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_raw::<D, 32>(deserializer)
}

/// Opaque 128-bit context identifier.
///
/// Binary-compatible with `Partition`. In the storage layer, `Context` is the
/// association tag within an `Address` (e.g., file identity for dedup reasoning),
/// distinct from the `Partition` which identifies the data partition.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    IntoBytes,
    FromBytes,
    Immutable,
    Serialize,
    Deserialize,
    bitcode::Encode,
    bitcode::Decode,
)]
#[serde(transparent)]
pub struct Context {
    #[serde(
        serialize_with = "serialize_hex",
        deserialize_with = "deserialize_context"
    )]
    /// The raw 16 bytes of the identifier.
    data: [u8; 16],
}

/// Opaque 128-bit partition identifier.
///
/// Binary-compatible with `Context`. In the Lore domain, a `Partition` represents
/// a repository identifier; the storage layer uses it to segregate data without
/// understanding what the partition represents.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    IntoBytes,
    FromBytes,
    Immutable,
    Serialize,
    Deserialize,
    bitcode::Encode,
    bitcode::Decode,
)]
#[serde(transparent)]
pub struct Partition {
    #[serde(
        serialize_with = "serialize_hex",
        deserialize_with = "deserialize_context"
    )]
    /// The raw 16 bytes of the identifier.
    data: [u8; 16],
}

/// Opaque 256-bit content hash.
///
/// Identifies a piece of content by the digest of its bytes. Two pieces of
/// identical content share the same hash.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    IntoBytes,
    FromBytes,
    Immutable,
    Serialize,
    Deserialize,
    bitcode::Encode,
    bitcode::Decode,
)]
#[serde(transparent)]
pub struct Hash {
    #[serde(
        serialize_with = "serialize_hex",
        deserialize_with = "deserialize_hash"
    )]
    /// The raw 32 bytes of the hash digest.
    data: [u8; 32],
}

pub const HASH_STRING_LENGTH: usize = std::mem::size_of::<Hash>() * 2;

/// Full address of a piece of content.
///
/// Pairs a content hash with a context identifier, so the same content can be
/// addressed under different contexts.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    IntoBytes,
    FromBytes,
    Immutable,
    bitcode::Encode,
    bitcode::Decode,
)]
pub struct Address {
    /// Content hash.
    pub hash: Hash,
    /// Context identifier paired with the hash.
    pub context: Context,
}

/// Header describing a stored piece of content.
///
/// Records how the payload is stored and how large it is, both as held in
/// storage and once fully reassembled.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    IntoBytes,
    FromBytes,
    Immutable,
    KnownLayout,
    Serialize,
    Deserialize,
    bitcode::Encode,
    bitcode::Decode,
)]
pub struct Fragment {
    /// Flags
    pub flags: u32,
    /// Payload size
    pub size_payload: u32,
    /// Size of the uncompressed and reassembled content
    pub size_content: u64,
}

/// Reference to one fragment within larger reassembled content.
///
/// Names the fragment by its payload hash and records where its bytes sit in
/// the full content.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, IntoBytes, Immutable, FromBytes)]
pub struct FragmentReference {
    /// Payload hash
    pub hash: Hash,
    /// Offset in the full uncompressed and reassembled content of this fragment
    pub offset_content: u64,
}

/// Lightweight sanity validator for a [`Fragment`] received from a remote peer
/// on the read path.
///
/// Unlike the stricter ingress validators in `lore-storage`, this is
/// intentionally permissive about flag bits — peers may legitimately set
/// server-managed flags — and only enforces the invariants that protect a
/// reader against OOM or malformed responses:
///
/// - `0 < size_payload <= FRAGMENT_SIZE_THRESHOLD`
/// - `size_payload <= size_content`
/// - Non-fragmented fragments have `size_content <= FRAGMENT_SIZE_THRESHOLD`
///   (covers both uncompressed and compressed single-fragment responses;
///   only fragmented reference lists legitimately address content larger
///   than the threshold)
/// - Fragmented fragments are not also compressed (these flags are
///   mutually exclusive by protocol)
///
/// A peer that violates these is either buggy or hostile; failing fast here
/// avoids allocating defragment buffers sized off a compromised `size_content`
/// or streaming a payload that can't be valid.
pub fn validate_fragment_response(fragment: &Fragment) -> Result<(), &'static str> {
    if fragment.size_payload == 0 {
        return Err("fragment response has size_payload == 0");
    }
    if (fragment.size_payload as usize) > FRAGMENT_SIZE_THRESHOLD {
        return Err("fragment response size_payload exceeds FRAGMENT_SIZE_THRESHOLD");
    }
    if fragment.size_payload as u64 > fragment.size_content {
        return Err("fragment response size_payload exceeds size_content");
    }

    let is_fragmented = (fragment.flags & FragmentFlags::PayloadFragmented.bits()) != 0;
    let is_compressed = (fragment.flags & FragmentFlags::PayloadCompressed.bits()) != 0;

    if is_fragmented && is_compressed {
        return Err("fragment response has both fragmented and compressed flags set");
    }

    // Non-fragmented fragments materialize their full `size_content` into a
    // single buffer on read (directly for uncompressed, via decompression
    // for compressed). Only fragmented reference lists legitimately point
    // at arbitrarily large content.
    if !is_fragmented && (fragment.size_content as usize) > FRAGMENT_SIZE_THRESHOLD {
        return Err(
            "non-fragmented fragment response size_content exceeds FRAGMENT_SIZE_THRESHOLD",
        );
    }

    Ok(())
}

impl From<Hash> for Bytes {
    fn from(hash: Hash) -> Self {
        Bytes::from_owner(hash.data)
    }
}

impl AsRef<[u8]> for Hash {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

impl From<Hash> for [u8; 32] {
    fn from(hash: Hash) -> Self {
        hash.data
    }
}

impl From<[u8; 32]> for Hash {
    fn from(data: [u8; 32]) -> Self {
        Hash { data }
    }
}

impl From<Bytes> for Hash {
    fn from(bytes: Bytes) -> Self {
        bytes.as_bytes().into()
    }
}

impl From<&Bytes> for Hash {
    fn from(bytes: &Bytes) -> Self {
        bytes.as_bytes().into()
    }
}

impl From<&[u8]> for Hash {
    fn from(bytes: &[u8]) -> Self {
        Hash::read_from_prefix(bytes).unwrap_or_default().0
    }
}

impl From<&[u8; size_of::<Hash>()]> for Hash {
    fn from(bytes: &[u8; size_of::<Hash>()]) -> Self {
        Hash::read_from_bytes(bytes).unwrap_or_default()
    }
}

impl From<[u8; 16]> for Context {
    fn from(data: [u8; 16]) -> Self {
        Context { data }
    }
}

impl From<Context> for Bytes {
    fn from(context: Context) -> Self {
        Bytes::from_owner(context.data)
    }
}

impl From<Bytes> for Context {
    fn from(bytes: Bytes) -> Self {
        bytes.as_bytes().into()
    }
}

impl From<&Bytes> for Context {
    fn from(bytes: &Bytes) -> Self {
        bytes.as_bytes().into()
    }
}

impl From<&[u8]> for Context {
    fn from(bytes: &[u8]) -> Self {
        Context::read_from_prefix(bytes).unwrap_or_default().0
    }
}

impl AsRef<[u8]> for Context {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

impl From<&[u8; size_of::<Context>()]> for Context {
    fn from(bytes: &[u8; size_of::<Context>()]) -> Self {
        Context::read_from_bytes(bytes).unwrap_or_default()
    }
}

impl From<&uuid::Uuid> for Context {
    fn from(uuid: &uuid::Uuid) -> Self {
        uuid.as_bytes().into()
    }
}

impl From<uuid::Uuid> for Context {
    fn from(uuid: uuid::Uuid) -> Self {
        uuid.as_bytes().into()
    }
}

impl From<&Context> for uuid::Uuid {
    fn from(context: &Context) -> Self {
        uuid::Uuid::from_bytes(context.data)
    }
}

impl From<Context> for uuid::Uuid {
    fn from(context: Context) -> Self {
        uuid::Uuid::from_bytes(context.data)
    }
}

impl From<Context> for [u8; 16] {
    fn from(context: Context) -> Self {
        context.data
    }
}

impl From<Partition> for Bytes {
    fn from(partition: Partition) -> Self {
        Bytes::from_owner(partition.data)
    }
}

impl From<Bytes> for Partition {
    fn from(bytes: Bytes) -> Self {
        bytes.as_bytes().into()
    }
}

impl From<&[u8]> for Partition {
    fn from(bytes: &[u8]) -> Self {
        Partition::read_from_prefix(bytes).unwrap_or_default().0
    }
}

impl AsRef<[u8]> for Partition {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

impl From<&uuid::Uuid> for Partition {
    fn from(uuid: &uuid::Uuid) -> Self {
        uuid.as_bytes().into()
    }
}

impl From<uuid::Uuid> for Partition {
    fn from(uuid: uuid::Uuid) -> Self {
        uuid.as_bytes().into()
    }
}

impl From<Partition> for uuid::Uuid {
    fn from(partition: Partition) -> Self {
        uuid::Uuid::from_bytes(partition.into())
    }
}

impl From<Partition> for [u8; 16] {
    fn from(partition: Partition) -> Self {
        partition.data
    }
}

impl From<[u8; 16]> for Partition {
    fn from(data: [u8; 16]) -> Self {
        Partition { data }
    }
}

impl From<&[u8; size_of::<Partition>()]> for Partition {
    fn from(bytes: &[u8; size_of::<Partition>()]) -> Self {
        Partition::read_from_bytes(bytes).unwrap_or_default()
    }
}

impl From<&Bytes> for Partition {
    fn from(bytes: &Bytes) -> Self {
        bytes.as_bytes().into()
    }
}

impl From<Context> for Partition {
    fn from(context: Context) -> Self {
        Partition { data: context.data }
    }
}

impl From<Partition> for Context {
    fn from(partition: Partition) -> Self {
        Context {
            data: partition.data,
        }
    }
}

impl From<Address> for Bytes {
    fn from(address: Address) -> Self {
        Bytes::from_owner(address)
    }
}

impl From<Bytes> for Address {
    fn from(bytes: Bytes) -> Self {
        bytes.as_bytes().into()
    }
}

impl From<&Bytes> for Address {
    fn from(bytes: &Bytes) -> Self {
        bytes.as_bytes().into()
    }
}

impl From<&[u8]> for Address {
    fn from(bytes: &[u8]) -> Self {
        Address::read_from_prefix(bytes).unwrap_or_default().0
    }
}

impl AsRef<[u8]> for Address {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl From<Fragment> for Bytes {
    fn from(fragment: Fragment) -> Self {
        Bytes::from_owner(fragment)
    }
}

impl From<Bytes> for Fragment {
    fn from(bytes: Bytes) -> Self {
        bytes.as_bytes().into()
    }
}

impl From<&Bytes> for Fragment {
    fn from(bytes: &Bytes) -> Self {
        bytes.as_bytes().into()
    }
}

impl From<&[u8]> for Fragment {
    fn from(bytes: &[u8]) -> Self {
        Fragment::read_from_prefix(bytes).unwrap_or_default().0
    }
}

impl AsRef<[u8]> for Fragment {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl From<FragmentReference> for Bytes {
    fn from(reference: FragmentReference) -> Self {
        Bytes::from_owner(reference)
    }
}

impl From<Bytes> for FragmentReference {
    fn from(bytes: Bytes) -> Self {
        bytes.as_bytes().into()
    }
}

impl From<&Bytes> for FragmentReference {
    fn from(bytes: &Bytes) -> Self {
        bytes.as_bytes().into()
    }
}

impl From<&[u8]> for FragmentReference {
    fn from(bytes: &[u8]) -> Self {
        FragmentReference::read_from_prefix(bytes)
            .unwrap_or_default()
            .0
    }
}

impl AsRef<[u8]> for FragmentReference {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

pub trait ZeroHeapAlloc<SelfType = Self>
where
    SelfType: zerocopy::FromBytes,
{
    /// Heap this type's boxed values are taken from, or `None` for the global
    /// allocator.
    ///
    /// The value comes back as a [`crate::allocator::HeapBox`], which releases
    /// it through this same allocator. That is not an implementation detail an
    /// override may work around: a dedicated rpmalloc heap counts every thread
    /// as its owner, so a free that skips the heap corrupts it — see
    /// [`crate::allocator::node_block_allocator`].
    fn heap_allocator() -> Option<&'static (dyn std::alloc::GlobalAlloc + Sync)> {
        None
    }

    fn new_from_heap_zeroed() -> crate::allocator::HeapBox<Self>
    where
        Self: Sized + zerocopy::FromBytes,
    {
        crate::allocator::HeapBox::new_zeroed_in(Self::heap_allocator())
    }
}

pub trait CloneHeapAlloc: zerocopy::IntoBytes + zerocopy::Immutable {
    /// Heap this type's boxed clones are taken from, or `None` for the global
    /// allocator. Same contract as [`ZeroHeapAlloc::heap_allocator`].
    fn heap_allocator() -> Option<&'static (dyn std::alloc::GlobalAlloc + Sync)> {
        None
    }

    fn clone_on_heap(&self) -> crate::allocator::HeapBox<Self>
    where
        Self: Sized,
    {
        crate::allocator::HeapBox::copy_from_in(self, <Self as CloneHeapAlloc>::heap_allocator())
    }
}

impl Address {
    pub fn is_zero(&self) -> bool {
        self.hash.is_zero()
    }

    pub fn zero_context_hash(hash: Hash) -> Self {
        Address {
            context: Context::default(),
            hash,
        }
    }
}

impl Partition {
    pub fn is_zero(&self) -> bool {
        self.data == [0; 16]
    }

    pub fn data(&self) -> &[u8; 16] {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut [u8; 16] {
        &mut self.data
    }
}

impl Hash {
    pub fn hash_buffer(buffer: &[u8]) -> Self {
        let hash = blake3::hash(buffer);
        Hash {
            data: *hash.as_bytes(),
        }
    }

    pub fn is_zero(&self) -> bool {
        self.data == [0; 32]
    }

    pub fn data(&self) -> &[u8; 32] {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut [u8; 32] {
        &mut self.data
    }

    pub fn from_u64(value: u64) -> Self {
        let mut hash = Hash::default();
        hash.data[..std::mem::size_of::<u64>()].copy_from_slice(u64::to_le_bytes(value).as_slice());
        hash
    }

    pub fn to_u64(&self) -> u64 {
        u64::from_le_bytes(self.data[..std::mem::size_of::<u64>()].try_into().unwrap())
    }

    pub fn from_context(value: Context) -> Self {
        let mut hash = Hash::default();
        hash.data[..16].copy_from_slice(value.data().as_slice());
        hash
    }

    pub fn to_context(&self) -> Context {
        let slice = &self.data[..16];
        slice.into()
    }
}

impl Context {
    pub fn is_zero(&self) -> bool {
        self.data == [0; 16]
    }

    pub fn data(&self) -> &[u8; 16] {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut [u8; 16] {
        &mut self.data
    }
}

impl Distribution<Hash> for StandardUniform {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> Hash {
        let mut data = [0u8; size_of::<Hash>()];
        rng.fill(&mut data);
        Hash { data }
    }
}

impl Distribution<Context> for StandardUniform {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> Context {
        let mut data = [0u8; size_of::<Context>()];
        rng.fill(&mut data);
        Context { data }
    }
}

impl Distribution<Partition> for StandardUniform {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> Partition {
        let mut data = [0u8; size_of::<Partition>()];
        rng.fill(&mut data);
        Partition { data }
    }
}

impl Distribution<Address> for StandardUniform {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> Address {
        Address {
            context: rng.random::<Context>(),
            hash: rng.random::<Hash>(),
        }
    }
}

impl std::hash::Hash for Hash {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_bytes().hash(state);
    }
}

impl std::hash::Hash for Address {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_bytes().hash(state);
    }
}

impl std::hash::Hash for Context {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_bytes().hash(state);
    }
}

impl std::hash::Hash for Partition {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_bytes().hash(state);
    }
}

impl std::hash::Hash for Fragment {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_bytes().hash(state);
    }
}

fn from_hex<T>(s: &str) -> Result<T, hex::FromHexError>
where
    T: zerocopy::IntoBytes + zerocopy::FromBytes + Default,
{
    if std::mem::size_of::<T>() * 2 != s.len() {
        return Err(hex::FromHexError::InvalidStringLength);
    }

    let mut val = T::default();
    hex::decode_to_slice(s, val.as_mut_bytes())?;
    Ok(val)
}

impl FromStr for Hash {
    type Err = hex::FromHexError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        from_hex::<Hash>(s)
    }
}

impl FromStr for Context {
    type Err = hex::FromHexError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        from_hex::<Context>(s)
    }
}

impl FromStr for Partition {
    type Err = hex::FromHexError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        from_hex::<Partition>(s)
    }
}

impl FromStr for Address {
    type Err = hex::FromHexError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('-').collect();
        match parts.len() {
            0 => Ok(Address::default()),
            1 => Ok(Address {
                hash: Hash::from_str(parts[0])?,
                context: Context::default(),
            }),
            2 => Ok(Address {
                hash: Hash::from_str(parts[0])?,
                context: Context::from_str(parts[1])?,
            }),
            _ => Err(hex::FromHexError::InvalidStringLength),
        }
    }
}

impl Display for Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.data))
    }
}

impl Debug for Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self}")
    }
}

impl Display for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.data))
    }
}

impl Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(self, f)
    }
}

impl Display for Partition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.data))
    }
}

impl Debug for Partition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self}")
    }
}

impl Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", self.hash, self.context)
    }
}

impl Debug for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self}")
    }
}

impl Serialize for Address {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if serializer.is_human_readable() {
            serializer.serialize_str(&format!("{self}"))
        } else {
            serializer.serialize_bytes(self.as_bytes())
        }
    }
}

struct AddressVisitor;

impl<'de> de::Visitor<'de> for AddressVisitor {
    type Value = Address;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "address format <64 hex>-<32 hex>")
    }

    fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(v)
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(&v)
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Address::from_str(v).map_err(|err| {
            let exp = format!("address format <64 hex>-<32 hex>, {err}");
            serde::de::Error::invalid_value(serde::de::Unexpected::Str(v), &exp.as_str())
        })
    }

    fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        // A short buffer would otherwise convert to the zero address, which is a
        // meaningful value elsewhere, so a truncated one must not silently
        // become it.
        if v.len() != std::mem::size_of::<Address>() {
            return Err(serde::de::Error::invalid_length(
                v.len(),
                &"48 bytes: a 32-byte hash followed by a 16-byte context",
            ));
        }
        Ok(v.into())
    }

    fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_bytes(v.as_slice())
    }

    fn visit_borrowed_bytes<E>(self, v: &'de [u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_bytes(v)
    }
}

impl<'de> Deserialize<'de> for Address {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // A self-describing format is asked what it holds, so both the text and
        // the byte form read back. Bitcode cannot answer, so it is read as the
        // bytes `Serialize` wrote for it.
        if deserializer.is_human_readable() {
            deserializer.deserialize_any(AddressVisitor)
        } else {
            deserializer.deserialize_bytes(AddressVisitor)
        }
    }
}

impl ZeroHeapAlloc for Fragment {}
impl CloneHeapAlloc for Fragment {}

impl From<Address> for AddressNotFound {
    fn from(address: Address) -> Self {
        let bytes: [u8; 48] = {
            let mut buf = [0u8; 48];
            buf[..32].copy_from_slice(address.hash.data());
            buf[32..].copy_from_slice(address.context.data());
            buf
        };
        AddressNotFound { address: bytes }
    }
}

impl From<Hash> for PayloadNotFound {
    fn from(hash: Hash) -> Self {
        PayloadNotFound { hash: *hash.data() }
    }
}

pub struct VecBytes<T>(pub Vec<T>);
impl<T: zerocopy::IntoBytes + zerocopy::Immutable> AsRef<[u8]> for VecBytes<T> {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}
