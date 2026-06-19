// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::mem::size_of;

use bytes::Bytes;
use lore_base::types::Context;
use uuid::Uuid;

const UUID_SIZE: usize = 16;
pub const REPLICATION_HEADER_SIZE: usize = UUID_SIZE + size_of::<Context>();
const _: [(); UUID_SIZE] = [(); size_of::<Uuid>()];

#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReplicationHeader {
    pub correlation_id: Uuid,
    pub repository: Context,
}

impl From<&[u8]> for ReplicationHeader {
    fn from(bytes: &[u8]) -> Self {
        if bytes.len() < REPLICATION_HEADER_SIZE {
            return ReplicationHeader::default();
        }

        let mut correlation_id = [0; UUID_SIZE];
        correlation_id.copy_from_slice(&bytes[..UUID_SIZE]);
        let repository = Context::from(&bytes[UUID_SIZE..REPLICATION_HEADER_SIZE]);

        ReplicationHeader {
            correlation_id: Uuid::from_bytes(correlation_id),
            repository,
        }
    }
}

impl From<Bytes> for ReplicationHeader {
    fn from(bytes: Bytes) -> Self {
        bytes.as_ref().into()
    }
}

impl ReplicationHeader {
    pub fn to_bytes(&self) -> Bytes {
        let mut bytes = [0; REPLICATION_HEADER_SIZE];
        bytes[..UUID_SIZE].copy_from_slice(self.correlation_id.as_bytes());
        bytes[UUID_SIZE..].copy_from_slice(self.repository.as_ref());
        Bytes::copy_from_slice(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replication_header_roundtrips_bytes() {
        let correlation_id = Uuid::from_bytes([1; UUID_SIZE]);
        let repository = Context::from([2; 16]);
        let header = ReplicationHeader {
            correlation_id,
            repository,
        };

        let bytes = header.to_bytes();
        assert_eq!(bytes.len(), REPLICATION_HEADER_SIZE);
        assert_eq!(ReplicationHeader::from(bytes), header);
    }
}
