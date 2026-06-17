// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use bytes::Bytes;
use lore_base::types::Context;
use uuid::Uuid;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

#[derive(Clone, Debug, Default, IntoBytes, FromBytes, Immutable, PartialEq)]
pub struct ReplicationHeader {
    pub correlation_id: Uuid,
    pub repository: Context,
}

impl From<&[u8]> for ReplicationHeader {
    fn from(bytes: &[u8]) -> Self {
        Self::read_from_prefix(bytes).unwrap_or_default().0
    }
}

impl From<Bytes> for ReplicationHeader {
    fn from(bytes: Bytes) -> Self {
        bytes.as_bytes().into()
    }
}

impl AsRef<[u8]> for ReplicationHeader {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}
