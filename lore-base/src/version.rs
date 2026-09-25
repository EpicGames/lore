// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The library version: the package version, `+`, and a build name written into finished artifacts
//! after the build.
//!
//! The build name is not compiled in: a value that changed on every build would change this crate,
//! and every crate that depends on it, on every build. Every build compiles the same version slot
//! instead, and `lore-stamp` writes the build name into the slot of a finished binary, shared
//! library or static library. The package version is compiled in, so an artifact reports the
//! package version it was built with, whichever `lore-stamp` stamped it. An artifact that was never
//! stamped reports `<package version>+local`.
use std::borrow::Cow;
use std::ffi::CString;
use std::sync::LazyLock;

/// The bytes that open the version slot.
///
/// `lore-stamp` finds the slot by searching an artifact for these bytes, so they must occur nowhere
/// else in it. Code linked into a stamped artifact must not use them except to initialize the slot:
/// comparing against them compiles a second copy.
pub const STAMP_MARKER: &[u8; 32] = b"lore-build-version-slot/v1:8e41f";

/// Size of the build name field that follows the marker, in bytes, the terminating NUL included.
pub const STAMP_BUILD_CAPACITY: usize = 96;

/// The build name an artifact that was never stamped reports.
const LOCAL_BUILD_NAME: &str = "local";

/// The marker immediately followed by the build name field, the layout `lore-stamp` writes into.
#[repr(C)]
struct StampSlot {
    marker: [u8; STAMP_MARKER.len()],
    build: [u8; STAMP_BUILD_CAPACITY],
}

/// The version slot. A build name field holding only NULs was never stamped.
static STAMP_SLOT: StampSlot = StampSlot {
    marker: *STAMP_MARKER,
    build: [0; STAMP_BUILD_CAPACITY],
};

/// The version of this build: `<package version>+<build name>`, with the stamped build name, or
/// `local` if the artifact was never stamped.
pub static LORE_LIBRARY_VERSION: LazyLock<String> = LazyLock::new(|| {
    let field = stamped_build_field();
    version_name(build_name(&field).as_deref().unwrap_or(LOCAL_BUILD_NAME))
});

/// [`LORE_LIBRARY_VERSION`] as a NUL-terminated string. The version holds no NUL, so the
/// conversion never falls back to the empty string.
pub static LORE_LIBRARY_VERSION_CSTR: LazyLock<CString> =
    LazyLock::new(|| CString::new(LORE_LIBRARY_VERSION.as_str()).unwrap_or_default());

/// The version a build named `build` reports: `<package version>+<build>`.
pub fn version_name(build: &str) -> String {
    format!("{}+{build}", env!("CARGO_PKG_VERSION"))
}

/// The build name field as `lore-stamp` left it.
///
/// The slot is read with a volatile read: `lore-stamp` rewrites it after compilation, and a plain
/// read of the immutable static may be folded to its NUL-filled initializer. Reading the whole slot
/// keeps the unread marker from being split away from the build name field.
fn stamped_build_field() -> [u8; STAMP_BUILD_CAPACITY] {
    // SAFETY: the pointer comes from a live, initialized static, so it is valid and aligned.
    unsafe { std::ptr::read_volatile(&raw const STAMP_SLOT) }.build
}

/// The build name `field` holds, the bytes before its first NUL, or `None` if it holds none.
fn build_name(field: &[u8]) -> Option<Cow<'_, str>> {
    let length = field
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(field.len());
    (length > 0).then(|| String::from_utf8_lossy(&field[..length]))
}
