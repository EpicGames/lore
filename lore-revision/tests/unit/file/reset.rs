// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Which layer mounts lie at or below a path a reset names.

use lore_revision::file::reset::mount_at_or_below;
use lore_revision::util::path::RelativePath;

use crate::helper::relative_path;

/// Every mount lies below the root, which covers no path as a path.
#[test]
fn every_mount_lies_below_the_root() {
    assert!(mount_at_or_below(
        &RelativePath::new(),
        &relative_path("Tools/sdk")
    ));
}
