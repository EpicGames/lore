// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use bytes::Bytes;
use lore::remote::call::blocking_check_answer;
use lore::remote::call::blocking_request_stop;
use lore::remote::command::LoreCommand;
use lore::remote::message::Answer;
use lore::remote::message::CONTROL_FRAME_LIMIT;
use lore::remote::message::FrameKind;
use lore::remote::message::Header;
use lore::remote::message::MessageError;
use lore::remote::message::MessageToClient;
use lore::remote::message::MessageToServer;
use lore::remote::message::Opening;
use lore::remote::message::Schemas;
use lore::remote::message::blocking_read_answer;
use lore::remote::message::blocking_read_message;
use lore::remote::message::blocking_read_opening;
use lore::remote::message::blocking_read_stop_result;
use lore::remote::message::encode_message;
use lore::remote::message::write_hello;
use lore::remote::message::write_message;
use lore::remote::message::write_payload;
use lore::remote::message::write_refusal;
use lore::remote::message::write_stop;
use lore::remote::message::write_stop_result;
use lore::repository::LoreRepositoryDeleteArgs;
use lore::repository::LoreRepositoryInstanceCloneArgs;
use lore::repository::LoreRepositoryStatusArgs;
use lore::revision_tree::add::LoreRevisionTreeAddArgs;
use lore::revision_tree::add::LoreRevisionTreeAddEntry;
use lore::revision_tree::delete::LoreRevisionTreeDeleteArgs;
use lore::revision_tree::delete::LoreRevisionTreeDeleteEntry;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::modify::LoreRevisionTreeModifyArgs;
use lore::revision_tree::modify::LoreRevisionTreeModifyEntry;
use lore::revision_tree::move_node::LoreRevisionTreeMoveArgs;
use lore::revision_tree::move_node::LoreRevisionTreeMoveEntry;
use lore_base::encoding::encode_position;
use lore_base::env::CallEnvironment;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_revision::event::LoreBytes;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreString;

/// `message` as a peer reads it back, as a `Message`, from the bytes written for it, beside the
/// payload it views.
fn read_back<Message: bitcode::DecodeOwned>(message: &impl bitcode::Encode) -> (Message, Bytes) {
    let mut written = Vec::new();
    write_message(&mut written, message).expect("a message must write");
    blocking_read_message(&mut written.as_slice())
        .expect("a message must read back")
        .expect("a whole message must be present")
}

/// `command` as the service reads it back from the message a client writes for it.
fn relayed(command: LoreCommand) -> LoreCommand {
    let message = MessageToServer {
        globals: LoreGlobalArgs::default(),
        command,
        environment: CallEnvironment::default(),
    };
    read_back::<MessageToServer>(&message).0.command
}

#[test]
fn header_to_and_from_bytes() {
    let header = Header::new(0xffeeddcc, FrameKind::Message);

    let bytes = header.to_bytes();
    let processed_header = Header::from_bytes(&bytes);

    assert!(processed_header.is_ok());
    assert_eq!(processed_header.unwrap().payload_size, header.payload_size);

    let mut bad_bytes = bytes;
    bad_bytes[4] = 0xff;
    let bad_processed_header = Header::from_bytes(&bad_bytes);

    assert!(bad_processed_header.is_err());
}

/// The bytes `write` writes.
fn written(write: impl FnOnce(&mut Vec<u8>) -> Result<(), MessageError>) -> Vec<u8> {
    let mut bytes = Vec::new();
    write(&mut bytes).expect("a frame must write");
    bytes
}

/// A frame of protocol version 3 holding `payload`.
fn frame(kind: FrameKind, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![3];
    bytes.extend_from_slice(&Header::new(payload.len() as u32, kind).to_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

#[test]
fn a_hello_carries_the_schemas_of_this_build() {
    let hello = written(write_hello);
    assert_eq!(
        blocking_read_opening(&mut hello.as_slice()).expect("a hello must read"),
        Some(Opening::Hello(Schemas::THIS_BUILD))
    );
    assert_eq!(
        blocking_read_answer(&mut hello.as_slice()).expect("a hello must read"),
        Answer::Hello(Schemas::THIS_BUILD)
    );
}

/// The hello of a client of another build reads as that build's schemas, which the service then
/// compares with its own.
#[test]
fn a_hello_of_another_build_reads_as_its_schemas() {
    let mut payload = 7u64.to_le_bytes().to_vec();
    payload.extend_from_slice(&9u64.to_le_bytes());
    let schemas = Schemas {
        to_server: 7,
        to_client: 9,
    };
    assert_ne!(schemas, Schemas::THIS_BUILD);
    assert_eq!(
        blocking_read_opening(&mut frame(FrameKind::Hello, &payload).as_slice())
            .expect("a hello must read"),
        Some(Opening::Hello(schemas))
    );
}

#[test]
fn a_stop_or_stop_result_of_the_wrong_size_is_an_error() {
    for payload in [&[][..], &[1, 1]] {
        assert!(blocking_read_opening(&mut frame(FrameKind::Stop, payload).as_slice()).is_err());
    }
    assert!(
        blocking_read_stop_result(&mut frame(FrameKind::StopResult, &[0; 3]).as_slice()).is_err()
    );
}

#[test]
fn a_hello_of_the_wrong_size_is_an_error() {
    for payload in [&[0u8; 8][..], &[0; 17], &[]] {
        let hello = frame(FrameKind::Hello, payload);
        assert!(blocking_read_opening(&mut hello.as_slice()).is_err());
        assert!(blocking_read_answer(&mut hello.as_slice()).is_err());
    }
}

#[test]
fn a_stop_carries_its_force() {
    for force in [false, true] {
        let stop = written(|bytes| write_stop(bytes, force));
        assert_eq!(
            blocking_read_opening(&mut stop.as_slice()).expect("a stop must read"),
            Some(Opening::Stop { force })
        );
    }
}

/// A client of a build from before protocol version 3 writes version 2, whose byte is 1.
#[test]
fn an_opening_of_another_protocol_reads_as_that_protocol() {
    let mut opening = frame(FrameKind::Message, b"payload");
    opening[0] = 1;
    assert_eq!(
        blocking_read_opening(&mut opening.as_slice()).expect("the protocol byte must read"),
        Some(Opening::OtherProtocol(1))
    );
}

#[test]
fn a_connection_closed_before_its_opening_reads_as_none() {
    assert_eq!(
        blocking_read_opening(&mut [].as_slice()).expect("a closed connection must read"),
        None
    );
}

/// A call opens with a hello, so a message in its place is an error rather than decoded.
#[test]
fn a_message_without_a_hello_is_an_error() {
    let message = written(|bytes| write_payload(bytes, b"payload"));
    assert!(blocking_read_opening(&mut message.as_slice()).is_err());
}

#[test]
fn a_refusal_carries_its_reason() {
    let refusal = written(|bytes| write_refusal(bytes, "another build"));
    assert_eq!(
        blocking_read_answer(&mut refusal.as_slice()).expect("a refusal must read"),
        Answer::Refusal("another build".to_string())
    );
}

/// A service of an older protocol closes the connection on a hello it can't read.
#[test]
fn a_service_that_closes_without_answering_refuses_the_call() {
    assert!(matches!(
        blocking_read_answer(&mut [].as_slice()).expect("a closed connection must read"),
        Answer::Closed
    ));
}

/// A connection that the peer reset, as the OS may report a peer that closed it with this side's
/// data unread.
struct Reset;

impl std::io::Read for Reset {
    fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::ErrorKind::ConnectionReset.into())
    }
}

