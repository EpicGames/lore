// What a merge and a cherry-pick carry out of the source revision's metadata.
//
// Each test drives the production primitives and reads the resulting revision's
// metadata back off its state, so what is asserted is what a reader of the
// revision sees.

#![allow(clippy::disallowed_methods)] // Test fixtures write to the filesystem outside the repo write-token discipline.

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Arc;

    use lore_base::types::Hash;
    use lore_revision::branch;
    use lore_revision::commit;
    use lore_revision::commit::CommitOptions;
    use lore_revision::file;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::BranchId;
    use lore_revision::lore::LORE_CONTEXT;
    use lore_revision::lore::RepositoryId;
    use lore_revision::lore::runtime;
    use lore_revision::metadata;
    use lore_revision::metadata::Metadata;
    use lore_revision::metadata::MetadataInherit;
    use lore_revision::metadata::MetadataType;
    use lore_revision::node::NodeFlags;
    use lore_revision::repository;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryWriteToken;
    use lore_revision::stage;
    use lore_revision::stage::StageOptions;
    use lore_revision::state::State;

    include!("helper.rs");

    /// A key no lore build knows, standing in for one a review system attaches
    /// through `lore revision metadata set`.
    const THIRD_PARTY: &str = "crowd-status-checks";

    /// The metadata the source revision carries, as a review system and an
    /// earlier merge would have left it.
    const SOURCE_KEYS: [(&str, &str); 6] = [
        (metadata::CHANGE_REQUEST, "CR-1234"),
        (metadata::REVIEWED_BY, "source.reviewer"),
        (metadata::CREATED_BY, "source.author"),
        (metadata::MERGED_BY, "source.merger"),
        (metadata::FAST_FORWARD_MERGE, "1"),
        (THIRD_PARTY, "green"),
    ];

    /// Written by the merge itself, so finding it proves the revision carries
    /// live metadata rather than none.
    const MERGE_MESSAGE: &str = "merge feature into main";

    /// Identity every operation below runs as. Without one the commit path
    /// never writes `created-by` or `committed-by`.
    const OPERATOR: &str = "operator@example.com";

    /// `merge_start` and `cherry_pick` read the remote unless `globals.offline`
    /// is set.
    async fn offline_execution() -> Arc<lore_revision::interface::ExecutionContext> {
        let _ = test_store_create().await.expect("Failed to create stores");
        let execution = Arc::new(lore_revision::interface::ExecutionContext::new_client(
            lore_revision::interface::LoreGlobalArgs::default().set_offline(),
            lore_revision::relay::EventDispatcher::no_dispatch(),
        ));
        execution.set_user_id(OPERATOR).await;
        execution
    }

    struct Fixture {
        repository: Arc<RepositoryContext>,
        write_token: RepositoryWriteToken,
        repo_path: PathBuf,
        main_branch_id: BranchId,
        _tempdir: TempDir,
    }

    impl Fixture {
        async fn new() -> Self {
            let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
            let tempdir = generate_tempdir();
            let repo_path = tempdir.to_path_buf();
            std::fs::create_dir_all(repo_path.as_path()).expect("Create repo directory failed");

            let main_branch_id = BranchId::from(uuid::Uuid::now_v7());
            let write_token = repository::RepositoryWriteToken::acquire(repo_path.as_path()).await;
            let repository = repository::create_local(
                repo_path.as_path(),
                &write_token,
                repository_id,
                main_branch_id,
                branch::DEFAULT_DEFAULT_NAME.to_string(),
                repository::RepositoryConfig::default(),
                false,
            )
            .await
            .expect("Failed to initialize repository");

            Self {
                repository,
                write_token,
                repo_path,
                main_branch_id,
                _tempdir: tempdir,
            }
        }

        fn write_file(&self, relative: &str, content: &[u8]) {
            let absolute = self.repo_path.join(relative);
            if let Some(parent) = absolute.parent() {
                std::fs::create_dir_all(parent).expect("Failed to create parent dir");
            }
            let mut file = std::fs::File::options()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(absolute.as_path())
                .expect("Failed to open file");
            file.write_all(content).expect("Failed to write file");
        }

        fn delete_file(&self, relative: &str) {
            let _ = std::fs::remove_file(self.repo_path.join(relative));
        }

        async fn stage_all(&self) {
            file::stage::stage(
                self.repository.clone(),
                &self.write_token,
                LoreArray::from_vec(vec![LoreString::from(&self.repo_path)]),
                StageOptions {
                    case_change: stage::StageCaseChange::Error,
                    node_flags: NodeFlags::NoFlags,
                    file_id: None,
                    no_children: false,
                    scan: true,
                },
            )
            .await
            .expect("Failed to stage repository");
        }

        async fn commit(&self, message: &str) -> Hash {
            Box::pin(commit::commit(
                self.repository.clone(),
                &self.write_token,
                CommitOptions {
                    message: message.to_string(),
                    link_messages: std::collections::HashMap::new(),
                    link: None,
                    layer_messages: std::collections::HashMap::new(),
                    layer: None,
                },
            ))
            .await
            .expect("Failed to commit revision")
        }

        async fn stage_and_commit(&self, message: &str) -> Hash {
            self.stage_all().await;
            self.commit(message).await
        }

        /// Attach [`SOURCE_KEYS`] to the staged state.
        async fn set_source_metadata(&self) {
            let keys: Vec<&[u8]> = SOURCE_KEYS.iter().map(|(key, _)| key.as_bytes()).collect();
            let values: Vec<&[u8]> = SOURCE_KEYS
                .iter()
                .map(|(_, value)| value.as_bytes())
                .collect();
            let formats = vec![MetadataType::String; SOURCE_KEYS.len()];
            metadata::set::set_revision(
                self.repository.clone(),
                &self.write_token,
                &keys,
                &values,
                &formats,
            )
            .await
            .expect("Failed to set revision metadata");
        }

        async fn create_branch(&self, name: &str) -> BranchId {
            branch::create::create(
                self.repository.clone(),
                &self.write_token,
                name.to_string(),
                None,
                String::new(),
                false,
            )
            .await
            .expect("Failed to create branch");
            let (_revision, branch_id) =
                lore_revision::instance::load_current_anchor(&self.repository)
                    .await
                    .expect("Failed to load current anchor after branch create");
            branch_id
        }

        async fn switch_to(&self, branch_id: BranchId, revision: Hash) {
            lore_revision::instance::store_current_anchor_branch(&self.repository, branch_id)
                .await
                .expect("Failed to store anchor branch");
            lore_revision::instance::store_current_anchor(&self.repository, revision)
                .await
                .expect("Failed to store anchor revision");
        }

        /// The revision's metadata, `None` when it carries none.
        async fn metadata_of(&self, revision: Hash) -> Option<Metadata> {
            let state = State::deserialize(self.repository.clone(), revision)
                .await
                .expect("Failed to deserialize revision state");
            let hash = state.metadata_hash();
            if hash.is_zero() {
                return None;
            }
            Some(
                Metadata::deserialize(self.repository.clone(), hash)
                    .await
                    .expect("Failed to deserialize revision metadata"),
            )
        }
    }

    /// Read `key` off a revision, `None` when absent.
    fn string_key(metadata: &Option<Metadata>, key: &str) -> Option<String> {
        metadata
            .as_ref()
            .and_then(|metadata| metadata.get_string(key).ok())
            .map(str::to_string)
    }

    /// Merge a feature branch whose tip carries [`SOURCE_KEYS`] into main under
    /// `inherit`, answering the merge revision's metadata.
    async fn merge_scenario(inherit: MetadataInherit) -> Option<Metadata> {
        Box::pin(merge_scenario_with(inherit, Default::default())).await
    }

    /// [`merge_scenario`] with the caller's own metadata for the auto commit.
    async fn merge_scenario_with(
        inherit: MetadataInherit,
        metadata: lore_revision::commit::CommitMetadata,
    ) -> Option<Metadata> {
        let fixture = Fixture::new().await;

        fixture.write_file("base.txt", b"base\n");
        let base_revision = fixture.stage_and_commit("base").await;
        let main_branch = fixture.main_branch_id;

        let feature_branch = fixture.create_branch("feature").await;
        fixture.write_file("feature.txt", b"feature\n");
        fixture.stage_all().await;
        fixture.set_source_metadata().await;
        fixture.commit("feature work").await;

        fixture.switch_to(main_branch, base_revision).await;
        fixture.delete_file("feature.txt");

        let merged = Box::pin(branch::merge::merge_start(
            fixture.repository.clone(),
            &fixture.write_token,
            feature_branch,
            branch::merge::MergeStartOptions {
                message: MERGE_MESSAGE.to_string(),
                no_commit: false,
                scope: branch::merge::MergeScope::MainOnly,
                inherit_metadata: inherit,
                metadata,
            },
        ))
        .await
        .expect("merge_start failed");

        fixture.metadata_of(merged).await
    }

    /// The caller can state who authored the merge. A client that connects as
    /// an account id rather than a user name names the author itself, as it
    /// can for a plain commit through `commit_with_metadata`; the operator
    /// keys stay the identity's, and the source revision's provenance is
    /// still not carried.
    #[tokio::test]
    async fn a_merge_records_the_authorship_the_caller_states() {
        let execution = offline_execution().await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let authorship = lore_revision::commit::CommitMetadata {
                    keys: LoreArray::from_vec(vec![LoreString::from(metadata::CREATED_BY)]),
                    values: LoreArray::from_vec(vec![LoreString::from("Display Name")]),
                    formats: LoreArray::from_vec(vec![
                        lore_revision::interface::LoreMetadataType::String,
                    ]),
                };
                let merged =
                    Box::pin(merge_scenario_with(MetadataInherit::default(), authorship)).await;

                assert_eq!(
                    string_key(&merged, metadata::CREATED_BY).as_deref(),
                    Some("Display Name"),
                    "the stated author is what the merge records"
                );
                assert_eq!(
                    string_key(&merged, metadata::COMMITTED_BY).as_deref(),
                    Some(OPERATOR),
                    "the committer stays the operating identity"
                );
                assert_eq!(
                    string_key(&merged, metadata::MERGED_BY).as_deref(),
                    Some(OPERATOR),
                    "the merger stays the operating identity"
                );
                for (key, value) in SOURCE_KEYS {
                    assert_ne!(
                        string_key(&merged, key).as_deref(),
                        Some(value),
                        "{key} must still not be carried from the source revision"
                    );
                }
            }))
            .await
            .expect("test task panicked");
    }

    /// An inherit list naming nothing carries nothing, so none of the source
    /// revision's provenance reaches the merge revision.
    #[tokio::test]
    async fn a_merge_does_not_inherit_the_source_revision_attribution() {
        let execution = offline_execution().await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let merged = Box::pin(merge_scenario(MetadataInherit::default())).await;

                assert_eq!(
                    string_key(&merged, metadata::MESSAGE).as_deref(),
                    Some(MERGE_MESSAGE),
                    "the merge revision must carry its own metadata"
                );
                assert_eq!(
                    string_key(&merged, metadata::MERGED_BY).as_deref(),
                    Some(OPERATOR),
                    "a merge records the operator as merger"
                );
                assert_eq!(
                    string_key(&merged, metadata::CREATED_BY).as_deref(),
                    Some(OPERATOR),
                    "an uncarried created-by falls to the committer"
                );
                for (key, value) in SOURCE_KEYS {
                    assert_ne!(
                        string_key(&merged, key).as_deref(),
                        Some(value),
                        "{key} must not be carried by an inherit list that names nothing"
                    );
                }
            }))
            .await
            .expect("test task panicked");
    }

    /// A named key reaches the merge revision. Everything unnamed is dropped,
    /// third-party keys included — the list governs both alike.
    #[tokio::test]
    async fn a_merge_carries_only_the_keys_the_caller_names() {
        let execution = offline_execution().await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let merged = Box::pin(merge_scenario(MetadataInherit::from_keys([
                    metadata::CHANGE_REQUEST,
                ])))
                .await;

                assert_eq!(
                    string_key(&merged, metadata::CHANGE_REQUEST).as_deref(),
                    Some("CR-1234"),
                    "a named key must reach the merge revision"
                );
                assert_eq!(
                    string_key(&merged, THIRD_PARTY),
                    None,
                    "an unnamed third-party key must not be carried"
                );
                assert_eq!(
                    string_key(&merged, metadata::REVIEWED_BY),
                    None,
                    "an unnamed lore key must not be carried either"
                );
            }))
            .await
            .expect("test task panicked");
    }

    /// The sentinel carries keys lore does not know, and is still bounded by
    /// the reserved sets.
    #[tokio::test]
    async fn the_sentinel_carries_third_party_keys_but_never_attribution() {
        let execution = offline_execution().await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let merged = Box::pin(merge_scenario(MetadataInherit::All)).await;

                assert_eq!(
                    string_key(&merged, THIRD_PARTY).as_deref(),
                    Some("green"),
                    "the sentinel must carry a third-party key"
                );
                assert_eq!(
                    string_key(&merged, metadata::CHANGE_REQUEST).as_deref(),
                    Some("CR-1234"),
                    "the sentinel must carry the change request"
                );
                assert_ne!(
                    string_key(&merged, metadata::MERGED_BY).as_deref(),
                    Some("source.merger"),
                    "the sentinel must not forward the source revision's merger"
                );
                assert_eq!(
                    string_key(&merged, metadata::FAST_FORWARD_MERGE),
                    None,
                    "the sentinel must not forward a claim about another revision"
                );
            }))
            .await
            .expect("test task panicked");
    }

    /// `created-by` names the origin of the work rather than the operation, so
    /// an inherit list may carry it and the commit leaves it as it arrived.
    #[tokio::test]
    async fn an_inherited_created_by_survives_the_commit() {
        let execution = offline_execution().await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let merged = Box::pin(merge_scenario(MetadataInherit::from_keys([
                    metadata::CREATED_BY,
                ])))
                .await;

                assert_eq!(
                    string_key(&merged, metadata::CREATED_BY).as_deref(),
                    Some("source.author"),
                    "a carried created-by must not be replaced by the committer"
                );
            }))
            .await
            .expect("test task panicked");
    }

    /// A cherry-pick reaches the same filter, and records `cherry-picked-from`
    /// after it, so the key naming the operation survives.
    #[tokio::test]
    async fn a_cherry_pick_does_not_inherit_the_source_revision_attribution() {
        let execution = offline_execution().await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = Fixture::new().await;

                fixture.write_file("base.txt", b"base\n");
                let base_revision = fixture.stage_and_commit("base").await;
                let main_branch = fixture.main_branch_id;

                let _feature_branch = fixture.create_branch("feature").await;
                fixture.write_file("feature.txt", b"feature\n");
                fixture.stage_all().await;
                fixture.set_source_metadata().await;
                let feature_revision = fixture.commit("feature work").await;

                fixture.switch_to(main_branch, base_revision).await;
                fixture.delete_file("feature.txt");

                let picked = lore_revision::revision::cherry_pick::cherry_pick(
                    fixture.repository.clone(),
                    &fixture.write_token,
                    feature_revision,
                    lore_revision::revision::cherry_pick::CherryPickOptions {
                        message: "cherry-pick feature work".to_string(),
                        no_commit: false,
                        inherit_metadata: MetadataInherit::default(),
                    },
                )
                .await
                .expect("cherry_pick failed");

                let metadata = fixture.metadata_of(picked).await;

                for (key, value) in SOURCE_KEYS {
                    assert_ne!(
                        string_key(&metadata, key).as_deref(),
                        Some(value),
                        "{key} must not be carried onto a cherry-pick that names nothing"
                    );
                }

                assert_eq!(
                    string_key(&metadata, metadata::MERGED_BY),
                    None,
                    "a cherry-pick is not a merge and records no merger"
                );

                let picked_from = metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get_hash(metadata::CHERRY_PICKED_FROM).ok());
                assert_eq!(
                    picked_from,
                    Some(feature_revision),
                    "the cherry-pick must record the revision it picked"
                );
            }))
            .await
            .expect("test task panicked");
    }
}
