// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

mod find_revision_match {
    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::BranchId;
    use lore_base::types::Hash;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::layer::find_revision_match;
    use lore_revision::metadata::Metadata;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::state::State;

    use crate::branch::null_repository;
    use crate::branch::write_not_a_state;
    use crate::helper::setup_test_execution;
    use crate::helper::test_execution_with;
    use crate::paged_remote::above_unverified;
    use crate::paged_remote::counted_client_stores;

    /// The metadata that sets `link` to `value`, with its hash, written to `repository`.
    async fn link_metadata(repository: &Arc<RepositoryContext>, value: &str) -> (Metadata, Hash) {
        let mut metadata = Metadata::new();
        metadata
            .set_string("link", value)
            .expect("setting the link");
        let metadata_hash = metadata
            .serialize(repository.clone())
            .await
            .expect("writing the metadata");
        (metadata, metadata_hash)
    }

    /// Write a line of revisions of `repository` above `parent`, numbered from 1, whose metadata
    /// sets `link` to each of `values` in turn, returning them oldest first.
    async fn write_linked_line(
        repository: &Arc<RepositoryContext>,
        mut parent: Hash,
        values: &[&str],
    ) -> Vec<Hash> {
        let mut line = Vec::with_capacity(values.len());
        for (number, value) in (1..).zip(values) {
            let state = State::new();
            state.set_parent_self(parent);
            state.set_revision_number(number);
            state.set_metadata_hash(link_metadata(repository, value).await.1);
            let token = repository
                .try_write_token()
                .expect("a null context carries a write token");
            parent = state
                .serialize(repository.clone(), token)
                .await
                .expect("serializing the revision");
            line.push(parent);
        }
        line
    }

    /// A revision of `repository` numbered 1 whose metadata sets `link` to `1`, with that metadata
    /// and its hash.
    async fn linked_source(repository: &Arc<RepositoryContext>) -> (Arc<State>, Metadata, Hash) {
        let source = write_linked_line(repository, Hash::default(), &["1"]).await[0];
        let source = State::deserialize(repository.clone(), source)
            .await
            .expect("reading the source revision");
        let (metadata, metadata_hash) = link_metadata(repository, "1").await;
        (source, metadata, metadata_hash)
    }

    /// A page that lists a revision of the layer's history without its own state fails the
    /// search, whether the state is missing, cut short, or another revision's, and whether the
    /// revision is the one asked for or one below it, though the layer's latest revision above it
    /// matches the source revision.
    #[tokio::test]
    async fn a_page_listing_a_revision_without_its_own_state_fails_the_search() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let repository = null_repository().await;
                let (source, metadata, metadata_hash) = linked_source(&repository).await;

