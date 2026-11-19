// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A file SWFS lists as modified that holds what the revision holds is named in the thaw ending
//! the operation that found it so, as a reset of it would be, which takes it off SWFS's list and the
//! record.
use std::sync::Arc;

use bytes::Bytes;
use lore_base::test_util::TempDir;
use lore_revision::fs::filesystem_provider::InstanceOperation;
use lore_revision::fs::swfs::api_interface::SwfsInterface;
use lore_revision::fs::swfs::filesystem::SwfsOperation;
use lore_revision::fs::swfs::frozen_files::FrozenSnapshot;
use lore_revision::fs::swfs::mount_resources::MountResources;
use lore_revision::fs::swfs::paths::MountPath;
use lore_revision::instance::store_current_anchor;
use lore_revision::instance::store_current_anchor_branch;
use lore_revision::lore::BranchId;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::NodeComparison;
use lore_storage::ContentHashes;

use super::fake_frozen::FakeFrozen;
use super::listing_fixture::Held;
use super::listing_fixture::address_of;
use super::listing_fixture::in_context;
use super::listing_fixture::node;
use super::listing_fixture::revision_state;
use super::listing_fixture::stored;
use super::listing_fixture::writable_test_repository;
use super::listing_fixture::written;

struct Fixture {
    operation: SwfsOperation,
    repository: Arc<RepositoryContext>,
    _mount: TempDir,
}

/// An operation over a mount whose stored revision holds `back.txt`, `edited.txt` and `staged.txt`
/// as `original`, and `origin.txt` and `renamed.txt` as `moved`. SWFS lists each as modified, and
/// froze `back.txt` written back to what the revision holds, `edited.txt` holding other bytes,
/// `staged.txt` holding what a staged state holds, and `renamed.txt` moved from `origin.txt`
/// holding what the revision holds at both, and `over.txt`, which the revision holds as
/// `original`, renamed there without bytes of its own from an origin SWFS lists no longer. The
/// mount kept a move to `kept.txt`, which SWFS froze holding what the revision holds there, and
/// SWFS lists `moved` as renamed, below which it froze `moved/in.txt` holding what the revision
/// holds there.
async fn fixture() -> Fixture {
    let mount = TempDir::new("swfs-unchanged");
    let repository = writable_test_repository().await;
    let revision = revision_state(
        &repository,
        &[
            ("back.txt", Held::File(b"original")),
            ("edited.txt", Held::File(b"original")),
            ("staged.txt", Held::File(b"original")),
            ("origin.txt", Held::File(b"moved")),
            ("renamed.txt", Held::File(b"moved")),
            ("over.txt", Held::File(b"original")),
            ("kept.txt", Held::File(b"original")),
            ("moved/in.txt", Held::File(b"original")),
        ],
    )
    .await;
    store_current_anchor(&repository, stored(&repository, &revision).await)
        .await
        .expect("storing the current anchor");
    store_current_anchor_branch(&repository, BranchId::from([5; 16]))
        .await
        .expect("storing the current branch");
    // Stored, as a staged state's content is, so a comparison can find it.
    revision_state(&repository, &[("staged.txt", Held::File(b"staged"))]).await;
    let mount_root = mount.path().join("mount");
    std::fs::create_dir_all(&mount_root).expect("making the mount directory");
    let mount_path = MountPath::new(&mount_root).expect("a mount path");
    let resources = MountResources::for_test(repository.clone(), Some(revision), mount_path).await;
    resources.modified().set_trusted(true);
    for path in [
        "back.txt",
        "edited.txt",
        "staged.txt",
        "renamed.txt",
        "over.txt",
        "kept.txt",
        "moved/in.txt",
    ] {
        resources.modified().record(written(path)).await;
    }
    resources
        .renamed()
        .moved("elsewhere.txt", "kept.txt", false, true);
    let frozen = FakeFrozen::default()
        .with_file("\\back.txt", b"original")
        .with_file("\\kept.txt", b"original")
        .with_file("\\edited.txt", b"edited")
        .with_file("\\staged.txt", b"staged")
        .with_file("\\renamed.txt", b"moved")
        .with_move("\\origin.txt", "\\renamed.txt")
        .with_unreadable_frozen_file("\\over.txt", 8)
        .with_renamed("\\over.txt")
        .with_file("\\moved\\in.txt", b"original")
        .with_renamed("\\moved");
    let snapshot = FrozenSnapshot::capture(Arc::new(frozen)).expect("listing the frozen files");
    Fixture {
        operation: SwfsOperation::for_test_frozen(
            SwfsInterface::unmounted(resources),
            &mount_root,
            snapshot,
        ),
        repository,
        _mount: mount,
    }
}

