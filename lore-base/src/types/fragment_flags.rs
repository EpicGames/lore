// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use bitflags::bitflags;

bitflags! {
    /// Bit flags describing how a fragment payload is stored and handled.
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FragmentFlags: u32 {
        /// Payload is fragmented, i.e it is list of fragments
        const PayloadFragmented = 0b1;
        /// Payload is compressed in storage using LZ4 (bit 1)
        const PayloadCompressedLZ4 = 0b10;
        /// Payload is compressed in storage using Oodle2 (bit 2)
        const PayloadCompressedOodle2 = 0b100;
        /// Payload is compressed in storage using Zstd (bit 3)
        const PayloadCompressedZstd = 0b1000;
        /// Payload is compressed (group flag, bits 1-7)
        const PayloadCompressed = 0b11111110;
        /// Payload has been obliterated (bit 8)
        const PayloadObliterated = 0b100000000;
        /// Payload is being obliterated (bit 9)
        const PayloadObliterating = 0b1000000000;
        /// Group mask to check if the payload is anywhere in the obliteration process (bits 8-9)
        const PayloadObliteration = 0b1100000000;
        /// Payload should have local cache priority
        const PayloadLocalCachePriority = 0b10000000000000000;
        /// Payload represents a revision state
        const PayloadRevisionState = 0b100000000000000000;
        /// Payload is stored upstream (exists)
        const PayloadStoredDurable = 0b1000000000000000000;
        /// Payload is stored locally
        const PayloadStoredLocal = 0b100000000000000000000;
        /// Payload stored flags
        const PayloadStored = 0b111000000000000000000;
        /// Payload should not be replicated by the receiver
        const PayloadDoNotReplicate = 0b1000000000000000000000;
    }
}

impl FragmentFlags {
    pub fn as_u32(&self) -> u32 {
        self.bits()
    }
}

impl From<FragmentFlags> for u32 {
    fn from(flags: FragmentFlags) -> Self {
        flags.bits()
    }
}

impl From<u32> for FragmentFlags {
    fn from(value: u32) -> Self {
        Self::from_bits_truncate(value)
    }
}

impl std::cmp::PartialEq<FragmentFlags> for u32 {
    fn eq(&self, value: &FragmentFlags) -> bool {
        value.bits() == *self
    }
}

impl std::ops::BitAnd<FragmentFlags> for u32 {
    type Output = Self;

    fn bitand(self, rhs: FragmentFlags) -> Self {
        self & rhs.bits()
    }
}

impl std::ops::BitAndAssign<FragmentFlags> for u32 {
    fn bitand_assign(&mut self, rhs: FragmentFlags) {
        *self &= rhs.bits();
    }
}

impl std::ops::BitOr<FragmentFlags> for u32 {
    type Output = Self;

    fn bitor(self, rhs: FragmentFlags) -> Self {
        self | rhs.bits()
    }
}

impl std::ops::BitOrAssign<FragmentFlags> for u32 {
    fn bitor_assign(&mut self, rhs: FragmentFlags) {
        *self |= rhs.bits();
    }
}
