// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;
use std::sync::Arc;

use lore_base::types::Hash;
use lore_proto::lore::model::v1 as model_v1;
use lore_proto::lore::thin_client::v1 as thin_client_v1;
use lore_revision::change::Flags;
use lore_revision::change::NodeChange;
use lore_revision::change::NodeChangeState;
use lore_revision::link::LinkPinChange;
use lore_revision::node::NodeFlags;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryContextCreationArgs;
use lore_revision::state;
use lore_revision::util::path::RelativePath;
use lore_server::grpc::thinclient::v1::helpers::*;
use lore_storage::Address;
use lore_storage::Context;
use lore_transport::ProtocolError;

async fn test_context() -> Arc<RepositoryContext> {
    let immutable = lore_storage::local::immutable_store::LocalImmutableStore::new(
        None,
        lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
    )
    .await
    .expect("immutable store");
    let mutable = Arc::new(
        lore_storage::local::mutable_store::LocalMutableStore::new(
            None::<&std::path::Path>,
            lore_storage::MutableStoreSettings::default(),
            immutable.clone(),
        )
        .await
        .expect("mutable store"),
    );
    Arc::new(RepositoryContext::new(RepositoryContextCreationArgs {
        paths: None,
        immutable_store: immutable,
        mutable_store: mutable,
        id: Context::from(uuid::Uuid::now_v7()).into(),
        instance_id: lore_revision::instance::InstanceId::generate(),
        remote: Err(ProtocolError::from(lore_base::error::NoRemote)),
        filter: Arc::default(),
        filesystem_provider: None,
    }))
}

/// The contexts are deliberately non-zero and the sides deliberately
/// differ: a hash-only projection compares equal on `hash`, and one that
/// reads a single side twice compares equal on both.
fn side_addresses() -> (Address, Address) {
    (
        Address {
            hash: Hash::hash_buffer(&[1, 2, 3]),
            context: Context::from([7u8; 16]),
        },
        Address {
            hash: Hash::hash_buffer(&[4, 5, 6]),
            context: Context::from([9u8; 16]),
        },
    )
}

fn make_change(action: lore_revision::change::FileAction) -> NodeChange {
    let ctx = futures::executor::block_on(test_context());
    let state = state::State::new();
    let (address_from, address_to) = side_addresses();
    NodeChange {
        action,
        flags: Flags::None,
        from: NodeChangeState {
            mapping: lore_revision::state::NodeMapping {
                path: RelativePath::from_str("dir/file.txt").unwrap(),
                node: 1,
                repository: ctx.clone(),
                state: state.clone(),
            },
            address: address_from,
            observed: None,
            mode: 0,
            flags: NodeFlags::File,
        },
        to: NodeChangeState {
            mapping: lore_revision::state::NodeMapping {
                path: RelativePath::from_str("dir/file.txt").unwrap(),
                node: 2,
                repository: ctx,
                state,
            },
            address: address_to,
            observed: None,
            mode: 0,
            flags: NodeFlags::File,
        },
    }
}

/// `node_change_to_diff_change` is a pure projection — it copies the
/// caller-supplied `link_repository_index` into the wire message
/// unchanged. Partition-id → index resolution lives in the handler
/// (`PartitionTable`); this helper does not look at `repository.id`.
#[tokio::test]
async fn node_change_propagates_index_as_given() {
    let change = make_change(lore_revision::change::FileAction::Add);

    let mapped = node_change_to_diff_change(&change, 0).await;
    assert_eq!(mapped.link_repository_index, 0);
    assert_eq!(mapped.path, "dir/file.txt");
    assert_eq!(mapped.action, thin_client_v1::Action::Add as i32);

    let mapped = node_change_to_diff_change(&change, 7).await;
    assert_eq!(mapped.link_repository_index, 7);
}

