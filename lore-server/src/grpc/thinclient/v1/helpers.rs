// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Helpers shared by `lore.thin_client.v1` RPC handlers.

use std::sync::Arc;

use bytes::Bytes;
use lore_base::types::Hash;
use lore_proto::lore::model::v1 as model_v1;
use lore_proto::lore::thin_client::v1 as thin_client_v1;
use lore_proto::lore::thin_client::v1::revision_diff_request;
use lore_proto::lore::thin_client::v1::revision_info_request;
use lore_proto::lore::thin_client::v1::revision_tree_request;
use lore_revision::branch;
use lore_revision::change::FileAction;
use lore_revision::change::NodeChange;
use lore_revision::lore::BranchId;
use lore_revision::metadata::Metadata;
use lore_revision::node::NodeFlags;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision;
use lore_revision::revision::ResolveSearchLocation;
use lore_revision::state::State;
use lore_telemetry::tracing::fields::BRANCH_ID;
use lore_telemetry::tracing::fields::METADATA;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use lore_telemetry::tracing::fields::REVISION;
use tonic::Status;
use tracing::debug;
use tracing::warn;

use crate::grpc::FilterSlowDownExt;
use crate::grpc::warn_error_to_status;

/// Normalised form of the per-RPC `oneof` (identifier | signature) used
/// by the thin-client surface. Lets handlers share resolution code
/// regardless of which request type's Query they originated from.
pub(super) enum RevisionSpec {
    Signature(Bytes),
    Identifier(model_v1::RevisionIdentifier),
}

impl From<revision_info_request::Query> for RevisionSpec {
    fn from(query: revision_info_request::Query) -> Self {
        match query {
            revision_info_request::Query::Signature(sig) => Self::Signature(sig),
            revision_info_request::Query::Identifier(id) => Self::Identifier(id),
        }
    }
}

impl From<revision_tree_request::Query> for RevisionSpec {
    fn from(query: revision_tree_request::Query) -> Self {
        match query {
            revision_tree_request::Query::Signature(sig) => Self::Signature(sig),
            revision_tree_request::Query::Identifier(id) => Self::Identifier(id),
        }
    }
}

impl From<revision_diff_request::QueryFrom> for RevisionSpec {
    fn from(query: revision_diff_request::QueryFrom) -> Self {
        match query {
            revision_diff_request::QueryFrom::SignatureFrom(sig) => Self::Signature(sig),
            revision_diff_request::QueryFrom::IdentifierFrom(id) => Self::Identifier(id),
        }
    }
}

impl From<revision_diff_request::QueryTo> for RevisionSpec {
    fn from(query: revision_diff_request::QueryTo) -> Self {
        match query {
            revision_diff_request::QueryTo::SignatureTo(sig) => Self::Signature(sig),
            revision_diff_request::QueryTo::IdentifierTo(id) => Self::Identifier(id),
        }
    }
}

/// Resolves a `RevisionSpec` to a concrete revision `Hash`.
///
/// Signature queries pass through; identifier queries with `number == 0`
/// resolve to the branch's latest revision via `branch::load_latest`;
/// non-zero numbers resolve via `revision::resolve("branch@N")`. The
/// `is_not_found` / non-not-found split routes user-input misses to
/// `Status::not_found` (quiet) and server-side faults to
/// `Status::internal` (with structured warn).
pub(super) async fn resolve_signature(
    repository: &Arc<RepositoryContext>,
    spec: RevisionSpec,
) -> Result<Hash, Status> {
    match spec {
        RevisionSpec::Signature(signature) => Ok(Hash::from(signature)),
        RevisionSpec::Identifier(identifier) => {
            let branch_id = BranchId::from(&identifier.branch_id);
            if identifier.number == 0 {
                debug!({BRANCH_ID} = %branch_id, "Resolving branch latest");
                branch::load_latest(repository.clone(), branch_id)
                    .await
                    .map_err(|err| {
                        if err.is_branch_not_found() {
                            Status::not_found(format!("Branch {branch_id} not found"))
                        } else {
                            warn!(
                                {REPOSITORY_ID} = %repository.id, {BRANCH_ID} = %branch_id, ?err,
                                "Failed to load branch latest revision",
                            );
                            warn_error_to_status(&err, |e| Status::internal(e.to_string()))
                        }
                    })
            } else {
                let signature = format!("{branch_id}@{}", identifier.number);
                revision::resolve(
                    repository.clone(),
                    signature,
                    None,
                    ResolveSearchLocation::Local,
                )
                .await
                .map_err(|err| {
                    if err.is_not_found() || err.is_revision_not_found() {
                        Status::not_found(format!(
                            "Revision {branch_id}@{} not found",
                            identifier.number
                        ))
                    } else {
                        warn!(
                            {REPOSITORY_ID} = %repository.id,
                            {BRANCH_ID} = %branch_id,
                            number = identifier.number,
                            ?err,
                            "Failed to resolve revision identifier",
                        );
                        warn_error_to_status(&err, |e| Status::internal(e.to_string()))
                    }
                })
            }
        }
    }
}

