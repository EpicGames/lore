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
        path: None,
        immutable_store,
        mutable_store,
        id: lore_base::types::Context::from(uuid::Uuid::now_v7()).into(),
        instance_id: lore_revision::instance::InstanceId::generate(),
        remote: Err(lore_transport::ProtocolError::from(
            lore_base::error::NoRemote,
        )),
        filter: std::sync::Arc::default(),
        format: lore_revision::repository::RepositoryFormat::Lore,
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
    fn with_format(self, format: lore_revision::repository::RepositoryFormat) -> Self;
    fn with_filesystem_provider(
        self,
        filesystem_provider: std::sync::Arc<
            dyn lore_revision::fs::filesystem_provider::FilesystemProvider,
        >,
    ) -> Self;
}

impl RepositoryContextCreationArgsExt for lore_revision::repository::RepositoryContextCreationArgs {
    fn with_path(mut self, path: impl AsRef<std::path::Path>) -> Self {
        self.path = Some(path.as_ref().to_owned());
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

    fn with_format(mut self, format: lore_revision::repository::RepositoryFormat) -> Self {
        self.format = format;
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

pub struct TempDir(std::path::PathBuf);

impl TempDir {
    #[allow(dead_code)]
    pub fn new(prefix: &str) -> Self {
        use rand::distr::SampleString;
        let name = format!(
            "{prefix}{}",
            rand::distr::Alphanumeric.sample_string(&mut rand::rng(), 8)
        );
        let path = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&path).expect("Failed to create temp directory");
        let path = std::fs::canonicalize(path).expect("Canonicalize temporary test dir");
        Self(path)
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl std::ops::Deref for TempDir {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

impl AsRef<std::path::Path> for TempDir {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Test fixture cleanup; not subject to repository write-token discipline.
        #[allow(clippy::disallowed_methods)]
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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

pub fn setup_test_execution() -> std::sync::Arc<lore_revision::interface::ExecutionContext> {
    std::sync::Arc::new(
        lore_revision::interface::ExecutionContext::new_client_with_user_id(
            lore_revision::interface::LoreGlobalArgs::default(),
            lore_revision::relay::EventDispatcher::no_dispatch(),
            "test-user".to_string(),
        ),
    )
}
