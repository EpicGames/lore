// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

#[allow(dead_code)]
pub async fn test_store_create() -> Result<
    (
        std::sync::Arc<dyn lore_storage::ImmutableStore>,
        std::sync::Arc<dyn lore_storage::MutableStore>,
        std::sync::Arc<lore_revision::interface::ExecutionContext>,
    ),
    lore_storage::StoreError,
> {
    let execution = setup_test_execution();
    lore_base::runtime::LORE_CONTEXT
        .scope(execution, async move {
            let immutable = lore_storage::local::immutable_store::create(
                None::<&str>, /* No on disk path, in-memory only */
                lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
                false, /* Do not deserialize all buckets on start */
                lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
            )
            .await?;
            let mutable: std::sync::Arc<dyn lore_storage::MutableStore> =
                lore_storage::local::mutable_store::create(
                    None::<&str>, /* No on disk path, in-memory only */
                    lore_storage::MutableStoreSettings::default(),
                    immutable.clone(),
                )
                .await?;
            Ok((immutable, mutable, lore_revision::lore::execution_context()))
        })
        .await
}

#[allow(dead_code)]
pub fn default_repository_creation_args(
    immutable_store: std::sync::Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: std::sync::Arc<dyn lore_storage::MutableStore>,
) -> lore_revision::repository::RepositoryContextCreationArgs {
    lore_revision::repository::RepositoryContextCreationArgs {
        paths: None,
        immutable_store,
        mutable_store,
        id: lore_base::types::Context::from(uuid::Uuid::now_v7()).into(),
        instance_id: lore_revision::instance::InstanceId::generate(),
        remote: Err(lore_transport::ProtocolError::from(
            lore_base::error::NoRemote,
        )),
        filter: std::sync::Arc::default(),
        filesystem_provider: None,
    }
}

#[allow(dead_code)]
pub trait RepositoryContextCreationArgsExt {
    fn with_path(self, path: impl AsRef<std::path::Path>) -> Self;
    fn with_id(self, id: lore_revision::lore::RepositoryId) -> Self;
    fn with_instance_id(self, id: lore_revision::instance::InstanceId) -> Self;
    fn with_remote(
        self,
        remote: Result<std::sync::Arc<lore_transport::Connection>, lore_transport::ProtocolError>,
    ) -> Self;
    fn with_filter(self, filter: std::sync::Arc<lore_revision::filter::Filter>) -> Self;
    fn with_filesystem_provider(
        self,
        filesystem_provider: std::sync::Arc<
            dyn lore_revision::fs::filesystem_provider::FilesystemProvider,
        >,
    ) -> Self;
}

impl RepositoryContextCreationArgsExt for lore_revision::repository::RepositoryContextCreationArgs {
    fn with_path(mut self, path: impl AsRef<std::path::Path>) -> Self {
        self.paths = Some(lore_revision::repository::RepositoryPaths::new(
            path.as_ref().to_path_buf(),
            path.as_ref().join(lore_revision::repository::DOT_LORE),
        ));
        self
    }

    fn with_id(mut self, id: lore_revision::lore::RepositoryId) -> Self {
        self.id = id;
        self
    }

    fn with_instance_id(mut self, id: lore_revision::instance::InstanceId) -> Self {
        self.instance_id = id;
        self
    }

    fn with_remote(
        mut self,
        remote: Result<std::sync::Arc<lore_transport::Connection>, lore_transport::ProtocolError>,
    ) -> Self {
        self.remote = remote;
        self
    }

    fn with_filter(mut self, filter: std::sync::Arc<lore_revision::filter::Filter>) -> Self {
        self.filter = filter;
        self
    }

    fn with_filesystem_provider(
        mut self,
        filesystem_provider: std::sync::Arc<
            dyn lore_revision::fs::filesystem_provider::FilesystemProvider,
        >,
    ) -> Self {
        self.filesystem_provider = Some(filesystem_provider);
        self
    }
}

#[allow(unused_imports)]
pub use lore_base::test_util::TempDir;