/// A service of an older protocol closes the connection on a hello it can't read, with the call
/// that follows the hello unread, which the OS may report as a reset.
#[test]
fn a_service_that_resets_the_connection_refuses_the_call() {
    assert!(matches!(
        blocking_read_answer(&mut Reset).expect("a reset connection must read as closed"),
        Answer::Closed
    ));
    assert_eq!(
        blocking_read_stop_result(&mut Reset).expect("a reset connection must read as closed"),
        None
    );
}

/// A service of another protocol answers in its own, as this one answers a client of another, so
/// that a client of any protocol can tell it was refused.
#[test]
fn an_answer_of_another_protocol_reads_as_one() {
    for protocol in [1, 4] {
        let mut hello = written(write_hello);
        hello[0] = protocol;
        assert_eq!(
            blocking_read_answer(&mut hello.as_slice()).expect("an answer of any protocol reads"),
            Answer::OtherProtocol(protocol)
        );
    }
}

#[test]
fn a_stop_result_carries_its_status_and_message() {
    for (status, message) in [(0, ""), (51, "Mounts are busy: /mnt/a")] {
        let result = written(|bytes| write_stop_result(bytes, status, message));
        assert_eq!(
            blocking_read_stop_result(&mut result.as_slice()).expect("a stop result must read"),
            Some((status, message.to_string()))
        );
    }
}

#[test]
fn a_service_that_closes_without_answering_a_stop_reads_as_none() {
    assert_eq!(
        blocking_read_stop_result(&mut [].as_slice()).expect("a closed connection must read"),
        None
    );
}

/// Text longer than a control frame holds is cut at a character boundary, so that the frame is
/// read rather than refused.
#[test]
fn a_long_text_is_cut_to_fit_a_control_frame() {
    // Offset by one byte, so that the limit falls inside a character.
    let text = format!("a{}", "é".repeat(CONTROL_FRAME_LIMIT as usize));
    let refusal = written(|bytes| write_refusal(bytes, &text));
    let Answer::Refusal(read) =
        blocking_read_answer(&mut refusal.as_slice()).expect("a long refusal must read")
    else {
        panic!("a refusal must read as one");
    };
    assert!(!read.is_empty() && text.starts_with(&read));

    let result = written(|bytes| write_stop_result(bytes, 51, &text));
    let (status, read) = blocking_read_stop_result(&mut result.as_slice())
        .expect("a long stop result must read")
        .expect("a stop result must be present");
    assert_eq!(status, 51);
    assert!(!read.is_empty() && text.starts_with(&read));
}

/// A control frame larger than the limit is an error, though it carries all the bytes its header
/// claims, so that a peer can't make the reader allocate what a header claims. A frame of exactly
/// the limit reads.
#[test]
fn a_control_frame_larger_than_the_limit_is_an_error() {
    let limit = CONTROL_FRAME_LIMIT as usize;
    let at_limit = vec![b'a'; limit];
    let past_limit = vec![b'a'; limit + 1];

    assert!(matches!(
        blocking_read_answer(&mut frame(FrameKind::Refusal, &at_limit).as_slice()),
        Ok(Answer::Refusal(_))
    ));
    assert!(blocking_read_answer(&mut frame(FrameKind::Refusal, &past_limit).as_slice()).is_err());

    assert!(matches!(
        blocking_read_stop_result(&mut frame(FrameKind::StopResult, &at_limit).as_slice()),
        Ok(Some(_))
    ));
    assert!(
        blocking_read_stop_result(&mut frame(FrameKind::StopResult, &past_limit).as_slice())
            .is_err()
    );
}

/// A reader of control frames refuses a message from its header, leaving its payload unread, so
/// that a peer can't make it allocate a payload it would refuse.
#[test]
fn a_reader_of_control_frames_refuses_a_message_before_its_payload() {
    let payload = vec![b'a'; CONTROL_FRAME_LIMIT as usize + 1];
    let message = frame(FrameKind::Message, &payload);

    let mut opening = message.as_slice();
    assert!(blocking_read_opening(&mut opening).is_err());
    assert_eq!(opening.len(), payload.len(), "an opening read the payload");

    let mut answer = message.as_slice();
    assert!(blocking_read_answer(&mut answer).is_err());
    assert_eq!(answer.len(), payload.len(), "an answer read the payload");

    let mut stop_result = message.as_slice();
    assert!(blocking_read_stop_result(&mut stop_result).is_err());
    assert_eq!(
        stop_result.len(),
        payload.len(),
        "a stop result read the payload"
    );
}

