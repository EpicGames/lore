// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::text::ValidateText;

mod binary_tests;
mod event_interval_tests;
mod metadata_repr_tests;

use lore_revision::auth::AuthMode;
use lore_revision::interface::*;

/// An empty array holds a null pointer whichever constructor built it. `Drop` frees only a
/// non-null pointer with a count above zero, so any other pairing leaks.
#[test]
fn an_empty_array_holds_no_buffer() {
    let from_empty_vec = LoreArray::<u32>::from_vec(Vec::new());

    assert!(
        from_empty_vec.ptr.is_null(),
        "from_vec allocated for an empty vec, and Drop's `count > 0` guard skips that buffer"
    );
    assert_eq!(from_empty_vec.count, 0);
    assert_eq!(from_empty_vec, LoreArray::default());
}

/// A zero-sized element type gives a zero-sized layout at every count, so nothing is
/// allocated, yet the count and the elements must survive.
#[test]
fn a_zero_sized_element_type_keeps_its_count_without_allocating() {
    let array = LoreArray::<()>::from_vec(vec![(); 3]);

    // The dangling pointer is the observable proof that nothing was allocated: an allocator
    // would not answer the alignment as an address. A slice also needs it non-null.
    assert_eq!(
        array.ptr,
        std::ptr::NonNull::<()>::dangling().as_ptr(),
        "a zero-sized type must take a dangling pointer, not an allocation"
    );
    assert_eq!(array.len(), 3);
    assert_eq!(array.as_slice(), [(), (), ()]);
    assert_eq!(array.clone().as_slice(), [(), (), ()]);
}

