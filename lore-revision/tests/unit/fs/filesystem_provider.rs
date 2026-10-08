// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// A fixture builds filesystem state directly, outside any repository; what these
// test is how the provider reads it.
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use lore_revision::fs::filesystem_provider::DirectoryEntry;
use lore_revision::fs::filesystem_provider::FileInfo;
use lore_revision::fs::filesystem_provider::FilesystemDiffIntent;
use lore_revision::fs::filesystem_provider::FilesystemDiffTree;
use lore_revision::fs::filesystem_provider::FilesystemProvider;
use lore_revision::fs::filesystem_provider::FsError;
use lore_revision::fs::filesystem_provider::InstanceOperation;
use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
use lore_revision::fs::filesystem_provider::StaticDispatchInstanceOperation;
use lore_revision::fs::filesystem_provider::create_empty_directory;
use lore_revision::fs::filesystem_provider::test_util::TestOperation;
use lore_revision::fs::filesystem_provider::with_operation;
use lore_revision::fs::filesystem_provider::with_operation_if;
use lore_revision::lore::RepositoryId;
use lore_revision::node::NodeID;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::util::path::RelativePath;
use parking_lot::Mutex;

use crate::repository::test_helpers::RepositoryContextCreationArgsExt;
use crate::repository::test_helpers::default_repository_creation_args;

#[derive(Default)]
pub struct TestFilesystemProvider {
    pub begin_count: Arc<AtomicUsize>,
    pub file_info_count: Arc<AtomicUsize>,
    pub holds_name_count: Arc<AtomicUsize>,
    pub names_folding_count: Arc<AtomicUsize>,
    pub finalize_events: Arc<Mutex<Vec<bool>>>,
    /// Paths whose scan fails.
    pub failing_scans: Arc<Mutex<Vec<String>>>,
    /// Paths whose scan finds the named node stale.
    pub stale_on_scan: Arc<Mutex<Vec<(String, NodeID)>>>,
    finalize_fails: bool,
    write_fails: bool,
    holds_paths: bool,
}

impl TestFilesystemProvider {
    pub fn new() -> TestFilesystemProvider {
        Self::default()
    }

    /// A provider whose operations record the finalize and then report it failed.
    pub fn failing_finalize() -> TestFilesystemProvider {
        Self {
            finalize_fails: true,
            ..Self::new()
        }
    }

    /// A provider whose operations report every write they are asked for as failed.
    pub fn failing_writes() -> TestFilesystemProvider {
        Self {
            write_fails: true,
            ..Self::new()
        }
    }

    /// A provider that reports every path as a file it holds, spelled as asked, which is
    /// what a caller resolving a path that exists reads.
    pub fn holding_every_path() -> TestFilesystemProvider {
        Self {
            holds_paths: true,
            ..Self::new()
        }
    }

    pub fn begins(&self) -> usize {
        self.begin_count.load(Ordering::Acquire)
    }

    /// How many paths were looked up through operations this provider began.
    pub fn file_infos(&self) -> usize {
        self.file_info_count.load(Ordering::Acquire)
    }

    /// How many single-name lookups and directory reads a case resolution cost.
    pub fn name_lookups(&self) -> usize {
        self.holds_name_count.load(Ordering::Acquire)
    }

    pub fn directory_reads(&self) -> usize {
        self.names_folding_count.load(Ordering::Acquire)
    }
}

/// A repository over `filesystem`, with the stores every context needs.
pub async fn test_repository(filesystem: Arc<TestFilesystemProvider>) -> Arc<RepositoryContext> {
    let (immutable_store, mutable_store, _context) =
        test_store_create().await.expect("Making test stores");
    Arc::new(RepositoryContext::new(
        default_repository_creation_args(immutable_store, mutable_store)
            .with_filesystem_provider(filesystem),
    ))
}