/// The client completes a call with `ServiceUnavailable` wherever the service didn't accept its
/// hello: a refusal, schemas of another build in either direction, or no answer at all.
#[test]
fn a_client_fails_a_call_the_service_did_not_accept() {
    let ours = Schemas::THIS_BUILD;
    assert!(blocking_check_answer(&mut written(write_hello).as_slice(), Ok(())).is_ok());

    let hello_of = |to_server: u64, to_client: u64| {
        let mut payload = to_server.to_le_bytes().to_vec();
        payload.extend_from_slice(&to_client.to_le_bytes());
        frame(FrameKind::Hello, &payload)
    };
    for answer in [
        written(|bytes| write_refusal(bytes, "another build")),
        hello_of(ours.to_server ^ 1, ours.to_client),
        hello_of(ours.to_server, ours.to_client ^ 1),
        {
            let mut later = written(write_hello);
            later[0] = 4;
            later
        },
        Vec::new(),
    ] {
        let error = blocking_check_answer(&mut answer.as_slice(), Ok(()))
            .expect_err("a call the service didn't accept must fail");
        assert!(error.is_service_unavailable(), "{error}");
    }
    let error =
        blocking_check_answer(&mut Reset, Ok(())).expect_err("a reset connection must fail");
    assert!(error.is_service_unavailable(), "{error}");
}

/// A call whose write failed against a service that closed the connection without answering, as
/// one that exited does, fails with `ServiceUnavailable` and gives the write's failure.
#[test]
fn a_client_gives_the_write_that_failed_against_a_closed_connection() {
    let write_error = || MessageError::internal("the write of the call");
    let written_text = write_error().to_string();
    let error = blocking_check_answer(&mut [].as_slice(), Err(write_error()))
        .expect_err("a call the service didn't answer must fail");
    assert!(error.is_service_unavailable(), "{error}");
    assert!(
        error.to_string().contains(&written_text),
        "{error} must give {written_text}"
    );

    // A refusal gives its own reason, which explains a write it cut short.
    let refusal = written(|bytes| write_refusal(bytes, "another build"));
    let error = blocking_check_answer(&mut refusal.as_slice(), Err(write_error()))
        .expect_err("a refused call must fail");
    assert!(error.is_service_unavailable(), "{error}");
    assert!(!error.to_string().contains(&written_text), "{error}");

    // A service that accepted the call leaves the write's failure as it is.
    let error = blocking_check_answer(&mut written(write_hello).as_slice(), Err(write_error()))
        .expect_err("a call whose write failed must fail");
    assert!(!error.is_service_unavailable(), "{error}");
}

/// A stop writes the request a service reads as one, and completes with the service's answer,
/// or with `ServiceUnavailable` where the service closes the connection without one.
#[test]
fn a_client_stop_reads_the_service_answer() {
    for force in [false, true] {
        let mut request = Vec::new();
        let answer = written(|bytes| write_stop_result(bytes, 51, "busy"));
        assert_eq!(
            blocking_request_stop(&mut request, &mut answer.as_slice(), force)
                .expect("an answered stop must succeed"),
            (51, "busy".to_string())
        );
        assert_eq!(
            blocking_read_opening(&mut request.as_slice()).expect("the request must read"),
            Some(Opening::Stop { force })
        );
    }

    let error = blocking_request_stop(&mut Vec::new(), &mut [].as_slice(), false)
        .expect_err("an unanswered stop must fail");
    assert!(error.is_service_unavailable(), "{error}");
}

#[test]
fn a_frame_other_than_a_message_is_not_read_as_one() {
    let hello = written(write_hello);
    assert!(blocking_read_message::<MessageToClient, _>(&mut hello.as_slice()).is_err());
}

#[test]
fn a_message_of_another_protocol_is_an_error() {
    let mut message =
        written(|bytes| write_message(bytes, &MessageToClient::<LoreEvent>::ApiResult(0)));
    message[0] = 1;
    assert!(blocking_read_message::<MessageToClient, _>(&mut message.as_slice()).is_err());
}

/// A message from a client carries its globals, its command and its environment.
#[tokio::test]
async fn message_to_server_to_and_from_bytes() {
    let path = LoreString::from_str("abc");
    let paths = LoreArray::from_vec(vec![
        LoreString::from_str("abc"),
        LoreString::from_str("def"),
    ]);
    let environment = CallEnvironment {
        values: [Some("/tmp/global".to_string()), None, None],
    };
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
        environment: environment.clone(),
    };

    let (read, _payload) = read_back::<MessageToServer>(&message);

    assert_eq!(read.globals.repository_path, path);
    assert_eq!(read.environment, environment);
    match read.command {
        LoreCommand::RepositoryStatus(repository_status) => {
            assert_eq!(repository_status.paths.as_slice(), paths.as_slice());
        }
        _ => {
            panic!("Unexpected command");
        }
    }
}

/// Text crosses as the bytes the caller sent, whether or not they are UTF-8: the service checks
/// it, so that it refuses what the caller's entry point refuses.
#[test]
fn text_that_is_not_utf8_crosses_unchanged() {
    let path = LoreString::from_bytes(b"repo\xff");
    let message = MessageToServer {
        globals: LoreGlobalArgs {
            repository_path: path.clone(),
            ..Default::default()
        },
        command: LoreCommand::LinkListStaged(lore::link::LoreLinkListStagedArgs {}),
        environment: CallEnvironment::default(),
    };

    assert_eq!(
        read_back::<MessageToServer>(&message)
            .0
            .globals
            .repository_path,
        path
    );
}

/// The service refuses text that is not UTF-8 with the error the caller's own entry point gives,
/// before a handler could read it as `&str`.
#[tokio::test]
async fn the_service_refuses_text_that_is_not_utf8() {
    let message = MessageToServer {
        globals: LoreGlobalArgs {
            repository_path: LoreString::from_bytes(b"repo\xff"),
            ..Default::default()
        },
        command: LoreCommand::LinkListStaged(lore::link::LoreLinkListStagedArgs {}),
        environment: CallEnvironment::default(),
    };

    let status = Box::pin(read_back::<MessageToServer>(&message).0.invoke(None)).await;

    assert_eq!(
        status,
        lore_revision::event::LoreErrorCode::InvalidArguments as i32
    );
}

