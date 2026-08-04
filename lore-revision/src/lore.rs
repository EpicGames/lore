// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(non_camel_case_types)]

// Re-export fundamental types from lore-base for internal use
pub(crate) use lore_base::types::Address;
pub(crate) use lore_base::types::CloneHeapAlloc;
pub(crate) use lore_base::types::Context;
pub(crate) use lore_base::types::DirectDownload;
pub(crate) use lore_base::types::Fragment;
pub(crate) use lore_base::types::FragmentReference;
pub(crate) use lore_base::types::HASH_STRING_LENGTH;
pub(crate) use lore_base::types::Hash;
pub(crate) use lore_base::types::Partition;
pub(crate) use lore_base::types::TypedBytes;
pub(crate) use lore_base::types::TypedBytesMut;
pub(crate) use lore_base::types::VecBytes;
pub(crate) use lore_base::types::ZeroHeapAlloc;
pub(crate) use zerocopy::FromBytes;
pub(crate) use zerocopy::IntoBytes;

// Re-export runtime items
pub use crate::runtime::*;

pub(crate) unsafe fn extend_lifetime<'a, T>(data: &'a T) -> &'static T
where
    T: ?Sized,
{
    unsafe { std::mem::transmute::<&'a T, &'static T>(data) }
}

/// In the URC domain, a Partition is the repository identifier.
pub type RepositoryId = lore_base::types::Partition;

/// Branch identifier — a 16-byte opaque ID.
pub type BranchId = lore_base::types::Context;

#[macro_export]
macro_rules! bitflagsops {
    ($to:ty, $base:ty) => {
        impl $to {
            pub fn as_u32(&self) -> u32 {
                self.bits() as u32
            }
        }

        impl From<$to> for $base {
            fn from(flags: $to) -> Self {
                flags.bits()
            }
        }

        impl std::cmp::PartialEq<$to> for $base {
            fn eq(&self, value: &$to) -> bool {
                value.bits() == *self
            }
        }

        impl std::ops::BitAnd<$to> for $base {
            type Output = Self;

            fn bitand(self, rhs: $to) -> $base {
                self & rhs.bits()
            }
        }

        impl std::ops::BitAndAssign<$to> for $base {
            fn bitand_assign(&mut self, rhs: $to) {
                *self &= rhs.bits();
            }
        }

        impl std::ops::BitOr<$to> for $base {
            type Output = Self;

            fn bitor(self, rhs: $to) -> $base {
                self | rhs.bits()
            }
        }

        impl std::ops::BitOrAssign<$to> for $base {
            fn bitor_assign(&mut self, rhs: $to) {
                *self |= rhs.bits();
            }
        }
    };
}