#[async_trait]
impl FilesystemProvider for TestFilesystemProvider {
    async fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError> {
        self.begin_count.fetch_add(1, Ordering::AcqRel);
        Ok(Arc::new(InstanceOperationImpl::new(
            StaticDispatchInstanceOperation::Test(TestOperation {
                file_info_count: self.file_info_count.clone(),
                holds_name_count: self.holds_name_count.clone(),
                names_folding_count: self.names_folding_count.clone(),
                finalize_events: self.finalize_events.clone(),
                finalize_fails: self.finalize_fails,
                write_fails: self.write_fails,
                holds_paths: self.holds_paths,
                failing_scans: self.failing_scans.clone(),
                stale_on_scan: self.stale_on_scan.clone(),
            }),
        )))
    }
}

#[tokio::test]
async fn one_operation_covers_every_repository_in_the_filesystem() {
    let (immutable_store, mutable_store, _context) =
        test_store_create().await.expect("Making test stores");
    let filesystem = Arc::new(TestFilesystemProvider::new());
    let parent = Arc::new(RepositoryContext::new(
        default_repository_creation_args(immutable_store, mutable_store)
            .with_filesystem_provider(filesystem.clone()),
    ));
    let link = parent.to_link_context(RepositoryId::from([1; 16])).await;

    assert!(
        Arc::ptr_eq(&parent.file_system(), &link.file_system()),
        "A link takes its parent's provider, which is what makes one operation cover both"
    );

    let operation = parent.file_system().begin_operation().await.unwrap();

    assert_eq!(
        1,
        filesystem.begins(),
        "The tree began more than one operation"
    );
    assert_eq!(Vec::<bool>::new(), *(filesystem.finalize_events.lock()));

    operation.finalize().await.expect("Finalize failed");

    assert_eq!(1, filesystem.begins());
    assert_eq!(vec![false], *(filesystem.finalize_events.lock()));
}

#[tokio::test]
async fn work_that_needs_no_operation_opens_none() {
    let filesystem = Arc::new(TestFilesystemProvider::new());
    let repository = test_repository(filesystem.clone()).await;

    let handed: Option<()> =
        with_operation_if(repository.file_system(), false, async |operation| {
            Ok::<_, FsError>(operation.map(|_| ()))
        })
        .await
        .expect("The work succeeded");

    assert!(
        handed.is_none(),
        "Work that needs no operation was handed one"
    );
    assert_eq!(0, filesystem.begins());
    assert!(
        filesystem.finalize_events.lock().is_empty(),
        "An operation that was never opened was finalized"
    );
}

#[tokio::test]
async fn work_that_needs_an_operation_opens_one_and_finalizes_it() {
    let filesystem = Arc::new(TestFilesystemProvider::new());
    let repository = test_repository(filesystem.clone()).await;

    let handed: Option<()> = with_operation_if(repository.file_system(), true, async |operation| {
        Ok::<_, FsError>(operation.map(|_| ()))
    })
    .await
    .expect("The work succeeded");

    assert!(
        handed.is_some(),
        "Work that needs an operation was handed none"
    );
    assert_eq!(1, filesystem.begins());
    assert_eq!(vec![false], *(filesystem.finalize_events.lock()));
}

#[tokio::test]
async fn a_failing_operation_is_still_finalized_where_one_was_needed() {
    let filesystem = Arc::new(TestFilesystemProvider::new());
    let repository = test_repository(filesystem.clone()).await;

    let result: Result<(), FsError> =
        with_operation_if(repository.file_system(), true, async |_operation| {
            Err(FsError::internal("Work failed"))
        })
        .await;

    result.expect_err("The work's error should be reported");
    assert_eq!(
        vec![false],
        *(filesystem.finalize_events.lock()),
        "A failed operation was left unfinalized"
    );
}

#[tokio::test]
async fn a_failing_operation_is_still_finalized() {
    let filesystem = Arc::new(TestFilesystemProvider::new());
    let repository = test_repository(filesystem.clone()).await;

    let result: Result<(), FsError> =
        with_operation(repository.file_system(), async |_operation| {
            Err(FsError::internal("Work failed"))
        })
        .await;

    result.expect_err("The work's error should be reported");
    assert_eq!(
        vec![false],
        *(filesystem.finalize_events.lock()),
        "A failed operation was left unfinalized"
    );
}

