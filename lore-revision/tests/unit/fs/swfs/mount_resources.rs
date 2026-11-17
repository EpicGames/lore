// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// The fixture writes the instance's external `.lore` directly, as a clone leaves it.
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;
use std::sync::Arc;

use lore_base::env::AUTH_PATH_VAR;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_base::types::Hash;
use lore_revision::fs::swfs::mount_resources::MountResources;
use lore_revision::fs::swfs::mount_resources::SwfsExecutionToken;
use lore_revision::fs::swfs::paths::MountPath;
use lore_revision::fs::swfs::paths::SwfsPath;
use lore_revision::global::external_dir::external_dir_for_instance;
use lore_revision::global::external_dir::resolve::resolve;
use lore_revision::immutable::is_stored_local;
use lore_revision::instance::InstanceId;
use lore_revision::instance::load_current_anchor_boxed;
use lore_revision::instance::store_current_anchor;
use lore_revision::instance::store_current_anchor_branch;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::CONFIG;
use lore_revision::repository::DOT_LORE;
use lore_revision::repository::ID;
use lore_revision::repository::RepositoryConfig;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryContextCreationArgs;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::repository::create_client_immutable_store;
use lore_revision::repository::create_client_mutable_store;
use lore_storage::FlushOptions;
use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
use lore_transport::Connection;
use lore_transport::ProtocolError;
use zerocopy::IntoBytes;

use crate::helper::committed_repository;
use crate::helper::setup_test_execution;
use crate::immutable::serving::Serving;
use crate::immutable::serving::serving_remote;
use crate::repository::test_helpers::cached_remote_url;
use crate::repository::test_helpers::with_auth_path;

/// The content of the file the source repository commits.
const COMMITTED: &[u8] = b"committed";

/// An SWFS instance mounted as the service mounts it, whose store holds the instance's anchors and
/// nothing of the revision they name, as after the store evicted it: the remote holds the revision.
struct EvictedMount {
    resources: MountResources,
    revision: Hash,
    instance_id: InstanceId,
    mount_path: MountPath,
    _source: TempDir,
    _mount_parent: TempDir,
}

/// The SWFS configuration of an instance of a repository on the remote at `remote_url`.
fn swfs_config(remote_url: &str) -> String {
    format!("remote_url = \"{remote_url}\"\n[vfs]\nvfs_type = \"Swfs\"\n")
}

/// The external directory of `instance_id`.
fn instance_directory(instance_id: InstanceId) -> PathBuf {
    external_dir_for_instance(&resolve().expect("the external root resolves"), instance_id)
        .expect("the external directory resolves")
}

/// Writes the external `.lore` of `instance_id`, an instance of `repository` at `revision` on
/// `branch`, as a clone leaves it, with no content in its store.
async fn write_instance(
    instance_id: InstanceId,
    repository: RepositoryId,
    (revision, branch): (Hash, lore_revision::lore::BranchId),
    config: &str,
) {
    let dot_lore = instance_directory(instance_id).join(DOT_LORE);
    std::fs::create_dir_all(&dot_lore).expect("the external directory is created");
    std::fs::write(dot_lore.join(CONFIG), config).expect("the config is written");
    std::fs::write(dot_lore.join(ID), repository.as_bytes()).expect("the ID is written");
    let config: RepositoryConfig = toml::from_str(config).expect("the config parses");
    let (immutable, _immutable_guard) = create_client_immutable_store(
        &config,
        &dot_lore,
        ImmutableStoreCreateOptions::none(),
        false,
    )
    .await
    .expect("the immutable store opens");
    let (mutable, _mutable_guard) =
        create_client_mutable_store(&config, &dot_lore, immutable.clone())
            .await
            .expect("the mutable store opens");
    let instance = Arc::new(
        RepositoryContext::new(RepositoryContextCreationArgs {
            paths: None,
            immutable_store: immutable,
            mutable_store: mutable,
            id: repository,
            instance_id,
            remote: Err(ProtocolError::from(lore_base::error::NoRemote)),
            filter: Arc::default(),
            filesystem_provider: None,
        })
        .with_write_token(RepositoryWriteToken::acquire(&dot_lore).await),
    );
    store_current_anchor_branch(&instance, branch)
        .await
        .expect("the current branch is stored");
    store_current_anchor(&instance, revision)
        .await
        .expect("the current revision is stored");
    instance
        .flush(FlushOptions::default())
        .await
        .expect("the anchors flush");
}

