// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore::remote::command::LoreCommand;
use lore::remote::message::MessageError;
use lore::remote::message::MessageToServer;
use lore::remote::message::SerializationType;
use lore::remote::message::V1Header;
use lore::remote::message::blocking_read_v1_message;
use lore::remote::message::write_v1_message;
use lore::repository::LoreRepositoryStatusArgs;
use lore::revision_tree::add::LoreRevisionTreeAddArgs;
use lore::revision_tree::add::LoreRevisionTreeAddEntry;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::modify::LoreRevisionTreeModifyArgs;
use lore::revision_tree::modify::LoreRevisionTreeModifyEntry;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreString;

#[test]
fn header_to_and_from_bytes() {
    let header = V1Header::new(0xffeeddcc, SerializationType::Bincode);

    let bytes = header.to_bytes();
    let processed_header = V1Header::from_bytes(&bytes);

    assert!(processed_header.is_ok());
    assert_eq!(processed_header.unwrap().payload_size, header.payload_size);

    let mut bad_bytes = bytes;
    bad_bytes[4] = 0xff;
    let bad_processed_header = V1Header::from_bytes(&bad_bytes);

    assert!(bad_processed_header.is_err());
}

#[tokio::test]
async fn message_to_server_to_and_from_bytes() {
    let path = LoreString::from_str("abc");
    let paths = LoreArray::from_vec(vec![
        LoreString::from_str("abc"),
        LoreString::from_str("def"),
    ]);
    let message = MessageToServer {
        globals: LoreGlobalArgs {
            repository_path: path.clone(),
            ..Default::default()
        },
        command: LoreCommand::RepositoryStatus(LoreRepositoryStatusArgs {
            staged: 0,
            scan: 0,
            check_dirty: 0,
            reset: 0,
            sync_point: 0,
            revision_only: 0,
            count: 0,
            paths: paths.clone(),
        }),
    };

    let message_bytes = write_v1_message(message, SerializationType::Json).unwrap();

    let processed_message: Result<Option<(V1Header, MessageToServer)>, MessageError> =
        blocking_read_v1_message(&mut message_bytes.as_slice());

    assert!(processed_message.is_ok());
    let processed_message = processed_message.unwrap();
    assert!(processed_message.is_some());
    let processed_message = processed_message.unwrap();
    assert_eq!(processed_message.1.globals.repository_path, path);
    match processed_message.1.command {
        LoreCommand::RepositoryStatus(repository_status) => {
            assert_eq!(repository_status.paths.as_slice(), paths.as_slice());
        }
        _ => {
            panic!("Unexpected command");
        }
    }
}

/// A command carrying an address must survive both wire encodings. Bincode is
/// the one that used to fail: `Hash`, `Context` and `Address` were read with
/// `deserialize_any`, which a non-self-describing format cannot answer, so every
/// such command was unreadable however it was written.
#[tokio::test]
async fn a_command_carrying_an_address_survives_both_serializations() {
    let address = Address {
        hash: Hash::from([0x37u8; 32]),
        context: Context::from([0x73u8; 16]),
    };
    let args = LoreRevisionTreeAddArgs {
        batch_id: 900,
        handle: LoreRevisionTree { handle_id: 5 },
        entries: LoreArray::from_vec(vec![LoreRevisionTreeAddEntry {
            entry_id: 1,
            parent_node_id: 0,
            parent_entry_index: 0,
            name: LoreString::from_str("payload.bin"),
            kind: 1,
            mode: 0o644,
            size: 4096,
            address,
        }]),
    };

    for (serialization, label) in [
        (SerializationType::Json, "json"),
        (SerializationType::Bincode, "bincode"),
    ] {
        let message = MessageToServer {
            globals: LoreGlobalArgs::default(),
            command: LoreCommand::RevisionTreeAdd(args.clone()),
        };
        let message_bytes = write_v1_message(message, serialization).unwrap();
        let processed: Result<Option<(V1Header, MessageToServer)>, MessageError> =
            blocking_read_v1_message(&mut message_bytes.as_slice());
        let processed = processed
            .unwrap_or_else(|error| panic!("{label} must read back: {error:?}"))
            .expect("a whole message must be present");

        match processed.1.command {
            LoreCommand::RevisionTreeAdd(read_back) => {
                assert_eq!(
                    read_back.entries.as_slice()[0].address,
                    address,
                    "{label} must carry the address unchanged"
                );
                assert_eq!(read_back.entries.as_slice(), args.entries.as_slice());
            }
            _ => panic!("Unexpected command"),
        }
    }
}

