// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use core::str;
use std::ops::BitAnd;
use std::ops::BitAndAssign;
use std::ops::BitOr;
use std::ops::BitOrAssign;
use std::sync::Arc;

use bitflags::bitflags;
use bytes::BytesMut;
use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
use lore_error_set::prelude::*;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

use crate::immutable;
use crate::immutable::ReadFromImmutable;
use crate::immutable::read_options_from_repository;
use crate::lore::Address;
use crate::lore::Hash;
use crate::lore::TypedBytesMut;
use crate::repository::RepositoryContext;

// Append only hash table for string data which can be serialized/deserialized in
// single calls to the immutable store

const PRIME_TABLE: [usize; 8] = [
    3943, 33211, 210127, 548579, 1378673, 3222269, 8181421, 26196523,
];

#[error_set]
pub enum NameTableError {}

bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct NameTableFlags: u32 {
        /// Name table is dirty
        const Dirty = 0b1;
    }
}

impl NameTableFlags {
    pub fn as_u32(&self) -> u32 {
        self.bits()
    }
}

impl From<NameTableFlags> for u32 {
    fn from(flags: NameTableFlags) -> Self {
        flags.bits()
    }
}

impl BitAnd<NameTableFlags> for u32 {
    type Output = Self;

    fn bitand(self, rhs: NameTableFlags) -> Self {
        self & rhs.bits()
    }
}

impl BitAndAssign<NameTableFlags> for u32 {
    fn bitand_assign(&mut self, rhs: NameTableFlags) {
        *self &= rhs.bits();
    }
}

impl BitOr<NameTableFlags> for u32 {
    type Output = Self;

    fn bitor(self, rhs: NameTableFlags) -> Self {
        self | rhs.bits()
    }
}

impl BitOrAssign<NameTableFlags> for u32 {
    fn bitor_assign(&mut self, rhs: NameTableFlags) {
        *self |= rhs.bits();
    }
}

/// Name table state
#[repr(C)]
#[derive(Copy, Clone, Default, IntoBytes, FromBytes, Immutable)]
struct NameTableState {
    /// Hashtable capacity
    _reserved_0: u64,
    /// Hashtable current count
    entry_count: u64,
    /// Entry payload
    entry_payload: Hash,
    /// Data capacity
    _reserved_1: u64,
    /// Data current count
    _reserved_2: u64,
    /// Data payload
    data_payload: Hash,
    /// Flags
    _reserved_3: u32,
    /// Reserved for future use
    _reserved_4: u32,
}

/// Entry in name table
#[repr(C)]
#[derive(Copy, Clone, Default, IntoBytes, FromBytes, Immutable)]
struct NameTableEntry {
    /// Hash key
    key: u64,
    /// Value offset
    offset: u64,
}

/// Name table
#[derive(Clone, Default)]
pub struct NameTableData {
    /// Entries buffer
    entry_buffer: BytesMut,
    /// Data buffer
    data_buffer: BytesMut,
}

/// Name table container
pub struct NameTable {
    /// Data
    data: parking_lot::RwLock<NameTableData>,
}

impl Default for NameTable {
    fn default() -> Self {
        let entry_capacity = PRIME_TABLE[0];
        let data_capacity = FRAGMENT_SIZE_THRESHOLD;
        Self {
            data: parking_lot::RwLock::new(NameTableData {
                entry_buffer: BytesMut::zeroed_count::<NameTableEntry>(entry_capacity),
                data_buffer: BytesMut::with_capacity(data_capacity),
            }),
        }
    }
}

