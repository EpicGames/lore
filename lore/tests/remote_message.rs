// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-FileCopyrightText: 2026 glitch-ux
// SPDX-License-Identifier: MIT
use lore::remote::command::LoreCommand;
use lore::remote::message::MessageError;
use lore::remote::message::MessageToServer;
use lore::remote::message::SerializationType;
use lore::remote::message::V1Header;
use lore::remote::message::blocking_read_v1_message;
use lore::remote::message::write_v1_message;
use lore::repository::LoreRepositoryStatusArgs;
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

#[test]
fn errors_on_invalid_serialization_type_in_header() {
    // A valid V1 version byte (0), a zero payload size, and 0xff as the
    // serialization-type byte, which is not a valid SerializationType. Reading a
    // message with such a header must surface a recoverable error rather than
    // panicking the process.
    let bytes: [u8; 6] = [0, 0, 0, 0, 0, 0xff];

    let result: Result<Option<(V1Header, MessageToServer)>, MessageError> =
        blocking_read_v1_message(&mut bytes.as_slice());

    assert!(result.is_err());
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