/// Skipping the allocation must not skip the elements: `Drop` still runs each one.
#[test]
fn a_zero_sized_element_type_still_drops_every_element() {
    static DROPPED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    struct Counted;
    impl Drop for Counted {
        fn drop(&mut self) {
            DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    drop(LoreArray::from_vec(vec![Counted, Counted, Counted]));

    assert_eq!(
        DROPPED.load(std::sync::atomic::Ordering::Relaxed),
        3,
        "every element must drop even though nothing was allocated"
    );
}

/// Deserializing an empty sequence routes through `from_vec`.
#[test]
fn a_deserialized_empty_array_holds_no_buffer() {
    let decoded: LoreArray<u32> =
        serde_json::from_str("[]").expect("an empty sequence deserializes");

    assert!(
        decoded.ptr.is_null(),
        "deserializing an empty array allocated a buffer Drop will not free"
    );
}

/// Arguments are logged whole, so a rendering that named every element of a
/// caller's path list would be the bulk of a log.
#[test]
fn a_long_array_renders_as_its_count() {
    let at_limit = LoreArray::from_vec(vec![7u32; DEBUG_ELEMENT_LIMIT]);
    assert_eq!(
        format!("{at_limit:?}"),
        format!("{:?}", [7u32; DEBUG_ELEMENT_LIMIT])
    );

    let over_limit = LoreArray::from_vec(vec![7u32; DEBUG_ELEMENT_LIMIT + 1]);
    assert_eq!(
        format!("{over_limit:?}"),
        format!("[{} items...]", DEBUG_ELEMENT_LIMIT + 1)
    );
}

/// `{"iss":"lore","sub":"alice","name":"Alice","exp":2000000000,"aud":["example.com"]}`
const ALICE_TOKEN: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJuYW1lIjoiQWxpY2UiLCJleHAiOjIwMDAwMDAwMDAsImF1ZCI6WyJleGFtcGxlLmNvbSJdfQ.signature";

/// The tokens cross the C boundary as raw bytes and `validate` reads them as
/// text, so they have to be checked like every other string a call carries.
#[test]
fn a_token_that_is_not_utf8_is_reported_by_field() {
    let globals = LoreGlobalArgs {
        identity_token: LoreString::from_bytes(&[b'a', 0xff, 0xfe]),
        ..Default::default()
    };
    assert_eq!(
        globals
            .validate_text()
            .expect_err("invalid text must be reported")
            .field(),
        "identity_token"
    );

    let globals = LoreGlobalArgs {
        access_token: LoreString::from_bytes(&[b'a', 0xff, 0xfe]),
        ..Default::default()
    };
    assert_eq!(
        globals
            .validate_text()
            .expect_err("invalid text must be reported")
            .field(),
        "access_token"
    );
}

#[test]
fn an_unset_auth_mode_is_auto() {
    let mut globals = LoreGlobalArgs::default();
    assert!(globals.validate().is_ok());
    assert_eq!(globals.auth_mode, AuthMode::Auto);
}

#[test]
fn no_credential_arguments_is_valid() {
    let mut globals = LoreGlobalArgs::default();
    assert!(globals.validate().is_ok());
    assert!(globals.identity.is_empty());
}

#[test]
fn identity_alone_is_valid_and_untouched() {
    let mut globals = LoreGlobalArgs {
        identity: "bob".into(),
        ..Default::default()
    };
    assert!(globals.validate().is_ok());
    assert_eq!(globals.identity.as_str(), "bob");
}

#[test]
fn identity_token_resolves_the_identity_it_names() {
    let mut globals = LoreGlobalArgs {
        identity_token: ALICE_TOKEN.into(),
        ..Default::default()
    };
    assert!(globals.validate().is_ok());
    assert_eq!(globals.identity.as_str(), "alice");
}

#[test]
fn access_token_alone_resolves_the_identity_it_names() {
    // Mode 2: only an access token. It names the identity, and operations
    // that need an authentication token fail later rather than reading one
    // out of the store.
    let mut globals = LoreGlobalArgs {
        access_token: ALICE_TOKEN.into(),
        ..Default::default()
    };
    assert!(globals.validate().is_ok());
    assert_eq!(globals.identity.as_str(), "alice");
}

#[test]
fn both_tokens_take_the_identity_from_the_identity_token() {
    // Mode 3: both supplied. The authentication token is the authority on
    // identity.
    let mut globals = LoreGlobalArgs {
        identity_token: ALICE_TOKEN.into(),
        access_token: "authz-token".into(),
        ..Default::default()
    };
    assert!(globals.validate().is_ok());
    assert_eq!(globals.identity.as_str(), "alice");
}

#[test]
fn access_token_naming_no_identity_is_rejected() {
    // With no identity token to fall back on, an access token that names no
    // subject leaves the call with no identity to act as.
    let mut globals = LoreGlobalArgs {
        access_token: "not-a-jwt".into(),
        ..Default::default()
    };
    assert!(globals.validate().is_err());
    assert!(globals.identity.is_empty());
}

#[test]
fn identity_and_access_token_are_mutually_exclusive() {
    let mut globals = LoreGlobalArgs {
        identity: "alice".into(),
        access_token: ALICE_TOKEN.into(),
        ..Default::default()
    };
    assert!(globals.validate().is_err());
}

#[test]
fn identity_and_identity_token_are_mutually_exclusive() {
    let mut globals = LoreGlobalArgs {
        identity: "alice".into(),
        identity_token: ALICE_TOKEN.into(),
        ..Default::default()
    };
    // Rejected even when they agree: one of them has to be the authority.
    assert!(globals.validate().is_err());
}

#[test]
fn identity_token_naming_no_identity_is_rejected() {
    let mut globals = LoreGlobalArgs {
        identity_token: "not-a-jwt".into(),
        ..Default::default()
    };
    assert!(globals.validate().is_err());
    assert!(globals.identity.is_empty());
}

/// A name arriving across the C boundary can hold any byte sequence. The
/// formatting paths run on every dispatched command, so they must render
/// such a string instead of assuming UTF-8.
#[test]
fn lore_string_renders_invalid_utf8_as_replacement_characters() {
    let value = LoreString::from_bytes(&[b'a', 0xff, 0xfe, b'b']);

    assert_eq!(value.as_bytes(), &[b'a', 0xff, 0xfe, b'b']);
    assert_eq!(format!("{value}"), "a\u{fffd}\u{fffd}b");
    assert_eq!(format!("{value:?}"), "a\u{fffd}\u{fffd}b");
}

/// Unlike formatting, serialization must not substitute: it carries the
/// command to the service, where a replacement-character name would be
/// accepted as valid text that the in-process path would have rejected.
#[test]
fn lore_string_serialization_rejects_invalid_utf8() {
    let value = LoreString::from_bytes(&[b'a', 0xff, 0xfe, b'b']);
    assert!(
        serde_json::to_string(&value).is_err(),
        "serializing non-UTF-8 text must fail rather than substitute"
    );

    let valid = LoreString::from_str("doc.md");
    assert_eq!(
        serde_json::to_string(&valid).expect("valid text must serialize"),
        "\"doc.md\""
    );
}

/// Equality compares the raw bytes, so strings that differ only in an
/// invalid sequence stay distinguishable.
#[test]
fn lore_string_equality_compares_bytes() {
    assert_eq!(LoreString::from_str("same"), LoreString::from_str("same"));
    assert_ne!(
        LoreString::from_bytes(&[0xff]),
        LoreString::from_bytes(&[0xfe])
    );
}

/// Every call clones its arguments before anything checks them, so cloning
/// must copy the bytes rather than read them as text.
#[test]
fn lore_string_clone_copies_bytes_that_are_not_utf8() {
    let value = LoreString::from_bytes(&[b'a', 0xff, 0xfe, b'b']);

    let cloned = value.clone();
    assert_eq!(cloned.as_bytes(), &[b'a', 0xff, 0xfe, b'b']);

    let mut assigned = LoreString::from_str("replaced");
    assigned.clone_from(&value);
    assert_eq!(assigned.as_bytes(), &[b'a', 0xff, 0xfe, b'b']);
}

/// The type documents an empty string as a NULL pointer with length 0, so
/// every way of building one has to answer that, or the same value reaches
/// a C caller in more than one shape.
#[test]
fn lore_string_empty_is_a_null_pointer_of_zero_length() {
    let mut assigned = LoreString::from_str("replaced");
    assigned.clone_from(&LoreString::default());

    for empty in [
        LoreString::default(),
        LoreString::from_bytes(&[]),
        LoreString::from_str(""),
        LoreString::from(String::new()),
        LoreString::from_str("").clone(),
        assigned,
    ] {
        assert!(empty.string.is_null());
        assert_eq!(empty.len(), 0);
        assert_eq!(empty.as_str(), "");
        assert_eq!(empty, LoreString::default());
    }
}

/// Now that the library hands a C consumer a NULL pointer for every empty
/// string, one comes back in an argument struct with a length the caller
/// filled in from its own bookkeeping. The pointer decides whether there is
/// text to read, so reading such a string answers empty instead of
/// dereferencing NULL.
#[test]
fn lore_string_null_pointer_is_empty_whatever_the_length_claims() {
    let claimed = LoreString {
        string: std::ptr::null(),
        length: 7,
    };

    assert!(claimed.is_empty());
    assert_eq!(claimed.len(), 0);
    assert_eq!(claimed.as_bytes(), b"");
    assert_eq!(claimed.as_str(), "");
    assert!(claimed.validate_text().is_ok());
}

#[test]
fn validate_text_accepts_valid_utf8_and_empty_strings() {
    assert!(LoreString::from_str("doc.md").validate_text().is_ok());
    assert!(LoreString::default().validate_text().is_ok());
    assert!(LoreString::from_str("ünïcøde").validate_text().is_ok());
}

#[test]
fn validate_text_rejects_bytes_that_are_not_utf8() {
    assert!(
        LoreString::from_bytes(&[b'a', 0xff])
            .validate_text()
            .is_err()
    );
}

/// An array reports which element failed, so the rejection points at one
/// entry rather than the whole field.
#[test]
fn validate_text_names_the_array_element_that_failed() {
    let strings = LoreArray::from_vec(vec![
        LoreString::from_str("first"),
        LoreString::from_str("second"),
        LoreString::from_bytes(&[0xff]),
    ]);

    let error = strings
        .validate_text()
        .map_err(|error| error.inside("paths"))
        .expect_err("the element must fail");

    assert_eq!(error.field(), "paths[2]");
}

#[test]
fn validate_text_passes_arguments_that_hold_no_text() {
    assert!(LoreArray::<LoreString>::default().validate_text().is_ok());
    assert!(LoreGlobalArgs::default().validate_text().is_ok());
}

#[test]
fn validate_text_names_the_failing_field_of_the_global_arguments() {
    let globals = LoreGlobalArgs {
        identity: LoreString::from_bytes(&[b'i', 0xff]),
        ..LoreGlobalArgs::default()
    };

    let error = globals
        .validate_text()
        .map_err(|error| error.inside("globals"))
        .expect_err("the identity must fail");
    assert_eq!(error.field(), "globals.identity");
}