#[tokio::test]
async fn a_successful_operation_reports_its_value() {
    let filesystem = Arc::new(TestFilesystemProvider::new());
    let repository = test_repository(filesystem.clone()).await;

    let value: u32 = with_operation(repository.file_system(), async |_operation| {
        Ok::<_, FsError>(7)
    })
    .await
    .expect("The work succeeded");

    assert_eq!(7, value);
    assert_eq!(1, filesystem.begins());
    assert_eq!(vec![false], *(filesystem.finalize_events.lock()));
}

#[tokio::test]
async fn an_operation_that_wrote_finalizes_as_changed() {
    let filesystem = Arc::new(TestFilesystemProvider::new());
    let repository = test_repository(filesystem.clone()).await;

    with_operation(repository.file_system(), async |operation| {
        operation.create_dir_all(&relative("written")).await
    })
    .await
    .expect("The work succeeded");

    assert_eq!(
        vec![true],
        *(filesystem.finalize_events.lock()),
        "A write the operation performed was not reported to the finalize"
    );
}

#[tokio::test]
async fn an_operation_that_only_read_finalizes_as_unchanged() {
    let filesystem = Arc::new(TestFilesystemProvider::new());
    let repository = test_repository(filesystem.clone()).await;

    with_operation(repository.file_system(), async |operation| {
        operation.file_info(&relative("read")).await.map(|_| ())
    })
    .await
    .expect("The work succeeded");

    assert_eq!(
        vec![false],
        *(filesystem.finalize_events.lock()),
        "A read was reported to the finalize as a write"
    );
}

/// A write that failed leaves the same stale cache behind as one that succeeded, so what
/// the operation was asked for is what it reports.
#[tokio::test]
async fn an_operation_whose_write_failed_finalizes_as_changed() {
    let filesystem = Arc::new(TestFilesystemProvider::failing_writes());
    let repository = test_repository(filesystem.clone()).await;

    let result: Result<(), FsError> = with_operation(repository.file_system(), async |operation| {
        operation.create_dir_all(&relative("written")).await
    })
    .await;

    result.expect_err("The write failed");
    assert_eq!(vec![true], *(filesystem.finalize_events.lock()));
}

#[tokio::test]
async fn a_finalize_failure_is_reported_where_the_work_succeeded() {
    let filesystem = Arc::new(TestFilesystemProvider::failing_finalize());
    let repository = test_repository(filesystem.clone()).await;

    let result: Result<(), FsError> =
        with_operation(repository.file_system(), async |_operation| Ok(())).await;

    result.expect_err("A finalize failure should be reported");
}

#[tokio::test]
async fn the_works_error_is_reported_ahead_of_a_finalize_failure() {
    let filesystem = Arc::new(TestFilesystemProvider::failing_finalize());
    let repository = test_repository(filesystem.clone()).await;

    let result: Result<(), FsError> =
        with_operation(repository.file_system(), async |_operation| {
            Err(FsError::internal("Work failed"))
        })
        .await;

    let error = result.expect_err("The work failed");
    assert!(
        format!("{error}").contains("Work failed"),
        "The finalize failure displaced the work's error: {error}"
    );
}

