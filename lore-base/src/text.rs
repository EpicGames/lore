// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Checking that text arriving across the C boundary holds valid UTF-8.
//!
//! An FFI string is a pointer and a length over bytes nothing has validated, and
//! reading it as `&str` is only sound once it is known to be UTF-8. [`ValidateText`]
//! lets an entry point check a whole set of arguments in one pass, so the code
//! behind it can treat its text as `&str` without re-checking.
//!
//! Derive it with `lore_macro::ValidateText` for a struct carrying text, and
//! declare a type that carries none with [`carries_no_text`].

use crate::error::InvalidArguments;

/// Names the text field that failed the check, the way a caller reading the
/// arguments would write it: `path`, or `entries[3].name` for a field of an
/// element in an array.
///
/// Each [`ValidateText`] implementation reports the path relative to itself and
/// its container prepends its own name, so nothing is allocated unless a check
/// fails.
#[derive(Debug)]
pub struct TextNotUtf8 {
    field: String,
}

impl TextNotUtf8 {
    /// Report a value that is not itself a field as having failed.
    pub fn here() -> Self {
        Self {
            field: String::new(),
        }
    }

    /// The failing field's name, for the rejection message.
    pub fn field(&self) -> &str {
        &self.field
    }

    /// Report this failure as coming from `field` of the containing value.
    pub fn inside(self, field: &str) -> Self {
        let inner = self.field;
        let field = if inner.is_empty() {
            field.to_string()
        } else if inner.starts_with('[') {
            format!("{field}{inner}")
        } else {
            format!("{field}.{inner}")
        };
        Self { field }
    }

    /// Report this failure as coming from the element at `index`.
    pub fn at(self, index: usize) -> Self {
        let inner = self.field;
        let field = if inner.is_empty() {
            format!("[{index}]")
        } else {
            format!("[{index}].{inner}")
        };
        Self { field }
    }
}

impl From<TextNotUtf8> for InvalidArguments {
    fn from(value: TextNotUtf8) -> Self {
        InvalidArguments {
            reason: format!("{} is not valid UTF-8", value.field()),
        }
    }
}

/// Check that every string a value carries holds valid UTF-8.
pub trait ValidateText {
    /// Reports the failing field relative to this value, for a container to
    /// prepend its own name to.
    fn validate_text(&self) -> Result<(), TextNotUtf8>;
}

/// Declare that a type holds no text, giving it a [`ValidateText`] that passes
/// everything.
///
/// A derived implementation walks every field, so a field type that is neither
/// text nor declared here fails to compile rather than going unchecked.
#[macro_export]
macro_rules! carries_no_text {
    ($($type:ty),* $(,)?) => {
        $(impl $crate::text::ValidateText for $type {
            fn validate_text(&self) -> Result<(), $crate::text::TextNotUtf8> {
                Ok(())
            }
        })*
    };
}

carries_no_text!(
    u8,
    u16,
    u32,
    u64,
    i8,
    i16,
    i32,
    i64,
    usize,
    f32,
    f64,
    bool,
    crate::log::LoreLogLevel,
    crate::types::Address,
    crate::types::Context,
    crate::types::Hash,
    crate::types::KeyType,
    crate::types::Partition,
);

#[cfg(test)]
mod tests {
    use super::*;

    /// The failing field is named the way a caller reading the arguments would
    /// write it, so the rejection says which string to fix.
    #[test]
    fn a_failure_names_the_field_it_came_from() {
        assert_eq!(TextNotUtf8::here().inside("path").field(), "path");
        assert_eq!(
            TextNotUtf8::here().at(2).inside("paths").field(),
            "paths[2]"
        );
        assert_eq!(
            TextNotUtf8::here()
                .inside("remote_url")
                .inside("remote_config")
                .field(),
            "remote_config.remote_url"
        );
        assert_eq!(
            TextNotUtf8::here()
                .inside("name")
                .at(3)
                .inside("entries")
                .field(),
            "entries[3].name"
        );
    }

    #[test]
    fn a_failure_reads_as_an_invalid_argument_naming_the_field() {
        let error = InvalidArguments::from(TextNotUtf8::here().inside("identity"));

        assert_eq!(
            error.to_string(),
            "invalid arguments: identity is not valid UTF-8"
        );
    }

    #[test]
    fn a_value_that_carries_no_text_passes() {
        assert!(7u64.validate_text().is_ok());
        assert!(crate::types::Hash::default().validate_text().is_ok());
    }
}
