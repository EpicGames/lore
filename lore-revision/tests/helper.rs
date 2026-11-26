// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

/// Held by each test from setting the process-wide log callback until it restores it, so that
/// two tests running at once don't take each other's messages or callback.
#[allow(dead_code)]
pub static LOG_CAPTURE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    pub(crate) _tempdir: TempDir,
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
    test_repository_create_with_remote(
        immutable_store,
        mutable_store,
        repository_id,
        Err(lore_transport::ProtocolError::from(
            lore_base::error::NoRemote,
        )),
    )
    .await
}

/// [`test_repository_create`] whose context answers `remote` as its remote connection.
#[allow(dead_code)]
pub async fn test_repository_create_with_remote(
    immutable_store: std::sync::Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: std::sync::Arc<dyn lore_storage::MutableStore>,
    repository_id: lore_revision::lore::RepositoryId,
    remote: Result<std::sync::Arc<lore_transport::Connection>, lore_transport::ProtocolError>,
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
                .with_instance_id(created.instance_id)
                .with_remote(remote),
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

/// A context over `instance`, holding its write token, whose view holds `globs` and whose ignore
/// slot is empty, a leading `!` re-including as it does in a view file.
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
    std::sync::Arc::new(
        lore_revision::repository::RepositoryContext::new(
            default_repository_creation_args(immutable_store, mutable_store)
                .with_path(&instance.path)
                .with_id(instance.repository.id)
                .with_instance_id(instance.repository.instance_id)
                .with_filter(std::sync::Arc::new(test_filter(ignore, view))),
        )
        .with_write_token(instance.write_token.share()),
    )
}

/// A filter whose ignore slot holds `ignore` and whose view holds `view`, a leading `!`
/// re-including as it does in a filter file.
#[allow(dead_code)]
pub fn test_filter(ignore: &[&str], view: &[&str]) -> lore_revision::filter::Filter {
    let mut filter = lore_revision::filter::Filter::default();
    for (slot, globs) in [(&mut filter.ignore, ignore), (&mut filter.view, view)] {
        for glob in globs {
            match glob.strip_prefix('!') {
                Some(inclusion) => slot.add_inclusion(inclusion).expect("Filter inclusion"),
                None => slot.add_exclusion(glob).expect("Filter exclusion"),
            }
        }
    }
    filter
}