/// `TestOperation` panics when the walk is reached, so the diff returning at all is
/// the assertion: a filter-excluded path is answered without an operation.
#[tokio::test]
async fn an_excluded_path_reaches_no_operation() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    lore_base::runtime::LORE_CONTEXT
        .scope(execution, async move {
            let mut filter = lore_revision::filter::Filter::default();
            filter
                .ignore
                .add_exclusion("secret")
                .expect("exclusion rule");
            let repository = Arc::new(RepositoryContext::new(
                default_repository_creation_args(immutable_store, mutable_store)
                    .with_filesystem_provider(Arc::new(TestFilesystemProvider::new()))
                    .with_filter(Arc::new(filter)),
            ));
            let operation = repository.file_system().begin_operation().await.unwrap();
            let state = State::new();
            let tree = || FilesystemDiffTree {
                repository: repository.clone(),
                state: state.clone(),
            };
            let mut changes = lore_revision::state::diff_filesystem(
                &operation,
                tree(),
                tree(),
                Some(
                    lore_revision::util::path::RelativePath::new_from_initial_path("secret")
                        .expect("path"),
                ),
                lore_revision::filter::FilterMode::Full,
                FilesystemDiffIntent::Report,
                Arc::new(Vec::new()),
            )
            .await
            .expect("An excluded path is not an error");

            assert!(
                changes.next().await.is_none(),
                "An excluded path reported changes"
            );
            let stats = changes
                .finish()
                .await
                .expect("An excluded path is not an error");
            assert_eq!(0, stats.file_add.load(std::sync::atomic::Ordering::Relaxed));
        })
        .await;
}

/// The walk hands this to every component above a staged path, so it has to report
/// a directory that holds nothing a directory node would store.
#[test]
fn a_directory_info_is_an_existing_directory_with_no_content() {
    let info = FileInfo::Directory;
    assert!(info.exists());
    assert!(info.is_dir());
    assert!(!info.is_file());
    assert_eq!(0, info.size());
    assert_eq!(0, info.mtime());
}

/// The mode read straight off the metadata, which is the path [`FileInfo::mode`] has to
/// reproduce for a node staged either way to land the same one.
fn metadata_to_mode(metadata: &std::fs::Metadata, previous: u16) -> u16 {
    lore_revision::util::fs::mode_from_observed(
        metadata.is_file(),
        lore_revision::util::fs::file_executable_observed(metadata),
        previous,
    )
}

/// A node staged from a `FileInfo` has to land the size, time and mode a node
/// staged from the metadata itself would, since the two are the same walk before
/// and after the file information became the currency between them. A directory
/// states none of what a file stores, which is none of what a directory node takes.
#[test]
fn file_information_answers_what_the_metadata_helpers_answer() {
    let dir = lore_base::test_util::TempDir::new("lore-fs-provider-test-");
    let path = dir.path().join("file");
    std::fs::write(&path, b"content").expect("write");

    let check = |path: &Path| {
        let metadata = std::fs::metadata(path).expect("metadata");
        let info = FileInfo::from_metadata(&metadata);
        assert_eq!(lore_revision::util::fs::file_size(&metadata), info.size());
        assert_eq!(lore_revision::util::fs::file_mtime(&metadata), info.mtime());
        assert_eq!(metadata.is_dir(), info.is_dir());
        assert_eq!(metadata.is_file(), info.is_file());
        for previous in [0, lore_revision::node::NodeFileMode::Executable.bits()] {
            assert_eq!(
                metadata_to_mode(&metadata, previous),
                info.mode(previous),
                "mode for {} from previous {previous}",
                path.display()
            );
        }
    };

    check(&path);

    let metadata = std::fs::metadata(dir.path()).expect("metadata");
    let directory = FileInfo::from_metadata(&metadata);
    assert_eq!(FileInfo::Directory, directory);
    for previous in [0, lore_revision::node::NodeFileMode::Executable.bits()] {
        assert_eq!(
            metadata_to_mode(&metadata, previous),
            directory.mode(previous),
            "mode for a directory from previous {previous}"
        );
    }

    #[cfg(target_family = "unix")]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("set executable");
        check(&path);
        let info = FileInfo::from_metadata(&std::fs::metadata(&path).expect("metadata"));
        assert_eq!(
            lore_revision::node::NodeFileMode::Executable.bits(),
            info.mode(0),
            "an executable file reports the bit whatever the node held"
        );
    }
}