/// A consumer keys its content and metadata lookups on the whole
/// `(hash, context)` pair, not the hash alone.
#[tokio::test]
async fn node_change_carries_whole_address_on_every_side_it_reports() {
    use lore_revision::change::FileAction;

    let (address_from, address_to) = side_addresses();
    let from = Some(model_v1::Address::from(address_from));
    let to = Some(model_v1::Address::from(address_to));
    for (action, expected_from, expected_to) in [
        (FileAction::Keep, from.clone(), to.clone()),
        (FileAction::Add, None, to.clone()),
        (FileAction::Delete, from.clone(), None),
        (FileAction::Move, from.clone(), to.clone()),
    ] {
        let mapped = node_change_to_diff_change(&make_change(action), 0).await;

        assert_eq!(mapped.content_from, expected_from, "{action:?} from side");
        assert_eq!(mapped.content_to, expected_to, "{action:?} to side");
    }
}

/// A directory's hash is over its children, so a consumer can tell from
/// it whether the directory holds the same entries as before.
#[tokio::test]
async fn node_change_on_directory_reports_its_hash() {
    let (address_from, address_to) = side_addresses();
    let mut change = make_change(lore_revision::change::FileAction::Keep);
    change.from.flags = NodeFlags::NoFlags;
    change.to.flags = NodeFlags::NoFlags;

    let mapped = node_change_to_diff_change(&change, 0).await;

    assert_eq!(mapped.node_type, thin_client_v1::NodeType::Directory as i32);
    assert_eq!(
        mapped.content_from,
        Some(model_v1::Address::from(address_from))
    );
    assert_eq!(mapped.content_to, Some(model_v1::Address::from(address_to)));
}

/// An empty file has no fragment, so its hash stays zero while the
/// context a metadata lookup keys on is still its own.
#[tokio::test]
async fn node_change_on_empty_file_still_carries_its_context() {
    let empty = Address {
        hash: Hash::default(),
        context: side_addresses().0.context,
    };
    let mut change = make_change(lore_revision::change::FileAction::Keep);
    change.from.address = empty;
    change.to.address = empty;

    let mapped = node_change_to_diff_change(&change, 0).await;

    let expected = Some(model_v1::Address::from(empty));
    assert_eq!(mapped.content_from, expected);
    assert_eq!(mapped.content_to, expected);
}

/// Tracking is a link property: a file change reports it as false.
#[tokio::test]
async fn node_change_on_file_is_not_tracking() {
    let change = make_change(lore_revision::change::FileAction::Add);

    let mapped = node_change_to_diff_change(&change, 0).await;
    assert!(!mapped.tracking);
}

/// `node_type` reflects the surviving side: `to.flags` for non-delete
/// actions, `from.flags` for deletes. The index passed in is opaque
/// to the helper, but the surviving-side rule still drives `node_type`.
#[tokio::test]
async fn node_change_delete_node_type_comes_from_from_side() {
    let mut change = make_change(lore_revision::change::FileAction::Delete);
    change.from.flags = NodeFlags::Link;
    change.to.flags = NodeFlags::NoFlags;

    let mapped = node_change_to_diff_change(&change, 0).await;
    assert_eq!(mapped.node_type, thin_client_v1::NodeType::Link as i32);
}

#[tokio::test]
async fn diff_conflict_pair_carries_per_half_indices() {
    let from = make_change(lore_revision::change::FileAction::Keep);
    let to = make_change(lore_revision::change::FileAction::Keep);

    let mapped = diff_conflict_from_pair(&(from, to), 0, 3).await;
    assert_eq!(
        mapped.change_from.as_ref().unwrap().link_repository_index,
        0,
        "from-half carries its own index",
    );
    assert_eq!(
        mapped.change_to.as_ref().unwrap().link_repository_index,
        3,
        "to-half carries its own index, distinct from from-half",
    );
}

