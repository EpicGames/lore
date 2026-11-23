// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_base::types::TypedBytes as _;
use lore_revision::lore::RepositoryId;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::repository::prefetch::PrefetchCounts;
use lore_revision::repository::prefetch::PrefetchTracker;
use lore_revision::repository::prefetch::name_table_of;
use lore_revision::repository::prefetch::prefetch_changed_node_blocks;
use lore_revision::repository::prefetch::prefetch_changed_node_blocks_in_chunks;
use lore_revision::state::State;
use lore_revision::state::allow_all_repositories;
use lore_revision::store::composite::CompositeStoreBuilder;
use lore_storage::ImmutableStore;
use rand::random;

include!("helper.rs");

/// Files enough to spread the root's children over more than one node block.
const FILES: usize = 1_200;

/// A cap on the blocks of a compared state that no state of these tests reaches.
const MAX_BLOCKS: usize = 1_024;

/// The stores of a server whose local tier starts empty in front of a store holding the
/// repository's content.
struct Tiers {
    /// A context over the store holding the content.
    holding: Arc<RepositoryContext>,
    /// A context over the composite of the empty local tier and the holding store.
    composite: Arc<RepositoryContext>,
    local: Arc<dyn ImmutableStore>,
    mutable: Arc<dyn lore_storage::MutableStore>,
}

async fn tiers() -> Tiers {
    let (holding, mutable, _execution) = test_store_create().await.expect("holding store");
    let local = lore_storage::local::immutable_store::create(
        None::<&str>,
        lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
        false,
        lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
    )
    .await
    .expect("local tier");
    let composite = CompositeStoreBuilder::default()
        .with_durable("holding".to_string(), holding.clone())
        .expect("durable tier")
        .with_local("local".to_string(), local.clone())
        .expect("local tier")
        .build()
        .expect("composite store");
    let id = random::<RepositoryId>();
    Tiers {
        holding: Arc::new(RepositoryContext::new_server_context(
            holding,
            mutable.clone(),
            id,
            allow_all_repositories(),
        )),
        composite: Arc::new(RepositoryContext::new_server_context(
            Arc::new(composite),
            mutable.clone(),
            id,
            allow_all_repositories(),
        )),
        local,
        mutable,
    }
}

/// A revision of `files` files under the root, serialized to `repository`'s store.
async fn revision_with_files(repository: &Arc<RepositoryContext>, files: usize) -> Hash {
    let state = State::new();
    state.set_revision_number(1);
    add_files_and_serialize(repository, state, 0..files).await
}

/// The child of `parent` that adds the files `added` under the root, serialized to `repository`'s
/// store.
async fn child_adding_files(
    repository: &Arc<RepositoryContext>,
    parent: Hash,
    added: std::ops::Range<usize>,
) -> Hash {
    let state = State::deserialize(repository.clone(), parent)
        .await
        .expect("deserialize parent");
    state.set_parent_self(parent);
    state.set_revision_number(state.revision_number() + 1);
    add_files_and_serialize(repository, state, added).await
}

async fn add_files_and_serialize(
    repository: &Arc<RepositoryContext>,
    state: Arc<State>,
    files: std::ops::Range<usize>,
) -> Hash {
    for index in files {
        let name = format!("file-{index}.txt");
        state
            .node_add(
                repository.clone(),
                ROOT_NODE,
                Node {
                    flags: NodeFlags::File.bits(),
                    name_hash: lore_storage::hash::hash_string(&name),
                    ..Default::default()
                },
                &name,
            )
            .await
            .expect("node_add");
    }
    let tempdir = generate_tempdir();
    let token = RepositoryWriteToken::acquire(tempdir.path()).await;
    state
        .serialize(repository.clone(), &token)
        .await
        .expect("serialize state")
}

/// The addresses of the node blocks the state of `revision` stores, in block order.
async fn node_blocks(repository: &Arc<RepositoryContext>, revision: Hash) -> Vec<Address> {
    let state = State::deserialize(repository.clone(), revision)
        .await
        .expect("deserialize state");
    let (nodes, _file_metadata) = state
        .block_address_lists(repository.clone())
        .await
        .expect("block address lists");
    nodes
        .as_type_slice::<Hash>()
        .iter()
        .filter(|hash| !hash.is_zero())
        .map(|&hash| Address::zero_context_hash(hash))
        .collect()
}

/// The addresses of the name tables the node blocks at `blocks` name.
async fn name_tables(repository: &Arc<RepositoryContext>, blocks: &[Address]) -> Vec<Address> {
    let mut name_tables = Vec::new();
    for &block in blocks {
        let content = lore_revision::immutable::read(
            repository.clone(),
            block,
            None,
            lore_revision::immutable::read_options_from_repository(repository),
        )
        .await
        .expect("read node block");
        name_tables.extend(name_table_of(&content));
    }
    name_tables
}

