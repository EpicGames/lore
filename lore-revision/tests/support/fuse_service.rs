// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Real-mount helpers shared by the FUSE service tests: a service-like mount manager, FUSE
//! instances on a shared store, an OS twin to commit from, and mount-table probes.
//!
//! Mounted by `tests/fuse_mount.rs`, beside `crate::helper` and `crate::fuse_test_util`, whose
//! fixtures and probes this re-exports. Tests take `SERIAL`: they share the process-wide
//! `MountManagerState` and the `LORE_GLOBAL_PATH` override.
#![allow(dead_code)]

use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::fs::mount_manager_state::MountManagerState;
use lore_revision::global::external_dir::external_dir_for_instance;
use lore_revision::instance::InstanceMetadata;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::DOT_LORE;
use lore_revision::repository::RepositoryConfig;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::repository::SharedStoreToUseConfig;
use lore_revision::repository::VfsConfig;
use lore_revision::repository::VfsType;
use lore_revision::repository::create_local;
use lore_revision::state::State;

pub use crate::fuse_test_util::MountGuard;
pub use crate::fuse_test_util::entry_source;
pub use crate::fuse_test_util::is_mounted;
pub use crate::fuse_test_util::mount_entry;
pub use crate::helper::*;

pub static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
pub static GLOBAL_DIR: std::sync::OnceLock<TempDir> = std::sync::OnceLock::new();
pub static MANAGER: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

pub const REMOTE_URL: &str = "lore://fuse-mount-test.invalid/repository";

/// Points global data at a per-process directory. Called under `SERIAL`, before anything reads
/// the variable.
pub fn use_private_global_dir() {
    GLOBAL_DIR.get_or_init(|| {
        let dir = TempDir::new("lore-fuse-mount-global-");
        // SAFETY: every test in this binary holds `SERIAL` while this runs, and nothing in this
        // process reads the environment concurrently with the first call.
        unsafe { std::env::set_var("LORE_GLOBAL_PATH", dir.path()) };
        dir
    });
}

/// The execution context a test runs under, with the service's mount manager initialised once.
pub async fn service_execution() -> Arc<lore_revision::interface::ExecutionContext> {
    use_private_global_dir();
    let execution = setup_test_execution();
    MANAGER
        .get_or_init(|| {
            LORE_CONTEXT.scope(execution.clone(), async {
                MountManagerState::initialize()
                    .await
                    .expect("mount manager initialises over an empty registry");
            })
        })
        .await;
    execution
}

pub fn shared_store_config(store: &Path, vfs_type: VfsType) -> RepositoryConfig {
    RepositoryConfig {
        remote_url: Some(REMOTE_URL.to_string()),
        shared_store_to_use: Some(SharedStoreToUseConfig {
            use_shared_store: Some(true),
            shared_store_path: Some(store.display().to_string()),
        }),
        vfs: Some(VfsConfig {
            vfs_type,
            base_time: None,
        }),
        ..Default::default()
    }
}

pub async fn create_instance(
    path: &Path,
    repository_id: RepositoryId,
    config: RepositoryConfig,
) -> (Arc<RepositoryContext>, RepositoryWriteToken) {
    let write_token = RepositoryWriteToken::acquire(path).await;
    let repository = create_local(
        path,
        &write_token,
        repository_id,
        lore_base::types::Context::from(uuid::Uuid::from_u128(0x5eed)),
        lore_revision::branch::DEFAULT_DEFAULT_NAME.to_string(),
        config,
        false,
    )
    .await
    .expect("create_local succeeds");
    (repository, write_token)
}

pub fn canonical_dir(prefix: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new(prefix);
    let path = std::fs::canonicalize(dir.path()).expect("tempdir canonicalizes");
    (dir, path)
}

pub async fn list_names(path: &Path) -> Vec<String> {
    let path = path.to_path_buf();
    let mut names = lore_base::lore_spawn_blocking!(move || -> std::io::Result<Vec<String>> {
        std::fs::read_dir(&path)?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect()
    })
    .await
    .expect("listing task completes")
    .expect("listing the mount succeeds");
    names.sort();
    names
}