fn make_pin_change() -> LinkPinChange {
    LinkPinChange {
        link_path: "libs/shared".to_string(),
        link_repository: lore_base::types::RepositoryId::from(uuid::Uuid::now_v7()),
        revision_from: Hash::hash_buffer(&[1, 2, 3]),
        revision_to: Hash::hash_buffer(&[4, 5, 6]),
        tracking_from: false,
        tracking_to: false,
    }
}

/// A moved pin carries both revisions as the link's content addresses,
/// each resolving under the linked repository rather than the parent.
#[test]
fn pin_change_carries_both_revisions_under_the_linked_repository() {
    let change = make_pin_change();
    let mapped = link_pin_change_to_diff_change(&change, 2);

    assert_eq!(mapped.path, "libs/shared");
    assert!(mapped.path_from.is_empty());
    assert_eq!(mapped.action, thin_client_v1::Action::Keep as i32);
    assert_eq!(mapped.node_type, thin_client_v1::NodeType::Link as i32);
    let linked_context = Context::from(change.link_repository);
    assert_eq!(
        mapped.content_from,
        Some(model_v1::Address::from(Address {
            hash: change.revision_from,
            context: linked_context,
        })),
    );
    assert_eq!(
        mapped.content_to,
        Some(model_v1::Address::from(Address {
            hash: change.revision_to,
            context: linked_context,
        })),
    );
    assert_eq!(mapped.link_repository_index, 2);
    assert!(!mapped.automerged);
}

/// `tracking` describes the entry the change resolves to, so it mirrors
/// the pin's "to" side.
#[test]
fn pin_change_tracking_mirrors_to_side() {
    let mut change = make_pin_change();

    change.tracking_from = true;
    change.tracking_to = false;
    assert!(!link_pin_change_to_diff_change(&change, 0).tracking);

    change.tracking_from = false;
    change.tracking_to = true;
    assert!(link_pin_change_to_diff_change(&change, 0).tracking);
}

/// `revision_record` takes the commit message, timestamp and committer from the metadata, leaves a
/// text field that is not valid UTF-8 empty and a timestamp of another size zero, and passes every
/// other entry on.
#[test]
fn revision_record_reads_the_common_fields() {
    use lore_revision::lore::BranchId;
    use lore_revision::metadata;
    use lore_revision::metadata::Metadata;

    let branch = BranchId::from(uuid::Uuid::now_v7());
    let mut stored = Metadata::new();
    stored
        .set_binary(metadata::MESSAGE, b"\xffnot text")
        .expect("message");
    stored.set_timestamp(1234).expect("timestamp");
    stored
        .set_string(metadata::COMMITTED_BY, "committer@example.com")
        .expect("committer");
    stored.set_string("custom", "value").expect("custom entry");
    let mut other = Vec::new();
    let revision = revision_record(Hash::from_u64(7), 3, branch, &stored, |key, _, _| {
        other.push(key.to_string());
    });
    assert_eq!(revision.signature, bytes::Bytes::from(Hash::from_u64(7)));
    assert_eq!(
        revision.identifier,
        Some(model_v1::RevisionIdentifier {
            branch_id: branch.into(),
            number: 3,
        })
    );
    assert_eq!(revision.number, 3);
    assert_eq!(revision.commit_message, "");
    assert_eq!(revision.timestamp, 1234);
    assert_eq!(revision.committed_by, "committer@example.com");
    assert_eq!(other, ["custom"]);

    let mut stored = Metadata::new();
    stored
        .set_string(metadata::MESSAGE, "text")
        .expect("message");
    stored
        .set_binary(metadata::TIMESTAMP, &[1, 2, 3])
        .expect("timestamp");
    stored
        .set_binary(metadata::COMMITTED_BY, b"\xfe")
        .expect("committer");
    let revision = revision_record(Hash::from_u64(7), 3, branch, &stored, |_, _, _| {});
    assert_eq!(revision.commit_message, "text");
    assert_eq!(revision.timestamp, 0);
    assert_eq!(revision.committed_by, "");
}