/// The first of `addresses` the local tier of `tiers` lacks a payload for.
async fn first_lacking(tiers: &Tiers, addresses: &[Address]) -> Option<Address> {
    let local = RepositoryContext::new_server_context(
        tiers.local.clone(),
        tiers.mutable.clone(),
        tiers.composite.id,
        allow_all_repositories(),
    );
    lore_revision::immutable::first_not_held_locally(&local, addresses)
        .await
        .expect("local query")
}

mod prefetch_changed_node_blocks {
    use super::*;

    /// Prefetches the blocks at which `revision`'s state differs from `against`'s, counting into
    /// a tracker of its own.
    async fn prefetch(tiers: &Tiers, revision: Hash, against: Hash) -> PrefetchCounts {
        prefetch_against_each(tiers, revision, &[against], MAX_BLOCKS).await
    }

    /// Prefetches the blocks at which `revision`'s state differs from any of `against`'s,
    /// comparing no state of more than `max_blocks` blocks, counting into a tracker of its own.
    async fn prefetch_against_each(
        tiers: &Tiers,
        revision: Hash,
        against: &[Hash],
        max_blocks: usize,
    ) -> PrefetchCounts {
        let tracker = Arc::new(PrefetchTracker::default());
        prefetch_changed_node_blocks(
            tiers.composite.clone(),
            tiers.local.clone(),
            revision,
            against.to_vec(),
            max_blocks,
            tracker.clone(),
        )
        .await;
        tracker.counts()
    }

    /// Against the empty state every block of a state differs, so each the local tier lacks is
    /// fetched into it, and so is the name table of each block fetched.
    #[tokio::test]
    async fn fills_the_local_tier_with_the_changed_blocks_and_name_tables_it_lacks() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let tiers = tiers().await;
                let revision = revision_with_files(&tiers.holding, FILES).await;
                let blocks = node_blocks(&tiers.holding, revision).await;
                assert!(blocks.len() > 1, "{} blocks", blocks.len());
                assert_eq!(first_lacking(&tiers, &blocks).await, Some(blocks[0]));

                let counts = prefetch(&tiers, revision, Hash::default()).await;

