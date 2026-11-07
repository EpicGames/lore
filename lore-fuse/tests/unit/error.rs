// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_fuse::error::*;

#[test]
fn an_io_error_is_internal() {
    let io_error = std::io::Error::other("device closed");
    let error: FuseError = io_error.into();

    assert!(matches!(error, FuseError::Internal(_)));
    assert!(error.to_string().contains("device closed"));
}