/// A put relayed to the service carries the bytes its items view, and the service reads them
/// where they arrived rather than from a copy.
#[test]
fn a_put_carries_its_bytes_to_the_service() {
    use lore::storage::put::LoreStoragePutArgs;
    use lore::storage::put::LoreStoragePutItem;

    let contents: [&[u8]; 3] = [b"first", b"", b"third item"];
    let items = contents
        .iter()
        .enumerate()
        .map(|(id, bytes)| LoreStoragePutItem {
            id: id as u64,
            partition: Default::default(),
            context: Default::default(),
            data: LoreBytes {
                ptr: bytes.as_ptr().cast(),
                len: bytes.len(),
            },
            remote_write: 0,
            local_cache: 0,
            remote_query: 0,
            fixed_size_chunk: 0,
        })
        .collect();
    let message = MessageToServer {
        globals: LoreGlobalArgs::default(),
        command: LoreCommand::StoragePut(LoreStoragePutArgs {
            handle: Default::default(),
            items: LoreArray::from_vec(items),
        }),
        environment: CallEnvironment::default(),
    };

    let (read, payload) = read_back::<MessageToServer>(&message);

    let LoreCommand::StoragePut(args) = read.command else {
        panic!("Unexpected command");
    };
    let payload = payload.as_ptr_range();
    for (item, expected) in args.items.as_slice().iter().zip(contents) {
        // SAFETY: `payload` is alive, and the view points into it.
        assert_eq!(unsafe { item.data.as_slice() }, expected);
        if !expected.is_empty() {
            assert!(
                payload.contains(&item.data.ptr.cast()),
                "the view must point into the payload"
            );
        }
    }
}

/// The service encodes an event it is handed by reference, and a client reads it back as the
/// event, with the bytes a data event views.
#[test]
fn a_borrowed_data_event_reaches_a_client_as_itself() {
    use lore_revision::store::event::LoreStorageGetDataEventData;

    let contents = b"fragment payload";
    let event = LoreEvent::StorageGetData(LoreStorageGetDataEventData {
        id: 3,
        address: Address::default(),
        offset: 64,
        bytes: LoreBytes {
            ptr: contents.as_ptr().cast(),
            len: contents.len(),
        },
    });

    let (read, _payload) = read_back::<MessageToClient>(&MessageToClient::Event(&event));
    match read {
        MessageToClient::Event(LoreEvent::StorageGetData(data)) => {
            assert_eq!((data.id, data.offset), (3, 64));
            // SAFETY: the payload is alive, and the view points into it.
            assert_eq!(unsafe { data.bytes.as_slice() }, contents);
        }
        _ => panic!("expected a data event"),
    }
}

/// The event of the variant at `position`, decoded from the position followed by the fewest zero
/// bytes its payload decodes from, which hold empty text and arrays, zero numbers and the first
/// variant of each enum.
fn zero_event(position: u32) -> LoreEvent {
    let mut encoded = Vec::new();
    encode_position(position, &mut encoded);
    for _ in 0..4096 {
        if let Ok(event) = bitcode::decode(&encoded) {
            return event;
        }
        encoded.push(0);
    }
    panic!("the payload of the variant at {position} must decode from zero bytes");
}

/// Every event reaches a client as itself, each variant at a position of its own.
#[test]
fn every_event_reaches_a_client_as_itself() {
    let mut kinds = std::collections::BTreeSet::new();
    for position in 0..LoreEvent::VARIANTS {
        let event = zero_event(position);
        assert!(
            kinds.insert(event.discriminant()),
            "the variant at {position} must be the only one there"
        );
        let (read, _payload) = read_back::<MessageToClient>(&MessageToClient::Event(&event));
        match read {
            MessageToClient::Event(read) => assert!(
                read == event,
                "the variant at {position} must read back as itself"
            ),
            _ => panic!("expected the event at {position}"),
        }
    }
}

/// A command whose arguments carry no fields still has to reach the service as
/// itself.
#[tokio::test]
async fn link_list_staged_survives_the_wire() {
    use lore::link::LoreLinkListStagedArgs;

    let command = relayed(LoreCommand::LinkListStaged(LoreLinkListStagedArgs {}));
    assert!(
        matches!(command, LoreCommand::LinkListStaged(_)),
        "must read back as the same command: {command:?}"
    );
}

/// A LATEST history listing relayed to the service carries the branch and the entry limit.
#[tokio::test]
async fn branch_latest_list_args_survive_the_wire() {
    use lore::branch::LoreBranchLatestListArgs;

    let args = LoreBranchLatestListArgs {
        branch: LoreString::from_str("release/5.4"),
        limit: 7,
    };

    match relayed(LoreCommand::BranchLatestList(args.clone())) {
        LoreCommand::BranchLatestList(read_back) => {
            assert_eq!(read_back, args, "must carry every field unchanged");
        }
        other => panic!("Unexpected command: {other:?}"),
    }
}

/// A delete names its repository by text alone, so the service deletes the repository the caller
/// named only if that text crosses the wire unchanged.
#[tokio::test]
async fn repository_delete_survives_the_wire() {
    let args = LoreRepositoryDeleteArgs {
        repository_url: LoreString::from_str("lore://127.0.0.1:41337/org/project"),
    };

    match relayed(LoreCommand::RepositoryDelete(args.clone())) {
        LoreCommand::RepositoryDelete(read_back) => {
            assert_eq!(read_back, args, "must carry the URL unchanged");
        }
        other => panic!("Unexpected command: {other:?}"),
    }
}

/// An instance clone names its destination by text alone, so the service clones to the path the
/// caller named only if that text crosses the wire unchanged.
#[tokio::test]
async fn repository_instance_clone_survives_the_wire() {
    let args = LoreRepositoryInstanceCloneArgs {
        target_path: LoreString::from_str("/work/clones/second"),
        allow_copy: 1,
    };

    match relayed(LoreCommand::RepositoryInstanceClone(args.clone())) {
        LoreCommand::RepositoryInstanceClone(read_back) => {
            assert_eq!(
                read_back, args,
                "must carry the target path and the allow-copy flag unchanged"
            );
        }
        other => panic!("Unexpected command: {other:?}"),
    }
}

