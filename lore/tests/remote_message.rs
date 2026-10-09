// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use bytes::Bytes;
use lore::remote::command::LoreCommand;
use lore::remote::message::Header;
use lore::remote::message::MessageToClient;
use lore::remote::message::MessageToServer;
use lore::remote::message::SerializationType;
use lore::remote::message::blocking_read_message;
use lore::remote::message::encode_message;
use lore::remote::message::write_message;
use lore::remote::message::write_payload;
use lore::repository::LoreRepositoryDeleteArgs;
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
use lore_base::env::CallEnvironment;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::event::LoreBytes;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreString;

/// `message` as a peer reads it back from the bytes written for it, beside the payload it views.
fn read_back<Message: bitcode::Encode + bitcode::DecodeOwned>(
    message: &Message,
) -> (Message, Bytes) {
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
    read_back(&message).0.command
}

#[test]
fn header_to_and_from_bytes() {
    let header = Header::new(0xffeeddcc, SerializationType::Bitcode);

    let bytes = header.to_bytes();
    let processed_header = Header::from_bytes(&bytes);

    assert!(processed_header.is_ok());
    assert_eq!(processed_header.unwrap().payload_size, header.payload_size);

    let mut bad_bytes = bytes;
    bad_bytes[4] = 0xff;
    let bad_processed_header = Header::from_bytes(&bad_bytes);

    assert!(bad_processed_header.is_err());
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
        values: [Some("/tmp/global".to_string()), None],
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

    let (read, _payload) = read_back(&message);

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

    assert_eq!(read_back(&message).0.globals.repository_path, path);
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

    let status = Box::pin(read_back(&message).0.invoke(None)).await;

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

    let (read, payload) = read_back(&message);

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

    let mut written = Vec::new();
    write_payload(
        &mut written,
        &encode_message(&MessageToClient::Event(&event)),
    )
    .unwrap();
    let (read, _payload): (MessageToClient, Bytes) = blocking_read_message(&mut written.as_slice())
        .expect("an event must read back")
        .expect("a whole message must be present");

    match read {
        MessageToClient::Event(LoreEvent::StorageGetData(data)) => {
            assert_eq!((data.id, data.offset), (3, 64));
            // SAFETY: the payload is alive, and the view points into it.
            assert_eq!(unsafe { data.bytes.as_slice() }, contents);
        }
        _ => panic!("expected a data event"),
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
        options: LoreRevisionTreeCommitOptions { remote_write: 1 },
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

    match read_back(&MessageToClient::Event(event)).0 {
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
