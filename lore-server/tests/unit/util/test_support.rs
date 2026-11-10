// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use lore_base::types::BranchPoint;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_server::grpc::get_write_token;
use lore_server::grpc::handlers::branch_push;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::store::test_support::GatedLoadStore;

pub fn address_with_random_context(address: lore_storage::Address) -> lore_storage::Address {
    lore_storage::Address {
        context: rand::random::<lore_storage::Context>(),
        hash: address.hash,
    }
}

pub fn setup_test_execution() -> std::sync::Arc<lore_revision::interface::ExecutionContext> {
    std::sync::Arc::new(lore_revision::interface::ExecutionContext::new_client(
        LoreGlobalArgs::default(),
        lore_revision::relay::EventDispatcher::no_dispatch(),
    ))
}

pub fn allow_all_authorizer()
-> std::sync::Arc<dyn lore_server::authnz::repository_authorizer::RepositoryAuthorizer> {
    std::sync::Arc::new(lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer)
}

/// Every push permitted, protected branches included: for tests of `push`
/// itself rather than of who may call it.
pub const PUSH_ANY: lore_server::grpc::handlers::branch_push::PushAccess =
    lore_server::grpc::handlers::branch_push::PushAccess {
        write: true,
        push_protected: true,
    };

/// Creates the default branch with one revision, and `branch`, named `test-name`, off that
/// revision, and answers the revision.
pub async fn create_test_branch(
    repository_context: Arc<RepositoryContext>,
    branch: BranchId,
) -> Hash {
    let write_token = get_write_token();
    let main = branch::create(
        repository_context.clone(),
        &write_token,
        BranchId::from(uuid::Uuid::now_v7()),
        branch::DEFAULT_DEFAULT_NAME,
        branch::default_category(),
        "test-creator",
        1,
        vec![],
        false,
        false,
    )
    .await
    .expect("Could not create main branch");

    let state = lore_revision::state::State::new();
    state.set_parent_self(Hash::default());
    state.set_revision_number(1);
    let state_hash = state
        .serialize(repository_context.clone(), &write_token)
        .await
        .expect("Failed to serialize state");

    let latest = branch_push::push(
        repository_context.clone(),
        main,
        state_hash,
        PUSH_ANY,
        true,
        false,
        branch::DEFAULT_HISTORY_STEP_SIZE,
        lore_server::grpc::server::RevisionListAcceleration::default(),
        &mut branch_push::PushReport::new(branch_push::PushHandler::Legacy, Default::default()),
    )
    .await
    .expect("Failed to push latest revision")
    .revision;

    branch::create(
        repository_context.clone(),
        &write_token,
        branch,
        "test-name",
        branch::personal_category(),
        "BranchCreator",
        12345,
        vec![BranchPoint {
            branch: main,
            revision: latest,
        }],
        false,
        false,
    )
    .await
    .expect("Could not create test branch");

    latest
}

/// Runs a create that restores `branch_id` under `name` until it has claimed the name, and
/// stops it there.
pub async fn stop_restore_after_claim(
    repository_context: &Arc<RepositoryContext>,
    branch_id: BranchId,
    name: &'static str,
) {
    let (reached, mut steps) = tokio::sync::mpsc::unbounded_channel();
    let resume = Arc::new(tokio::sync::Notify::new());
    let mut task = tokio::task::JoinSet::new();
    let restore = {
        let repository_context = repository_context.clone();
        async move {
            branch::create(
                repository_context,
                &get_write_token(),
                branch_id,
                name,
                branch::personal_category(),
                "BranchCreator",
                12345,
                vec![],
                false,
                false,
            )
            .await
        }
    };
    lore_base::lore_spawn!(
        task,
        lore_revision::test_util::step_branch_writes(reached, resume.clone(), restore)
    );
    loop {
        match steps.recv().await {
            Some(lore_revision::test_util::BranchStep::RestoreNameClaimed) => break,
            Some(_) => resume.notify_one(),
            None => panic!("the restore ended before its claim"),
        }
    }
    drop(task);
}

/// A store over `mutable_store` that lets the first `passing` loads of `branch`'s metadata key in
/// `repository` through, and holds the loads after them until it opens.
pub fn gate_metadata_loads(
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    repository: RepositoryId,
    branch: BranchId,
    passing: usize,
) -> Arc<GatedLoadStore> {
    let (metadata_key, _key_type) = branch::mutable_key(
        lore_revision::repository::SALT_LORE,
        branch::METADATA,
        repository,
        branch,
    );
    GatedLoadStore::passing_first(mutable_store, [metadata_key], passing)
}

/// Polls `request` until a load `gated` holds is in flight, then drops it, as a request whose
/// deadline passes or whose client leaves is dropped, and opens the gate.
pub async fn drop_at_gated_load<T: std::fmt::Debug>(
    request: impl Future<Output = T>,
    gated: &GatedLoadStore,
) {
    let mut request = Box::pin(request);
    tokio::select! {
        ended = request.as_mut() => panic!("the request ended before its read: {ended:?}"),
        () = gated.wait_for_in_flight(|in_flight| *in_flight == 1) => {}
    }
    drop(request);
    gated.open();
}

/// Waits up to 10 seconds for the next message on `received`, and panics with `expected` unless it
/// is `message`.
pub async fn wait_for_message<T: PartialEq + std::fmt::Debug>(
    received: &mut UnboundedReceiver<T>,
    message: T,
    expected: &str,
) {
    let next = tokio::time::timeout(Duration::from_secs(10), received.recv()).await;
    assert_eq!(next.ok().flatten(), Some(message), "{expected}");
}