/// A command carrying an address carries every byte of it.
#[tokio::test]
async fn a_command_carrying_an_address_survives_the_wire() {
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

    match relayed(LoreCommand::RevisionTreeAdd(args.clone())) {
        LoreCommand::RevisionTreeAdd(read_back) => {
            assert_eq!(
                read_back.entries.as_slice()[0].address,
                address,
                "must carry the address unchanged"
            );
            assert_eq!(read_back.entries.as_slice(), args.entries.as_slice());
        }
        _ => panic!("Unexpected command"),
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

    match relayed(LoreCommand::RevisionTreeModify(args.clone())) {
        LoreCommand::RevisionTreeModify(read_back) => {
            assert_eq!(read_back.batch_id, args.batch_id);
            assert_eq!(read_back.handle.handle_id, args.handle.handle_id);
            assert_eq!(
                read_back.entries.as_slice(),
                entries.as_slice(),
                "must carry every entry field unchanged"
            );
        }
        other => panic!("Unexpected command: {other:?}"),
    }
}

/// The smallest batch entry in the namespace, and the one most likely to be
/// mistaken for needing no coverage: two integers, both of which a caller
/// correlates results by.
#[tokio::test]
async fn revision_tree_delete_batch_survives_the_wire() {
    let entries = LoreArray::from_vec(vec![
        LoreRevisionTreeDeleteEntry {
            entry_id: 7,
            node_id: 42,
        },
        LoreRevisionTreeDeleteEntry {
            entry_id: 0,
            node_id: 43,
        },
    ]);
    let args = LoreRevisionTreeDeleteArgs {
        batch_id: 900,
        handle: LoreRevisionTree { handle_id: 5 },
        entries: entries.clone(),
    };

    match relayed(LoreCommand::RevisionTreeDelete(args.clone())) {
        LoreCommand::RevisionTreeDelete(read_back) => {
            assert_eq!(read_back.batch_id, args.batch_id);
            assert_eq!(read_back.handle.handle_id, args.handle.handle_id);
            assert_eq!(
                read_back.entries.as_slice(),
                entries.as_slice(),
                "must carry every entry field unchanged"
            );
        }
        other => panic!("Unexpected command: {other:?}"),
    }
}

/// The only batch entry carrying both a string and two node ids, so a wire format
/// that lost the string's length or ran the fields out of order would show here.
#[tokio::test]
async fn revision_tree_move_batch_survives_the_wire() {
    let entries = LoreArray::from_vec(vec![
        LoreRevisionTreeMoveEntry {
            entry_id: 7,
            node_id: 42,
            destination_parent_id: 0,
            dst_name: LoreString::from_str("moved.bin"),
        },
        LoreRevisionTreeMoveEntry {
            entry_id: 0,
            node_id: 43,
            destination_parent_id: 42,
            dst_name: LoreString::from_str("renamed.bin"),
        },
    ]);
    let args = LoreRevisionTreeMoveArgs {
        batch_id: 900,
        handle: LoreRevisionTree { handle_id: 5 },
        entries: entries.clone(),
    };

    match relayed(LoreCommand::RevisionTreeMove(args.clone())) {
        LoreCommand::RevisionTreeMove(read_back) => {
            assert_eq!(read_back.batch_id, args.batch_id);
            assert_eq!(read_back.handle.handle_id, args.handle.handle_id);
            assert_eq!(
                read_back.entries.as_slice(),
                entries.as_slice(),
                "must carry every entry field unchanged"
            );
        }
        other => panic!("Unexpected command: {other:?}"),
    }
}

/// The only revision-tree verb that publishes anything, and the one whose
/// arguments no longer name the branch — the nested options struct is the part a
/// hand-written encoder is most likely to flatten away.
#[tokio::test]
async fn revision_tree_commit_survives_the_wire() {
    use lore::revision_tree::commit::LoreRevisionTreeCommitArgs;
    use lore::revision_tree::commit::LoreRevisionTreeCommitOptions;

    let args = LoreRevisionTreeCommitArgs {
        id: 4242,
        handle: LoreRevisionTree { handle_id: 9 },
        options: LoreRevisionTreeCommitOptions {
            remote_write: 1,
            parent_other: Hash::from_u64(0x2a),
        },
    };

    match relayed(LoreCommand::RevisionTreeCommit(args)) {
        LoreCommand::RevisionTreeCommit(read_back) => {
            assert_eq!(read_back, args, "must carry every field unchanged");
        }
        other => panic!("Unexpected command: {other:?}"),
    }
}

/// A metadata read delivers its value to the caller as an event, and an
/// out-of-process caller only ever sees the encoded form. Binary values are
/// the ones with no textual representation to fall back on, so they are the
/// case worth pinning.
#[tokio::test]
async fn a_metadata_event_carries_a_binary_value_to_a_client() {
    use lore_revision::event::LoreMetadataEventData;
    use lore_revision::interface::LoreBinary;
    use lore_revision::interface::LoreMetadata;

    let value = LoreMetadata::Binary(LoreBinary::from_bytes(b"raw\x00bytes"));
    let event = LoreEvent::Metadata(LoreMetadataEventData {
        key: LoreString::from_str("thumbnail"),
        value: value.clone(),
    });

    match read_back::<MessageToClient>(&MessageToClient::Event(event)).0 {
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
/// to cross the wire whole.
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

    match relayed(LoreCommand::RevisionTreeMetadataSet(args.clone())) {
        LoreCommand::RevisionTreeMetadataSet(read_back) => {
            assert_eq!(read_back.entries.as_slice(), entries.as_slice());
        }
        _ => panic!("Unexpected command"),
    }
}

/// A bisect step relayed to the service carries both ends of the range.
#[tokio::test]
async fn revision_bisect_args_survive_the_wire() {
    use lore::revision::LoreRevisionBisectArgs;

    let args = LoreRevisionBisectArgs {
        start: LoreString::from_str("main@3"),
        end: LoreString::from_str("main@11"),
    };

    match relayed(LoreCommand::RevisionBisect(args.clone())) {
        LoreCommand::RevisionBisect(read_back) => {
            assert_eq!(read_back, args, "must carry every field unchanged");
        }
        other => panic!("Unexpected command: {other:?}"),
    }
}

/// A cherry-pick routed through a service carries the metadata keys the revision it creates
/// inherits: an array of text, and the last field of the struct.
#[tokio::test]
async fn revision_cherry_pick_args_survive_the_wire() {
    use lore::revision::LoreRevisionCherryPickArgs;

    let args = LoreRevisionCherryPickArgs {
        revision: LoreString::from_str("main@7"),
        message: LoreString::from_str("pick"),
        no_commit: 1,
        inherit_metadata: LoreArray::from_vec(vec![LoreString::from_str("change-request")]),
    };

    match relayed(LoreCommand::RevisionCherryPick(args.clone())) {
        LoreCommand::RevisionCherryPick(read_back) => {
            assert_eq!(
                (read_back.revision, read_back.message, read_back.no_commit),
                (args.revision.clone(), args.message.clone(), args.no_commit)
            );
            assert_eq!(
                read_back.inherit_metadata.as_slice(),
                args.inherit_metadata.as_slice(),
                "must carry the inherited keys unchanged"
            );
        }
        other => panic!("Unexpected command: {other:?}"),
    }
}

/// A sync routed through a service carries its view filter file over the wire.
///
/// `view` names the file the working tree is left materialized under, and a sync that reached the
/// service without it is a sync of the revision alone: the same call, silently doing something
/// else. The last field of the struct, which is where a hand-written encoder stops early.
///
/// The arrays are compared as slices rather than with the struct: a `LoreArray` is a pointer and a
/// count, and its equality is the pointer's, so two arrays holding equal elements at different
/// addresses are unequal.
#[tokio::test]
async fn revision_sync_args_survive_the_wire() {
    use lore::revision::LoreRevisionSyncArgs;

    let args = LoreRevisionSyncArgs {
        revision: LoreString::from_str("main@7"),
        forward_changes: 0,
        reset: 1,
        root_files: LoreArray::from_vec(vec![LoreString::from_str("engine/root.uasset")]),
        dependency_tags: LoreArray::from_vec(vec![LoreString::from_str("editor")]),
        dependency_recursive: 1,
        dependency_depth_limit: 3,
        view: LoreString::from_str("/tmp/a narrow view.txt"),
    };

    match relayed(LoreCommand::RevisionSync(args.clone())) {
        LoreCommand::RevisionSync(read_back) => {
            assert_eq!(read_back.view, args.view, "must carry the view unchanged");
            assert_eq!(read_back.revision, args.revision);
            assert_eq!(read_back.root_files.as_slice(), args.root_files.as_slice());
            assert_eq!(
                read_back.dependency_tags.as_slice(),
                args.dependency_tags.as_slice()
            );
            assert_eq!(
                (
                    read_back.forward_changes,
                    read_back.reset,
                    read_back.dependency_recursive,
                    read_back.dependency_depth_limit,
                ),
                (
                    args.forward_changes,
                    args.reset,
                    args.dependency_recursive,
                    args.dependency_depth_limit,
                ),
                "must carry every flag unchanged"
            );
        }
        other => panic!("Unexpected command: {other:?}"),
    }
}

/// A shared store listing routed through a service carries whether to look up the instances using
/// each store, which decides whether the service loads every store it lists.
#[tokio::test]
async fn shared_store_list_args_survive_the_wire() {
    use lore::shared_store::LoreSharedStoreListArgs;

    let args = LoreSharedStoreListArgs {
        include_instances: 1,
    };

    match relayed(LoreCommand::SharedStoreList(args.clone())) {
        LoreCommand::SharedStoreList(read_back) => {
            assert_eq!(read_back, args, "must carry every field unchanged");
        }
        other => panic!("Unexpected command: {other:?}"),
    }
}

/// A branch list relayed to the service carries its creator filter byte for byte, since the filter
/// matches exactly and case-sensitively, and an empty filter, which keeps every creator.
#[test]
fn revision_graph_branch_list_survives_the_wire() {
    use lore::revision_graph::branch_list::LoreRevisionGraphBranchListArgs;
    use lore::storage::handle::LoreStore;

    let filtered = LoreRevisionGraphBranchListArgs {
        id: 31,
        store: LoreStore { handle_id: 6 },
        repository: Partition::from([0x5au8; 16]),
        include_archived: 1,
        creator: LoreString::from_str("Åsa.Builder"),
    };
    let unfiltered = LoreRevisionGraphBranchListArgs {
        creator: LoreString::default(),
        include_archived: 0,
        ..filtered.clone()
    };

    for args in [filtered, unfiltered] {
        match relayed(LoreCommand::RevisionGraphBranchList(args.clone())) {
            LoreCommand::RevisionGraphBranchList(read_back) => {
                assert_eq!(read_back, args, "must carry every field unchanged");
            }
            other => panic!("Unexpected command: {other:?}"),
        }
    }
}

/// A branch metadata set relayed to the service carries a value of every type, each under the tag
/// it was sent with, and the keys to remove.
#[test]
fn revision_graph_branch_metadata_set_survives_the_wire() {
    use lore::revision_graph::branch_metadata_set::LoreRevisionGraphBranchMetadataEntry;
    use lore::revision_graph::branch_metadata_set::LoreRevisionGraphBranchMetadataSetArgs;
    use lore::storage::handle::LoreStore;
    use lore_revision::interface::LoreBinary;
    use lore_revision::interface::LoreMetadata;

    let entry = |key: &str, value| LoreRevisionGraphBranchMetadataEntry {
        key: LoreString::from_str(key),
        value,
    };
    let args = LoreRevisionGraphBranchMetadataSetArgs {
        id: 14,
        store: LoreStore { handle_id: 4 },
        repository: Partition::from([0x21u8; 16]),
        branch: Context::from([0x42u8; 16]),
        entries: LoreArray::from_vec(vec![
            entry(
                "asset",
                LoreMetadata::Address(Address {
                    hash: Hash::from([0x37u8; 32]),
                    context: Context::from([0x73u8; 16]),
                }),
            ),
            entry("protect", LoreMetadata::Boolean(1)),
            entry(
                "thumbnail",
                LoreMetadata::Binary(LoreBinary::from_bytes(b"raw\x00bytes")),
            ),
            entry("owner", LoreMetadata::Context(Context::from([0x19u8; 16]))),
            entry("build", LoreMetadata::Hash(Hash::from([0x91u8; 32]))),
            entry("changelist", LoreMetadata::Numeric(4207)),
            entry(
                "pipeline",
                LoreMetadata::String(LoreString::from_str("nightly")),
            ),
        ]),
        remove: LoreArray::from_vec(vec![LoreString::from_str("reviewer")]),
    };

    match relayed(LoreCommand::RevisionGraphBranchMetadataSet(args.clone())) {
        LoreCommand::RevisionGraphBranchMetadataSet(read_back) => {
            assert_eq!(
                (
                    read_back.id,
                    read_back.store,
                    read_back.repository,
                    read_back.branch
                ),
                (args.id, args.store, args.repository, args.branch)
            );
            assert_eq!(
                read_back.entries.as_slice(),
                args.entries.as_slice(),
                "must carry every entry's key and typed value unchanged"
            );
            assert_eq!(
                read_back.remove.as_slice(),
                args.remove.as_slice(),
                "must carry the keys to remove unchanged"
            );
        }
        other => panic!("Unexpected command: {other:?}"),
    }
}

/// A branch create relayed to the service carries its name, its category and the revision
/// identifier it starts from, a number on a branch and a hash alike.
#[test]
fn revision_graph_branch_create_survives_the_wire() {
    use lore::revision_graph::LoreRevisionIdentifier;
    use lore::revision_graph::branch_create::LoreRevisionGraphBranchCreateArgs;
    use lore::storage::handle::LoreStore;

    let numbered = LoreRevisionGraphBranchCreateArgs {
        id: 16,
        store: LoreStore { handle_id: 4 },
        repository: Partition::from([0x21u8; 16]),
        name: LoreString::from_str("feature/Åsa"),
        category: LoreString::from_str("tools"),
        from: LoreRevisionIdentifier {
            hash: Hash::default(),
            branch: Context::from([0x42u8; 16]),
            revision_number: 25,
        },
    };
    let by_hash = LoreRevisionGraphBranchCreateArgs {
        category: LoreString::default(),
        from: LoreRevisionIdentifier {
            hash: Hash::from([0x37u8; 32]),
            ..Default::default()
        },
        ..numbered.clone()
    };

    for args in [numbered, by_hash] {
        match relayed(LoreCommand::RevisionGraphBranchCreate(args.clone())) {
            LoreCommand::RevisionGraphBranchCreate(read_back) => {
                assert_eq!(read_back, args, "must carry every field unchanged");
            }
            other => panic!("Unexpected command: {other:?}"),
        }
    }
}

/// A branch archive relayed to the service carries its branch id.
#[test]
fn revision_graph_branch_archive_survives_the_wire() {
    use lore::revision_graph::branch_archive::LoreRevisionGraphBranchArchiveArgs;
    use lore::storage::handle::LoreStore;

    let args = LoreRevisionGraphBranchArchiveArgs {
        id: 17,
        store: LoreStore { handle_id: 4 },
        repository: Partition::from([0x21u8; 16]),
        branch: Context::from([0x42u8; 16]),
    };

    match relayed(LoreCommand::RevisionGraphBranchArchive(args.clone())) {
        LoreCommand::RevisionGraphBranchArchive(read_back) => {
            assert_eq!(read_back, args, "must carry every field unchanged");
        }
        other => panic!("Unexpected command: {other:?}"),
    }
}

/// A branch sync relayed to the service carries its target and options.
#[test]
fn revision_graph_branch_sync_survives_the_wire() {
    use lore::revision_graph::LoreRevisionIdentifier;
    use lore::revision_graph::branch_sync::LoreRevisionGraphBranchSyncArgs;
    use lore::revision_graph::branch_sync::LoreRevisionGraphBranchSyncOptions;
    use lore::storage::handle::LoreStore;

    let args = LoreRevisionGraphBranchSyncArgs {
        id: 18,
        store: LoreStore { handle_id: 4 },
        repository: Partition::from([0x21u8; 16]),
        branch: Context::from([0x42u8; 16]),
        target: LoreRevisionIdentifier {
            hash: Hash::default(),
            branch: Context::from([0x42u8; 16]),
            revision_number: 25,
        },
        options: LoreRevisionGraphBranchSyncOptions {
            force: 1,
            expected_local_latest: Hash::from([0x63u8; 32]),
        },
    };

    match relayed(LoreCommand::RevisionGraphBranchSync(args.clone())) {
        LoreCommand::RevisionGraphBranchSync(read_back) => {
            assert_eq!(read_back, args, "must carry every field unchanged");
        }
        other => panic!("Unexpected command: {other:?}"),
    }
}

/// A branch push relayed to the service carries its options.
#[test]
fn revision_graph_branch_push_survives_the_wire() {
    use lore::revision_graph::branch_push::LoreRevisionGraphBranchPushArgs;
    use lore::revision_graph::branch_push::LoreRevisionGraphBranchPushOptions;
    use lore::storage::handle::LoreStore;

    let args = LoreRevisionGraphBranchPushArgs {
        id: 19,
        store: LoreStore { handle_id: 4 },
        repository: Partition::from([0x21u8; 16]),
        branch: Context::from([0x42u8; 16]),
        options: LoreRevisionGraphBranchPushOptions {
            force: 1,
            fast_forward_merge: 1,
        },
    };

    match relayed(LoreCommand::RevisionGraphBranchPush(args.clone())) {
        LoreCommand::RevisionGraphBranchPush(read_back) => {
            assert_eq!(read_back, args, "must carry every field unchanged");
        }
        other => panic!("Unexpected command: {other:?}"),
    }
}

/// The revision graph commands follow the commands that preceded them. Bitcode names a variant by
/// its position, in the first byte of the encoding, so a command added ahead of another shifts the
/// variant the other decodes as: the last of the earlier commands keeps the bytes it encodes as
/// without the revision graph commands, the revision graph commands take the positions after it in
/// the order they landed, and each reads back as itself, the branch metadata set with a value of
/// every type in `revision_graph_branch_metadata_set_survives_the_wire`.
#[test]
fn revision_graph_commands_follow_the_last_existing_command() {
    use lore::repository::LoreRepositoryInstanceCloneArgs;
    use lore::revision_graph::LoreRevisionIdentifier;
    use lore::revision_graph::branch_list::LoreRevisionGraphBranchListArgs;
    use lore::revision_graph::branch_metadata_get::LoreRevisionGraphBranchMetadataGetArgs;
    use lore::revision_graph::branch_resolve::LoreRevisionGraphBranchResolveArgs;
    use lore::revision_graph::revision_resolve::LoreRevisionGraphRevisionResolveArgs;
    use lore::storage::handle::LoreStore;

    const LAST_EXISTING_COMMAND: [u8; 10] = [161, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    assert_eq!(
        bitcode::encode(&LoreCommand::RepositoryInstanceClone(
            LoreRepositoryInstanceCloneArgs {
                target_path: LoreString::default(),
                allow_copy: 0,
            }
        )),
        LAST_EXISTING_COMMAND,
        "the last command before the revision graph commands must keep its encoding"
    );
    for (offset, command) in [
        (1, LoreCommand::RevisionGraphBranchList(Default::default())),
        (
            2,
            LoreCommand::RevisionGraphRevisionResolve(Default::default()),
        ),
        (
            3,
            LoreCommand::RevisionGraphBranchResolve(Default::default()),
        ),
        (
            4,
            LoreCommand::RevisionGraphBranchMetadataGet(Default::default()),
        ),
        (
            5,
            LoreCommand::RevisionGraphBranchMetadataSet(Default::default()),
        ),
        (
            6,
            LoreCommand::RevisionGraphBranchCreate(Default::default()),
        ),
        (
            7,
            LoreCommand::RevisionGraphBranchArchive(Default::default()),
        ),
        (8, LoreCommand::RevisionGraphBranchSync(Default::default())),
        (11, LoreCommand::RevisionGraphBranchPush(Default::default())),
    ] {
        assert_eq!(
            bitcode::encode(&command)[0],
            LAST_EXISTING_COMMAND[0] + offset,
            "{command:?} must take position {offset} after the last earlier command"
        );
    }

    let revision_resolve = LoreRevisionGraphRevisionResolveArgs {
        id: 11,
        store: LoreStore { handle_id: 4 },
        repository: Partition::from([0x21u8; 16]),
        revision: LoreRevisionIdentifier {
            hash: Hash::default(),
            branch: Context::from([0x42u8; 16]),
            revision_number: 25,
        },
    };
    match relayed(LoreCommand::RevisionGraphRevisionResolve(
        revision_resolve.clone(),
    )) {
        LoreCommand::RevisionGraphRevisionResolve(read_back) => {
            assert_eq!(
                read_back, revision_resolve,
                "must carry every field unchanged"
            );
        }
        other => panic!("Unexpected command: {other:?}"),
    }

    let branch_resolve = LoreRevisionGraphBranchResolveArgs {
        id: 12,
        store: LoreStore { handle_id: 4 },
        repository: Partition::from([0x21u8; 16]),
        name: LoreString::from_str("release/5.4"),
    };
    match relayed(LoreCommand::RevisionGraphBranchResolve(
        branch_resolve.clone(),
    )) {
        LoreCommand::RevisionGraphBranchResolve(read_back) => {
            assert_eq!(
                read_back, branch_resolve,
                "must carry every field unchanged"
            );
        }
        other => panic!("Unexpected command: {other:?}"),
    }

    let branch_list = LoreRevisionGraphBranchListArgs {
        id: 13,
        ..Default::default()
    };
    match relayed(LoreCommand::RevisionGraphBranchList(branch_list.clone())) {
        LoreCommand::RevisionGraphBranchList(read_back) => {
            assert_eq!(read_back, branch_list, "must carry every field unchanged");
        }
        other => panic!("Unexpected command: {other:?}"),
    }

    let branch_metadata_get = LoreRevisionGraphBranchMetadataGetArgs {
        id: 15,
        store: LoreStore { handle_id: 4 },
        repository: Partition::from([0x21u8; 16]),
        branch: Context::from([0x42u8; 16]),
        key: LoreString::from_str("protect"),
    };
    match relayed(LoreCommand::RevisionGraphBranchMetadataGet(
        branch_metadata_get.clone(),
    )) {
        LoreCommand::RevisionGraphBranchMetadataGet(read_back) => {
            assert_eq!(
                read_back, branch_metadata_get,
                "must carry every field unchanged"
            );
        }
        other => panic!("Unexpected command: {other:?}"),
    }
}

/// The command that moves the external root follows the last command before it, so no earlier
/// command's encoding moves.
#[test]
fn set_external_directory_follows_the_last_existing_command() {
    let last = bitcode::encode(&LoreCommand::BranchMergeConflictsList(
        lore::branch::LoreBranchMergeConflictsListArgs {},
    ))[0];
    let command = LoreCommand::ServiceSetExternalDirectory(
        lore::service::LoreServiceSetExternalDirectoryArgs {
            path: LoreString::from_str("/roots/next"),
            adopt: 1,
            abandon: 0,
        },
    );
    assert_eq!(bitcode::encode(&command)[0], last + 1);
    match relayed(command) {
        LoreCommand::ServiceSetExternalDirectory(read_back) => {
            assert_eq!(read_back.path.as_str(), "/roots/next");
            assert_eq!((read_back.adopt, read_back.abandon), (1, 0));
        }
        other => panic!("Unexpected command: {other:?}"),
    }
}

/// The move reply is the variant after `ApiResult`. A client's decoder from before it read the
/// existing variants from the same bytes, and refuses the move reply rather than reading it as
/// another message.
#[test]
fn the_move_reply_follows_the_api_result() {
    /// `MessageToClient` before the move reply.
    #[derive(bitcode::Encode, bitcode::Decode)]
    #[allow(clippy::large_enum_variant)]
    enum OlderMessageToClient {
        Event(LoreEvent),
        ApiResult(i32),
    }

    let event = LoreEvent::Log(lore_revision::relay::EventDispatcher::make_log(
        lore_base::log::LoreLogLevel::Info,
        "an event".to_string(),
    ));
    assert_eq!(
        encode_message(&MessageToClient::Event(&event)),
        encode_message(&OlderMessageToClient::Event(event.clone())),
        "an event keeps its encoding"
    );
    assert_eq!(
        encode_message(&MessageToClient::<LoreEvent>::ApiResult(-7)),
        encode_message(&OlderMessageToClient::ApiResult(-7)),
        "a result keeps its encoding"
    );

    let reply = encode_message(&MessageToClient::<LoreEvent>::ExternalRootMove);
    assert_eq!(
        reply,
        [2],
        "the move reply takes the position after the result"
    );
    assert!(matches!(
        bitcode::decode::<MessageToClient>(&reply),
        Ok(MessageToClient::ExternalRootMove)
    ));
    assert!(
        bitcode::decode::<OlderMessageToClient>(&reply).is_err(),
        "a decoder from before the move reply refuses it"
    );
}
