// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The schema `lore_macro::WireSchema` derives changes with each change to a type's bitcode
//! encoding, and only with those.
//!
//! Each module holds one version of the types, under the same names, so that a test compares a
//! type with an edit of itself.
#![allow(
    dead_code,
    reason = "the types exist for their schemas, and no value of one is made"
)]

use lore_base::env::AUTH_PATH_VAR;
use lore_base::env::CALL_VARIABLES;
use lore_base::env::CallEnvironment;
use lore_base::env::GLOBAL_PATH_VAR;
use lore_base::env::call_environment_schema;
use lore_base::wire_schema::WireSchema;

mod base {
    #[derive(lore_macro::WireSchema)]
    pub struct Args {
        pub path: String,
        pub count: u32,
        pub flags: Option<Vec<u8>>,
    }

    #[derive(lore_macro::WireSchema)]
    pub enum Command {
        Add(Args),
        Remove { path: String },
        Clear,
    }

    #[derive(lore_macro::WireSchema)]
    pub struct Message<Event = u32> {
        pub event: Event,
    }
}

mod reordered_fields {
    #[derive(lore_macro::WireSchema)]
    pub struct Args {
        pub count: u32,
        pub path: String,
        pub flags: Option<Vec<u8>>,
    }
}

mod renamed_field {
    #[derive(lore_macro::WireSchema)]
    pub struct Args {
        pub path: String,
        pub total: u32,
        pub flags: Option<Vec<u8>>,
    }
}

mod retyped_field {
    #[derive(lore_macro::WireSchema)]
    pub struct Args {
        pub path: String,
        pub count: u64,
        pub flags: Option<Vec<u8>>,
    }
}

mod added_field {
    #[derive(lore_macro::WireSchema)]
    pub struct Args {
        pub path: String,
        pub count: u32,
        pub flags: Option<Vec<u8>>,
        pub force: bool,
    }
}

mod skipped_field {
    #[derive(bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
    pub struct Args {
        pub path: String,
        pub count: u32,
        pub flags: Option<Vec<u8>>,
        #[bitcode(skip)]
        pub cached: u64,
    }
}

mod renamed_type {
    #[derive(lore_macro::WireSchema)]
    pub struct Arguments {
        pub path: String,
        pub count: u32,
        pub flags: Option<Vec<u8>>,
    }
}

mod inner_changed {
    #[derive(lore_macro::WireSchema)]
    pub struct Args {
        pub path: String,
        pub count: u32,
        pub flags: Option<Vec<u16>>,
    }

    #[derive(lore_macro::WireSchema)]
    pub enum Command {
        Add(Args),
        Remove { path: String },
        Clear,
    }
}

mod reordered_variants {
    #[derive(lore_macro::WireSchema)]
    pub enum Command {
        Remove { path: String },
        Add(super::base::Args),
        Clear,
    }
}

mod added_variant {
    #[derive(lore_macro::WireSchema)]
    pub enum Command {
        Add(super::base::Args),
        Remove { path: String },
        Clear,
        Reset,
    }
}

mod unnamed_variant_fields {
    #[derive(lore_macro::WireSchema)]
    pub enum Command {
        Add(super::base::Args),
        Remove(String),
        Clear,
    }
}

mod renamed_variant {
    #[derive(lore_macro::WireSchema)]
    pub enum Command {
        Add(super::base::Args),
        Delete { path: String },
        Clear,
    }
}

/// An enum of one variant holding the fields of `base::Args`.
mod enum_of_the_fields {
    #[derive(lore_macro::WireSchema)]
    pub enum Args {
        Args {
            path: String,
            count: u32,
            flags: Option<Vec<u8>>,
        },
    }
}

mod bounded {
    #[derive(bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
    pub struct Message<Event> {
        #[bitcode(bound_type = "Event")]
        pub event: Event,
    }
}

/// What the encoding doesn't carry leaves the schema alone: a field bitcode skips, a bitcode
/// option that keeps the field, and the type's name.
#[test]
fn what_the_encoding_does_not_carry_leaves_the_schema_alone() {
    assert_eq!(base::Args::SCHEMA, skipped_field::Args::SCHEMA);
    assert_eq!(base::Args::SCHEMA, renamed_type::Arguments::SCHEMA);
    assert_eq!(
        base::Message::<u32>::SCHEMA,
        bounded::Message::<u32>::SCHEMA
    );
}

#[test]
fn each_edit_to_a_struct_changes_its_schema() {
    for (edit, schema) in [
        ("reordered fields", reordered_fields::Args::SCHEMA),
        ("renamed field", renamed_field::Args::SCHEMA),
        ("retyped field", retyped_field::Args::SCHEMA),
        ("added field", added_field::Args::SCHEMA),
        ("changed field type", inner_changed::Args::SCHEMA),
        ("an enum of the fields", enum_of_the_fields::Args::SCHEMA),
    ] {
        assert_ne!(base::Args::SCHEMA, schema, "{edit}");
    }
}

#[test]
fn each_edit_to_an_enum_changes_its_schema() {
    for (edit, schema) in [
        ("reordered variants", reordered_variants::Command::SCHEMA),
        ("added variant", added_variant::Command::SCHEMA),
        ("renamed variant", renamed_variant::Command::SCHEMA),
        (
            "unnamed variant fields",
            unnamed_variant_fields::Command::SCHEMA,
        ),
        ("changed variant field", inner_changed::Command::SCHEMA),
    ] {
        assert_ne!(base::Command::SCHEMA, schema, "{edit}");
    }
}

#[test]
fn a_type_parameter_is_part_of_the_schema() {
    assert_eq!(
        base::Message::<u32>::SCHEMA,
        <base::Message as WireSchema>::SCHEMA
    );
    assert_ne!(base::Message::<u32>::SCHEMA, base::Message::<u64>::SCHEMA);
}

#[test]
fn a_container_is_part_of_the_schema() {
    assert_ne!(u8::SCHEMA, Option::<u8>::SCHEMA);
    assert_ne!(Option::<u8>::SCHEMA, Vec::<u8>::SCHEMA);
    assert_ne!(<[u8; 2]>::SCHEMA, <[u8; 3]>::SCHEMA);
}

/// Which value of a call's environment is which is decided by the order of the variables it
/// carries, so the order is part of its schema.
#[test]
fn the_variables_a_call_carries_are_part_of_its_schema() {
    assert_ne!(
        call_environment_schema(&[GLOBAL_PATH_VAR, AUTH_PATH_VAR]),
        call_environment_schema(&[AUTH_PATH_VAR, GLOBAL_PATH_VAR])
    );
    assert_ne!(
        CallEnvironment::SCHEMA,
        <[Option<String>; CALL_VARIABLES.len()] as WireSchema>::SCHEMA
    );
}
