// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What the mount resources answer SWFS's callbacks with, over a revision and no mount, and how
//! a mount registered as the service mounts it reads what its store lacks.
// The fixtures write an instance's external `.lore` directly, as a clone leaves it.
#![allow(clippy::disallowed_methods)]
// Each test drives an operation over its mount resources and a revision, whose future is
// larger than the library's threshold; a test runs it once.
#![allow(clippy::large_futures)]
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use dashmap::DashSet;
use lore_base::env::AUTH_PATH_VAR;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_base::types::Hash;
use lore_revision::fs::filesystem_provider::FileInfo;
use lore_revision::fs::swfs::mount_resources::MountResources;
use lore_revision::fs::swfs::mount_resources::SwfsExecutionToken;
use lore_revision::fs::swfs::paths::MountPath;
use lore_revision::fs::swfs::paths::RelativePathKey;
use lore_revision::fs::swfs::paths::SwfsPath;
use lore_revision::fs::swfs::served_tree::Served;
use lore_revision::fs::swfs::served_tree::ServedTree;
use lore_revision::global::external_dir::external_dir_for_instance;
use lore_revision::global::external_dir::resolve::resolve;
use lore_revision::immutable::is_stored_local;
use lore_revision::instance::InstanceId;
use lore_revision::instance::load_current_anchor_boxed;
use lore_revision::instance::store_current_anchor;
use lore_revision::instance::store_current_anchor_branch;
use lore_revision::instance::store_staged_anchor;
use lore_revision::layer::Layer;
use lore_revision::lore::BranchId;
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
use lore_revision::util::path::RelativePath;
use lore_storage::FlushOptions;
use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
use lore_transport::Connection;
use lore_transport::ProtocolError;
use zerocopy::IntoBytes;

use super::listing_fixture::Held;
use super::listing_fixture::in_context;
use super::listing_fixture::node;
use super::listing_fixture::revision_state;
use super::listing_fixture::stored;
use super::listing_fixture::test_repository;
use super::listing_fixture::writable_test_repository;
use crate::helper::committed_repository;
use crate::helper::setup_test_execution;
use crate::immutable::serving::Serving;
use crate::immutable::serving::serving_remote;
use crate::repository::test_helpers::cached_remote_url;
use crate::repository::test_helpers::with_auth_path;