/// An OS filesystem that holds its state in a `State` as far as a command asks it
/// ([`lore_revision::fs::filesystem_provider::FilesystemProvider::holds_state`]), as its
/// operations do, while they read and write the files themselves. Counts the operations begun on
/// it.
#[allow(dead_code)]
pub struct HoldingFilesystem {
    pub inner: lore_revision::fs::os::OsFilesystem,
    pub begins: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl lore_revision::fs::filesystem_provider::FilesystemProvider for HoldingFilesystem {
    async fn begin_operation(
        &self,
    ) -> Result<
        std::sync::Arc<lore_revision::fs::filesystem_provider::InstanceOperationImpl>,
        lore_revision::fs::filesystem_provider::FsError,
    > {
        self.begins
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(std::sync::Arc::new(
            lore_revision::fs::filesystem_provider::InstanceOperationImpl::new(
                lore_revision::fs::filesystem_provider::StaticDispatchInstanceOperation::Os(
                    self.inner.begin_operation(),
                ),
                self.holds_state(),
            ),
        ))
    }

    fn holds_state(&self) -> bool {
        true
    }
}

/// `repository`, with its stores, filter and write token, reading its working tree through the
/// [`HoldingFilesystem`] answered beside it.
#[allow(dead_code)]
pub fn test_holding_context(
    repository: &lore_revision::repository::RepositoryContext,
) -> (
    std::sync::Arc<lore_revision::repository::RepositoryContext>,
    std::sync::Arc<HoldingFilesystem>,
) {
    let path = repository.require_path().expect("A repository on disk");
    let filesystem = std::sync::Arc::new(HoldingFilesystem {
        inner: lore_revision::fs::os::OsFilesystem::new(path),
        begins: std::sync::atomic::AtomicUsize::new(0),
    });
    let context = std::sync::Arc::new(
        lore_revision::repository::RepositoryContext::new(
            default_repository_creation_args(
                repository.immutable_store(),
                repository.mutable_store.clone(),
            )
            .with_path(path)
            .with_id(repository.id)
            .with_instance_id(repository.instance_id)
            .with_filter(repository.filter.clone())
            .with_filesystem_provider(filesystem.clone()),
        )
        .with_write_token(
            repository
                .try_write_token()
                .expect("A repository opened for writing")
                .share(),
        ),
    );
    (context, filesystem)
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

/// Every path under `root` but the control directory, with a file's content and `None` for a
/// directory.
#[allow(dead_code)]
pub fn working_tree(root: &std::path::Path) -> std::collections::BTreeMap<String, Option<Vec<u8>>> {
    fn walk(
        root: &std::path::Path,
        directory: &std::path::Path,
        tree: &mut std::collections::BTreeMap<String, Option<Vec<u8>>>,
    ) {
        for entry in std::fs::read_dir(directory).expect("Failed to list directory") {
            let path = entry.expect("Failed to read directory entry").path();
            let relative = path
                .strip_prefix(root)
                .expect("Entry outside the root")
                .to_string_lossy()
                .replace('\\', "/");
            if relative == ".lore" {
                continue;
            }
            if path.is_dir() {
                tree.insert(relative, None);
                walk(root, &path, tree);
            } else {
                // Test fixture reads; not subject to repository write-token discipline.
                #[allow(clippy::disallowed_methods)]
                let content = std::fs::read(&path).expect("Failed to read file");
                tree.insert(relative, Some(content));
            }
        }
    }
    let mut tree = std::collections::BTreeMap::new();
    walk(root, root, &mut tree);
    tree
}

/// Whether the file system holding `dir` holds one spelling of a name and answers lookups in any
/// other. Windows and macOS do by default and Linux does not, but a mount can be either on any of
/// them, so a test asks rather than assumes.
#[allow(dead_code)]
pub fn case_insensitive(dir: &std::path::Path) -> bool {
    #[allow(clippy::disallowed_methods)]
    {
        let probe = dir.join("CaseProbe");
        std::fs::write(&probe, b"").expect("write probe");
        let insensitive = std::fs::metadata(dir.join("caseprobe")).is_ok();
        std::fs::remove_file(&probe).expect("remove probe");
        insensitive
    }
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

/// The instance's staged tree after a scan walk that stages ([`test_scan_with_intent`]) over it,
/// held in memory and not stored: the walk `lore stage --scan` does not take.
#[allow(dead_code)]
pub async fn test_staged_by_intent(
    repository: &std::sync::Arc<lore_revision::repository::RepositoryContext>,
) -> std::sync::Arc<lore_revision::state::State> {
    let (current, staged, _) =
        lore_revision::state::State::deserialize_current_and_staged(repository.clone())
            .await
            .expect("Failed to deserialize the current and staged states");
    let staged = match staged {
        Some(staged) => staged,
        None => lore_revision::state::State::deserialize(repository.clone(), current.revision())
            .await
            .expect("Failed to deserialize the staged state"),
    };
    test_scan_with_intent(
        repository.clone(),
        staged.clone(),
        current,
        lore_revision::fs::filesystem_provider::FilesystemDiffIntent::Stage(
            lore_revision::fs::filesystem_provider::StageIntent {
                node_flags: lore_revision::node::NodeFlags::NoFlags,
                file_id: None,
            },
        ),
    )
    .await;
    staged
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
    test_scan_path_result(repository, state_staged, state_current, path, intent)
        .await
        .expect("Failed to diff filesystem")
}

/// [`test_scan_path_with_intent`], answering the diff's failure rather than panicking on it.
#[allow(dead_code)]
pub async fn test_scan_path_result(
    repository: std::sync::Arc<lore_revision::repository::RepositoryContext>,
    state_staged: std::sync::Arc<lore_revision::state::State>,
    state_current: std::sync::Arc<lore_revision::state::State>,
    path: Option<lore_revision::util::path::RelativePath>,
    intent: lore_revision::fs::filesystem_provider::FilesystemDiffIntent,
) -> Result<Vec<lore_revision::change::NodeChange>, lore_revision::state::StateError> {
    test_scan_mounting(
        repository,
        state_staged,
        state_current,
        path,
        intent,
        std::sync::Arc::new(Vec::new()),
    )
    .await
}

/// [`test_scan_path_result`] in an instance mounting `layer_mounts`, whose content the walk leaves
/// to each layer, as a status of the instance does.
#[allow(dead_code)]
pub async fn test_scan_mounting(
    repository: std::sync::Arc<lore_revision::repository::RepositoryContext>,
    state_staged: std::sync::Arc<lore_revision::state::State>,
    state_current: std::sync::Arc<lore_revision::state::State>,
    path: Option<lore_revision::util::path::RelativePath>,
    intent: lore_revision::fs::filesystem_provider::FilesystemDiffIntent,
    layer_mounts: std::sync::Arc<Vec<lore_revision::state::LayerMountInfo>>,
) -> Result<Vec<lore_revision::change::NodeChange>, lore_revision::state::StateError> {
    let operation = lore_revision::fs::filesystem_provider::FilesystemProvider::begin_operation(
        repository.file_system().as_ref(),
    )
    .await
    .expect("Failed to start filesystem operation");
    let changes = match lore_revision::state::diff_filesystem(
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
        layer_mounts,
    )
    .await
    {
        Ok(changes) => changes.collect().await,
        Err(err) => Err(err),
    };
    operation
        .finalize()
        .await
        .expect("Failed to finish filesystem operation");
    changes
}

/// What `status` reports of `paths`, or of the whole repository for `None`, by path: the action
/// and whether the change is dirty. `scan` asks for `--scan`.
#[allow(dead_code)]
pub async fn test_status_files_at(
    repository: std::sync::Arc<lore_revision::repository::RepositoryContext>,
    paths: Option<Vec<lore_revision::util::path::RelativePath>>,
    scan: bool,
) -> std::collections::BTreeMap<String, (lore_revision::interface::LoreFileAction, bool)> {
    test_status_events_at(repository, paths, scan)
        .await
        .into_iter()
        .map(|(path, action, dirty)| (path, (action, dirty)))
        .collect()
}

/// Each change `status` reports of `paths`, or of the whole repository for `None`, in the order it
/// reports them, two of one path among them: its path, its action and whether it is dirty. `scan`
/// asks for `--scan`.
#[allow(dead_code)]
pub async fn test_status_events_at(
    repository: std::sync::Arc<lore_revision::repository::RepositoryContext>,
    paths: Option<Vec<lore_revision::util::path::RelativePath>>,
    scan: bool,
) -> Vec<(String, lore_revision::interface::LoreFileAction, bool)> {
    let files = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = files.clone();
    let callback: lore_revision::interface::LoreEventCallback = Some(Box::new(move |event| {
        if let lore_revision::event::LoreEvent::RepositoryStatusFile(data) = event {
            captured.lock().expect("status files").push((
                data.path.as_str().to_string(),
                data.action,
                data.flag_dirty != 0,
            ));
        }
    }));
    let execution = std::sync::Arc::new(
        lore_revision::interface::ExecutionContext::new_client_with_user_id(
            lore_revision::interface::LoreGlobalArgs::default(),
            lore_revision::relay::EventDispatcher::new(callback),
            "test-user".to_string(),
        ),
    );
    lore_base::runtime::LORE_CONTEXT
        .scope(
            execution.clone(),
            lore_revision::repository::status::status_boxed(
                repository,
                paths,
                lore_revision::repository::status::StatusOptions {
                    staged: true,
                    scan,
                    check_dirty: false,
                    reset: false,
                    sync_point: false,
                    revision_only: false,
                    count: false,
                },
            ),
        )
        .await
        .expect("status");
    execution.dispatcher.drain().await;
    std::mem::take(&mut *files.lock().expect("status files"))
}

/// Each path's action in `reported`, as status lists it.
#[allow(dead_code)]
pub fn test_status_actions(
    reported: std::collections::BTreeMap<String, (lore_revision::interface::LoreFileAction, bool)>,
) -> Vec<(String, lore_revision::interface::LoreFileAction)> {
    reported
        .into_iter()
        .map(|(path, (action, _))| (path, action))
        .collect()
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

/// A call whose global data directory is `global`, so no test touches the real one.
#[allow(dead_code)]
pub fn execution_in(
    global: &std::path::Path,
) -> std::sync::Arc<lore_revision::interface::ExecutionContext> {
    execution_in_with(global, lore_revision::interface::LoreGlobalArgs::default())
}

/// A call as [`execution_in`] makes it, with `globals`.
#[allow(dead_code)]
pub fn execution_in_with(
    global: &std::path::Path,
    globals: lore_revision::interface::LoreGlobalArgs,
) -> std::sync::Arc<lore_revision::interface::ExecutionContext> {
    let environment = lore_base::env::CallEnvironment {
        values: [Some(global.display().to_string()), None, None],
    };
    std::sync::Arc::new(
        lore_revision::interface::ExecutionContext::new_client_with_user_id(
            globals,
            lore_revision::relay::EventDispatcher::no_dispatch(),
            "test-user".to_string(),
        )
        .with_environment(environment),
    )
}

pub fn setup_test_execution() -> std::sync::Arc<lore_revision::interface::ExecutionContext> {
    test_execution_with(lore_revision::interface::LoreGlobalArgs::default())
}

/// An execution whose global arguments are `args`.
pub fn test_execution_with(
    args: lore_revision::interface::LoreGlobalArgs,
) -> std::sync::Arc<lore_revision::interface::ExecutionContext> {
    std::sync::Arc::new(
        lore_revision::interface::ExecutionContext::new_client_with_user_id(
            args,
            lore_revision::relay::EventDispatcher::no_dispatch(),
            "test-user".to_string(),
        ),
    )
}

/// Runs `body` in a client execution of its own with `globals`, answering what it answers and each
/// event it sent. Routes the log of the process to the execution of the logging task, holding
/// [`LOG_CAPTURE`] until every event has been recorded and the log callback is cleared again.
#[allow(dead_code)]
pub async fn recorded<F: std::future::Future>(
    globals: lore_revision::interface::LoreGlobalArgs,
    body: F,
) -> (F::Output, Vec<lore_revision::interface::LoreEvent>) {
    fn log_to_context(level: lore_base::log::LoreLogLevel, _: &str, message: &str) {
        if let Some(execution) = lore_revision::lore::try_execution_context() {
            execution
                .dispatcher
                .send(lore_revision::interface::LoreEvent::Log(
                    lore_revision::relay::EventDispatcher::make_log(level, message.to_string()),
                ));
        }
    }
    let capture = LOG_CAPTURE.lock().await;
    lore_base::log::set_log_callback(Some(log_to_context));
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recording = events.clone();
    let execution = std::sync::Arc::new(
        lore_revision::interface::ExecutionContext::new_client_with_user_id(
            globals,
            lore_revision::relay::EventDispatcher::new(Some(Box::new(
                move |event: &lore_revision::interface::LoreEvent| {
                    recording
                        .lock()
                        .expect("Failed to record an event")
                        .push(event.clone());
                },
            ))),
            "test-user".to_string(),
        ),
    );
    let output = lore_base::runtime::LORE_CONTEXT
        .scope(execution.clone(), body)
        .await;
    execution.dispatcher.drain().await;
    lore_base::log::set_log_callback(None);
    drop(capture);
    let events = std::mem::take(&mut *events.lock().expect("Failed to read the events"));
    (output, events)
}

/// [`recorded`] with the default global arguments, answering the message of each warning it
/// logged. Maps the future of [`recorded`] rather than awaiting it, so it adds no state machine
/// around `body`.
#[allow(dead_code)]
pub fn warned<F: std::future::Future>(
    body: F,
) -> impl std::future::Future<Output = (F::Output, Vec<String>)> {
    futures::FutureExt::map(
        recorded(lore_revision::interface::LoreGlobalArgs::default(), body),
        |(output, events)| (output, warning_messages(&events)),
    )
}

/// The message of each warning among `events`, in the order they were sent.
#[allow(dead_code)]
pub fn warning_messages(events: &[lore_revision::interface::LoreEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            lore_revision::interface::LoreEvent::Log(log)
                if log.level == lore_base::log::LoreLogLevel::Warn =>
            {
                Some(log.message.to_string())
            }
            _ => None,
        })
        .collect()
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

/// The children of `parent` in `state` named `name`.
#[allow(dead_code)]
pub async fn children_named(
    repository: &std::sync::Arc<lore_revision::repository::RepositoryContext>,
    state: &std::sync::Arc<lore_revision::state::State>,
    parent: lore_revision::node::NodeID,
    name: &str,
) -> Vec<lore_revision::node::NodeID> {
    let name_hash = lore_storage::hash::hash_string(name);
    let mut named = Vec::new();
    for child in state
        .node_children(repository.clone(), parent)
        .await
        .expect("Failed to list the children")
    {
        let node = state
            .node(repository.clone(), child)
            .await
            .expect("Failed to read a child");
        if node.name_hash == name_hash {
            named.push(child);
        }
    }
    named
}

/// `path` as a repository path.
#[allow(dead_code)]
pub fn relative_path(path: &str) -> lore_revision::util::path::RelativePath {
    lore_revision::util::path::RelativePath::new_from_initial_path(path).expect("Path init failed")
}

/// An execution context whose global force flag is set, which is what `--force` gives a
/// command.
#[allow(dead_code)]
pub fn forced_execution() -> std::sync::Arc<lore_revision::interface::ExecutionContext> {
    std::sync::Arc::new(lore_revision::interface::ExecutionContext::new_client(
        lore_revision::interface::LoreGlobalArgs {
            force: 1,
            ..Default::default()
        },
        lore_revision::relay::EventDispatcher::no_dispatch(),
    ))
}

/// Runs `body` inside the execution context a command runs in.
#[allow(dead_code)]
pub fn in_execution<F: std::future::Future<Output = ()> + Send + 'static>(
    body: F,
) -> impl std::future::Future<Output = ()> {
    #[allow(clippy::disallowed_methods)]
    let task = lore_base::runtime::runtime()
        .spawn(lore_base::runtime::LORE_CONTEXT.scope(setup_test_execution(), body));
    async move { task.await.expect("Test task failed") }
}

/// A repository opened as the command line opens it, so a status run finds its branch and
/// reads `ignore` as the `.loreignore` it holds, with `files` and one more file committed.
/// The write token it holds is what lets a scan persist a node it creates.
#[allow(dead_code)]
pub async fn committed_repository(
    ignore: &str,
    files: &[String],
) -> (
    std::sync::Arc<lore_revision::repository::RepositoryContext>,
    TempDir,
) {
    let tempdir = generate_tempdir();
    let path = tempdir.to_path_buf();
    let branch_id = lore_revision::lore::BranchId::from(uuid::Uuid::now_v7());
    let write_token =
        lore_revision::repository::RepositoryWriteToken::acquire(path.as_path()).await;
    lore_revision::repository::create_local(
        path.as_path(),
        &write_token,
        lore_revision::lore::RepositoryId::from(uuid::Uuid::now_v7()),
        branch_id,
        lore_revision::branch::DEFAULT_DEFAULT_NAME.to_string(),
        lore_revision::repository::RepositoryConfig::default(),
        false,
    )
    .await
    .expect("Failed to initialize repository");
    test_file_write(
        path.join(lore_revision::repository::DOT_LOREIGNORE)
            .as_path(),
        ignore.as_bytes(),
    );
    test_file_write(path.join("seed.txt").as_path(), b"seed");
    for file in files {
        let file = path.join(file);
        std::fs::create_dir_all(file.parent().expect("A file has a parent"))
            .expect("Create directory failed");
        test_file_write(file.as_path(), b"committed");
    }

    let repository = lore_revision::repository::load_and_connect_with_token(
        path.as_path(),
        lore_revision::repository::RepositoryAccess::ReadWrite,
        Some(write_token),
    )
    .await
    .expect("Failed to open the repository");
    lore_revision::instance::store_current_anchor_branch(&repository, branch_id)
        .await
        .expect("Failed to store anchor branch");

    stage_paths(&repository, &path, &["."], true, false).await;
    lore_revision::commit::commit_boxed(
        repository.clone(),
        repository
            .try_write_token()
            .expect("The repository was opened for writing"),
        lore_revision::commit::CommitOptions::new("Seed".to_string()),
    )
    .await
    .expect("Commit failed");

    (repository, tempdir)
}

/// Creates the branch `name` in `category` off the current branch of `repository`'s instance, as
/// `lore_branch_create` does: under `id` where one is given, and forced where `force` is set.
#[allow(dead_code)]
pub async fn create_branch_on_instance(
    repository: &std::sync::Arc<lore_revision::repository::RepositoryContext>,
    name: &str,
    id: Option<lore_revision::lore::BranchId>,
    category: &str,
    force: bool,
) -> Result<(), lore_revision::branch::create::CreateError> {
    let token = repository
        .try_write_token()
        .expect("The repository was opened for writing");
    lore_revision::branch::create::create(
        repository.clone(),
        token,
        name.to_string(),
        id,
        category.to_string(),
        force,
    )
    .await
}

/// `names`, relative to `root`, as the paths a command is given.
#[allow(dead_code)]
pub fn path_arguments(
    root: &std::path::Path,
    names: &[&str],
) -> lore_revision::interface::LoreArray<lore_revision::interface::LoreString> {
    lore_revision::interface::LoreArray::from_vec(
        names
            .iter()
            .map(|name| {
                lore_revision::interface::LoreString::from(
                    root.join(name).to_string_lossy().as_ref(),
                )
            })
            .collect(),
    )
}

/// Stages `names` below `root` through `repository` as `lore stage` does, scanning where `scan`
/// asks and forced where `force` does.
#[allow(dead_code)]
pub async fn stage_paths(
    repository: &std::sync::Arc<lore_revision::repository::RepositoryContext>,
    root: &std::path::Path,
    names: &[&str],
    scan: bool,
    force: bool,
) {
    let token = repository
        .try_write_token()
        .expect("The repository was opened for writing");
    let stage = lore_revision::file::stage::stage(
        repository.clone(),
        token,
        path_arguments(root, names),
        lore_revision::stage::StageOptions {
            scan,
            ..Default::default()
        },
    );
    if force {
        lore_base::runtime::LORE_CONTEXT
            .scope(forced_execution(), stage)
            .await
    } else {
        stage.await
    }
    .expect("Stage failed");
}

/// A repository holding `committed` and, committed after them, the rule `ignore` in its
/// `.loreignore`. Answers a context that reads the rule and one that does not, which stands
/// for what was done before the rule was added.
#[allow(dead_code)]
pub async fn ignoring_repository(
    committed: &[&str],
    ignore: &str,
) -> (
    std::sync::Arc<lore_revision::repository::RepositoryContext>,
    std::sync::Arc<lore_revision::repository::RepositoryContext>,
    TempDir,
) {
    let committed: Vec<String> = committed.iter().map(|name| name.to_string()).collect();
    let (unruled, tempdir) = committed_repository("", &committed).await;
    let root = tempdir.to_path_buf();
    let rule = lore_revision::repository::DOT_LOREIGNORE;
    test_file_write(root.join(rule).as_path(), ignore.as_bytes());
    let ruled = std::sync::Arc::new(unruled.to_filter_context(
        lore_revision::repository::load_filter(&root).expect("Filter load failed"),
    ));
    stage_paths(&ruled, &root, &[rule], true, false).await;
    lore_revision::commit::commit_boxed(
        ruled.clone(),
        ruled
            .try_write_token()
            .expect("The repository was opened for writing"),
        lore_revision::commit::CommitOptions::new("Ignore".to_string()),
    )
    .await
    .expect("Commit failed");
    (ruled, unruled, tempdir)
}

/// A committed repository ignoring `Intermediate/`, whose working tree holds the file
/// `src/Intermediate` and the directory `build/Intermediate`, neither tracked.
#[allow(dead_code)]
pub async fn intermediate_repository() -> (
    std::sync::Arc<lore_revision::repository::RepositoryContext>,
    TempDir,
) {
    let (repository, tempdir) = committed_repository("Intermediate/\n", &[]).await;
    let root = tempdir.to_path_buf();
    std::fs::create_dir_all(root.join("src")).expect("Create directory failed");
    test_file_write(root.join("src/Intermediate").as_path(), b"file");
    std::fs::create_dir_all(root.join("build/Intermediate")).expect("Create directory failed");
    test_file_write(
        root.join("build/Intermediate/a.txt").as_path(),
        b"directory",
    );
    (repository, tempdir)
}

/// A repository holding `ignore` in its `.loreignore`, whose file `src/Intermediate`, committed
/// before the rule, the working tree replaced with a directory holding `a.txt`.
#[allow(dead_code)]
pub async fn replaced_intermediate_repository(
    ignore: &str,
) -> (
    std::sync::Arc<lore_revision::repository::RepositoryContext>,
    TempDir,
) {
    let (ruled, _, tempdir) = ignoring_repository(&["src/Intermediate"], ignore).await;
    let root = tempdir.to_path_buf();
    // Test fixture writes; not subject to repository write-token discipline.
    #[allow(clippy::disallowed_methods)]
    std::fs::remove_file(root.join("src/Intermediate")).expect("Delete failed");
    std::fs::create_dir_all(root.join("src/Intermediate")).expect("Create directory failed");
    test_file_write(root.join("src/Intermediate/a.txt").as_path(), b"directory");
    (ruled, tempdir)
}

/// Mounts at `mount` in `repository`'s staged tree a link to the repository `linked_id`, at a
/// revision whose tree holds the file `src/Intermediate`, the link added there and not committed.
#[allow(dead_code)]
pub async fn mount_intermediate(
    repository: &std::sync::Arc<lore_revision::repository::RepositoryContext>,
    mount: &str,
    linked_id: lore_revision::lore::RepositoryId,
) {
    mount_intermediate_at(repository, mount, linked_id, false).await;
}

/// [`mount_intermediate`] at `mount` in the root or in a directory the staged tree holds, the link
/// staged for the next commit where `staged` says so. The tree is stored as staged on the revision
/// the instance is on.
#[allow(dead_code)]
pub async fn mount_intermediate_at(
    repository: &std::sync::Arc<lore_revision::repository::RepositoryContext>,
    mount: &str,
    linked_id: lore_revision::lore::RepositoryId,
    staged: bool,
) {
    let linked = repository.to_link_context(linked_id).await;
    let token = repository
        .try_write_token()
        .expect("The repository was opened for writing");
    let linked_tree = lore_revision::state::State::new();
    let src = linked_tree
        .node_add(
            linked.clone(),
            lore_revision::node::ROOT_NODE,
            lore_revision::node::Node {
                name_hash: lore_storage::hash::hash_string("src"),
                ..Default::default()
            },
            "src",
        )
        .await
        .expect("Adding src failed");
    linked_tree
        .node_add(
            linked.clone(),
            src,
            lore_revision::node::Node {
                flags: lore_revision::node::NodeFlags::File.bits(),
                name_hash: lore_storage::hash::hash_string("Intermediate"),
                ..Default::default()
            },
            "Intermediate",
        )
        .await
        .expect("Adding src/Intermediate failed");
    linked_tree.set_revision_number(1);
    let revision = linked_tree
        .serialize(linked, token)
        .await
        .expect("Serializing the linked tree failed");

    let tree = next_tree(repository).await;
    let (directory, name) = mount.rsplit_once('/').unwrap_or(("", mount));
    let directory = tree
        .find_node_link(repository.clone(), directory)
        .await
        .expect("Finding the link's directory failed")
        .node;
    let node = lore_revision::node::Node {
        flags: lore_revision::node::NodeFlags::Link.bits(),
        name_hash: lore_storage::hash::hash_string(name),
        child: lore_revision::node::ROOT_NODE,
        address: lore_base::types::Address {
            hash: revision,
            context: linked_id.into(),
        },
        ..Default::default()
    };
    let link = if staged {
        tree.node_add_staged(repository.clone(), directory, node, name)
            .await
    } else {
        tree.node_add(repository.clone(), directory, node, name)
            .await
    }
    .expect("Adding the link failed");
    tree.link_add(
        repository.clone(),
        linked_id,
        lore_revision::lore::BranchId::default(),
        revision,
        link,
        lore_revision::link::LinkFlags::NoFlags,
    )
    .await
    .expect("Registering the link failed");
    let (current, _branch) = lore_revision::instance::load_current_anchor_boxed(repository)
        .await
        .expect("Failed to load current anchor");
    tree.reparent_onto(current);
    let signature = tree
        .serialize(repository.clone(), token)
        .await
        .expect("Serializing the staged tree failed");
    lore_revision::instance::store_staged_anchor(repository, signature)
        .await
        .expect("Storing the staged anchor failed");
}

/// The tree the next commit takes: the staged one, or the current one where nothing is staged.
#[allow(dead_code)]
pub async fn next_tree(
    repository: &std::sync::Arc<lore_revision::repository::RepositoryContext>,
) -> std::sync::Arc<lore_revision::state::State> {
    let (current, staged, _) =
        lore_revision::state::State::deserialize_current_and_staged(repository.clone())
            .await
            .expect("Deserialize failed");
    staged.unwrap_or(current)
}

/// The node the next commit takes at `path`.
#[allow(dead_code)]
pub async fn next_node(
    repository: &std::sync::Arc<lore_revision::repository::RepositoryContext>,
    path: &str,
) -> lore_revision::node::Node {
    next_tree(repository)
        .await
        .find_node(repository.clone(), path)
        .await
        .unwrap_or_else(|err| panic!("No node at {path}: {err}"))
}

/// Whether `state` holds `path`.
#[allow(dead_code)]
pub async fn holds(
    repository: &std::sync::Arc<lore_revision::repository::RepositoryContext>,
    state: &std::sync::Arc<lore_revision::state::State>,
    path: &str,
) -> bool {
    state
        .find_node_link(repository.clone(), path)
        .await
        .is_ok_and(|link| link.is_valid())
}

/// Asserts that `case` took in the file `src/Intermediate` and left out the directory
/// `build/Intermediate`, as [`intermediate_repository`] holds them.
#[allow(dead_code)]
pub async fn assert_file_in_and_directory_out(
    repository: &std::sync::Arc<lore_revision::repository::RepositoryContext>,
    case: &str,
) {
    let tree = next_tree(repository).await;
    assert!(
        holds(repository, &tree, "src/Intermediate").await,
        "{case} left out the file src/Intermediate"
    );
    assert!(
        !holds(repository, &tree, "build/Intermediate").await,
        "{case} took in the directory build/Intermediate"
    );
}

/// The files a sparse tree is drawn from. The view `/*`, `!*/Saved/Config/` keeps the last.
#[allow(dead_code)]
pub const SPARSE_FILES: [&str; 6] = [
    "top.txt",
    "Engine/Build/b.txt",
    "Engine/Source/Runtime/r.cpp",
    "Game/Content/c.uasset",
    "Game/Saved/Logs/l.log",
    "Game/Saved/Config/a.ini",
];

/// A committed repository read through the view `/*`, `!*/Saved/Config/`, whose working tree
/// holds `Game/Saved/Config/a.ini`, `seed.txt`, `.loreignore` and the directories of every file,
/// and no other file. Unlike a clone under that view, it holds the directories the view keeps
/// nothing in.
#[allow(dead_code)]
pub async fn sparse_repository() -> (
    std::sync::Arc<lore_revision::repository::RepositoryContext>,
    TempDir,
) {
    let files: Vec<String> = SPARSE_FILES.iter().map(|file| file.to_string()).collect();
    let (repository, tempdir) = committed_repository("", &files).await;
    let root = tempdir.to_path_buf();
    test_file_write(
        root.join(lore_revision::repository::DOT_LORE)
            .join(lore_revision::repository::VIEW_FILTER)
            .as_path(),
        b"/*\n!*/Saved/Config/\n",
    );
    for file in &SPARSE_FILES[..5] {
        // Test fixture writes; not subject to repository write-token discipline.
        #[allow(clippy::disallowed_methods)]
        std::fs::remove_file(root.join(file)).expect("Delete failed");
    }
    let filter = lore_revision::repository::load_filter(&root).expect("Load filter failed");
    (
        std::sync::Arc::new(repository.to_filter_context(filter)),
        tempdir,
    )
}

/// The files of [`SPARSE_FILES`] the next commit takes a node of that `marked` holds for.
#[allow(dead_code)]
pub async fn sparse_files_marked(
    repository: &std::sync::Arc<lore_revision::repository::RepositoryContext>,
    marked: fn(&lore_revision::node::Node) -> bool,
) -> Vec<&'static str> {
    let tree = next_tree(repository).await;
    let mut found = Vec::new();
    for file in SPARSE_FILES {
        if tree
            .find_node(repository.clone(), file)
            .await
            .is_ok_and(|node| marked(&node))
        {
            found.push(file);
        }
    }
    found
}

/// Fixtures for revisions built in memory, on a path-less repository writing through an in-memory
/// token, which is the shape the memory-based revision tree handle runs on.
#[allow(dead_code)]
pub mod in_memory {
    use std::sync::Arc;

    use lore_base::types::Address;
    use lore_base::types::BranchId;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_base::types::TypedBytes as _;
    use lore_revision::commit::BasedOn;
    use lore_revision::commit::commit_in_memory_revision;
    use lore_revision::commit::weave_changes;
    use lore_revision::link::LinkFlags;
    use lore_revision::lore::RepositoryId;
    use lore_revision::metadata::Metadata;
    use lore_revision::node::Node;
    use lore_revision::node::NodeDelta;
    use lore_revision::node::NodeFlags;
    use lore_revision::node::NodeID;
    use lore_revision::node::ROOT_NODE;
    use lore_revision::repository::InMemoryContext;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryWriteToken;
    use lore_revision::state::Content;
    use lore_revision::state::Mark;
    use lore_revision::state::Rewrite;
    use lore_revision::state::State;
    use lore_storage::hash::hash_string;

    use super::RepositoryContextCreationArgsExt;

    struct Marker;
    impl InMemoryContext for Marker {}
    const MARKER: Marker = Marker;

    pub fn token() -> RepositoryWriteToken {
        RepositoryWriteToken::in_memory(&MARKER)
    }

    /// A path-less repository on the stores, writing through [`token`].
    pub fn repository(
        immutable: Arc<dyn lore_storage::ImmutableStore>,
        mutable: Arc<dyn lore_storage::MutableStore>,
    ) -> Arc<RepositoryContext> {
        repository_in(immutable, mutable, RepositoryId::from(uuid::Uuid::now_v7()))
    }

    /// [`repository`] as a new instance of the repository `id`.
    pub fn repository_in(
        immutable: Arc<dyn lore_storage::ImmutableStore>,
        mutable: Arc<dyn lore_storage::MutableStore>,
        id: RepositoryId,
    ) -> Arc<RepositoryContext> {
        Arc::new(
            RepositoryContext::new(
                super::default_repository_creation_args(immutable, mutable).with_id(id),
            )
            .with_write_token(token()),
        )
    }

    pub fn file(name: &str) -> Node {
        Node {
            flags: NodeFlags::File.bits(),
            mode: 0o644,
            size: 10,
            address: Address {
                hash: Hash::from_u64(0x5a),
                context: Context::from(uuid::Uuid::now_v7()),
            },
            name_hash: hash_string(name),
            ..Default::default()
        }
    }

    pub fn directory(name: &str) -> Node {
        Node {
            flags: NodeFlags::NoFlags.bits(),
            mode: 0o755,
            name_hash: hash_string(name),
            ..Default::default()
        }
    }

    pub fn branch_id() -> BranchId {
        Context::from(uuid::Uuid::now_v7())
    }

    /// A link named `name` to the root of the revision `pin` of `link_repository`.
    pub fn link_node(name: &str, link_repository: RepositoryId, pin: Hash) -> Node {
        Node {
            flags: NodeFlags::Link.bits(),
            mode: 0o755,
            name_hash: hash_string(name),
            child: ROOT_NODE,
            address: Address {
                hash: pin,
                context: Context::from(link_repository),
            },
            ..Default::default()
        }
    }

    /// A revision of the repository `link_repository` holding `l.txt`.
    pub async fn linked_revision(
        repository: &Arc<RepositoryContext>,
        link_repository: RepositoryId,
    ) -> Hash {
        linked_revision_holding(repository, link_repository, &["l.txt"]).await
    }

    /// A revision of the repository `link_repository` holding a file at each of `names`.
    pub async fn linked_revision_holding(
        repository: &Arc<RepositoryContext>,
        link_repository: RepositoryId,
        names: &[&str],
    ) -> Hash {
        let linked = repository.to_link_context(link_repository).await;
        let state = State::new();
        for name in names {
            add(&state, linked.clone(), ROOT_NODE, file(name), name).await;
        }
        commit(&linked, &state, Hash::default(), branch_id()).await
    }

    /// What [`linked_revision_storing`] places at a path.
    pub enum Linked<'a> {
        /// A file holding these bytes, which the store holds.
        File(&'a [u8]),
        /// A link to the root of the revision of the repository.
        Link(RepositoryId, Hash),
    }

    /// A revision of the repository `link_repository` holding each of `entries` at its path, with
    /// the directories leading to it. The store holds each file's content, so a read finds it, and
    /// the revision's link list holds each link.
    pub async fn linked_revision_storing(
        repository: &Arc<RepositoryContext>,
        link_repository: RepositoryId,
        entries: &[(&str, Linked<'_>)],
    ) -> Hash {
        let linked = repository.to_link_context(link_repository).await;
        let state = State::new();
        for (path, entry) in entries {
            let (directories, name) = path.rsplit_once('/').unwrap_or(("", path));
            let mut parent = ROOT_NODE;
            for step in directories.split('/').filter(|step| !step.is_empty()) {
                parent = match state
                    .find_subnode(linked.clone(), parent, hash_string(step))
                    .await
                {
                    Ok(found) => found,
                    Err(_) => add(&state, linked.clone(), parent, directory(step), step).await,
                };
            }
            let node = match entry {
                Linked::File(content) => {
                    let address = Address {
                        hash: lore_storage::hash::hash_slice(content),
                        context: Context::from(link_repository),
                    };
                    linked
                        .immutable_store()
                        .put(
                            linked.id,
                            address,
                            lore_storage::Fragment {
                                flags:
                                    lore_storage::fragment_flags::FragmentFlags::PayloadStoredLocal
                                        .bits(),
                                size_payload: content.len() as u32,
                                size_content: content.len() as u64,
                            },
                            Some(bytes::Bytes::copy_from_slice(content)),
                            false,
                        )
                        .await
                        .expect("storing the file's content must succeed");
                    Node {
                        size: content.len() as u64,
                        address,
                        ..file(name)
                    }
                }
                Linked::Link(nested, pin) => link_node(name, *nested, *pin),
            };
            let node_id = add(&state, linked.clone(), parent, node, name).await;
            if let Linked::Link(nested, pin) = entry {
                state
                    .link_add(
                        linked.clone(),
                        *nested,
                        BranchId::default(),
                        *pin,
                        node_id,
                        lore_revision::link::LinkFlags::NoFlags,
                    )
                    .await
                    .expect("registering the link must succeed");
            }
        }
        commit(&linked, &state, Hash::default(), branch_id()).await
    }

    /// Adds `node` as `name` under `parent`, staged as an add. Every write verb marks its staged
    /// action and dirties the ancestors, which is what the freeze walks; `node_add` alone leaves
    /// neither.
    pub async fn add(
        state: &State,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        node: Node,
        name: &str,
    ) -> NodeID {
        add_staged(state, repository, parent, node, name, NodeFlags::StagedAdd).await
    }

    /// Adds `node` as `name` under `parent`, staged with `staged`, such as an add a merge takes
    /// from its other side.
    pub async fn add_staged(
        state: &State,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        node: Node,
        name: &str,
        staged: NodeFlags,
    ) -> NodeID {
        let node_id = state
            .node_add(repository.clone(), parent, node, name)
            .await
            .expect("adding the node must succeed");
        state
            .node_mark(
                &repository,
                node_id,
                Mark::staged_and_dirty(staged, NodeFlags::DirtyAdd),
            )
            .await
            .expect("marking the addition must succeed");
        node_id
    }

    pub fn metadata_on(branch: BranchId) -> Metadata {
        let mut metadata = Metadata::new();
        metadata
            .set_branch(branch)
            .expect("setting the branch must succeed");
        metadata
    }

    /// Commits `state` on top of `current` on `branch`.
    pub async fn commit(
        repository: &Arc<RepositoryContext>,
        state: &Arc<State>,
        current: Hash,
        branch: BranchId,
    ) -> Hash {
        commit_in_memory_revision(
            repository.clone(),
            &token(),
            state.clone(),
            metadata_on(branch),
            current,
            Hash::default(),
            branch,
        )
        .await
        .expect("committing the revision must succeed")
    }

    /// A State linking a new repository at `a` and `b`, both pinned at `pin`, with that repository
    /// and the nodes of the two mounts.
    pub async fn linking_twice(
        repository: &Arc<RepositoryContext>,
        pin: Hash,
    ) -> (Arc<State>, RepositoryId, NodeID, NodeID) {
        let linked = RepositoryId::from(uuid::Uuid::now_v7());
        let state = State::new();
        let mut mounts = Vec::new();
        for name in ["a", "b"] {
            let node = add(
                &state,
                repository.clone(),
                ROOT_NODE,
                link_node(name, linked, pin),
                name,
            )
            .await;
            state
                .link_add(
                    repository.clone(),
                    linked,
                    BranchId::default(),
                    pin,
                    node,
                    LinkFlags::NoFlags,
                )
                .await
                .expect("registering the mount");
            mounts.push(node);
        }
        (state, linked, mounts[0], mounts[1])
    }

    pub async fn revision_state(repository: &Arc<RepositoryContext>, revision: Hash) -> Arc<State> {
        State::deserialize(repository.clone(), revision)
            .await
            .expect("deserializing the revision must succeed")
    }

    /// Sets the content of file `node_id` of `state` to `address`, staged with `staged`.
    pub async fn modify(
        state: &State,
        repository: &Arc<RepositoryContext>,
        node_id: NodeID,
        address: Address,
        staged: NodeFlags,
    ) {
        state
            .node_rewrite(
                repository,
                node_id,
                &Content {
                    mode: 0o644,
                    size: 11,
                    address,
                },
                Rewrite::Plain,
            )
            .await
            .expect("modifying the file");
        state
            .node_mark(
                repository,
                node_id,
                Mark::staged_and_dirty(staged, NodeFlags::DirtyModify),
            )
            .await
            .expect("staging the modification");
    }

    /// `merged`, woven again as the merge a sync stages for divergent history: the remote latest
    /// first, and the local latest, which the tree of `merged` was built from, second.
    pub async fn weave_as_divergent_merge(
        repository: &Arc<RepositoryContext>,
        merged: Hash,
        remote_latest: Hash,
        local_latest: Hash,
    ) -> Arc<State> {
        let state = revision_state(repository, merged).await;
        state.set_parent_self(remote_latest);
        state.set_parent_other(local_latest);
        weave_recorded_delta(repository, &state, BasedOn::Other).await;
        state
    }

    /// Weaves the history of `state` as committing it does, from the node delta it records.
    pub async fn weave_recorded_delta(
        repository: &Arc<RepositoryContext>,
        state: &Arc<State>,
        based_on: BasedOn,
    ) {
        let delta = state
            .delta_block(repository.clone())
            .await
            .expect("reading the delta")
            .to_aligned::<NodeDelta>();
        weave_changes(
            repository.clone(),
            state.clone(),
            based_on,
            delta.as_type_slice::<NodeDelta>(),
            None,
        )
        .await
        .expect("weaving the history");
    }
}

/// Every slot of `state` that holds no node a chain from the root reaches and is not on its
/// block's free list: what a discard leaked. A link's `child` names a node of the linked State,
/// so a link is not descended. Loads every block.
#[allow(dead_code)]
pub async fn test_leaked_slots(
    state: &std::sync::Arc<lore_revision::state::State>,
    repository: &std::sync::Arc<lore_revision::repository::RepositoryContext>,
) -> Vec<lore_revision::node::NodeID> {
    use lore_revision::node::BLOCK_NODE_COUNT;
    use lore_revision::node::ROOT_NODE;
    let mut reached = std::collections::BTreeSet::from([ROOT_NODE]);
    let mut pending = vec![ROOT_NODE];
    while let Some(directory) = pending.pop() {
        let children = state
            .node_children(repository.clone(), directory)
            .await
            .expect("a directory's children");
        for child in children {
            reached.insert(child);
            let node = state
                .node(repository.clone(), child)
                .await
                .expect("a child reads");
            if node.is_directory() && !node.is_link() {
                pending.push(child);
            }
        }
    }
    let mut leaked = Vec::new();
    for block_index in 0..state.block_count() {
        let block = state
            .block(repository.clone(), block_index)
            .await
            .expect("a block loads");
        let reader = block.read();
        let data = reader.node_block();
        let mut free = std::collections::BTreeSet::new();
        let mut at = data.node_unused as usize;
        for _ in 0..data.node_unused_count {
            free.insert(at);
            at = data.node[at].sibling as usize;
        }
        for index in (0..data.node_count as usize).filter(|index| !free.contains(index)) {
            let id = lore_revision::node::NodeID::try_from(block_index * BLOCK_NODE_COUNT + index)
                .expect("a node id");
            if !reached.contains(&id) {
                leaked.push(id);
            }
        }
    }
    leaked
}

/// A scratch directory named for `prefix` in the base [`lore_io::test_util::verified_scratch_base`]
/// finds for the build's temp directory, as a move of the external root needs.
#[allow(dead_code)]
pub fn verified_temp_dir(prefix: &str) -> lore_base::test_util::TempDir {
    static BASE: std::sync::LazyLock<std::path::PathBuf> = std::sync::LazyLock::new(|| {
        lore_io::test_util::verified_scratch_base(std::path::Path::new(env!("CARGO_TARGET_TMPDIR")))
    });
    lore_base::test_util::TempDir::new_in(&BASE, prefix)
}