#[allow(dead_code)]
pub fn generate_tempdir() -> TempDir {
    TempDir::new("lore-stage-test-")
}

/// A repository on its own temporary directory, with its anchor branch stored
/// and its write token held.
///
/// The directory is removed when this is dropped, so it has to outlive every use
/// of `repository`.
#[allow(dead_code)]
pub struct TestRepository {
    pub repository: std::sync::Arc<lore_revision::repository::RepositoryContext>,
    pub write_token: lore_revision::repository::RepositoryWriteToken,
    pub path: std::path::PathBuf,
    _tempdir: TempDir,
}

/// Creates a repository backed by `immutable_store` and `mutable_store` in a
/// fresh temporary directory, on a generated default branch.
///
/// Call from inside a `LORE_CONTEXT` scope; repository creation reads the
/// execution context.
#[allow(dead_code)]
pub async fn test_repository_create(
    immutable_store: std::sync::Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: std::sync::Arc<dyn lore_storage::MutableStore>,
    repository_id: lore_revision::lore::RepositoryId,
) -> TestRepository {
    let tempdir = generate_tempdir();
    let path = tempdir.to_path_buf();
    let default_branch_id = lore_base::types::Context::from(uuid::Uuid::now_v7());
    let write_token =
        lore_revision::repository::RepositoryWriteToken::acquire(path.as_path()).await;
    let created = lore_revision::repository::create_local(
        path.as_path(),
        &write_token,
        repository_id,
        default_branch_id,
        lore_revision::branch::DEFAULT_DEFAULT_NAME.to_string(),
        lore_revision::repository::RepositoryConfig::default(),
        false,
    )
    .await
    .expect("Failed to initialize repository");

    let repository = std::sync::Arc::new(
        lore_revision::repository::RepositoryContext::new(
            default_repository_creation_args(immutable_store, mutable_store)
                .with_path(&path)
                .with_id(repository_id)
                .with_instance_id(created.instance_id),
        )
        .with_write_token(write_token.share()),
    );

    lore_revision::instance::store_current_anchor_branch(&repository, default_branch_id)
        .await
        .expect("Failed to store anchor branch");

    TestRepository {
        repository,
        write_token,
        path,
        _tempdir: tempdir,
    }
}

/// A context over `instance` whose view holds `globs` and whose ignore slot is empty, a leading
/// `!` re-including as it does in a view file.
///
/// Each call mints its own filter, so two contexts hold different ones whether or not their
/// rules agree -- which is what a diff reads as two views.
#[allow(dead_code)]
pub fn test_view_context(
    instance: &TestRepository,
    immutable_store: std::sync::Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: std::sync::Arc<dyn lore_storage::MutableStore>,
    globs: &[&str],
) -> std::sync::Arc<lore_revision::repository::RepositoryContext> {
    test_filter_context(instance, immutable_store, mutable_store, &[], globs)
}

/// [`test_view_context`] with `ignore` in the ignore slot, for a test that has to tell the two
/// slots apart. A repository opened for real holds rules in both.
#[allow(dead_code)]
pub fn test_filter_context(
    instance: &TestRepository,
    immutable_store: std::sync::Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: std::sync::Arc<dyn lore_storage::MutableStore>,
    ignore: &[&str],
    view: &[&str],
) -> std::sync::Arc<lore_revision::repository::RepositoryContext> {
    let mut filter = lore_revision::filter::Filter::default();
    for (slot, globs) in [(&mut filter.ignore, ignore), (&mut filter.view, view)] {
        for glob in globs {
            match glob.strip_prefix('!') {
                Some(inclusion) => slot.add_inclusion(inclusion).expect("Filter inclusion"),
                None => slot.add_exclusion(glob).expect("Filter exclusion"),
            }
        }
    }
    std::sync::Arc::new(lore_revision::repository::RepositoryContext::new(
        default_repository_creation_args(immutable_store, mutable_store)
            .with_path(&instance.path)
            .with_id(instance.repository.id)
            .with_instance_id(instance.repository.instance_id)
            .with_filter(std::sync::Arc::new(filter)),
    ))
}