/// Resolves a `RevisionSpec` to `(signature, identifier)` by walking
/// the revision's `State` + `Metadata`. The returned identifier carries
/// the per-branch revision number and the branch id derived from the
/// metadata blob, so signature-only queries can echo the resolved
/// `(branch, number)` back in their response header.
pub(super) async fn resolve_to_identifier(
    repository: &Arc<RepositoryContext>,
    spec: RevisionSpec,
) -> Result<(Hash, model_v1::RevisionIdentifier), Status> {
    let signature = resolve_signature(repository, spec).await?;
    debug!({REVISION} = %signature, "Loaded resolved signature");
    let identifier = identifier_for_signature(repository, signature).await?;
    Ok((signature, identifier))
}

/// Walks the `State` + `Metadata` for a known-resolved `signature` and
/// returns the `(branch, number)` identifier. Use when you already have
/// a `Hash` in hand (e.g. for a 3-way diff base) and want to fill in
/// its proto identifier for a response header.
pub(super) async fn identifier_for_signature(
    repository: &Arc<RepositoryContext>,
    signature: Hash,
) -> Result<model_v1::RevisionIdentifier, Status> {
    if signature.is_zero() {
        return Ok(model_v1::RevisionIdentifier {
            branch_id: BranchId::default().into(),
            number: 0,
        });
    }

    let state = State::deserialize(repository.clone(), signature)
        .await
        .filter_slow_down()?
        .map_err(|err| {
            if err.is_not_found() {
                Status::not_found(format!("Revision {signature} not found"))
            } else {
                warn!(
                    {REPOSITORY_ID} = %repository.id, {REVISION} = %signature, ?err,
                    "Failed to deserialize revision state",
                );
                warn_error_to_status(&err, |e| Status::internal(e.to_string()))
            }
        })?;
    let metadata_hash = state.metadata_hash();
    let metadata = Metadata::deserialize(repository.clone(), metadata_hash)
        .await
        .map_err(|err| {
            warn!(
                {REPOSITORY_ID} = %repository.id,
                {REVISION} = %signature,
                {METADATA} = %metadata_hash,
                ?err,
                "Failed to deserialize revision metadata",
            );
            warn_error_to_status(&err, |e| Status::internal(e.to_string()))
        })?;
    let branch_id = metadata.get_branch().map_err(|err| {
        warn!(
            {REPOSITORY_ID} = %repository.id,
            {REVISION} = %signature,
            {METADATA} = %metadata_hash,
            ?err,
            "Revision metadata missing branch field",
        );
        warn_error_to_status(&err, |e| Status::internal(e.to_string()))
    })?;

    Ok(model_v1::RevisionIdentifier {
        branch_id: branch_id.into(),
        number: state.revision_number(),
    })
}

/// Maps internal `NodeFlags` to the v1 `NodeType` enum used by
/// `TreeNode` and `DiffChange`.
pub(super) fn node_flags_to_node_type(flags: NodeFlags) -> thin_client_v1::NodeType {
    if flags.contains(NodeFlags::File) {
        thin_client_v1::NodeType::File
    } else if flags.contains(NodeFlags::Link) {
        thin_client_v1::NodeType::Link
    } else {
        thin_client_v1::NodeType::Directory
    }
}

/// Maps internal `FileAction` to the v1 `Action` enum.
fn file_action_to_v1_action(action: FileAction) -> thin_client_v1::Action {
    match action {
        FileAction::Keep => thin_client_v1::Action::Keep,
        FileAction::Add => thin_client_v1::Action::Add,
        FileAction::Delete => thin_client_v1::Action::Delete,
        FileAction::Move => thin_client_v1::Action::Move,
        FileAction::Copy => thin_client_v1::Action::Copy,
    }
}

