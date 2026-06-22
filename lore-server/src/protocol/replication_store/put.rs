// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use bytes::Buf;
use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_storage::ImmutableStore;
use lore_storage::StoreError;
use lore_telemetry::tracing::fields::ADDRESS;
use lore_telemetry::tracing::fields::CORRELATION_ID;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use tracing::Span;
use tracing::debug;
use tracing::info_span;

use crate::protocol::replication_store::REPLICATION_SERVICE_USER_ID;
use crate::protocol::replication_store::header::ReplicationHeader;
use crate::protocol::storage::messages::MessageParseError;
use crate::quic::replication_store_service::server::ParsedReplicationStoreRequest;
use crate::quic::replication_store_service::server::RequestHandler;
use crate::util::setup_execution;

pub const BASE_REQUEST_SIZE: usize = size_of::<ReplicationHeader>() +
        size_of::<Address>() +
        size_of::<Fragment>() +
        // flags
        1;

#[derive(Clone, Debug, PartialEq)]
pub struct Put {
    pub header: ReplicationHeader,
    pub address: Address,
    pub fragment: Fragment,
    pub flags: u8,
    pub payload: Option<Bytes>,
}

impl Put {
    pub fn to_quic_chunks(self) -> [Bytes; 6] {
        [
            Bytes::default(), // command header
            Bytes::from_owner(self.header),
            Bytes::from_owner(self.address),
            Bytes::from_owner(self.fragment),
            Bytes::copy_from_slice(&[self.flags]),
            self.payload.unwrap_or_default(),
        ]
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PutFlags {
    pub force: bool,
}

impl From<u8> for PutFlags {
    fn from(value: u8) -> Self {
        Self { force: value == 1 }
    }
}

impl From<PutFlags> for u8 {
    fn from(flags: PutFlags) -> Self {
        if flags.force { 1 } else { 0 }
    }
}

pub fn parse(mut bytes: Bytes) -> Result<Put, MessageParseError> {
    if bytes.len() < BASE_REQUEST_SIZE {
        return Err(MessageParseError::InvalidFieldLength);
    };

    let header: ReplicationHeader = bytes.split_to(size_of::<ReplicationHeader>()).into();
    let address: Address = bytes.split_to(size_of::<Address>()).into();
    let fragment: Fragment = bytes.split_to(size_of::<Fragment>()).into();
    let flags: u8 = {
        let flags = bytes[0];
        bytes.advance(1);
        flags
    };
    let payload = if !bytes.is_empty() { Some(bytes) } else { None };

    // Defense-in-depth: even though peers are expected to pre-validate,
    // enforce fragment size bounds at this ingress too.
    if (fragment.size_payload as usize) > lore_base::types::FRAGMENT_SIZE_THRESHOLD {
        return Err(MessageParseError::InvalidFieldLength);
    }
    if let Some(payload) = payload.as_ref()
        && payload.len() != fragment.size_payload as usize
    {
        return Err(MessageParseError::InvalidFieldLength);
    }

    Ok(Put {
        header,
        address,
        fragment,
        flags,
        payload,
    })
}

pub fn create_handler(
    bytes: Bytes,
    immutable_store: Arc<dyn ImmutableStore>,
    message_context: &'static str,
) -> Result<ParsedReplicationStoreRequest, MessageParseError> {
    let request = parse(bytes)?;
    let handler = PutHandler {
        immutable_store,
        request,
        message_context,
    };

    Ok(ParsedReplicationStoreRequest::Put(handler))
}

#[derive(Debug)]
pub struct PutHandler {
    immutable_store: Arc<dyn ImmutableStore>,
    pub request: Put,
    message_context: &'static str,
}

#[async_trait::async_trait]
impl RequestHandler for PutHandler {
    fn span(&self) -> Span {
        info_span!("put",
            {CORRELATION_ID} = %self.request.header.correlation_id.as_hyphenated(),
            {REPOSITORY_ID} = %self.request.header.repository,
            message_context = self.message_context)
    }

    async fn run(self) -> Result<Vec<Bytes>, StoreError> {
        debug!({ADDRESS} = %self.request.address,
            payload_length = self.request.payload
                .as_ref().map_or(0, |b| b.len()),
            "put request");

        let execution = setup_execution(
            module_path!(),
            self.request.header.correlation_id.to_string(),
            REPLICATION_SERVICE_USER_ID.to_string(),
        );

        let flags = PutFlags::from(self.request.flags);

        LORE_CONTEXT
            .scope(execution, async move {
                self.immutable_store
                    .put(
                        self.request.header.repository.into(),
                        self.request.address,
                        self.request.fragment,
                        self.request.payload,
                        flags.force,
                    )
                    .await
            })
            .await?;

        Ok(vec![])
    }
}

#[cfg(test)]
pub mod tests {
    use lore_base::types::Context;
    use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
    use lore_revision::fragment;
    use lore_transport::quic::command_header::CommandHeader;
    use rand::random;
    use uuid::Uuid;

    use super::*;
    use crate::quic::replication_store_service::MAX_CHUNK_SIZE;
    use crate::quic::tests::collapse_bytes_without_header;

    #[test]
    fn is_under_max_chunk_size() {
        // base request plus max payload size
        let max_request_size = BASE_REQUEST_SIZE + FRAGMENT_SIZE_THRESHOLD;
        assert!(max_request_size + size_of::<CommandHeader>() <= MAX_CHUNK_SIZE);
    }

    #[test]
    fn parsing_without_payload_works() {
        let repository = random::<Context>();
        let (fragment, address, _) = fragment::generate_random();

        let input = Put {
            header: ReplicationHeader {
                correlation_id: Uuid::new_v4(),
                repository,
            },
            address,
            fragment,
            flags: 0,
            payload: None,
        };
        let input_bytes = collapse_bytes_without_header(&input.clone().to_quic_chunks());
        let output = parse(input_bytes).expect("parse should work");

        assert_eq!(input, output);
    }

    #[test]
    fn parsing_with_payload_works() {
        let repository = random::<Context>();
        let (fragment, address, payload) = fragment::generate_random();

        let input = Put {
            header: ReplicationHeader {
                correlation_id: Uuid::new_v4(),
                repository,
            },
            address,
            fragment,
            flags: 1,
            payload: Some(payload),
        };
        let input_bytes = collapse_bytes_without_header(&input.clone().to_quic_chunks());
        let output = parse(input_bytes).expect("parse should work");

        assert_eq!(input, output);
    }

    #[test]
    fn parsing_fails_if_too_small() {
        let repository = random::<Context>();
        let (fragment, address, _) = fragment::generate_random();

        let input = Put {
            header: ReplicationHeader {
                correlation_id: Uuid::new_v4(),
                repository,
            },
            address,
            fragment,
            flags: 0,
            payload: None,
        };
        let input_bytes = collapse_bytes_without_header(&input.to_quic_chunks());
        let output =
            parse(input_bytes.slice(0..input_bytes.len() - 1)).expect_err("parse should fail");

        assert_eq!(output, MessageParseError::InvalidFieldLength);
    }

    #[test]
    fn set_flags_are_parsed() {
        let input = PutFlags { force: true };
        let data: u8 = input.clone().into();
        let output: PutFlags = data.into();

        assert_eq!(input, output);
    }

    #[test]
    fn no_flags_are_parsed() {
        let input = PutFlags { force: false };
        let data: u8 = input.clone().into();
        let output: PutFlags = data.into();

        assert_eq!(input, output);
    }
}