/// Stages the whole working tree of `instance` and commits it, answering the state of the
/// revision that produced.
///
/// Scans rather than reading dirty flags, so a fixture that wrote its files directly is staged
/// whole.
#[allow(dead_code)]
pub async fn test_commit_tree(
    instance: &TestRepository,
    message: &str,
) -> std::sync::Arc<lore_revision::state::State> {
    lore_revision::file::stage::stage(
        instance.repository.clone(),
        &instance.write_token,
        lore_revision::interface::LoreArray::from_vec(vec![
            lore_revision::interface::LoreString::from(&instance.path),
        ]),
        lore_revision::stage::StageOptions {
            scan: true,
            ..Default::default()
        },
    )
    .await
    .expect("Failed to stage the fixture");
    test_commit(instance, message).await
}

/// Commits what `instance` holds staged, answering the state of the revision that produced.
///
/// For a fixture that staged something narrower than the whole tree, such as a move.
#[allow(dead_code)]
pub async fn test_commit(
    instance: &TestRepository,
    message: &str,
) -> std::sync::Arc<lore_revision::state::State> {
    lore_revision::commit::commit_boxed(
        instance.repository.clone(),
        &instance.write_token,
        lore_revision::commit::CommitOptions::new(message.to_string()),
    )
    .await
    .expect("Commit failed");
    let (revision, _branch) =
        lore_revision::instance::load_current_anchor_boxed(&instance.repository)
            .await
            .expect("Failed to load current anchor");
    lore_revision::state::State::deserialize(instance.repository.clone(), revision)
        .await
        .expect("Failed to deserialize the committed state")
}

/// Creates (or truncates) the file at `path` and writes `contents` to it.
///
/// Panics on failure, since a fixture the test cannot write invalidates what it
/// would go on to assert.
#[allow(dead_code)]
pub fn test_file_write(path: &std::path::Path, contents: &[u8]) {
    // Test fixture writes; not subject to repository write-token discipline.
    #[allow(clippy::disallowed_methods)]
    std::fs::write(path, contents)
        .unwrap_or_else(|_| panic!("Failed to write test file at {}", path.display()));
}

/// Reconciles the working tree against `state_staged`, mutating it in place as
/// `lore status --scan` does, and returns the changes detected.
#[allow(dead_code)]
pub async fn test_scan(
    repository: std::sync::Arc<lore_revision::repository::RepositoryContext>,
    state_staged: std::sync::Arc<lore_revision::state::State>,
    state_current: std::sync::Arc<lore_revision::state::State>,
) -> Vec<lore_revision::change::NodeChange> {
    test_scan_with_intent(
        repository,
        state_staged,
        state_current,
        lore_revision::fs::filesystem_provider::FilesystemDiffIntent::MarkDirty,
    )
    .await
}

/// [`test_scan`] under the given intent, for a walk that stages rather than marks.
#[allow(dead_code)]
pub async fn test_scan_with_intent(
    repository: std::sync::Arc<lore_revision::repository::RepositoryContext>,
    state_staged: std::sync::Arc<lore_revision::state::State>,
    state_current: std::sync::Arc<lore_revision::state::State>,
    intent: lore_revision::fs::filesystem_provider::FilesystemDiffIntent,
) -> Vec<lore_revision::change::NodeChange> {
    test_scan_path_with_intent(repository, state_staged, state_current, None, intent).await
}

