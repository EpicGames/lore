// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::BufMut;
use bytes::Bytes;
use bytes::BytesMut;
use lore_base::types::Address;
use lore_base::types::DirectDownload;
use lore_base::types::Fragment;
use lore_base::types::TypedBytes;
use lore_revision::lore::RepositoryId;
use lore_storage::ImmutableStore;
use lore_storage::StoreError;
use lore_storage::StoreMatch;
use tracing::debug;
use zerocopy::IntoBytes;

use crate::protocol::attribute_map::AttributeMap;
use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::messages::Message;
use crate::protocol::storage::messages::MessageHandleError;
use crate::protocol::storage::messages::MessageParseError;
use crate::protocol::storage::messages::Response;
use crate::protocol::storage::query::MAX_FRAGMENTS;

const EXPIRES_IN_BYTES: usize = size_of::<u64>();
const MAX_FRAGMENTS_LENGTH: usize = size_of::<Address>() * MAX_FRAGMENTS;
const DEFAULT_PRESIGN_EXPIRES_IN_SECONDS: u64 = 300;
const MAX_PRESIGN_EXPIRES_IN_SECONDS: u64 = 3600;

#[derive(Clone, Debug, PartialEq)]
pub struct PresignDownload {
    pub expires_in: Duration,
    pub address: Bytes,
}

impl PresignDownload {
    pub fn parse(bytes: Bytes) -> Result<Self, MessageParseError>
    where
        Self: Sized,
    {
        if bytes.len() < EXPIRES_IN_BYTES {
            return Err(MessageParseError::InvalidFieldLength);
        }

        let expires_in_seconds = u64::from_le_bytes(
            bytes[..EXPIRES_IN_BYTES]
                .try_into()
                .map_err(|_| MessageParseError::InvalidFieldLength)?,
        );
        let expires_in_seconds = if expires_in_seconds == 0 {
            DEFAULT_PRESIGN_EXPIRES_IN_SECONDS
        } else {
            expires_in_seconds.min(MAX_PRESIGN_EXPIRES_IN_SECONDS)
        };
        let address = bytes.slice(EXPIRES_IN_BYTES..);
        let length = address.len();
        if !length.is_multiple_of(size_of::<Address>()) {
            return Err(MessageParseError::InvalidQueryLength);
        }
        if length > MAX_FRAGMENTS_LENGTH {
            return Err(MessageParseError::TooManyFragments(
                MAX_FRAGMENTS,
                length / size_of::<Address>(),
            ));
        }

        Ok(Self {
            expires_in: Duration::from_secs(expires_in_seconds),
            address,
        })
    }
}

fn map_store_error(err: StoreError) -> MessageHandleError {
    match err {
        StoreError::AddressNotFound(_) => MessageHandleError::FragmentNotFound,
        StoreError::SlowDown(_) => MessageHandleError::SlowDown,
        StoreError::Oversized(_) => MessageHandleError::Oversized,
        StoreError::NotSupported(_) => MessageHandleError::NotImplemented,
        _ => MessageHandleError::StoreFailure,
    }
}

pub async fn handle_presign_download(
    address: &Bytes,
    expires_in: Duration,
    repository: RepositoryId,
    immutable_store: Arc<dyn ImmutableStore>,
) -> Result<LoreResponse, MessageHandleError> {
    let addresses = address.as_type_slice::<Address>();
    debug!(
        "Handling PresignDownload request for {} fragments in repository: {repository}",
        addresses.len()
    );

    let downloads = immutable_store
        .presign_downloads(repository, addresses, StoreMatch::MatchFull, expires_in)
        .await
        .map_err(map_store_error)?;

    Ok(LoreResponse::PresignDownload(PresignDownloadResponse {
        downloads,
    }))
}

#[async_trait]
impl Message for PresignDownload {
    #[tracing::instrument(name = "PresignDownload::handle", skip_all)]
    async fn handle(
        &self,
        context: Arc<AttributeMap>,
        immutable_store: Arc<dyn ImmutableStore>,
    ) -> Result<LoreResponse, MessageHandleError> {
        let repository = *context
            .get_or::<RepositoryId, MessageHandleError>(MessageHandleError::NotConnected)?;
        handle_presign_download(&self.address, self.expires_in, repository, immutable_store).await
    }
}

