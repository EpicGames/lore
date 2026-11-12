// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use std::sync::Arc;

use lore_revision::filter::FilterStates;
use lore_revision::fs::filesystem_provider::InstanceOperation as _;
use lore_revision::stage::KnownChild;
use lore_revision::stage::StageOptions;
use lore_revision::stage::stage_node_from_metadata;
use lore_revision::state::NodeMapping;
use lore_revision::util::path::RelativePath;

mod content_tests;
mod directory_children_tests;
mod move_tests;
mod parent_restore_tests;
mod resolution_tests;
pub(crate) mod sweep_tests;
mod undelete_tests;

/// Stages `name`, a child of the root held by node `found`, from the working tree.
pub(super) async fn stage_root_child(
    repository: &Arc<lore_revision::repository::RepositoryContext>,
    state: &Arc<lore_revision::state::State>,
    name: &str,
    found: lore_revision::node::NodeID,
    options: StageOptions,
) {
    let operation = repository
        .file_system()
        .begin_operation()
        .await
        .expect("a filesystem operation");
    let info = operation
        .file_info(&RelativePath::new().join(name))
        .await
        .expect("the file information");
    stage_node_from_metadata(
        &operation,
        NodeMapping::root(repository.clone(), state.clone()),
        name.to_string(),
        info,
        options,
        Arc::default(),
        None,
        KnownChild::Resolved(Some(found)),
        FilterStates::ROOT,
    )
    .await
    .expect("the stage");
    operation.finalize().await.expect("the operation finishes");
}