impl Fixture {
    /// Compares the file at `path` with `content`, as a diff compares a candidate with the node
    /// the revision holds there.
    async fn compare(&self, path: &str, content: &[u8]) -> NodeComparison {
        self.operation
            .file_holds_content(
                self.repository.clone(),
                &node(path),
                0,
                address_of(content),
                content.len() as u64,
                &ContentHashes::default(),
            )
            .await
            .expect("comparing the file")
    }

    /// The paths the thaw ending the operation names, once it finalizes, having `succeeded`. The
    /// operation froze nothing, so finalizing fails at the thaw, after it chose what to name.
    async fn named(&self, succeeded: bool) -> Vec<String> {
        let _ = self.operation.finalize(false, succeeded).await;
        self.operation.described_for_test()
    }
}

#[tokio::test]
async fn a_file_written_back_to_what_the_revision_holds_is_named() {
    in_context(Box::pin(async {
        let fixture = fixture().await;

        assert!(matches!(
            fixture.compare("back.txt", b"original").await,
            NodeComparison::Matches
        ));

        assert_eq!(fixture.named(true).await, ["back.txt"]);
    }))
    .await;
}

#[tokio::test]
async fn an_operation_that_fails_names_nothing_it_found_unchanged() {
    in_context(Box::pin(async {
        let fixture = fixture().await;
        fixture.compare("back.txt", b"original").await;

        assert_eq!(fixture.named(false).await, Vec::<String>::new());
    }))
    .await;
}

#[tokio::test]
async fn a_file_that_differs_is_not_named() {
    in_context(Box::pin(async {
        let fixture = fixture().await;

        assert!(matches!(
            fixture.compare("edited.txt", b"original").await,
            NodeComparison::Differs
        ));

        assert_eq!(fixture.named(true).await, Vec::<String>::new());
    }))
    .await;
}

/// A file found holding what a staged state holds, which the revision does not, would be described
/// as the revision holds it.
#[tokio::test]
async fn a_file_matching_other_content_than_the_revision_holds_is_not_named() {
    in_context(Box::pin(async {
        let fixture = fixture().await;

        assert!(matches!(
            fixture.compare("staged.txt", b"staged").await,
            NodeComparison::Matches
        ));

        assert_eq!(fixture.named(true).await, Vec::<String>::new());
    }))
    .await;
}

/// The thaw names a moved file with its origin, which would bring the origin back.
#[tokio::test]
async fn a_moved_file_is_not_named() {
    in_context(Box::pin(async {
        let fixture = fixture().await;

        assert!(matches!(
            fixture.compare("renamed.txt", b"moved").await,
            NodeComparison::Matches
        ));

        assert_eq!(fixture.named(true).await, Vec::<String>::new());
    }))
    .await;
}

/// A file renamed over another the revision holds, whose origin SWFS lists no longer, as after
/// the user made something there: its bytes are not the revision's at its path, so naming it would
/// replace the user's rename.
#[tokio::test]
async fn a_renamed_file_swfs_lists_no_origin_for_is_not_named() {
    in_context(Box::pin(async {
        let fixture = fixture().await;
        fixture.compare("over.txt", b"original").await;

        assert_eq!(fixture.named(true).await, Vec::<String>::new());
    }))
    .await;
}

/// A file below a directory SWFS lists as renamed holds bytes from below the directory's origin.
#[tokio::test]
async fn a_file_below_a_renamed_directory_is_not_named() {
    in_context(Box::pin(async {
        let fixture = fixture().await;
        fixture.compare("moved/in.txt", b"original").await;

        assert_eq!(fixture.named(true).await, Vec::<String>::new());
    }))
    .await;
}

/// A file a kept move leads to holds bytes from where it came from, however SWFS lists it.
#[tokio::test]
async fn a_file_a_kept_move_leads_to_is_not_named() {
    in_context(Box::pin(async {
        let fixture = fixture().await;
        fixture.compare("kept.txt", b"original").await;

        assert_eq!(fixture.named(true).await, Vec::<String>::new());
    }))
    .await;
}

/// What the operation wrote to the mount since it compared the file is left as it wrote it.
#[tokio::test]
async fn a_file_the_operation_wrote_since_is_not_named() {
    in_context(Box::pin(async {
        let fixture = fixture().await;
        fixture.compare("back.txt", b"original").await;
        fixture
            .operation
            .write_file(&node("back.txt"), Bytes::from_static(b"merged"))
            .await
            .expect("writing the file");

        assert_eq!(fixture.named(true).await, Vec::<String>::new());
    }))
    .await;
}
