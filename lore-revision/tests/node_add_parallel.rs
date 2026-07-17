// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_base::error::NoRemote;
    use lore_base::lore_spawn;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Context;
    use lore_revision::node::*;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryFormat;
    use lore_revision::state::State;
    use lore_storage::hash::hash_string;
    use lore_storage::local::immutable_store::LocalImmutableStore;
    use lore_transport::ProtocolError;
    use tokio::task::JoinSet;

    include!("helper.rs");

    const SIBLING_COUNT: usize = 128;
    const READER_TASKS: usize = 8;
    const READS_PER_TASK: usize = 10_000;

    /// Regression test for the `node_add` publish-before-init race.
    ///
    /// Adds many distinct siblings under one non-root directory concurrently
    /// while readers walk that directory's child chain. Before the reorder fix,
    /// a walker could observe a published-but-zeroed node (`parent == 0`) and
    /// fail `check_parent_link` with `InvalidNodeHierarchy`; the root parent was
    /// picked deliberately to be non-zero so that failure surfaces rather than
    /// the root case's silent early-termination. Afterwards the node is fully
    /// initialized before it is linked in, so every walk succeeds and the final
    /// chain holds exactly the added siblings — one each, no lost update or
    /// duplicate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn node_add_parallel_siblings_are_race_free() {
        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = Context::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();

                let immutable_store = LocalImmutableStore::new(
                    None,
                    lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let write_token =
                    lore_revision::repository::RepositoryWriteToken::acquire(path.as_path()).await;
                let repository = Arc::new(
                    RepositoryContext::new(
                        Some(path.clone()),
                        immutable_store.clone(),
                        mutable_store.clone(),
                        repository_id.into(),
                        lore_revision::instance::InstanceId::default(),
                        Err(ProtocolError::from(NoRemote)),
                        Arc::default(),
                        RepositoryFormat::Lore,
                    )
                    .with_write_token(write_token.share()),
                );

                let state = Arc::new(State::new());

                // Non-root directory parent (flags == 0 is a directory).
                let parent_name = "parent";
                let parent = state
                    .node_add(
                        repository.clone(),
                        ROOT_NODE,
                        Node {
                            name_hash: hash_string(parent_name),
                            ..Default::default()
                        },
                        parent_name,
                    )
                    .await
                    .expect("add parent");

                let names: Vec<String> = (0..SIBLING_COUNT)
                    .map(|i| format!("child-{i:04}"))
                    .collect();
                let done = Arc::new(AtomicBool::new(false));

                // Readers: walk the parent's child chain until the adds finish,
                // collecting any error the walk raises.
                let mut readers: JoinSet<Vec<String>> = JoinSet::new();
                for _ in 0..READER_TASKS {
                    let repo = repository.clone();
                    let state = state.clone();
                    let done = done.clone();
                    lore_spawn!(readers, async move {
                        let mut errors = Vec::new();
                        let mut reads = 0usize;
                        while !done.load(Ordering::Relaxed) && reads < READS_PER_TASK {
                            reads += 1;
                            if let Err(err) = state.node_children(repo.clone(), parent).await {
                                errors.push(format!("{err:?}"));
                            }
                        }
                        errors
                    });
                }

                // Adders: one distinct sibling each, all concurrent.
                let mut adders: JoinSet<()> = JoinSet::new();
                for name in &names {
                    let repo = repository.clone();
                    let state = state.clone();
                    let name = name.clone();
                    lore_spawn!(adders, async move {
                        state
                            .node_add(
                                repo,
                                parent,
                                Node {
                                    name_hash: hash_string(&name),
                                    ..Default::default()
                                },
                                &name,
                            )
                            .await
                            .expect("add child");
                    });
                }

                while let Some(joined) = adders.join_next().await {
                    joined.expect("adder task panicked");
                }
                done.store(true, Ordering::Relaxed);

                let mut all_errors = Vec::new();
                while let Some(joined) = readers.join_next().await {
                    all_errors.extend(joined.expect("reader task panicked"));
                }

                assert!(
                    all_errors.is_empty(),
                    "concurrent child-chain walks must not error; got {} error(s), first: {}",
                    all_errors.len(),
                    all_errors.first().map_or("", String::as_str),
                );

                let children = state
                    .node_children(repository.clone(), parent)
                    .await
                    .expect("final node_children");
                assert_eq!(
                    children.len(),
                    SIBLING_COUNT,
                    "final child count must equal the number of concurrent adds"
                );
                for name in &names {
                    state
                        .find_subnode(repository.clone(), parent, hash_string(name))
                        .await
                        .expect("every concurrently-added sibling must be findable");
                }
            }))
            .await
            .expect("Test task failed");
    }
}
