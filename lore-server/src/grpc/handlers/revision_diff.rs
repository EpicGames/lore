// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::RevisionDiffRequest;
use lore_proto::RevisionDiffResponse;
use lore_revision::change;
use lore_revision::diff;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::util::collect_stream::collect_stream_with_summary;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;
use tracing::warn;

use super::path_diff::link_pin_path_diffs;
use super::path_diff::map_to_path_diff;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_authorization;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::link_read_authorizer;
use crate::util::setup_execution;

#[tracing::instrument(name = "RevisionDiff::handle", skip_all)]
pub async fn handler(
    request: Request<RevisionDiffRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<RevisionDiffResponse>, Status> {
    let repository_id = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let authorization = get_authorization(request.extensions()).ok();
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();
    let revision_from = Hash::from(req.revision_from);
    let revision_to = Hash::from(req.revision_to);

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    debug!(%revision_from, %revision_to,
        "Handling revision diff",
    );

    let repository = Arc::new(
        RepositoryContext::new_server_context(immutable_store, mutable_store, repository_id)
            .with_link_read(link_read_authorizer(authorization)),
    );

    LORE_CONTEXT
        .scope(execution, async move {
            let state_source = State::deserialize(repository.clone(), revision_from)
                .await
                .filter_slow_down()?
                .map_err(|_err| Status::invalid_argument("Invalid from state"))?;
            let state_target = State::deserialize(repository.clone(), revision_to)
                .await
                .filter_slow_down()?
                .map_err(|_err| Status::invalid_argument("Invalid to state"))?;

            let pin_diffs =
                link_pin_path_diffs(&repository, &state_source, &state_target, repository_id)
                    .await?;

            let result = collect_stream_with_summary(|tx| {
                diff::diff_revision_paths(
                    repository.clone(),
                    state_source.clone(),
                    state_target.clone(),
                    None,
                    tx,
                )
            })
            .await;
            match result {
                Ok((_, mut changes)) => {
                    change::sort_by_path(&mut changes);
                    debug!("Found {} changes", changes.len());
                    let mut diffs = Vec::with_capacity(changes.len() + pin_diffs.len());
                    diffs.extend(pin_diffs);
                    for change in &changes {
                        if let Some(diff) = map_to_path_diff(change, repository_id).await {
                            diffs.push(diff);
                        }
                    }
                    Ok(Response::new(RevisionDiffResponse { diffs }))
                }
                Err(err) => {
                    warn!(?err, %revision_from, %revision_to,
                        "Failed to calculate diff",
                    );
                    Err(Status::internal(err.to_string()))
                }
            }
        })
        .await
}

#[cfg(test)]
mod tests {
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_proto::PathType;
    use lore_revision::link;
    use lore_revision::link::LinkFlags;
    use lore_revision::lore::BranchId;
    use lore_revision::lore::RepositoryId;
    use lore_revision::node::Node;
    use lore_revision::node::NodeFlags;
    use lore_revision::node::NodeID;
    use lore_revision::node::ROOT_NODE;
    use lore_revision::state;
    use lore_revision::util::collect_stream::collect_stream_with_summary;
    use lore_storage::hash::hash_string;
    use rand::random;

    use super::*;
    use crate::grpc::get_write_token;
    use crate::store::test_store_create;

    /// Serialize a single-file revision, returning its signature. No branch
    /// push: the diff walk resolves states by hash.
    async fn write_file_revision(
        repository: &Arc<RepositoryContext>,
        name: &str,
        contents: &[u8],
    ) -> Hash {
        let write_token = get_write_token();
        let state = Arc::new(state::State::new());
        state.set_revision_number(1);
        let address = lore_revision::immutable::write(
            repository.clone(),
            lore_storage::Context::default(),
            bytes::Bytes::copy_from_slice(contents),
            lore_storage::WriteOptions::default(),
        )
        .await
        .expect("write file content");
        let node = Node {
            flags: NodeFlags::File.bits(),
            name_hash: hash_string(name),
            address,
            ..Default::default()
        };
        state
            .node_add(repository.clone(), ROOT_NODE, node, name)
            .await
            .expect("add file node");
        state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("serialize linked state")
    }

    /// Serialize a parent revision holding a single link, registered in the
    /// link registry the way a real commit does.
    async fn write_parent_revision_with_link(
        repository: &Arc<RepositoryContext>,
        link_name: &str,
        link_repository: RepositoryId,
        link_revision: Hash,
    ) -> Hash {
        let write_token = get_write_token();
        let state = Arc::new(state::State::new());
        state.set_revision_number(1);
        let link_node = Node {
            flags: NodeFlags::Link.bits(),
            child: ROOT_NODE,
            address: Address {
                hash: link_revision,
                context: link_repository.into(),
            },
            name_hash: hash_string(link_name),
            ..Default::default()
        };
        let node_id: NodeID = state
            .node_add(repository.clone(), ROOT_NODE, link_node, link_name)
            .await
            .expect("add link node");
        state
            .link_add(
                repository.clone(),
                link_repository,
                BranchId::default(),
                link_revision,
                node_id,
                LinkFlags::NoFlags,
            )
            .await
            .expect("register link");
        state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("serialize parent state")
    }

    async fn write_parent_revision_without_link(repository: &Arc<RepositoryContext>) -> Hash {
        write_file_revision(repository, "README.txt", b"baseline\n").await
    }

    async fn compare_pins(
        repository: &Arc<RepositoryContext>,
        from: Hash,
        to: Hash,
    ) -> Vec<link::LinkPinChange> {
        let state_from = State::deserialize(repository.clone(), from)
            .await
            .expect("from state");
        let state_to = State::deserialize(repository.clone(), to)
            .await
            .expect("to state");
        link::diff_link_pins(repository.clone(), &state_from, &state_to)
            .await
            .expect("compare link pins")
    }

    async fn collect_diff(
        repository: Arc<RepositoryContext>,
        from: Hash,
        to: Hash,
    ) -> Vec<lore_revision::change::NodeChange> {
        let state_from = State::deserialize(repository.clone(), from)
            .await
            .expect("deserialize from state");
        let state_to = State::deserialize(repository.clone(), to)
            .await
            .expect("deserialize to state");
        let (_, changes) = collect_stream_with_summary(|tx| {
            diff::diff_revision_paths(repository, state_from, state_to, None, tx)
        })
        .await
        .expect("diff states");
        changes
    }

    /// Over real linked states: a pin move surfaces the linked repository's
    /// file change under the mount path stamped with its partition, and the
    /// registry comparison reports the pin move the walk omits.
    #[tokio::test]
    async fn pin_move_surfaces_linked_content_with_partition() {
        let parent_id = random::<Context>();
        let linked_id = random::<Context>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");

        #[allow(clippy::large_futures)]
        LORE_CONTEXT
            .scope(execution, async move {
                let parent = Arc::new(RepositoryContext::new_server_context(
                    immutable_store,
                    mutable_store,
                    parent_id.into(),
                ));
                let linked = Arc::new(parent.to_server_context(linked_id.into()));

                let link_v1 = write_file_revision(&linked, "a.txt", b"v1\n").await;
                let link_v2 = write_file_revision(&linked, "a.txt", b"v2\n").await;
                assert_ne!(link_v1, link_v2, "linked revisions must differ");

                let parent_v1 =
                    write_parent_revision_with_link(&parent, "linked", linked_id.into(), link_v1)
                        .await;
                let parent_v2 =
                    write_parent_revision_with_link(&parent, "linked", linked_id.into(), link_v2)
                        .await;

                let changes = collect_diff(parent.clone(), parent_v1, parent_v2).await;
                let paths: Vec<String> = changes
                    .iter()
                    .map(|change| change.path.to_string())
                    .collect();
                assert_eq!(
                    paths,
                    vec!["linked/a.txt".to_string()],
                    "the walk reports the linked file under the mount path and \
                     nothing for the link node itself",
                );

                let mapped = map_to_path_diff(&changes[0], parent.id)
                    .await
                    .expect("linked content maps to a diff");
                assert_eq!(
                    mapped.link_partition,
                    bytes::Bytes::from(RepositoryId::from(linked_id)),
                    "content walked out of the linked repository carries that \
                     repository as its partition",
                );

                let state_from = State::deserialize(parent.clone(), parent_v1)
                    .await
                    .expect("from state");
                let state_to = State::deserialize(parent.clone(), parent_v2)
                    .await
                    .expect("to state");
                let pin_changes = link::diff_link_pins(parent.clone(), &state_from, &state_to)
                    .await
                    .expect("compare link pins");
                assert_eq!(pin_changes.len(), 1, "one link moved: {pin_changes:?}");
                let pin_change = &pin_changes[0];
                assert_eq!(pin_change.link_path, "linked");
                assert_eq!(pin_change.revision_from, link_v1);
                assert_eq!(pin_change.revision_to, link_v2);

                let pin_diffs = link_pin_path_diffs(&parent, &state_from, &state_to, parent.id)
                    .await
                    .expect("map pin changes");
                assert_eq!(pin_diffs.len(), 1, "{pin_diffs:?}");
                assert_eq!(
                    pin_diffs[0]
                        .to
                        .as_ref()
                        .expect("moved pin has a to side")
                        .r#type,
                    PathType::Link as i32,
                    "the pin entry is typed as a link",
                );
                assert_eq!(
                    pin_diffs[0].link_partition,
                    bytes::Bytes::from(RepositoryId::from(linked_id)),
                );
            })
            .await;
    }

    /// A caller not authorized for the linked repository must not learn its
    /// paths: the walk stops at the link node instead of descending.
    #[tokio::test]
    async fn unauthorized_link_is_not_descended() {
        let parent_id = random::<Context>();
        let linked_id = random::<Context>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");

        #[allow(clippy::large_futures)]
        LORE_CONTEXT
            .scope(execution, async move {
                let parent = Arc::new(RepositoryContext::new_server_context(
                    immutable_store,
                    mutable_store,
                    parent_id.into(),
                ));
                let linked = Arc::new(parent.to_server_context(linked_id.into()));

                let link_v1 = write_file_revision(&linked, "a.txt", b"v1\n").await;
                let link_v2 = write_file_revision(&linked, "a.txt", b"v2\n").await;
                let parent_v1 =
                    write_parent_revision_with_link(&parent, "linked", linked_id.into(), link_v1)
                        .await;
                let parent_v2 =
                    write_parent_revision_with_link(&parent, "linked", linked_id.into(), link_v2)
                        .await;

                let forbidden = RepositoryId::from(linked_id);
                let restricted = Arc::new(
                    parent
                        .to_server_context(parent_id.into())
                        .with_link_read(Arc::new(move |id| id != forbidden)),
                );

                let changes = collect_diff(restricted.clone(), parent_v1, parent_v2).await;
                let paths: Vec<String> = changes
                    .iter()
                    .map(|change| change.path.to_string())
                    .collect();
                assert_eq!(
                    paths,
                    vec!["linked".to_string()],
                    "an unauthorized link is reported as a single entry for the \
                     mount path, with nothing from inside it",
                );
                assert!(
                    !paths.iter().any(|path| path.contains('/')),
                    "no path from inside the linked repository may leak: {paths:?}",
                );
            })
            .await;
    }

    /// A link added, removed, or left untouched contributes no pin change:
    /// the first two already reach the consumer from the content walk.
    #[tokio::test]
    async fn registry_comparison_classifies_add_remove_and_unchanged() {
        let parent_id = random::<Context>();
        let linked_id = random::<Context>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");

        #[allow(clippy::large_futures)]
        LORE_CONTEXT
            .scope(execution, async move {
                let parent = Arc::new(RepositoryContext::new_server_context(
                    immutable_store,
                    mutable_store,
                    parent_id.into(),
                ));
                let linked = Arc::new(parent.to_server_context(linked_id.into()));

                let link_v1 = write_file_revision(&linked, "a.txt", b"v1\n").await;
                let without_link = write_parent_revision_without_link(&parent).await;
                let with_link =
                    write_parent_revision_with_link(&parent, "linked", linked_id.into(), link_v1)
                        .await;

                let added = compare_pins(&parent, without_link, with_link).await;
                assert!(
                    added.is_empty(),
                    "adding a link is not a pin change: {added:?}"
                );

                let removed = compare_pins(&parent, with_link, without_link).await;
                assert!(
                    removed.is_empty(),
                    "removing a link is not a pin change: {removed:?}",
                );

                let unchanged = compare_pins(&parent, with_link, with_link).await;
                assert!(unchanged.is_empty(), "{unchanged:?}");
            })
            .await;
    }
}
