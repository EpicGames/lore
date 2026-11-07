// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

mod find_revision_match {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::BranchId;
    use lore_revision::layer::find_revision_match;
    use lore_revision::metadata::Metadata;
    use lore_revision::state::State;

    use crate::branch::null_repository;
    use crate::helper::setup_test_execution;
    use crate::paged_remote::above_unverified;

    /// A page that lists a revision of the layer's history without its own state fails the
    /// search, whether the state is missing, cut short, or another revision's, and whether the
    /// revision is the one asked for or one below it, though the layer's latest revision above it
    /// matches the source revision.
    #[tokio::test]
    async fn a_page_listing_a_revision_without_its_own_state_fails_the_search() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let repository = null_repository().await;
                let mut metadata = Metadata::new();
                metadata.set_string("link", "1").expect("setting the link");
                let metadata_hash = metadata
                    .serialize(repository.clone())
                    .await
                    .expect("writing the metadata");
                let source = State::new();
                source.set_revision_number(1);
                source.set_metadata_hash(metadata_hash);
                let token = repository
                    .try_write_token()
                    .expect("a null context carries a write token");
                let source = source
                    .serialize(repository.clone(), token)
                    .await
                    .expect("serializing the source revision");
                let source = State::deserialize(repository.clone(), source)
                    .await
                    .expect("reading the source revision");

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
}