pub async fn read_file(path: PathBuf) -> Vec<u8> {
    lore_base::lore_spawn_blocking!(move || std::fs::read(&path))
        .await
        .expect("read task completes")
        .expect("reading through the mount succeeds")
}

/// Commits `files` from an OS instance on `store`, sharing `repository_id` with the FUSE
/// instance, and answers the committed State.
pub async fn commit_from_twin(
    store: &Path,
    repository_id: RepositoryId,
    files: &[(&str, &[u8])],
) -> (TempDir, Arc<State>) {
    let twin = Box::pin(Twin::new(store, repository_id)).await;
    let state = Box::pin(twin.commit(files)).await;
    (twin.into_tempdir(), state)
}

/// An OS instance on the FUSE instance's shared store and repository, committing on the same
/// branch each time so successive commits do not diverge.
pub struct Twin(TestRepository);

impl Twin {
    pub async fn new(store: &Path, repository_id: RepositoryId) -> Self {
        let tempdir = generate_tempdir();
        let path = tempdir.to_path_buf();
        let (repository, write_token) = Box::pin(create_instance(
            &path,
            repository_id,
            shared_store_config(store, VfsType::None),
        ))
        .await;
        Self(TestRepository {
            repository,
            write_token,
            path,
            _tempdir: tempdir,
        })
    }

    /// Writes `files` over the twin's tree and commits the whole tree.
    pub async fn commit(&self, files: &[(&str, &[u8])]) -> Arc<State> {
        for (name, contents) in files {
            let full = self.0.path.join(name);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).expect("twin directories are created");
            }
            test_file_write(&full, contents);
        }
        Box::pin(test_commit_tree(&self.0, "twin commit")).await
    }

    fn into_tempdir(self) -> TempDir {
        let TestRepository { _tempdir, .. } = self.0;
        _tempdir
    }
}

pub fn instance_metadata(repository: &RepositoryContext, path: &Path) -> InstanceMetadata {
    InstanceMetadata {
        instance_id: repository.instance_id,
        path: path.display().to_string(),
        created: 0,
    }
}

pub fn dot_lore_of(repository: &RepositoryContext) -> PathBuf {
    external_dir_for_instance(repository.instance_id)
        .expect("external dir resolves")
        .join(DOT_LORE)
}

pub fn lore_mounts_at(path: &Path) -> usize {
    std::fs::read_to_string("/proc/self/mountinfo")
        .expect("mountinfo reads")
        .lines()
        .filter(|line| {
            line.split_whitespace().nth(4).map(Path::new) == Some(path)
                && line.contains(" - fuse.lore ")
        })
        .count()
}

pub async fn inode_of(path: PathBuf) -> u64 {
    lore_base::lore_spawn_blocking!(move || std::fs::metadata(&path).map(|meta| meta.ino()))
        .await
        .expect("stat task completes")
        .expect("stat through the mount succeeds")
}

/// An open directory handle on the mount root, which keeps the mount busy until dropped.
pub struct Holder(Option<std::fs::File>);

impl Holder {
    pub async fn open(path: &Path) -> Self {
        let path = path.to_path_buf();
        let file = lore_base::lore_spawn_blocking!(move || std::fs::File::open(&path))
            .await
            .expect("open task completes")
            .expect("the mount root opens");
        Self(Some(file))
    }

    pub async fn close(mut self) {
        if let Some(file) = self.0.take() {
            lore_base::lore_spawn_blocking!(move || drop(file))
                .await
                .expect("close task completes");
        }
    }
}

pub async fn fresh_fuse_instance(store: &Path, path: &Path) -> Arc<RepositoryContext> {
    let (repository, token) = create_instance(
        path,
        RepositoryId::from(uuid::Uuid::now_v7()),
        shared_store_config(store, VfsType::Fuse),
    )
    .await;
    drop(token);
    repository
}
