// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Allocations of a warm LOOKUP answered by the provider directly, no mount, counted on the
//! thread answering.

use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_fuse::abi::ROOT_ID;
use lore_revision::fs::fuse::attr::AttrContext;
use lore_revision::fs::fuse::filesystem::FuseFilesystem;

use crate::fs::fuse::fuse_fixture;
use crate::fs::fuse::fuse_test_util;

const MAX_READ: u32 = 1 << 17;

struct DisableCountingOnDrop;

impl Drop for DisableCountingOnDrop {
    fn drop(&mut self) {
        lore_base::allocator::set_allocation_counting(false);
    }
}

/// A provider over `files` under `repository`, not mounted, with `dir`'s listing built and the
/// name `attached`, longer than a name held inline, attached to `dir` since and looked up once.
async fn provider_with_an_attached_name(
    fixture: &fuse_fixture::FuseFixture,
    repository: Arc<lore_revision::repository::RepositoryContext>,
    attached: &str,
) -> (Arc<FuseFilesystem>, u64) {
    use lore_fuse::filesystem::Filesystem as _;
    let fs = FuseFilesystem::new(
        repository.clone(),
        fixture.state.clone(),
        AttrContext::for_mounting_user(MAX_READ, 1_700_000_000_000),
    )
    .await
    .expect("provider construction");
    let request = fuse_test_util::req();
    let dir = fs.lookup(&request, ROOT_ID, b"dir").await.expect("dir").ino;
    fs.lookup(&request, dir, b"held.txt")
        .await
        .expect("held.txt");
    let state = fs.state();
    let dir_node = state
        .find_subnode(
            repository.clone(),
            lore_revision::node::ROOT_NODE,
            lore_storage::hash::hash_string("dir"),
        )
        .await
        .expect("dir node");
    let node = lore_revision::node::Node {
        flags: lore_revision::node::NodeFlags::File.bits(),
        size: 5,
        ..lore_revision::node::Node::default()
    };
    state
        .node_attach(repository, dir_node, node, attached)
        .await
        .expect("attach");
    let first = fs
        .lookup(&request, dir, attached.as_bytes())
        .await
        .expect("the attached name");
    assert_ne!(first.ino, 0, "the attached name is found");
    let listing = fs.served().listings.get(dir_node).expect("dir's listing");
    assert!(
        matches!(
            listing.lookup(attached),
            lore_revision::fs::fuse::listing::Hit::Added(..)
        ),
        "the cached listing finds the name through its log, as attached since the build"
    );
    (fs, dir)
}

/// Counts the allocations of one LOOKUP of `name` in `dir`.
async fn lookup_allocations(fs: &FuseFilesystem, dir: u64, name: &str) -> u64 {
    use lore_fuse::filesystem::Filesystem as _;
    let request = fuse_test_util::req();
    lore_base::allocator::set_allocation_counting(true);
    let _restore = DisableCountingOnDrop;
    let before = lore_base::allocator::thread_allocation_count();
    let reply = fs
        .lookup(&request, dir, name.as_bytes())
        .await
        .expect("lookup");
    let after = lore_base::allocator::thread_allocation_count();
    assert_ne!(reply.ino, 0);
    after - before
}

/// Counts the allocations and the State runtime-lock reads of one LOOKUP of `name` in `dir`.
async fn lookup_allocations_and_runtime_reads(
    fs: &FuseFilesystem,
    dir: u64,
    name: &str,
) -> (u64, u64) {
    use lore_revision::state::lock_counting;
    struct StopRuntimeReadCounting;
    impl Drop for StopRuntimeReadCounting {
        fn drop(&mut self) {
            lock_counting::set_runtime_read_counting(false);
        }
    }
    lock_counting::set_runtime_read_counting(true);
    let _stop = StopRuntimeReadCounting;
    let reads = lock_counting::thread_runtime_read_count();
    let allocations = lookup_allocations(fs, dir, name).await;
    (
        allocations,
        lock_counting::thread_runtime_read_count() - reads,
    )
}

const LONG_NAME: &str = "a-name-well-past-what-a-name-holds-inline.o";

#[tokio::test(flavor = "current_thread")]
async fn warm_lookup_of_a_long_name_attached_since_the_listing_allocates_zero() {
    let fixture = fuse_fixture::committed(&[("dir/held.txt", b"held")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let (fs, dir) =
                provider_with_an_attached_name(&fixture, fixture.repository.clone(), LONG_NAME)
                    .await;
            assert_eq!(
                lookup_allocations_and_runtime_reads(&fs, dir, LONG_NAME).await,
                (0, 0),
                "a warm LOOKUP of a name attached since the listing: (allocations, runtime reads)"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn warm_lookup_of_a_name_attached_since_the_listing_under_a_view_allocates_zero() {
    use fuse_fixture::RepositoryContextCreationArgsExt as _;
    let fixture = fuse_fixture::committed(&[("dir/held.txt", b"held"), ("other/x", b"x")]).await;
    let mut filter = lore_revision::filter::Filter::default();
    filter
        .view
        .add_exclusion("other/")
        .expect("filter exclusion");
    let mutable_store = fixture
        .repository
        .try_mutable_store_arc()
        .expect("fixture repository must hold a write token");
    let viewed = Arc::new(lore_revision::repository::RepositoryContext::new(
        fuse_fixture::default_repository_creation_args(
            fixture.repository.immutable_store(),
            mutable_store,
        )
        .with_path(&fixture.path)
        .with_id(fixture.repository.id)
        .with_instance_id(fixture.repository.instance_id)
        .with_filter(Arc::new(filter)),
    ));
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let (fs, dir) = provider_with_an_attached_name(&fixture, viewed, LONG_NAME).await;
            assert_eq!(lookup_allocations(&fs, dir, LONG_NAME).await, 0);
        })
        .await;
}