#[derive(Debug, PartialEq)]
pub struct PresignDownloadResponse {
    pub downloads: Vec<DirectDownload>,
}

impl Response for PresignDownloadResponse {
    fn data(&self) -> Vec<Bytes> {
        let url_bytes = self
            .downloads
            .iter()
            .map(|download| download.url.len())
            .sum::<usize>();
        let record_overhead =
            size_of::<Address>() + size_of::<Fragment>() + size_of::<u64>() + size_of::<u32>();
        let mut buffer = BytesMut::with_capacity(
            size_of::<u32>() + self.downloads.len() * record_overhead + url_bytes,
        );

        buffer.put_u32_le(self.downloads.len() as u32);
        for download in &self.downloads {
            buffer.extend_from_slice(download.address.as_bytes());
            buffer.extend_from_slice(download.fragment.as_bytes());
            buffer.put_u64_le(download.expires_at_epoch_seconds);
            buffer.put_u32_le(download.url.len() as u32);
            buffer.extend_from_slice(download.url.as_bytes());
        }

        vec![buffer.freeze()]
    }
}

#[cfg(test)]
mod tests {
    use bytes::Buf;
    use lore_base::types::Context;
    use lore_base::types::FragmentFlags;
    use lore_base::types::Hash;
    use rand::random;

    use super::*;

    fn address(value: u64) -> Address {
        Address {
            hash: Hash::from_u64(value),
            context: Context::from([value as u8; 16]),
        }
    }

    fn request_bytes(expires_in_seconds: u64, addresses: &[Address]) -> Bytes {
        let mut bytes =
            BytesMut::with_capacity(size_of::<u64>() + std::mem::size_of_val(addresses));
        bytes.put_u64_le(expires_in_seconds);
        for address in addresses {
            bytes.extend_from_slice(address.as_bytes());
        }
        bytes.freeze()
    }

    #[test]
    fn test_parse_valid() {
        let addresses = [address(1), address(2)];
        let parsed = PresignDownload::parse(request_bytes(60, &addresses)).expect("parse failed");

        assert_eq!(parsed.expires_in, Duration::from_secs(60));
        assert_eq!(parsed.address.as_type_slice::<Address>(), &addresses);
    }

    #[test]
    fn test_parse_uses_default_expiry_for_zero() {
        let parsed = PresignDownload::parse(request_bytes(0, &[address(1)])).expect("parse failed");

        assert_eq!(
            parsed.expires_in,
            Duration::from_secs(DEFAULT_PRESIGN_EXPIRES_IN_SECONDS)
        );
    }

    #[test]
    fn test_parse_clamps_expiry() {
        let parsed =
            PresignDownload::parse(request_bytes(u64::MAX, &[address(1)])).expect("parse failed");

        assert_eq!(
            parsed.expires_in,
            Duration::from_secs(MAX_PRESIGN_EXPIRES_IN_SECONDS)
        );
    }

    #[test]
    fn test_parse_rejects_invalid_address_length() {
        let mut bytes = request_bytes(60, &[address(1)]).to_vec();
        bytes.push(1);

        assert_eq!(
            PresignDownload::parse(Bytes::from(bytes)),
            Err(MessageParseError::InvalidQueryLength)
        );
    }

    #[test]
    fn test_response_data() {
        let address = Address {
            hash: random::<Hash>(),
            context: random::<Context>(),
        };
        let fragment = Fragment {
            flags: FragmentFlags::PayloadStoredDurable.bits(),
            size_payload: 42,
            size_content: 42,
        };
        let response = PresignDownloadResponse {
            downloads: vec![DirectDownload {
                address,
                fragment,
                url: "http://127.0.0.1/object".to_string(),
                expires_at_epoch_seconds: 123,
            }],
        };

        let mut bytes = response.data().pop().expect("missing response bytes");
        assert_eq!(bytes.get_u32_le(), 1);
        assert_eq!(
            Address::from(&bytes.split_to(size_of::<Address>())),
            address
        );
        assert_eq!(
            Fragment::from(&bytes.split_to(size_of::<Fragment>())),
            fragment
        );
        assert_eq!(bytes.get_u64_le(), 123);
        let url_len = bytes.get_u32_le() as usize;
        assert_eq!(
            String::from_utf8(bytes.split_to(url_len).to_vec()).unwrap(),
            "http://127.0.0.1/object"
        );
        assert!(bytes.is_empty());
    }
}
