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
#[derive(Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, IntoBytes, FromBytes, Immutable)]
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

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn hash_hex_roundtrip() {
        let hash = Hash::from([
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
            0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67,
            0x89, 0xab, 0xcd, 0xef,
        ]);
        let s = hash.to_string();
        assert_eq!(s.len(), HASH_STRING_LENGTH);
        let parsed = Hash::from_str(&s).unwrap();
        assert_eq!(hash, parsed);
    }

    #[test]
    fn context_hex_roundtrip() {
        let ctx = Context::from([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
        let s = ctx.to_string();
        let parsed = Context::from_str(&s).unwrap();
        assert_eq!(ctx, parsed);
    }

    #[test]
    fn partition_hex_roundtrip() {
        let p = Partition::from([0xaa; 16]);
        let s = p.to_string();
        let parsed = Partition::from_str(&s).unwrap();
        assert_eq!(p, parsed);
    }

    #[test]
    fn address_display_fromstr_roundtrip() {
        let addr = Address {
            hash: Hash::from([0x42; 32]),
            context: Context::from([0x13; 16]),
        };
        let s = addr.to_string();
        let parsed = Address::from_str(&s).unwrap();
        assert_eq!(addr, parsed);
    }

    #[test]
    fn hash_serde_json_roundtrip() {
        let hash = Hash::from([0xab; 32]);
        let json = serde_json::to_string(&hash).unwrap();
        let parsed: Hash = serde_json::from_str(&json).unwrap();
        assert_eq!(hash, parsed);
    }

    #[test]
    fn context_serde_json_roundtrip() {
        let ctx = Context::from([0xcd; 16]);
        let json = serde_json::to_string(&ctx).unwrap();
        let parsed: Context = serde_json::from_str(&json).unwrap();
        assert_eq!(ctx, parsed);
    }

    #[test]
    fn partition_serde_json_roundtrip() {
        let p = Partition::from([0xef; 16]);
        let json = serde_json::to_string(&p).unwrap();
        let parsed: Partition = serde_json::from_str(&json).unwrap();
        assert_eq!(p, parsed);
    }

    #[test]
    fn address_serde_json_roundtrip() {
        let addr = Address {
            hash: Hash::from([0x11; 32]),
            context: Context::from([0x22; 16]),
        };
        let json = serde_json::to_string(&addr).unwrap();
        let parsed: Address = serde_json::from_str(&json).unwrap();
        assert_eq!(addr, parsed);
    }

    /// JSON is where these identifiers are read by people and by other tools, so
    /// the hex text is a compatibility guarantee, not an implementation detail.
    /// A round-trip alone would not catch the encoding flipping to bytes, since
    /// both directions would flip together.
    #[test]
    fn identifiers_serialize_as_hex_text_in_json() {
        assert_eq!(
            serde_json::to_string(&Hash::from([0xab; 32])).unwrap(),
            format!("\"{}\"", "ab".repeat(32))
        );
        assert_eq!(
            serde_json::to_string(&Context::from([0xcd; 16])).unwrap(),
            format!("\"{}\"", "cd".repeat(16))
        );
        assert_eq!(
            serde_json::to_string(&Partition::from([0xef; 16])).unwrap(),
            format!("\"{}\"", "ef".repeat(16))
        );
        let address = Address {
            hash: Hash::from([0x11; 32]),
            context: Context::from([0x22; 16]),
        };
        assert_eq!(
            serde_json::to_string(&address).unwrap(),
            format!("\"{}-{}\"", "11".repeat(32), "22".repeat(16))
        );
    }

    /// A non-self-describing format cannot be asked what a value is, so reading
    /// one of these back used to fail outright: the deserializers called
    /// `deserialize_any`, which bitcode rejects. Every command and record
    /// carrying an identifier depends on this.
    #[test]
    fn identifiers_roundtrip_through_bitcode() {
        let hash = Hash::from([0xab; 32]);
        assert_eq!(
            bitcode::deserialize::<Hash>(&bitcode::serialize(&hash).unwrap()).unwrap(),
            hash
        );

        let context = Context::from([0xcd; 16]);
        assert_eq!(
            bitcode::deserialize::<Context>(&bitcode::serialize(&context).unwrap()).unwrap(),
            context
        );

        let partition = Partition::from([0xef; 16]);
        assert_eq!(
            bitcode::deserialize::<Partition>(&bitcode::serialize(&partition).unwrap()).unwrap(),
            partition
        );

        let address = Address {
            hash: Hash::from([0x11; 32]),
            context: Context::from([0x22; 16]),
        };
        assert_eq!(
            bitcode::deserialize::<Address>(&bitcode::serialize(&address).unwrap()).unwrap(),
            address
        );
    }

    /// An identifier nested inside a struct is the shape that actually crosses
    /// the service wire, and it exercises the field attributes rather than the
    /// bare type.
    #[test]
    fn a_struct_of_identifiers_roundtrips_through_bitcode() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Record {
            address: Address,
            partition: Partition,
            hashes: Vec<Hash>,
        }

        let record = Record {
            address: Address {
                hash: Hash::from([0x37; 32]),
                context: Context::from([0x73; 16]),
            },
            partition: Partition::from([0x01; 16]),
            hashes: vec![Hash::from([0u8; 32]), Hash::from([0xff; 32])],
        };
        let encoded = bitcode::serialize(&record).unwrap();
        assert_eq!(bitcode::deserialize::<Record>(&encoded).unwrap(), record);
    }

    /// A short buffer converts to the zero address, which is a meaningful value
    /// elsewhere, so a truncated one must be refused rather than silently become
    /// it.
    #[test]
    fn a_truncated_address_is_refused_rather_than_zeroed() {
        let address = Address {
            hash: Hash::from([0x11; 32]),
            context: Context::from([0x22; 16]),
        };
        let encoded = bitcode::serialize(&address.as_bytes()[..40].to_vec()).unwrap();
        let parsed = bitcode::deserialize::<Address>(&encoded);
        assert!(
            parsed.is_err(),
            "a 40-byte address must not deserialize, got {parsed:?}"
        );
    }

    #[test]
    fn fragment_serde_json_roundtrip() {
        let frag = Fragment {
            flags: 0x1234,
            size_payload: 5678,
            size_content: 9012,
        };
        let json = serde_json::to_string(&frag).unwrap();
        let parsed: Fragment = serde_json::from_str(&json).unwrap();
        assert_eq!(frag, parsed);
    }

    #[test]
    fn hash_is_zero() {
        assert!(Hash::default().is_zero());
        assert!(!Hash::from([1; 32]).is_zero());
    }

    #[test]
    fn context_is_zero() {
        assert!(Context::default().is_zero());
        assert!(!Context::from([1; 16]).is_zero());
    }

    #[test]
    fn partition_is_zero() {
        assert!(Partition::default().is_zero());
        assert!(!Partition::from([1; 16]).is_zero());
    }

    #[test]
    fn address_is_zero() {
        assert!(Address::default().is_zero());
    }

    #[test]
    fn hash_from_u64_roundtrip() {
        let val: u64 = 0xdeadbeef_cafebabe;
        let hash = Hash::from_u64(val);
        assert_eq!(hash.to_u64(), val);
    }

    #[test]
    fn hash_context_roundtrip() {
        let ctx = Context::from([0x42; 16]);
        let hash = Hash::from_context(ctx);
        assert_eq!(hash.to_context(), ctx);
    }

    #[test]
    fn partition_context_conversion() {
        let ctx = Context::from([0x55; 16]);
        let p: Partition = ctx.into();
        assert_eq!(p.data(), ctx.data());
        let ctx2: Context = p.into();
        assert_eq!(ctx, ctx2);
    }

    #[test]
    fn context_uuid_roundtrip() {
        let ctx = Context::from([0x77; 16]);
        let uuid: uuid::Uuid = ctx.into();
        let ctx2: Context = (&uuid).into();
        assert_eq!(ctx, ctx2);
    }

    #[test]
    fn fragment_reference_layout() {
        assert_eq!(
            std::mem::size_of::<FragmentReference>(),
            std::mem::size_of::<Hash>() + std::mem::size_of::<u64>(),
        );
    }

    #[test]
    fn typed_bytes_count_and_slice() {
        use bytes::Bytes;
        let data: Vec<u32> = vec![1, 2, 3, 4];
        let vb = VecBytes(data);
        let bytes = Bytes::copy_from_slice(vb.as_ref());
        assert_eq!(bytes.count::<u32>(), 4);
        let slice = bytes.as_type_slice::<u32>();
        assert_eq!(slice, &[1, 2, 3, 4]);
    }

    #[test]
    fn typed_bytes_mut_zeroed_count() {
        use bytes::BytesMut;
        let buf = BytesMut::zeroed_count::<u64>(3);
        assert_eq!(buf.len(), 3 * std::mem::size_of::<u64>());
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn vec_bytes_as_ref() {
        let v = VecBytes(vec![1u32, 2, 3]);
        let bytes: &[u8] = v.as_ref();
        assert_eq!(bytes.len(), 3 * std::mem::size_of::<u32>());
    }

    #[test]
    fn hash_from_slice_and_as_bytes() {
        let test_hash = "0123456789abcdefabcdef09876543210123456789abcdefabcdef0987654321";
        let test_bytes: Vec<u8> = (0..test_hash.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&test_hash[i..i + 2], 16).unwrap())
            .collect();

        let h = Hash::from_str(test_hash).expect("Hash creation failed");
        assert_eq!(format!("{h}"), test_hash);
        let h2 = Hash::from(&test_bytes[..]);
        let h3 = Hash::from(h.as_bytes());
        assert_eq!(h, h2);
        assert_eq!(h, h3);
        assert_eq!(test_bytes, h.as_bytes());
    }

    #[test]
    fn context_from_slice_and_as_bytes() {
        let test_context = "0123456789abcdefabcdef0987654321";
        let test_bytes: Vec<u8> = (0..test_context.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&test_context[i..i + 2], 16).unwrap())
            .collect();

        let h = Context::from_str(test_context).expect("Context creation failed");
        assert_eq!(format!("{h}"), test_context);
        let h2 = Context::from(&test_bytes[..]);
        let h3 = Context::from(h.as_bytes());
        assert_eq!(h, h2);
        assert_eq!(h, h3);
        assert_eq!(test_bytes, h.as_bytes());
    }

    #[test]
    fn address_from_slice_and_as_bytes() {
        let test_context = "0123456789abccccabcdef0987654321";
        let test_hash = "0123456789abcdefddddef09876543210123456789abcdefabcdef0987654321";
        let test_addr = format!("{test_hash}-{test_context}");
        let test_str_bytes = format!("{test_hash}{test_context}");
        let test_bytes: Vec<u8> = (0..test_str_bytes.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&test_str_bytes[i..i + 2], 16).unwrap())
            .collect();

        let h = Address::from_str(&test_addr).expect("Address creation failed");
        assert_eq!(format!("{h}"), test_addr);
        let h2 = Address::from(&test_bytes[..]);
        assert_eq!(format!("{h2}"), test_addr);
        let h3 = Address::from(h.as_bytes());
        assert_eq!(h, h2);
        assert_eq!(h, h3);
        assert_eq!(test_bytes, h.as_bytes());
    }

    #[test]
    fn hash_buffer_deterministic() {
        let hash = Hash::hash_buffer(b"test hash");
        assert_eq!(
            "622eeba4ec46cec1ba0fb55b988f48b88856a1cc3b3d0064074f798af0b88597",
            format!("{hash}")
        );
    }

    #[test]
    fn formatting_debug_eq_display() {
        let hash = Hash::hash_buffer(b"test hash");
        let context =
            Context::from_str("0123456789abccccabcdef0987654321").expect("Context creation failed");
        let address = Address { hash, context };

        assert_eq!(format!("{hash}"), format!("{hash:?}"));
        assert_eq!(format!("{context}"), format!("{context:?}"));
        assert_eq!(format!("{address}"), format!("{address:?}"));
    }

    #[test]
    fn hash_from_str_invalid_hex() {
        assert!(Hash::from_str("not_valid_hex").is_err());
    }

    #[test]
    fn hash_from_str_wrong_length() {
        assert!(Hash::from_str("aabb").is_err());
    }

    #[test]
    fn hash_from_str_empty() {
        assert!(Hash::from_str("").is_err());
    }

    #[test]
    fn context_from_str_wrong_length() {
        assert!(Context::from_str("aabb").is_err());
    }

    #[test]
    fn partition_from_str_invalid_hex() {
        assert!(Partition::from_str("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").is_err());
    }

    #[test]
    fn address_from_str_too_many_parts() {
        assert!(Address::from_str("aa-bb-cc").is_err());
    }

    #[test]
    fn address_from_str_invalid_hash_part() {
        assert!(Address::from_str("not_hex").is_err());
    }

    #[test]
    fn hash_from_short_slice_returns_default() {
        let short: &[u8] = &[1, 2, 3];
        let hash = Hash::from(short);
        assert!(hash.is_zero());
    }

    #[test]
    fn context_from_short_slice_returns_default() {
        let short: &[u8] = &[1, 2];
        let ctx = Context::from(short);
        assert!(ctx.is_zero());
    }

    #[test]
    fn typed_bytes_to_aligned_already_aligned() {
        use bytes::Bytes;
        let data = vec![1u64, 2, 3];
        let bytes = Bytes::copy_from_slice(VecBytes(data).as_ref());
        let aligned = bytes.clone().to_aligned::<u64>();
        assert_eq!(aligned, bytes);
    }

    #[test]
    fn typed_bytes_to_aligned_empty() {
        use bytes::Bytes;
        let bytes = Bytes::new();
        let aligned = bytes.clone().to_aligned::<u64>();
        assert_eq!(aligned, bytes);
    }

    #[test]
    fn zero_heap_alloc_produces_zeroed_fragment() {
        let boxed = Fragment::new_from_heap_zeroed();
        assert_eq!(boxed.flags, 0);
        assert_eq!(boxed.size_payload, 0);
        assert_eq!(boxed.size_content, 0);
    }

    #[test]
    fn clone_heap_alloc_preserves_data() {
        let frag = Fragment {
            flags: 42,
            size_payload: 100,
            size_content: 200,
        };
        let boxed = frag.clone_on_heap();
        assert_eq!(*boxed, frag);
    }

    #[test]
    fn partition_bytes_roundtrip() {
        use bytes::Bytes;
        let p = Partition::from([0xab; 16]);
        let bytes: Bytes = p.into();
        assert_eq!(bytes.len(), 16);
        let recovered = Partition::from(&bytes);
        assert_eq!(p, recovered);
    }

    #[test]
    fn hash_bytes_roundtrip() {
        use bytes::Bytes;
        let h = Hash::from([0xcd; 32]);
        let bytes: Bytes = h.into();
        assert_eq!(bytes.len(), 32);
        let recovered = Hash::from(&bytes);
        assert_eq!(h, recovered);
    }

    #[test]
    fn context_bytes_roundtrip() {
        use bytes::Bytes;
        let ctx = Context::from([0xef; 16]);
        let bytes: Bytes = ctx.into();
        assert_eq!(bytes.len(), 16);
        let recovered = Context::from(&bytes);
        assert_eq!(ctx, recovered);
    }

    #[test]
    fn address_bytes_roundtrip() {
        use bytes::Bytes;
        let addr = Address {
            hash: Hash::from([0x11; 32]),
            context: Context::from([0x22; 16]),
        };
        let bytes: Bytes = addr.into();
        assert_eq!(bytes.len(), std::mem::size_of::<Address>());
        let recovered = Address::from(&bytes);
        assert_eq!(addr, recovered);
    }

    #[test]
    fn hash_from_array_preserves_data() {
        let arr = [0x42u8; 32];
        assert_eq!(Hash::from(arr).data(), &arr);
    }

    #[test]
    fn partition_from_array_preserves_data() {
        let arr = [0x42u8; 16];
        assert_eq!(Partition::from(arr).data(), &arr);
    }

    #[test]
    fn context_from_array_preserves_data() {
        let arr = [0x42u8; 16];
        assert_eq!(Context::from(arr).data(), &arr);
    }

    #[test]
    fn clone_and_resize_zeroed_grow() {
        use bytes::Bytes;
        let original = Bytes::from_static(&[1u8, 2, 3, 4]);
        let resized = original.clone_and_resize_zeroed::<u8>(8);
        assert_eq!(resized.len(), 8);
        assert_eq!(&resized[..4], &[1, 2, 3, 4]);
        assert_eq!(&resized[4..], &[0, 0, 0, 0]);
    }

    #[test]
    fn clone_and_resize_zeroed_same_size() {
        use bytes::Bytes;
        let original = Bytes::from_static(&[5u8, 6, 7, 8]);
        let resized = original.clone_and_resize_zeroed::<u8>(4);
        assert_eq!(resized.len(), 4);
        assert_eq!(&resized[..], &[5, 6, 7, 8]);
    }

    #[test]
    fn typed_bytes_mut_with_count_capacity() {
        use bytes::BytesMut;
        let buf = BytesMut::with_count_capacity::<u32>(5);
        assert_eq!(buf.capacity(), 5 * std::mem::size_of::<u32>());
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn typed_bytes_mut_set_count() {
        use bytes::BytesMut;
        let mut buf = BytesMut::zeroed_count::<u32>(4);
        assert_eq!(buf.count::<u32>(), 4);
        unsafe { buf.set_count::<u32>(2) };
        assert_eq!(buf.count::<u32>(), 2);
        assert_eq!(buf.len(), 2 * std::mem::size_of::<u32>());
    }

    #[test]
    fn fragment_bytes_roundtrip() {
        use bytes::Bytes;
        let frag = Fragment {
            flags: 0xaa,
            size_payload: 1000,
            size_content: 2000,
        };
        let bytes: Bytes = frag.into();
        let recovered = Fragment::from(&bytes);
        assert_eq!(frag, recovered);
    }

    #[test]
    fn fragment_as_ref_slice_roundtrip() {
        let frag = Fragment {
            flags: 7,
            size_payload: 64,
            size_content: 128,
        };
        let slice: &[u8] = frag.as_ref();
        let recovered = Fragment::from(slice);
        assert_eq!(frag, recovered);
    }

    #[test]
    fn fragment_reference_bytes_roundtrip() {
        use bytes::Bytes;
        let fref = FragmentReference {
            hash: Hash::from([0xbb; 32]),
            offset_content: 12345,
        };
        let bytes: Bytes = fref.into();
        let recovered = FragmentReference::from(&bytes);
        assert_eq!(fref, recovered);
    }

    #[test]
    fn fragment_reference_as_ref_roundtrip() {
        let fref = FragmentReference {
            hash: Hash::from([0xcc; 32]),
            offset_content: 99999,
        };
        let slice: &[u8] = fref.as_ref();
        let recovered = FragmentReference::from(slice);
        assert_eq!(fref, recovered);
    }

    #[test]
    fn partition_uuid_roundtrip() {
        let p = Partition::from([0x88; 16]);
        let uuid: uuid::Uuid = p.into();
        let p2: Partition = uuid.into();
        assert_eq!(p, p2);
    }

    #[test]
    fn address_zero_context_hash() {
        let hash = Hash::from([0xaa; 32]);
        let addr = Address::zero_context_hash(hash);
        assert_eq!(addr.hash, hash);
        assert!(addr.context.is_zero());
    }

    #[test]
    fn address_from_str_hash_only() {
        let hash_hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let addr = Address::from_str(hash_hex).unwrap();
        assert_eq!(format!("{}", addr.hash), hash_hex);
        assert!(addr.context.is_zero());
    }

    #[test]
    fn hash_buffer_empty_input() {
        let hash = Hash::hash_buffer(b"");
        assert!(!hash.is_zero());
        let hash2 = Hash::hash_buffer(b"");
        assert_eq!(hash, hash2);
    }

    #[test]
    fn partition_from_str_wrong_length() {
        assert!(Partition::from_str("aabb").is_err());
    }

    #[test]
    fn typed_bytes_from_type_static() {
        use bytes::Bytes;
        static DATA: [u32; 3] = [10, 20, 30];
        let bytes = Bytes::from_type_static(&DATA);
        assert_eq!(bytes.len(), 3 * std::mem::size_of::<u32>());
        let slice = bytes.as_type_slice::<u32>();
        assert_eq!(slice, &[10, 20, 30]);
    }

    #[test]
    fn typed_bytes_mut_as_type_slice_mut() {
        use bytes::BytesMut;
        let mut buf = BytesMut::zeroed_count::<u32>(4);
        let slice = buf.as_type_slice_mut::<u32>();
        slice[0] = 42;
        slice[1] = 99;
        let read_slice = buf.as_type_slice::<u32>();
        assert_eq!(read_slice[0], 42);
        assert_eq!(read_slice[1], 99);
    }

    /// A buffer with no capacity never allocated, so its pointer is dangling and aligned for
    /// bytes rather than for the type. A zero-length slice still requires an aligned pointer,
    /// so the count has to be answered without forming one.
    #[test]
    fn typed_bytes_mut_type_slices_are_empty_without_capacity() {
        use bytes::BytesMut;
        let mut buf = BytesMut::with_count_capacity::<FragmentReference>(0);
        assert!(buf.as_type_slice_mut::<FragmentReference>().is_empty());
        assert!(buf.as_type_slice::<FragmentReference>().is_empty());
    }

    /// A capacity too small to hold one element is the same case as none at all.
    #[test]
    fn typed_bytes_mut_type_slices_are_empty_below_one_element() {
        use bytes::BytesMut;
        let mut buf = BytesMut::with_capacity(std::mem::size_of::<FragmentReference>() - 1);
        assert!(buf.as_type_slice_mut::<FragmentReference>().is_empty());
        assert!(buf.as_type_slice::<FragmentReference>().is_empty());
    }

    mod validate_response {
        use super::*;

        #[test]
        fn accepts_uncompressed_unfragmented() {
            let fragment = Fragment {
                flags: 0,
                size_payload: 128,
                size_content: 128,
            };
            assert!(validate_fragment_response(&fragment).is_ok());
        }

        #[test]
        fn accepts_server_managed_flags_unlike_ingress_validator() {
            let fragment = Fragment {
                flags: FragmentFlags::PayloadStoredDurable.into(),
                size_payload: 128,
                size_content: 128,
            };
            assert!(validate_fragment_response(&fragment).is_ok());
        }

        #[test]
        fn accepts_compressed_within_threshold() {
            let fragment = Fragment {
                flags: FragmentFlags::PayloadCompressedLZ4.into(),
                size_payload: 100,
                size_content: 200,
            };
            assert!(validate_fragment_response(&fragment).is_ok());
        }

        #[test]
        fn accepts_fragmented_addressing_huge_content() {
            let fragment = Fragment {
                flags: FragmentFlags::PayloadFragmented.into(),
                size_payload: 80,
                size_content: 10 * 1024 * 1024 * 1024, // 10 GiB of referenced content
            };
            assert!(validate_fragment_response(&fragment).is_ok());
        }

        #[test]
        fn rejects_zero_size_payload() {
            let fragment = Fragment {
                flags: 0,
                size_payload: 0,
                size_content: 0,
            };
            assert!(validate_fragment_response(&fragment).is_err());
        }

        #[test]
        fn rejects_oversized_payload() {
            let fragment = Fragment {
                flags: 0,
                size_payload: FRAGMENT_SIZE_THRESHOLD as u32 + 1,
                size_content: FRAGMENT_SIZE_THRESHOLD as u64 + 1,
            };
            assert!(validate_fragment_response(&fragment).is_err());
        }

        #[test]
        fn rejects_payload_greater_than_content() {
            let fragment = Fragment {
                flags: 0,
                size_payload: 200,
                size_content: 100,
            };
            assert!(validate_fragment_response(&fragment).is_err());
        }

        #[test]
        fn rejects_non_fragmented_oversized_content() {
            // Compressed or uncompressed, a non-fragmented fragment must not
            // claim size_content larger than FRAGMENT_SIZE_THRESHOLD because
            // it materializes into a single buffer on read.
            let fragment = Fragment {
                flags: FragmentFlags::PayloadCompressedLZ4.into(),
                size_payload: 1000,
                size_content: FRAGMENT_SIZE_THRESHOLD as u64 + 1,
            };
            assert!(validate_fragment_response(&fragment).is_err());
        }

        #[test]
        fn rejects_fragmented_and_compressed_combo() {
            let fragment = Fragment {
                flags: (FragmentFlags::PayloadFragmented | FragmentFlags::PayloadCompressedLZ4)
                    .into(),
                size_payload: 80,
                size_content: 2000,
            };
            assert!(validate_fragment_response(&fragment).is_err());
        }
    }
}