/// [`test_scan_with_intent`] scoped to `path`, which is what makes the walk resolve the
/// ancestors of a path the tree does not hold.
#[allow(dead_code)]
pub async fn test_scan_path_with_intent(
    repository: std::sync::Arc<lore_revision::repository::RepositoryContext>,
    state_staged: std::sync::Arc<lore_revision::state::State>,
    state_current: std::sync::Arc<lore_revision::state::State>,
    path: Option<lore_revision::util::path::RelativePath>,
    intent: lore_revision::fs::filesystem_provider::FilesystemDiffIntent,
) -> Vec<lore_revision::change::NodeChange> {
    let operation = lore_revision::fs::filesystem_provider::FilesystemProvider::begin_operation(
        repository.file_system().as_ref(),
    )
    .await
    .expect("Failed to start filesystem operation");
    let changes = lore_revision::state::diff_filesystem(
        &operation,
        lore_revision::fs::filesystem_provider::FilesystemDiffTree {
            repository: repository.clone(),
            state: state_staged,
        },
        lore_revision::fs::filesystem_provider::FilesystemDiffTree {
            repository,
            state: state_current,
        },
        path,
        lore_revision::filter::FilterMode::Full,
        intent,
        std::sync::Arc::new(Vec::new()),
    )
    .await
    .expect("Failed to diff filesystem")
    .collect()
    .await
    .expect("Failed to diff filesystem");
    operation
        .finalize()
        .await
        .expect("Failed to finish filesystem operation");
    changes
}

/// [`test_scan`] read one change at a time, for the walk's other consumption path.
///
/// Answers in arrival order, which interleaves subtrees by completion, and waits for the walk so
/// the answer is the whole of it.
#[allow(dead_code)]
pub async fn test_scan_streaming(
    repository: std::sync::Arc<lore_revision::repository::RepositoryContext>,
    state_staged: std::sync::Arc<lore_revision::state::State>,
    state_current: std::sync::Arc<lore_revision::state::State>,
) -> Vec<lore_revision::change::NodeChange> {
    let operation = lore_revision::fs::filesystem_provider::FilesystemProvider::begin_operation(
        repository.file_system().as_ref(),
    )
    .await
    .expect("Failed to start filesystem operation");
    let mut stream = lore_revision::state::diff_filesystem(
        &operation,
        lore_revision::fs::filesystem_provider::FilesystemDiffTree {
            repository: repository.clone(),
            state: state_staged,
        },
        lore_revision::fs::filesystem_provider::FilesystemDiffTree {
            repository,
            state: state_current,
        },
        None,
        lore_revision::filter::FilterMode::Full,
        lore_revision::fs::filesystem_provider::FilesystemDiffIntent::Report,
        std::sync::Arc::new(Vec::new()),
    )
    .await
    .expect("Failed to diff filesystem");

    let mut changes = Vec::new();
    while let Some(change) = stream.next().await {
        changes.push(change);
    }
    stream.finish().await.expect("Failed to diff filesystem");
    operation
        .finalize()
        .await
        .expect("Failed to finish filesystem operation");
    changes
}

/// The anchor revision's state, deserialized twice: once as the current
/// revision and once as the staged state a scan reconciles in place.
#[allow(dead_code)]
pub async fn test_anchor_states(
    repository: &std::sync::Arc<lore_revision::repository::RepositoryContext>,
) -> (
    std::sync::Arc<lore_revision::state::State>,
    std::sync::Arc<lore_revision::state::State>,
) {
    let (revision, _branch) = lore_revision::instance::load_current_anchor_boxed(repository)
        .await
        .expect("Failed to load current anchor");
    let current = lore_revision::state::State::deserialize(repository.clone(), revision)
        .await
        .expect("Failed to deserialize current state");
    let staged = lore_revision::state::State::deserialize(repository.clone(), revision)
        .await
        .expect("Failed to deserialize staged state");
    (current, staged)
}

pub fn setup_test_execution() -> std::sync::Arc<lore_revision::interface::ExecutionContext> {
    std::sync::Arc::new(
        lore_revision::interface::ExecutionContext::new_client_with_user_id(
            lore_revision::interface::LoreGlobalArgs::default(),
            lore_revision::relay::EventDispatcher::no_dispatch(),
            "test-user".to_string(),
        ),
    )
}

/// The action letter against the path for each change, as a walk's consumers read it.
///
/// The action is carried because two sides route a path by which of them admits it, and a path
/// alone does not say which route it took.
#[allow(dead_code)]
pub fn test_reported(changes: &[lore_revision::change::NodeChange]) -> Vec<(String, String)> {
    changes
        .iter()
        .map(|change| {
            (
                change.action.as_string_short().to_string(),
                change.path().as_str().to_string(),
            )
        })
        .collect()
}