impl NameTable {
    pub async fn deserialize(
        repository: Arc<RepositoryContext>,
        hash: Hash,
    ) -> Result<Self, NameTableError> {
        let state = NameTableState::read_from_immutable(
            repository.clone(),
            Address::zero_context_hash(hash),
            read_options_from_repository(&repository).with_priority(),
        )
        .await
        .internal("deserializing name table")?;

        // For now bound the size of the name table, since it is deprecated and not in active use
        let mut entry_buffer = BytesMut::from(
            immutable::read(
                repository.clone(),
                Address::zero_context_hash(state.entry_payload),
                Some(0..size_of::<NameTableEntry>() * PRIME_TABLE.last().unwrap_or(&0)), /* Size bound */
                read_options_from_repository(&repository)
                    .with_cache()
                    .with_priority(),
            )
            .await
            .internal("reading name table entry buffer")?,
        );

        // Align entry buffer count to a prime
        let entry_count = entry_buffer.count::<NameTableEntry>();
        let mut iprime = 0;
        while iprime < PRIME_TABLE.len() && entry_count > PRIME_TABLE[iprime] {
            iprime += 1;
        }
        if iprime < PRIME_TABLE.len() && entry_count != PRIME_TABLE[iprime] {
            entry_buffer.resize(PRIME_TABLE[iprime], 0);
        }

        let data_buffer = BytesMut::from(
            immutable::read(
                repository.clone(),
                Address::zero_context_hash(state.data_payload),
                Some(0..256 * 1024 * 1024), /* Size bound to 256MiB */
                read_options_from_repository(&repository)
                    .with_cache()
                    .with_priority(),
            )
            .await
            .internal("reading name table data buffer")?,
        );

        Ok(Self {
            data: parking_lot::RwLock::new(NameTableData {
                entry_buffer,
                data_buffer,
            }),
        })
    }

    /// Look up a name by its hash in the deprecated name table.
    ///
    /// The `NameTable` type is deprecated — no new data is written using this
    /// format. It is only read during V0 block deserialization to migrate old
    /// repositories. The bounds checks and UTF-8 validation here are sufficient
    /// to protect against malicious or corrupt entries; out-of-bounds offsets
    /// and invalid UTF-8 return an empty string rather than causing UB.
    ///
    /// Returns the name string if found, or an empty string if the hash is not
    /// present or the entry references out-of-bounds or invalid data.
    ///
    /// # Safety justification
    ///
    /// The returned `&str` borrows from `self` but is constructed from a raw
    /// pointer into `data_buffer` behind an `RwLock`. This is sound because
    /// `BytesMut` owns its heap allocation, the `NameTable` is append-only
    /// (entries are never removed or relocated), and all callers immediately
    /// copy the result to an owned `String` before the borrow expires.
    pub fn load(&self, hash: u64) -> &str {
        // TODO(mjansson): Garbage collect nametable by iterating entire merkle tree and rebuiling
        // nametable, cleaning out stale entries
        let data = self.data.read();
        let entry = data.entry_buffer.as_type_slice::<NameTableEntry>();
        if entry.is_empty() {
            return "";
        }
        let mut slot = (hash as usize) % entry.len();

        let start = slot;
        loop {
            if entry[slot].key == hash {
                break;
            }
            if entry[slot].key == 0 {
                return "";
            }
            slot += 1;
            if slot >= entry.len() {
                slot = 0;
            }
            if slot == start {
                return "";
            }
        }

        let data_slice = data.data_buffer.as_ref();
        let offset = entry[slot].offset as usize;
        let length_header_size = std::mem::size_of::<u32>();

        // Validate that offset + 4-byte length header fits in the data buffer.
        if offset
            .checked_add(length_header_size)
            .is_none_or(|end| end > data_slice.len())
        {
            return "";
        }

        // SAFETY: offset..offset+4 is within data_slice bounds (checked above).
        let raw_pointer = unsafe { data_slice.as_ptr().add(offset).cast::<u32>() };
        let length = unsafe { raw_pointer.read_unaligned() } as usize;

        // Validate that the string payload fits in the remaining data buffer.
        if (offset + length_header_size)
            .checked_add(length)
            .is_none_or(|end| end > data_slice.len())
        {
            return "";
        }

        // SAFETY: offset+4..offset+4+length is within data_slice bounds (checked above).
        let bytes = unsafe { std::slice::from_raw_parts(raw_pointer.add(1).cast::<u8>(), length) };
        str::from_utf8(bytes).unwrap_or("")
    }
}