/// Mounts an instance of a committed repository whose store lacks the revision, with the remote
/// holding it: the instance as a clone leaves it, mounted as the service mounts it when it starts.
async fn evicted_mount() -> EvictedMount {
    let (source, source_dir) = committed_repository("", &["dir/file.txt".to_string()]).await;
    let anchor = load_current_anchor_boxed(&source)
        .await
        .expect("the source has a current revision");
    let remote_url = cached_remote_url(Connection::detached(vec![serving_remote(
        &source,
        Serving::default(),
    )]));
    let instance_id = InstanceId::generate();
    write_instance(instance_id, source.id, anchor, &swfs_config(&remote_url)).await;
    let mount_parent = TempDir::new("lore-swfs-mount-resources-");
    let mount_path = MountPath::new(&mount_parent.child("mount")).expect("the mount path is valid");

    let resources = MountResources::premount_instance(
        instance_id,
        mount_path.clone(),
        &instance_directory(instance_id),
    )
    .await
    .expect("the mount loads the revision its store lacks from the remote");

    EvictedMount {
        resources,
        revision: anchor.0,
        instance_id,
        mount_path,
        _source: source_dir,
        _mount_parent: mount_parent,
    }
}

/// A mount whose store has evicted the revision it serves loads it from the remote and serves its
/// tree, and reads a file's content from the remote and keeps it in the store.
#[tokio::test]
async fn a_mount_serves_a_revision_its_store_lacks_from_the_remote() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let mount = evicted_mount().await;
            let repository = mount.resources.repository();
            let served = mount
                .resources
                .current_read_state()
                .expect("the mount serves a revision");
            let root = mount
                .resources
                .enumerate_directory(SwfsExecutionToken::for_test(), SwfsPath("\\"))
                .await
                .expect("the root lists");
            let address = served
                .find_node(repository.clone(), "dir/file.txt")
                .await
                .expect("the revision holds the file")
                .address;
            assert!(!is_stored_local(repository.clone(), address).await);

            let read = read_committed(&mount.resources).await;

            assert_eq!(served.revision(), mount.revision);
            let mut names: Vec<String> = root.entries().into_iter().map(|entry| entry.0).collect();
            names.sort();
            assert_eq!(names, [".loreignore", "dir", "seed.txt"]);
            assert_eq!(read, COMMITTED);
            assert!(is_stored_local(repository, address).await);
        })
        .await;
}

/// Reads the whole committed file through `resources`, answering its bytes.
async fn read_committed(resources: &MountResources) -> Vec<u8> {
    let mut bytes = vec![0; COMMITTED.len()];
    let (read, _) = resources
        .read_file(
            SwfsExecutionToken::for_test(),
            SwfsPath("\\dir\\file.txt"),
            0..bytes.len(),
            &mut bytes,
        )
        .await
        .expect("the file reads");
    bytes.truncate(read);
    bytes
}

/// A mount whose remote can't be reached serves what its store holds, including what an earlier
/// mount of the instance fetched and flushed as the service closed it.
#[tokio::test]
async fn a_mount_without_its_remote_serves_what_its_store_holds() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let mount = evicted_mount().await;
            read_committed(&mount.resources).await;
            mount.resources.flush_fetched().await;
            let EvictedMount {
                resources,
                instance_id,
                mount_path,
                ..
            } = mount;
            drop(resources);
            let directory = instance_directory(instance_id);
            std::fs::write(
                directory.join(DOT_LORE).join(CONFIG),
                swfs_config("unregistered://remote"),
            )
            .expect("the config is written");

            let remounted = MountResources::premount_instance(instance_id, mount_path, &directory)
                .await
                .expect("the mount loads the revision its store holds");

            assert_eq!(read_committed(&remounted).await, COMMITTED);
        })
        .await;
}

/// A mount's callbacks read their configuration and credentials from the environment of the call
/// that mounted the instance, so the service authenticates the mount as that caller.
#[tokio::test]
async fn a_mounts_callbacks_read_the_environment_of_the_call_that_mounted_it() {
    let caller = TempDir::new("lore-swfs-mount-caller-auth-");
    let auth_path = caller.path().display().to_string();
    let call = with_auth_path(&caller);

    let read = LORE_CONTEXT
        .scope(call, async {
            let mount = evicted_mount().await;
            mount
                .resources
                .run_with_context(async { lore_base::env::var(AUTH_PATH_VAR) })
                .await
        })
        .await;

    assert_eq!(read, Some(auth_path));
}