/// The forwarder routes a lookup to the operation rather than refusing one, which
/// is what lets a test observe the paths an operation was asked about.
#[tokio::test]
async fn a_lookup_through_an_operation_is_counted() {
    let filesystem = Arc::new(TestFilesystemProvider::new());
    let operation = filesystem.begin_operation().await.expect("an operation");
    let path = RelativePath::new_from_initial_path("a/b").expect("path");

    let info = operation
        .file_info(&path)
        .await
        .expect("a lookup is answered");

    assert!(!info.exists());
    assert_eq!(1, filesystem.file_infos());
}

/// An operation rooted at `root`, which is what a caller names its paths against.
async fn os_operation(root: &Path) -> Arc<InstanceOperationImpl> {
    FilesystemProvider::begin_operation(&lore_revision::fs::os::OsFilesystem::new(root))
        .await
        .expect("beginning an operation over the OS filesystem")
}

fn relative(path: &str) -> RelativePath {
    RelativePath::new_from_initial_path(path).expect("relative path")
}

#[tokio::test]
async fn a_missing_directory_is_created_with_its_ancestors() {
    let dir = lore_base::test_util::TempDir::new("lore-fs-provider-empty-dir-");
    let operation = os_operation(dir.path()).await;

    create_empty_directory::<FsError>(&operation, &relative("outer/inner"))
        .await
        .expect("an empty directory is created");

    assert!(dir.path().join("outer").join("inner").is_dir());
}

#[tokio::test]
async fn a_directory_already_there_is_the_state_asked_for() {
    let dir = lore_base::test_util::TempDir::new("lore-fs-provider-empty-dir-");
    let operation = os_operation(dir.path()).await;
    std::fs::create_dir_all(dir.path().join("held")).expect("create directory");

    create_empty_directory::<FsError>(&operation, &relative("held"))
        .await
        .expect("a directory already there is accepted");

    assert!(dir.path().join("held").is_dir());
}

/// A file standing where a directory belongs is reported rather than passed over: the
/// caller holds a directory and has nothing to put in the file's place.
#[tokio::test]
async fn a_file_standing_where_the_directory_belongs_is_reported() {
    let dir = lore_base::test_util::TempDir::new("lore-fs-provider-empty-dir-");
    let operation = os_operation(dir.path()).await;
    std::fs::write(dir.path().join("held"), b"content").expect("write file");

    create_empty_directory::<FsError>(&operation, &relative("held"))
        .await
        .expect_err("a file standing there is not the state asked for");

    assert!(
        dir.path().join("held").is_file(),
        "the file standing there was replaced"
    );
}