                let blocks_count = blocks.len() as u64;
                assert_eq!(
                    counts,
                    PrefetchCounts {
                        runs: 1,
                        finished: 1,
                        blocks_changed: blocks_count,
                        blocks_missing: blocks_count,
                        blocks_fetched: blocks_count,
                        name_tables_fetched: blocks_count,
                        elapsed: counts.elapsed,
                        ..PrefetchCounts::default()
                    }
                );
                assert_eq!(first_lacking(&tiers, &blocks).await, None);
                let name_tables = name_tables(&tiers.holding, &blocks).await;
                assert_eq!(name_tables.len(), blocks.len());
                assert_eq!(first_lacking(&tiers, &name_tables).await, None);
            })
            .await;
    }

    /// Compared with the local tier one block at a time, a prefetch goes on past the chunks whose
    /// blocks the local tier holds and fetches the rest.
    #[tokio::test]
    async fn compared_block_by_block_fetches_every_changed_block_the_local_tier_lacks() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let tiers = tiers().await;
                let parent = revision_with_files(&tiers.holding, FILES).await;
                let child = child_adding_files(&tiers.holding, parent, FILES..FILES + 1).await;
                let child_blocks = node_blocks(&tiers.holding, child).await;
                let [c0, c1, c2] = child_blocks[..] else {
                    panic!("{} child blocks", child_blocks.len());
                };
                prefetch(&tiers, child, parent).await;
                assert_eq!(first_lacking(&tiers, &[c0, c2]).await, None);
                assert_eq!(first_lacking(&tiers, &[c1]).await, Some(c1));
                let tracker = Arc::new(PrefetchTracker::default());

                prefetch_changed_node_blocks_in_chunks(
                    tiers.composite.clone(),
                    tiers.local.clone(),
                    child,
                    vec![Hash::default()],
                    MAX_BLOCKS,
                    tracker.clone(),
                    1,
                )
                .await;

                let counts = tracker.counts();
                assert_eq!(
                    (
                        counts.finished,
                        counts.blocks_changed,
                        counts.blocks_missing,
                        counts.blocks_fetched,
                        counts.failed
                    ),
                    (1, 3, 1, 1, 0),
                    "{counts}"
                );
                assert_eq!(first_lacking(&tiers, &child_blocks).await, None);
            })
            .await;
    }

    /// A prefetch of changed blocks the local tier already holds finds nothing missing and
    /// fetches nothing.
    #[tokio::test]
    async fn fetches_nothing_the_local_tier_holds() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let tiers = tiers().await;
                let revision = revision_with_files(&tiers.holding, FILES).await;
                prefetch(&tiers, revision, Hash::default()).await;

                let counts = prefetch(&tiers, revision, Hash::default()).await;

                assert!(counts.blocks_changed > 1, "{counts}");
                assert_eq!(
                    (
                        counts.finished,
                        counts.blocks_missing,
                        counts.blocks_fetched,
                        counts.name_tables_fetched,
                        counts.failed
                    ),
                    (1, 0, 0, 0, 0)
                );
            })
            .await;
    }

    /// Only both states' blocks at the indexes where their block hashes differ are fetched; a
    /// block both store at the same index is left alone.
    #[tokio::test]
    async fn fetches_only_the_blocks_at_indexes_that_differ() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let tiers = tiers().await;
                let parent = revision_with_files(&tiers.holding, FILES).await;
                // The new file takes a slot in the last block, which has room, and the root's
                // first child changes in the first.
                let child = child_adding_files(&tiers.holding, parent, FILES..FILES + 1).await;
                let parent_blocks = node_blocks(&tiers.holding, parent).await;
                let child_blocks = node_blocks(&tiers.holding, child).await;
                let [p0, p1, p2] = parent_blocks[..] else {
                    panic!("{} parent blocks", parent_blocks.len());
                };
                let [c0, c1, c2] = child_blocks[..] else {
                    panic!("{} child blocks", child_blocks.len());
                };
                assert_eq!((c0 == p0, c1 == p1, c2 == p2), (false, true, false));

                let counts = prefetch(&tiers, child, parent).await;

                assert_eq!(
                    (counts.blocks_changed, counts.blocks_fetched, counts.failed),
                    (4, 4, 0),
                    "{counts}"
                );
                assert_eq!(first_lacking(&tiers, &[p0, p2, c0, c2]).await, None);
                assert_eq!(first_lacking(&tiers, &[p1]).await, Some(p1));
            })
            .await;
    }

    /// A block that differs from more than one of the states compared against is compared and
    /// fetched once.
    #[tokio::test]
    async fn names_each_block_once_where_several_states_differ_from_it() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let tiers = tiers().await;
                let parent = revision_with_files(&tiers.holding, FILES).await;
                let child = child_adding_files(&tiers.holding, parent, FILES..FILES + 1).await;

                let counts =
                    prefetch_against_each(&tiers, child, &[parent, parent], MAX_BLOCKS).await;

                assert_eq!(
                    (counts.blocks_changed, counts.blocks_fetched, counts.failed),
                    (4, 4, 0),
                    "{counts}"
                );
            })
            .await;
    }

    /// A state of more blocks than the prefetch is given to compare is skipped, and nothing is
    /// fetched.
    #[tokio::test]
    async fn skips_a_state_of_more_blocks_than_it_compares() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let tiers = tiers().await;
                let revision = revision_with_files(&tiers.holding, FILES).await;
                let blocks = node_blocks(&tiers.holding, revision).await;

                let counts =
                    prefetch_against_each(&tiers, revision, &[Hash::default()], blocks.len() - 1)
                        .await;

                assert_eq!(
                    counts,
                    PrefetchCounts {
                        runs: 1,
                        skipped: 1,
                        ..PrefetchCounts::default()
                    }
                );
                assert_eq!(first_lacking(&tiers, &blocks).await, Some(blocks[0]));
            })
            .await;
    }

    /// A prefetch with nothing it can load to compare against fetches nothing and counts only the
    /// failed load.
    #[tokio::test]
    async fn compares_nothing_when_no_state_against_loads() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let tiers = tiers().await;
                let revision = revision_with_files(&tiers.holding, FILES).await;

                let counts = prefetch(&tiers, revision, random::<Hash>()).await;

                assert_eq!(
                    counts,
                    PrefetchCounts {
                        runs: 1,
                        failed: 1,
                        ..PrefetchCounts::default()
                    }
                );
            })
            .await;
    }

    /// A block a state has past the end of the state it is compared with differs from none and
    /// is fetched.
    #[tokio::test]
    async fn fetches_the_blocks_a_grown_state_added() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let tiers = tiers().await;
                let parent = revision_with_files(&tiers.holding, FILES).await;
                let child = child_adding_files(&tiers.holding, parent, FILES..FILES * 2).await;
                let parent_blocks = node_blocks(&tiers.holding, parent).await;
                let child_blocks = node_blocks(&tiers.holding, child).await;
                assert!(
                    child_blocks.len() > parent_blocks.len(),
                    "{} child blocks, {} parent blocks",
                    child_blocks.len(),
                    parent_blocks.len()
                );

                let counts = prefetch(&tiers, child, parent).await;

                assert_eq!(counts.failed, 0, "{counts}");
                assert_eq!(
                    first_lacking(&tiers, &child_blocks[parent_blocks.len()..]).await,
                    None
                );
            })
            .await;
    }
}