/// A batch verb reaches the service as an array of entries rather than a flat
/// argument set, so every entry field has to survive the wire — including the
/// address bytes, which no other command carries in an array element.
#[tokio::test]
async fn revision_tree_modify_batch_survives_the_wire() {
    let entries = LoreArray::from_vec(vec![
        LoreRevisionTreeModifyEntry {
            entry_id: 7,
            node_id: 42,
            mode: 0o600,
            size: 4096,
            address: Address {
                hash: Hash::from_u64(0xfeed),
                context: Context::from(uuid::Uuid::now_v7()),
            },
        },
        LoreRevisionTreeModifyEntry {
            entry_id: 0,
            node_id: 43,
            mode: 0o644,
            size: 0,
            address: Address::default(),
        },
    ]);
    let args = LoreRevisionTreeModifyArgs {
        batch_id: 900,
        handle: LoreRevisionTree { handle_id: 5 },
        entries: entries.clone(),
    };

    for (serialization, label) in [
        (SerializationType::Json, "json"),
        (SerializationType::Bincode, "bincode"),
    ] {
        let message = MessageToServer {
            globals: LoreGlobalArgs::default(),
            command: LoreCommand::RevisionTreeModify(args.clone()),
        };
        let message_bytes = write_v1_message(message, serialization).unwrap();
        let processed: Result<Option<(V1Header, MessageToServer)>, MessageError> =
            blocking_read_v1_message(&mut message_bytes.as_slice());
        let processed = processed
            .unwrap_or_else(|error| panic!("{label} must read back: {error:?}"))
            .expect("a whole message must be present");

        match processed.1.command {
            LoreCommand::RevisionTreeModify(read_back) => {
                assert_eq!(read_back.batch_id, args.batch_id, "{label}");
                assert_eq!(read_back.handle.handle_id, args.handle.handle_id, "{label}");
                assert_eq!(
                    read_back.entries.as_slice(),
                    entries.as_slice(),
                    "{label} must carry every entry field unchanged"
                );
            }
            other => panic!("Unexpected command: {other:?}"),
        }
    }
}

/// A metadata read delivers its value to the caller as an event, and an
/// out-of-process caller only ever sees the serialized form. Binary values are
/// the ones with no textual representation to fall back on, so they are the
/// case worth pinning.
///
/// Only JSON is exercised: `LoreEvent` is adjacently tagged, which bitcode
/// cannot deserialize, so no event of any kind survives a `Bincode`
/// connection today. That gap is independent of the payload type.
#[tokio::test]
async fn a_metadata_event_carries_a_binary_value_to_a_client() {
    use lore::remote::message::MessageToClient;
    use lore_revision::event::LoreEvent;
    use lore_revision::event::LoreMetadataEventData;
    use lore_revision::interface::LoreBinary;
    use lore_revision::interface::LoreMetadata;

    let value = LoreMetadata::Binary(LoreBinary::from_bytes(b"raw\x00bytes"));
    let event = LoreEvent::Metadata(LoreMetadataEventData {
        key: LoreString::from_str("thumbnail"),
        value: value.clone(),
    });

    let bytes = write_v1_message(MessageToClient::Event(event), SerializationType::Json).unwrap();
    let read: Result<Option<(V1Header, MessageToClient)>, MessageError> =
        blocking_read_v1_message(&mut bytes.as_slice());
    let read = read
        .unwrap_or_else(|error| panic!("a metadata event must read back: {error:?}"))
        .expect("a whole message must be present");

    match read.1 {
        MessageToClient::Event(LoreEvent::Metadata(data)) => {
            assert_eq!(data.key.as_str(), "thumbnail");
            assert_eq!(
                data.value, value,
                "the binary payload must survive the wire"
            );
        }
        _ => panic!("expected a metadata event"),
    }
}

/// The set verb carries a typed value rather than text, so the value union has
/// to cross the wire in both serializations — a command that only survives one
/// of them is unusable on the other transport.
#[tokio::test]
async fn revision_tree_metadata_set_batch_survives_the_wire() {
    use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetArgs;
    use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetEntry;
    use lore_revision::interface::LoreBinary;
    use lore_revision::interface::LoreMetadata;

    let entries = LoreArray::from_vec(vec![
        LoreRevisionTreeMetadataSetEntry {
            entry_id: 1,
            key: LoreString::from_str("blob"),
            value: LoreMetadata::Binary(LoreBinary::from_bytes(&[0x00, 0xff, 0x01])),
        },
        LoreRevisionTreeMetadataSetEntry {
            entry_id: 2,
            key: LoreString::from_str("count"),
            value: LoreMetadata::Numeric(4207),
        },
    ]);
    let args = LoreRevisionTreeMetadataSetArgs {
        batch_id: 900,
        handle: LoreRevisionTree { handle_id: 5 },
        entries: entries.clone(),
    };

    for (serialization, label) in [
        (SerializationType::Json, "json"),
        (SerializationType::Bincode, "bincode"),
    ] {
        let message = MessageToServer {
            globals: LoreGlobalArgs::default(),
            command: LoreCommand::RevisionTreeMetadataSet(args.clone()),
        };
        let message_bytes = write_v1_message(message, serialization).unwrap();
        let processed: Result<Option<(V1Header, MessageToServer)>, MessageError> =
            blocking_read_v1_message(&mut message_bytes.as_slice());
        let processed = processed
            .unwrap_or_else(|error| panic!("{label} must read back: {error:?}"))
            .expect("a whole message must be present");

        match processed.1.command {
            LoreCommand::RevisionTreeMetadataSet(read_back) => {
                assert_eq!(read_back.entries.as_slice(), entries.as_slice(), "{label}");
            }
            _ => panic!("Unexpected command"),
        }
    }
}