/// Every entry the operation lists at `path`, which is what a caller collecting a directory
/// before acting on it reads, in name order so a test can name the entry it means.
async fn read_all(operation: &InstanceOperationImpl, path: &str) -> Vec<DirectoryEntry> {
    let mut listing = operation
        .read_directory(&relative(path))
        .await
        .expect("a directory is listed");
    let mut entries = vec![];
    while let Some(entry) = listing.next().await {
        entries.push(entry.expect("an entry is described"));
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    entries
}

#[tokio::test]
async fn a_directory_lists_the_children_it_holds() {
    let dir = lore_base::test_util::TempDir::new("lore-fs-provider-listing-");
    let operation = os_operation(dir.path()).await;
    std::fs::create_dir_all(dir.path().join("held").join("inner")).expect("create directory");
    std::fs::write(dir.path().join("held").join("file.txt"), b"content").expect("write file");

    let entries = read_all(&operation, "held").await;

    assert_eq!(
        vec!["file.txt", "inner"],
        entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<&str>>()
    );
    assert_eq!(
        7,
        entries[0].info.size(),
        "an entry carries what the listing measured at the name"
    );
    assert_eq!(
        lore_storage::hash::hash_string("file.txt"),
        entries[0].name_hash,
        "an entry carries the hash of the name it names"
    );
    assert!(entries[1].info.is_dir());
}

/// A listing describes the children the repository tracks and passes over the rest: a link
/// is left out rather than described as what it points at.
#[cfg(target_family = "unix")]
#[tokio::test]
async fn a_listing_passes_over_what_the_repository_holds_nothing_for() {
    let dir = lore_base::test_util::TempDir::new("lore-fs-provider-listing-");
    let operation = os_operation(dir.path()).await;
    let held = dir.path().join("held");
    std::fs::create_dir_all(&held).expect("create directory");
    std::fs::write(held.join("file.txt"), b"content").expect("write file");
    std::os::unix::fs::symlink(held.join("file.txt"), held.join("link.txt"))
        .expect("create symlink");

    let entries = read_all(&operation, "held").await;

    assert_eq!(
        vec!["file.txt"],
        entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<&str>>()
    );
}

/// A name that is not text is reported rather than passed over: it hashes to a node the tree
/// does not hold, so nothing can be done with it that is not a guess.
///
/// This covers the listing end to end, which takes a filesystem willing to hold such a name.
/// One enforcing UTF-8 -- ZFS with `utf8only=on`, APFS -- refuses it in the write below,
/// before any of the code under test runs, and the test steps aside there rather than
/// reporting that limit as a failure of the listing.
/// [`crate::fs::os::a_listed_name_that_is_not_text_is_reported`] covers the same
/// reporting from an assembled entry, so the invariant stays covered on such a filesystem.
#[cfg(target_family = "unix")]
#[tokio::test]
async fn a_name_that_is_not_text_is_reported() {
    use std::os::unix::ffi::OsStrExt;

    let dir = lore_base::test_util::TempDir::new("lore-fs-provider-listing-");
    let operation = os_operation(dir.path()).await;
    let held = dir.path().join("held");
    std::fs::create_dir_all(&held).expect("create directory");
    let name = held.join(std::ffi::OsStr::from_bytes(b"\xff"));
    // Any failure here is the filesystem refusing the name: the write is the same one the
    // neighbouring tests do, so a temp directory that could not be written to at all would
    // take those with it rather than showing up only here.
    if std::fs::write(&name, b"content").is_err() {
        return;
    }

    let mut listing = operation
        .read_directory(&relative("held"))
        .await
        .expect("a directory is listed");

    assert!(
        listing.next().await.expect("an entry").is_err(),
        "a name that is not text is reported"
    );
}

#[tokio::test]
async fn an_empty_directory_lists_nothing() {
    let dir = lore_base::test_util::TempDir::new("lore-fs-provider-listing-");
    let operation = os_operation(dir.path()).await;
    std::fs::create_dir_all(dir.path().join("held")).expect("create directory");

    assert!(read_all(&operation, "held").await.is_empty());
}

/// A path holding no directory is reported where it is read, rather than answering as a
/// directory holding nothing.
#[tokio::test]
async fn a_path_holding_no_directory_is_reported() {
    let dir = lore_base::test_util::TempDir::new("lore-fs-provider-listing-");
    let operation = os_operation(dir.path()).await;
    std::fs::write(dir.path().join("held"), b"content").expect("write file");

    assert!(
        operation.read_directory(&relative("held")).await.is_err(),
        "a file holds no children"
    );
    assert!(
        operation
            .read_directory(&relative("missing"))
            .await
            .is_err(),
        "a path holding nothing holds no children"
    );
}

#[tokio::test]
async fn finalizing_twice_is_refused() {
    let filesystem = Arc::new(TestFilesystemProvider::new());
    let repository = test_repository(filesystem.clone()).await;

    let operation = repository.file_system().begin_operation().await.unwrap();
    operation.finalize().await.expect("Finalize failed");
    operation
        .finalize()
        .await
        .expect_err("A second finalize should be refused");

    assert_eq!(
        vec![false],
        *(filesystem.finalize_events.lock()),
        "The refused finalize reached the filesystem"
    );
}

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

pub fn setup_test_execution() -> std::sync::Arc<lore_revision::interface::ExecutionContext> {
    std::sync::Arc::new(
        lore_revision::interface::ExecutionContext::new_client_with_user_id(
            lore_revision::interface::LoreGlobalArgs::default(),
            lore_revision::relay::EventDispatcher::no_dispatch(),
            "test-user".to_string(),
        ),
    )
}