/// Convert an internal `NodeChange` into a v1 `DiffChange`. The
/// `to.flags` drive `node_type` for non-delete actions; for deletes
/// `from.flags` is the surviving record of what the path used to be.
/// `content_from` / `content_to` carry the from / to side's CAS hash,
/// or empty bytes for ADD (no from) and DELETE (no to).
///
/// `link_repository_index` is passed through verbatim; the handler
/// resolves it, since the per-stream partition table lives there.
pub(super) fn node_change_to_diff_change(
    change: &NodeChange,
    link_repository_index: u32,
) -> thin_client_v1::DiffChange {
    let action = file_action_to_v1_action(change.action);
    let node_type = match action {
        thin_client_v1::Action::Delete => node_flags_to_node_type(change.from.flags),
        _ => node_flags_to_node_type(change.to.flags),
    };
    let path_from = change
        .from_path
        .as_ref()
        .map(|p| p.to_string())
        .unwrap_or_default();
    let content_from = if action == thin_client_v1::Action::Add {
        Bytes::new()
    } else {
        change.from.address.hash.into()
    };
    let content_to = if action == thin_client_v1::Action::Delete {
        Bytes::new()
    } else {
        change.to.address.hash.into()
    };
    thin_client_v1::DiffChange {
        path: change.path.to_string(),
        path_from,
        action: action as i32,
        node_type: node_type as i32,
        content_from,
        content_to,
        automerged: change.flags.is_conflict_automerged(),
        link_repository_index,
    }
}

/// Convert a 3-way merge conflict pair `(base→from, base→to)` into a
/// v1 `DiffConflict`. The pair's `from.address` on both halves is the
/// common-ancestor content for that path, so `change_from.content_from
/// == change_to.content_from` per the proto contract. The two halves
/// take separate indices: they can land in different partitions.
pub(super) fn diff_conflict_from_pair(
    pair: &(NodeChange, NodeChange),
    link_repository_index_from: u32,
    link_repository_index_to: u32,
) -> thin_client_v1::DiffConflict {
    thin_client_v1::DiffConflict {
        change_from: Some(node_change_to_diff_change(
            &pair.0,
            link_repository_index_from,
        )),
        change_to: Some(node_change_to_diff_change(
            &pair.1,
            link_repository_index_to,
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::str::FromStr;

    use lore_proto::lore::thin_client::v1 as thin_client_v1;
    use lore_revision::change::Flags;
    use lore_revision::change::NodeChange;
    use lore_revision::change::NodeChangeState;
    use lore_revision::lore::RepositoryId;
    use lore_revision::node::NodeFlags;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryFormat;
    use lore_revision::state;
    use lore_revision::util::path::RelativePath;
    use lore_storage::Address;
    use lore_storage::Context;
    use lore_storage::Hash;
    use lore_transport::ProtocolError;

    use super::*;

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
        Arc::new(RepositoryContext::new(
            Some(PathBuf::default()),
            immutable,
            mutable,
            RepositoryId::from(uuid::Uuid::now_v7()),
            lore_revision::instance::InstanceId::generate(),
            Err(ProtocolError::from(lore_base::error::NoRemote)),
            Arc::default(),
            RepositoryFormat::Lore,
        ))
    }

    fn make_change(action: lore_revision::change::FileAction) -> NodeChange {
        let ctx = futures::executor::block_on(test_context());
        let state = Arc::new(state::State::new());
        let address = Address {
            hash: Hash::hash_buffer(&[1, 2, 3]),
            context: Context::default(),
        };
        NodeChange {
            action,
            path: RelativePath::from_str("dir/file.txt").unwrap(),
            from_path: None,
            flags: Flags::None,
            from: NodeChangeState {
                node: 1,
                repository: ctx.clone(),
                state: state.clone(),
                address,
                flags: NodeFlags::File,
            },
            to: NodeChangeState {
                node: 2,
                repository: ctx,
                state,
                address,
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

        let mapped = node_change_to_diff_change(&change, 0);
        assert_eq!(mapped.link_repository_index, 0);
        assert_eq!(mapped.path, "dir/file.txt");
        assert_eq!(mapped.action, thin_client_v1::Action::Add as i32);

        let mapped = node_change_to_diff_change(&change, 7);
        assert_eq!(mapped.link_repository_index, 7);
    }

    /// `node_type` reflects the surviving side: `to.flags` for non-delete
    /// actions, `from.flags` for deletes. The index passed in is opaque
    /// to the helper, but the surviving-side rule still drives `node_type`.
    #[tokio::test]
    async fn node_change_delete_node_type_comes_from_from_side() {
        let mut change = make_change(lore_revision::change::FileAction::Delete);
        change.from.flags = NodeFlags::Link;
        change.to.flags = NodeFlags::NoFlags;

        let mapped = node_change_to_diff_change(&change, 0);
        assert_eq!(mapped.node_type, thin_client_v1::NodeType::Link as i32);
    }

    #[tokio::test]
    async fn diff_conflict_pair_carries_per_half_indices() {
        let from = make_change(lore_revision::change::FileAction::Keep);
        let to = make_change(lore_revision::change::FileAction::Keep);

        let mapped = diff_conflict_from_pair(&(from, to), 0, 3);
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
}