                for case in above_unverified(metadata_hash).await {
                    metadata
                        .serialize(case.writer.clone())
                        .await
                        .expect("writing the layer revision's metadata");

                    let found = find_revision_match(
                        repository.clone(),
                        case.client,
                        BranchId::from([7; 16]),
                        source.clone(),
                        case.local,
                        Some("link"),
                    )
                    .await;

                    assert!(
                        found.as_ref().is_err_and(|error| error.is_internal()),
                        "{} state for {}: {found:?}",
                        case.kind,
                        case.position
                    );
                }
            })
            .await;
    }

    /// Without `search_nearest`, a batch of the layer's history that adds no revision fails the
    /// search, as a batch does where the line leaves the local store at a blob that is not a
    /// revision state and no remote lists it from there, whether the layer's latest revision or a
    /// parent names the blob.
    #[tokio::test]
    async fn without_search_nearest_a_layer_batch_that_adds_no_revision_fails_the_search() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let repository = null_repository().await;
                let (source, _, _) = linked_source(&repository).await;
                let not_a_state = write_not_a_state(&repository).await;
                let above = write_linked_line(&repository, not_a_state, &["layer"]).await[0];

                for (named_by, latest) in [("the latest", not_a_state), ("a parent", above)] {
                    let found = find_revision_match(
                        repository.clone(),
                        repository.clone(),
                        BranchId::from([7; 16]),
                        source.clone(),
                        latest,
                        Some("link"),
                    )
                    .await;

                    assert!(
                        found.as_ref().is_err_and(|error| error.is_internal()),
                        "named by {named_by}: {found:?}"
                    );
                }
            })
            .await;
    }

    /// Without a remote, a layer line that leaves the local store is searched as far as the store
    /// holds it: a layer revision above a revision the store lacks matches the source revision.
    #[tokio::test]
    async fn a_search_without_a_remote_compares_the_layer_revisions_the_store_holds() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let repository = null_repository().await;
                let (source, _, _) = linked_source(&repository).await;
                let layer_line =
                    write_linked_line(&repository, Hash::from([9; 32]), &["1", "layer"]).await;

                let found = find_revision_match(
                    repository.clone(),
                    repository.clone(),
                    BranchId::from([7; 16]),
                    source.clone(),
                    layer_line[1],
                    Some("link"),
                )
                .await
                .expect("the search finds the match");

                assert_eq!(found, (layer_line[0], source.revision()));
            })
            .await;
    }

    /// With `search_nearest`, a round compares the source revisions it loaded before a layer batch
    /// that adds no revision ends the search: a source revision in the second batch of the source
    /// history matches a layer revision of the first batch, though the layer's line ends there.
    #[tokio::test]
    async fn search_nearest_matches_a_later_source_batch_past_the_end_of_the_layer_line() {
        let globals = LoreGlobalArgs {
            search_nearest: 1,
            ..Default::default()
        };
        LORE_CONTEXT
            .scope(test_execution_with(globals), async {
                let repository = null_repository().await;
                let mut values = vec!["source"; 150];
                values[29] = "match";
                let source_line = write_linked_line(&repository, Hash::default(), &values).await;
                let source = State::deserialize(repository.clone(), source_line[149])
                    .await
                    .expect("reading the source revision");
                let layer_line =
                    write_linked_line(&repository, Hash::default(), &["layer", "match", "layer"])
                        .await;

                let found = find_revision_match(
                    repository.clone(),
                    repository.clone(),
                    BranchId::from([7; 16]),
                    source,
                    layer_line[2],
                    Some("link"),
                )
                .await
                .expect("the search finds the match");

                assert_eq!(found, (layer_line[1], source_line[29]));
            })
            .await;
    }

    /// With `search_nearest`, a source line that leaves the local store, with no remote to list it
    /// from there, is searched as far as the store holds it: a source revision below the revision
    /// searched from matches the layer's revision.
    #[tokio::test]
    async fn search_nearest_compares_the_source_revisions_the_store_holds() {
        let globals = LoreGlobalArgs {
            search_nearest: 1,
            ..Default::default()
        };
        LORE_CONTEXT
            .scope(test_execution_with(globals), async {
                let repository = null_repository().await;
                let source_line =
                    write_linked_line(&repository, Hash::from([9; 32]), &["1", "source"]).await;
                let source = State::deserialize(repository.clone(), source_line[1])
                    .await
                    .expect("reading the source revision");
                let layer_line = write_linked_line(&repository, Hash::default(), &["1"]).await;

                let found = find_revision_match(
                    repository.clone(),
                    repository.clone(),
                    BranchId::from([7; 16]),
                    source,
                    layer_line[0],
                    Some("link"),
                )
                .await
                .expect("the search finds the match");

                assert_eq!(found, (layer_line[0], source_line[0]));
            })
            .await;
    }

    /// A layer line of 250 revisions, three batches of the layer's history, whose revision at index
    /// 10 matches the revision [`linked_source`] writes, with that revision.
    async fn matching_in_the_third_batch(
        repository: &Arc<RepositoryContext>,
    ) -> (Vec<Hash>, Arc<State>) {
        let (source, _, _) = linked_source(repository).await;
        let mut values = vec!["layer"; 250];
        values[10] = "1";
        let layer_line = write_linked_line(repository, Hash::default(), &values).await;
        (layer_line, source)
    }

    /// A layer revision in the third batch of the layer's history matches the source revision.
    #[tokio::test]
    async fn a_search_matches_a_layer_revision_in_the_third_batch() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let repository = null_repository().await;
                let (layer_line, source) = matching_in_the_third_batch(&repository).await;

                let found = find_revision_match(
                    repository.clone(),
                    repository.clone(),
                    BranchId::from([7; 16]),
                    source.clone(),
                    layer_line[249],
                    Some("link"),
                )
                .await
                .expect("the search finds the match");

                assert_eq!(found, (layer_line[10], source.revision()));
            })
            .await;
    }

    /// A search ends once the layer revisions it compared reach the search limit, counted over its
    /// rounds: a limit of 150 ends it after the second batch, though the third holds a match.
    #[tokio::test]
    async fn a_search_stops_once_the_layer_revisions_it_compared_reach_the_search_limit() {
        let globals = LoreGlobalArgs {
            search_limit: 150,
            ..Default::default()
        };
        LORE_CONTEXT
            .scope(test_execution_with(globals), async {
                let repository = null_repository().await;
                let (layer_line, source) = matching_in_the_third_batch(&repository).await;

                let found = find_revision_match(
                    repository.clone(),
                    repository.clone(),
                    BranchId::from([7; 16]),
                    source,
                    layer_line[249],
                    Some("link"),
                )
                .await;

                assert!(
                    found.as_ref().is_err_and(|error| error.is_internal()),
                    "{found:?}"
                );
            })
            .await;
    }

    /// With `search_nearest`, a round keeps the layer revisions it compared while the source line
    /// grows: a source revision in the third batch of the source's history matches a layer revision
    /// in the second batch of the layer's, which only a round that kept that batch compares.
    #[tokio::test]
    async fn search_nearest_keeps_the_layer_revisions_while_the_source_line_grows() {
        let globals = LoreGlobalArgs {
            search_nearest: 1,
            search_limit: 250,
            ..Default::default()
        };
        LORE_CONTEXT
            .scope(test_execution_with(globals), async {
                let repository = null_repository().await;
                let mut source_values = vec!["source"; 350];
                source_values[100] = "match";
                let source_line =
                    write_linked_line(&repository, Hash::default(), &source_values).await;
                let source = State::deserialize(repository.clone(), source_line[349])
                    .await
                    .expect("reading the source revision");
                let mut layer_values = vec!["layer"; 250];
                layer_values[100] = "match";
                let layer_line =
                    write_linked_line(&repository, Hash::default(), &layer_values).await;

                let found = find_revision_match(
                    repository.clone(),
                    repository.clone(),
                    BranchId::from([7; 16]),
                    source,
                    layer_line[249],
                    Some("link"),
                )
                .await
                .expect("the search finds the match");

                assert_eq!(found, (layer_line[100], source_line[100]));
            })
            .await;
    }

    /// A search that finds no match on a layer line that ends stops where the line ends: it reads
    /// the line's revisions a few times, not once a round until the search limit.
    #[tokio::test]
    async fn a_search_stops_where_the_layer_line_ends() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let (stores, counting) = counted_client_stores().await;
                let (source, _, _) = linked_source(&stores.writer).await;
                let layer_line =
                    write_linked_line(&stores.writer, Hash::default(), &["layer", "layer"]).await;
                counting.take_reads();

                let found = find_revision_match(
                    stores.writer.clone(),
                    stores.writer.clone(),
                    BranchId::from([7; 16]),
                    source,
                    layer_line[1],
                    Some("link"),
                )
                .await;

                assert!(
                    found.as_ref().is_err_and(|error| error.is_internal()),
                    "{found:?}"
                );
                let reads = counting.take_reads();
                assert!(reads.payloads < 20, "{reads:?}");
            })
            .await;
    }
}
