// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Wire codec: decodes kernel requests and encodes replies without copying.

pub mod dirent;
pub mod reply;
pub mod request;

pub use request::DecodeError;
pub use request::Decoded;
pub use request::Request;
pub use request::decode;