async fn resources(revision: &[(&str, Held<'_>)], mount: &TempDir) -> MountResources {
    let repository = test_repository().await;
    let state = revision_state(&repository, revision).await;
    let mount_path = MountPath::new(&mount.path().join("mount")).expect("a mount path");
    MountResources::for_test(repository, Some(state), mount_path).await
}

async fn lists(resources: &MountResources, path: &str) -> bool {
    resources
        .enumerate_directory(SwfsExecutionToken::for_test(), SwfsPath(path))
        .await
        .is_ok()
}

#[tokio::test]
async fn listing_a_directory_succeeds() {
    in_context(async {
        let mount = TempDir::new("swfs-mount-resources");
        let resources = resources(
            &[("d/a.txt", Held::File(b"a")), ("d/b.txt", Held::File(b"b"))],
            &mount,
        )
        .await;

        assert!(lists(&resources, "\\d").await);
    })
    .await;
}

#[tokio::test]
async fn listing_a_link_node_fails_rather_than_panics() {
    in_context(async {
        let mount = TempDir::new("swfs-mount-resources");
        let resources = resources(&[("linked", Held::Link)], &mount).await;

        assert!(!lists(&resources, "\\linked").await);
    })
    .await;
}

#[tokio::test]
async fn listing_a_file_fails_rather_than_panics() {
    in_context(async {
        let mount = TempDir::new("swfs-mount-resources");
        let resources = resources(&[("f.txt", Held::File(b"f"))], &mount).await;

        assert!(!lists(&resources, "\\f.txt").await);
    })
    .await;
}

#[tokio::test]
async fn updating_the_state_serves_a_link_the_stored_staged_state_holds() {
    in_context(async {
        let mount = TempDir::new("swfs-mount-resources");
        let repository = writable_test_repository().await;
        let linked_id = RepositoryId::from([0x11; 16]);
        let linked = repository.to_link_context(linked_id).await;
        let linked_state = revision_state(&linked, &[("src/lib.txt", Held::File(b"linked"))]).await;
        let pin = stored(&linked, &linked_state).await;
        let source = linked_state
            .find_node_link(linked.clone(), "src")
            .await
            .expect("finding src")
            .node;
        let current = revision_state(&repository, &[("file.txt", Held::File(b"parent"))]).await;
        let staged = revision_state(
            &repository,
            &[
                ("file.txt", Held::File(b"parent")),
                (
                    "lib",
                    Held::LinkTo {
                        repository: linked_id,
                        pin,
                        source,
                    },
                ),
            ],
        )
        .await;
        let current_revision = stored(&repository, &current).await;
        let staged_revision = stored(&repository, &staged).await;
        store_current_anchor(&repository, current_revision)
            .await
            .expect("storing the current anchor");
        store_current_anchor_branch(&repository, BranchId::from([5; 16]))
            .await
            .expect("storing the current branch");
        store_staged_anchor(&repository, staged_revision)
            .await
            .expect("storing the staged anchor");
        let mount_path = MountPath::new(&mount.path().join("mount")).expect("a mount path");
        let resources = MountResources::for_test(repository, None, mount_path).await;

        resources
            .update_state(SwfsExecutionToken::for_test())
            .await
            .expect("updating the state");

        let served = resources
            .served_tree()
            .resolve(&node("lib/lib.txt"))
            .await
            .expect("resolving the linked file");
        assert!(matches!(
            served,
            Some(Served::Node { repository, .. }) if repository.id == linked_id
        ));
    })
    .await;
}

/// Mount resources serving a parent holding `file.txt` and a link at `lib` to a repository whose
/// `src` holds `lib.txt`, and a layer at `deps/lay` holding `layered.txt`.
async fn resources_with_a_link_and_a_layer(mount: &TempDir) -> MountResources {
    let repository = writable_test_repository().await;
    let linked_id = RepositoryId::from([0x11; 16]);
    let linked = repository.to_link_context(linked_id).await;
    let linked_state = revision_state(&linked, &[("src/lib.txt", Held::File(LINKED))]).await;
    let pin = stored(&linked, &linked_state).await;
    let source = linked_state
        .find_node_link(linked.clone(), "src")
        .await
        .expect("finding src")
        .node;
    let layered_id = RepositoryId::from([0x33; 16]);
    let layered = repository.to_layer_context(layered_id).await;
    let layered_state = revision_state(&layered, &[("lay/layered.txt", Held::File(LAYERED))]).await;
    let layer = Layer {
        target_path: "deps/lay".to_owned(),
        source_path: "lay/".to_owned(),
        repository: layered_id,
        metadata: None,
        current: stored(&layered, &layered_state).await,
        staged: Hash::default(),
    };
    let current = revision_state(
        &repository,
        &[
            ("file.txt", Held::File(b"parent")),
            (
                "lib",
                Held::LinkTo {
                    repository: linked_id,
                    pin,
                    source,
                },
            ),
        ],
    )
    .await;
    let served = ServedTree::load(repository.clone(), Some(current.clone()), None, &[layer])
        .await
        .expect("loading the served tree");
    let mount_path = MountPath::new(&mount.path().join("mount")).expect("a mount path");
    let resources = MountResources::for_test(repository, Some(current), mount_path).await;
    resources.serve_for_test(served);
    resources
}

const LINKED: &[u8] = b"linked content";
const LAYERED: &[u8] = b"layered content";

async fn entries(resources: &MountResources, path: &str) -> Vec<(String, bool, u64)> {
    let mut entries = resources
        .enumerate_directory(SwfsExecutionToken::for_test(), SwfsPath(path))
        .await
        .expect("listing the directory")
        .entries();
    entries.sort();
    entries
}

async fn read(resources: &MountResources, path: &str) -> Vec<u8> {
    let mut buffer = vec![0; 64];
    let (read, _) = resources
        .read_file(
            SwfsExecutionToken::for_test(),
            SwfsPath(path),
            0..buffer.len(),
            &mut buffer,
        )
        .await
        .expect("reading the file");
    buffer.truncate(read);
    buffer
}

#[tokio::test]
async fn a_link_and_a_layer_are_listed_as_directories_of_what_they_mount() {
    in_context(async {
        let mount = TempDir::new("swfs-mount-resources");
        let resources = resources_with_a_link_and_a_layer(&mount).await;

        assert_eq!(
            entries(&resources, "\\").await,
            vec![
                ("deps".to_owned(), true, 0),
                ("file.txt".to_owned(), false, 6),
                ("lib".to_owned(), true, 0),
            ]
        );
        assert_eq!(
            entries(&resources, "\\lib").await,
            vec![("lib.txt".to_owned(), false, LINKED.len() as u64)]
        );
        assert_eq!(
            entries(&resources, "\\deps\\lay").await,
            vec![("layered.txt".to_owned(), false, LAYERED.len() as u64)]
        );
    })
    .await;
}

#[tokio::test]
async fn linked_and_layered_files_are_read_from_their_own_repositories() {
    in_context(async {
        let mount = TempDir::new("swfs-mount-resources");
        let resources = resources_with_a_link_and_a_layer(&mount).await;

        assert_eq!(read(&resources, "\\lib\\lib.txt").await, LINKED);
        assert_eq!(read(&resources, "\\deps\\lay\\layered.txt").await, LAYERED);
        assert_eq!(read(&resources, "\\file.txt").await, b"parent");
    })
    .await;
}

#[tokio::test]
async fn linked_paths_report_what_they_are() {
    in_context(async {
        let mount = TempDir::new("swfs-mount-resources");
        let resources = resources_with_a_link_and_a_layer(&mount).await;
        let info = async |path: &str| {
            resources
                .get_file_info(SwfsExecutionToken::for_test(), &node(path))
                .await
                .ok()
        };

        assert_eq!(info("lib").await, Some(FileInfo::Directory));
        assert_eq!(info("deps").await, Some(FileInfo::Directory));
        assert!(matches!(
            info("lib/lib.txt").await,
            Some(FileInfo::File { .. })
        ));
        assert_eq!(info("lib/missing.txt").await, None);
    })
    .await;
}

#[tokio::test]
async fn with_no_revision_any_directory_lists_as_empty() {
    in_context(async {
        let mount = TempDir::new("swfs-mount-resources");
        let repository = test_repository().await;
        let mount_path = MountPath::new(&mount.path().join("mount")).expect("a mount path");
        // What a clone's first thaw finds: what it wrote is on the mount, and no revision yet
        // for the mount to serve.
        let resources = MountResources::for_test(repository, None, mount_path).await;

        assert!(lists(&resources, "\\").await);
        assert!(lists(&resources, "\\src").await);
        // Nor is any recorded as listed: a later thaw that resizes a file in one lists it first,
        // which is what keeps the file from being padded to its old size.
        assert!(!resources.listed_for_test(""));
        assert!(!resources.listed_for_test("src"));
    })
    .await;
}

#[tokio::test]
async fn a_thaw_notes_the_unlisted_directories_it_described_into_until_they_are_listed() {
    in_context(async {
        let mount = TempDir::new("swfs-mount-resources");
        let resources = resources(
            &[
                ("src/lib/a.rs", Held::File(b"a")),
                ("top.txt", Held::File(b"t")),
            ],
            &mount,
        )
        .await;
        assert!(lists(&resources, "\\").await);
        let written = DashSet::new();
        for written_path in ["src/lib/a.rs", "top.txt"] {
            written.insert(RelativePathKey(
                RelativePath::new_from_initial_path(written_path).expect("a relative path"),
            ));
        }

        resources.note_described_unlisted(&written);

        // The root was listed, so only the directories below it are left to list first.
        assert!(!resources.described_unlisted_for_test(""));
        assert!(resources.described_unlisted_for_test("src"));
        assert!(resources.described_unlisted_for_test("SRC/lib"));

        assert!(lists(&resources, "\\src\\lib").await);
        assert!(!resources.described_unlisted_for_test("src/lib"));
        assert!(resources.described_unlisted_for_test("src"));
    })
    .await;
}

/// The user's own `.loreignore`, where SWFS holds it changed, is the instance's ignore rules in
/// place of the revision's, as on a plain instance: the record takes in what only the revision's
/// rules ignore, and rules that change leave it to be rebuilt.
#[tokio::test]
async fn the_users_ignore_rules_take_the_place_of_the_revisions() {
    in_context(async {
        let mount = TempDir::new("swfs-users-ignore-rules");
        let resources = resources(&[(".loreignore", Held::File(b"*.tmp\n"))], &mount).await;
        let record = resources.modified().clone();
        resources
            .use_users_ignore_rules(SwfsExecutionToken::for_test(), None)
            .await;
        record.set_trusted(true);
        assert!(record.ignores(&node("a.tmp")).await, "the revision's rules");

        let users = Bytes::from_static(b"*.log\n");
        resources
            .use_users_ignore_rules(SwfsExecutionToken::for_test(), Some(users.clone()))
            .await;

        assert!(
            !record.is_trusted(),
            "rules that changed leave the record untrusted"
        );
        assert!(!record.ignores(&node("a.tmp")).await);
        assert!(record.ignores(&node("a.log")).await);

        record.set_trusted(true);
        resources
            .use_users_ignore_rules(SwfsExecutionToken::for_test(), Some(users))
            .await;
        assert!(record.is_trusted(), "the same rules again change nothing");
    })
    .await;
}

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

/// A callback's work runs with its thread marked to run the file operations it starts, and the
/// mark ends with the callback.
#[tokio::test]
async fn a_callback_runs_with_its_thread_running_its_own_file_operations() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let mount = evicted_mount().await;

            // On a thread of its own, as SWFS calls it: the callback blocks its thread.
            let (during, after) = std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        let during = mount.resources.run_in_runtime_with_context(async |_token| {
                            lore_io::pool::runs_file_operations_on_this_thread()
                        });
                        (during, lore_io::pool::runs_file_operations_on_this_thread())
                    })
                    .join()
                    .expect("the callback's thread")
            });

            assert!(during, "marked for the callback");
            assert!(!after, "and no longer");
        })
        .await;
}
