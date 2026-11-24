// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The schema of a type's bitcode encoding, which a client and the service compare before they
//! exchange messages, as ADR-00025 decides.
//!
//! Bitcode identifies a field and an enum variant by position, so two builds whose message types
//! differ can read each other's messages as different values. [`WireSchema::SCHEMA`] hashes what a
//! decode depends on: the order of a type's fields and variants, their names, and the schema of
//! each field's type. Two builds that agree on it read each other's messages as written.
//!
//! Types derive it with `lore_macro::WireSchema` beside `bitcode::Encode` and `bitcode::Decode`.
//! The hash is FNV-1a, computed at compile time. A schema is a `const` built from the schemas of
//! its fields' types, so the hash has to run in `const` code, and it is folded one short piece at a
//! time: a name, a count, a field's schema. FNV-1a does that in a few lines, without the dependency
//! `xxhash-rust`'s `const_xxh3` would add to this crate, whose one-shot hash is made for long
//! inputs. It guards against a peer from another build, not against a peer that forges a hash.

/// A hash of the schema of a type's bitcode encoding.
pub trait WireSchema {
    /// Equal for two types when their bitcode encodings agree field for field and name for name.
    const SCHEMA: u64;
}

/// The hash a schema starts from: the FNV-1a offset basis.
const SCHEMA_SEED: u64 = 0xcbf2_9ce4_8422_2325;

const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Mixes `bytes` into `hash`.
const fn schema_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
        index += 1;
    }
    hash
}

/// Mixes `value` into `hash`.
pub const fn schema_u64(hash: u64, value: u64) -> u64 {
    schema_bytes(hash, &value.to_le_bytes())
}

/// Mixes `text` into `hash`, its length first, so that two names in a row can't read as one.
pub const fn schema_text(hash: u64, text: &str) -> u64 {
    schema_bytes(schema_u64(hash, text.len() as u64), text.as_bytes())
}

/// The schema of a type with no parts, such as a primitive, named for itself.
pub const fn schema_leaf(name: &str) -> u64 {
    schema_text(SCHEMA_SEED, name)
}

/// The schema of a type that holds values of one other type, such as a container.
const fn schema_of_one(name: &str, inner: u64) -> u64 {
    schema_u64(schema_leaf(name), inner)
}

macro_rules! leaf_schema {
    ($($leaf:ty),* $(,)?) => {
        $(impl WireSchema for $leaf {
            const SCHEMA: u64 = schema_leaf(stringify!($leaf));
        })*
    };
}

leaf_schema!(
    bool, u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize, f32, f64, char, String
);

impl<T: WireSchema> WireSchema for Option<T> {
    const SCHEMA: u64 = schema_of_one("Option", T::SCHEMA);
}

impl<T: WireSchema> WireSchema for Vec<T> {
    const SCHEMA: u64 = schema_of_one("Vec", T::SCHEMA);
}

impl<T: WireSchema + ?Sized> WireSchema for Box<T> {
    const SCHEMA: u64 = schema_of_one("Box", T::SCHEMA);
}

impl<T: WireSchema> WireSchema for [T] {
    const SCHEMA: u64 = schema_of_one("slice", T::SCHEMA);
}

impl<T: WireSchema, const N: usize> WireSchema for [T; N] {
    const SCHEMA: u64 = schema_u64(schema_of_one("array", T::SCHEMA), N as u64);
}
