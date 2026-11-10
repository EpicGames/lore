// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `FuseFilesystem` answering the `lore_fuse::Filesystem` trait directly, no mount: every test
//! builds a real committed `State` and calls the trait methods as the dispatcher would.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_fuse::abi::file::open_flags;
use lore_fuse::codec::dirent::DirentWriter;
use lore_fuse::filesystem::Filesystem;
use lore_fuse::reply::Errno;
use lore_fuse::reply::ReadFault;
use lore_fuse::reply::ReplyOutcome;
use lore_revision::filter::Filter;
use lore_revision::fs::fuse::attr::AttrContext;
use lore_revision::fs::fuse::filesystem::FuseFilesystem;
use lore_revision::fs::fuse::filesystem::OpenMode;
use lore_revision::fs::fuse::fragment_cache::DEFAULT_FRAGMENT_CACHE;
use lore_revision::fs::fuse::fragment_cache::FragmentCacheStrategy;
use lore_revision::fs::fuse::fragment_cache::LeafPolicy;
use lore_revision::fs::fuse::inode::ROOT_INO;
use lore_revision::fs::fuse::inode_number::Identity;
use lore_revision::fs::fuse::inode_number::IdentityKind;
use lore_revision::fs::fuse::inode_number::derive_inode_number;
use lore_revision::fs::fuse::listing::entry_flags;
use lore_revision::fs::fuse::listing_cache::LISTING_BLOCK_BYTES;
use lore_revision::fs::fuse::listing_cache::LISTING_CACHE_BYTES;
use lore_revision::fs::fuse::listing_cache::ListingCache;
use lore_revision::fs::fuse::read::FETCH_DEADLINE;
use lore_revision::fs::fuse::read::RecentMisses;
use lore_revision::fs::fuse::read::read_file;
use lore_revision::fs::fuse::spelling::DEFAULT_TIMEOUTS;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::InMemoryContext;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::state::Mark;
use lore_storage::FlushOptions;
use lore_storage::ImmutableStore;
use lore_storage::StoreError;
use lore_storage::StoreGetData;
use lore_storage::StoreMatchResult;
use lore_storage::StoreObliterateStats;
use lore_storage::hash::hash_string;

use crate::fs::fuse::fuse_fixture;
use crate::fs::fuse::fuse_fixture::RepositoryContextCreationArgsExt;
use crate::fs::fuse::fuse_test_util;
use crate::fs::fuse::fuse_test_util::req;

const BASE_TIME: u64 = 1_700_000_000_000;

/// A handle OPENDIR never returns: the stream reads the current listing unpinned.
const NO_HANDLE: u64 = 0;

fn attr_context() -> AttrContext {
    AttrContext {
        uid: 1000,
        gid: 1000,
        blksize: 1 << 17,
        base_time: BASE_TIME,
    }
}

/// Builds the provider inside an execution context and returns it outside of one, so every
/// later call also proves the provider carries the context it captured.
async fn provider(
    repository: Arc<RepositoryContext>,
    state: Arc<lore_revision::state::State>,
) -> Arc<FuseFilesystem> {
    lore_base::runtime::LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            FuseFilesystem::new(repository, state, attr_context()),
        )
        .await
        .expect("provider construction")
}

async fn mounted(files: &[(&str, &[u8])]) -> (fuse_fixture::FuseFixture, Arc<FuseFilesystem>) {
    let fixture = fuse_fixture::committed(files).await;
    let fs = provider(fixture.repository.clone(), fixture.state.clone()).await;
    (fixture, fs)
}

fn view_repository(
    fixture: &fuse_fixture::FuseFixture,
    exclusions: &[&str],
) -> Arc<RepositoryContext> {
    let mut filter = Filter::default();
    for glob in exclusions {
        match glob.strip_prefix('!') {
            Some(inclusion) => filter
                .view
                .add_inclusion(inclusion)
                .expect("filter inclusion"),
            None => filter.view.add_exclusion(glob).expect("filter exclusion"),
        }
    }
    derived_repository(fixture, fixture.repository.immutable_store(), Some(filter))
}

fn derived_repository(
    fixture: &fuse_fixture::FuseFixture,
    immutable_store: Arc<dyn ImmutableStore>,
    filter: Option<Filter>,
) -> Arc<RepositoryContext> {
    let mutable_store = fixture
        .repository
        .try_mutable_store_arc()
        .expect("fixture repository must hold a write token");
    let mut args = fuse_fixture::default_repository_creation_args(immutable_store, mutable_store)
        .with_path(&fixture.path)
        .with_id(fixture.repository.id)
        .with_instance_id(fixture.repository.instance_id);
    if let Some(filter) = filter {
        args = args.with_filter(Arc::new(filter));
    }
    Arc::new(RepositoryContext::new(args))
}

/// One decoded READDIR or READDIRPLUS record.
#[derive(Debug, Clone)]
struct Record {
    nodeid: u64,
    ino: u64,
    off: u64,
    kind: u32,
    name: Vec<u8>,
    entry_valid: u64,
}

fn u64_at(buf: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(buf[at..at + 8].try_into().expect("8 bytes"))
}

fn u32_at(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(buf[at..at + 4].try_into().expect("4 bytes"))
}

/// Decodes `fuse_dirent` records, each optionally prefixed by a `fuse_entry_out`.
fn decode(buf: &[u8], plus: bool) -> Vec<Record> {
    let entry_out = std::mem::size_of::<lore_fuse::abi::EntryOut>();
    let mut records = Vec::new();
    let mut at = 0;
    while at < buf.len() {
        let (nodeid, entry_valid) = if plus {
            let values = (u64_at(buf, at), u64_at(buf, at + 16));
            at += entry_out;
            values
        } else {
            (0, 0)
        };
        let ino = u64_at(buf, at);
        let off = u64_at(buf, at + 8);
        let namelen = u32_at(buf, at + 16) as usize;
        let kind = u32_at(buf, at + 20);
        let name = buf[at + 24..at + 24 + namelen].to_vec();
        at += (24 + namelen).div_ceil(8) * 8;
        records.push(Record {
            nodeid,
            ino,
            off,
            kind,
            name,
            entry_valid,
        });
    }
    records
}

async fn readdirplus(
    fs: &FuseFilesystem,
    ino: u64,
    offset: u64,
    capacity: usize,
) -> (Vec<Record>, usize) {
    let (page, emitted) = readdirplus_page(fs, ino, offset, capacity).await;
    (decode(&page, true), emitted)
}

/// One READDIRPLUS page as written, and how many lookups it counted.
async fn readdirplus_page(
    fs: &FuseFilesystem,
    ino: u64,
    offset: u64,
    capacity: usize,
) -> (Vec<u8>, usize) {
    let mut buffer = vec![0u8; capacity];
    let mut writer = DirentWriter::new(&mut buffer);
    let emitted = fs
        .readdirplus(&req(), ino, NO_HANDLE, offset, &mut writer)
        .await
        .expect("readdirplus");
    let used = writer.finish();
    buffer.truncate(used);
    (buffer, emitted)
}

async fn readdir(fs: &FuseFilesystem, ino: u64, offset: u64, capacity: usize) -> Vec<Record> {
    readdir_on(fs, ino, NO_HANDLE, offset, capacity).await
}

async fn readdir_on(
    fs: &FuseFilesystem,
    ino: u64,
    fh: u64,
    offset: u64,
    capacity: usize,
) -> Vec<Record> {
    let mut buffer = vec![0u8; capacity];
    let mut writer = DirentWriter::new(&mut buffer);
    fs.readdir(&req(), ino, fh, offset, &mut writer)
        .await
        .expect("readdir");
    let used = writer.finish();
    decode(&buffer[..used], false)
}

fn nlookup(fs: &FuseFilesystem, ino: u64) -> u64 {
    fs.inodes().get(ino).map_or(0, |entry| entry.nlookup)
}

/// Fails `page`, one READDIRPLUS reply as written, as the dispatcher reports an aborted one.
fn fail_reply(fs: &FuseFilesystem, page: &[u8]) {
    for ino in lore_fuse::codec::dirent::referenced_nodeids(page) {
        fs.reply_failed(&req(), &ReplyOutcome::Entry { ino });
    }
}

// ---------------------------------------------------------------------------------------------
// Task 38: directory and metadata ops
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn lookup_root_child_returns_entry_with_derived_ino_and_nlookup_one() {
    let (fixture, fs) = mounted(&[("a.txt", b"hello"), ("dir/b.txt", b"bee")]).await;
    // A time is recorded for the path, but only a swap records one a mount reports. Stored
    // here: the fixture's commit records none for a file written in the clock tick it reads.
    let path =
        lore_revision::util::path::RelativePath::new_from_initial_path("a.txt").expect("path");
    lore_base::runtime::LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            lore_revision::state::file_modified_time_store(
                fixture.repository.clone(),
                &path,
                BASE_TIME + 3_600_000,
            ),
        )
        .await;
    let recorded =
        lore_revision::state::file_modified_time(fixture.repository.clone(), &path).await;
    assert_ne!(recorded, 0);

    let reply = fs.lookup(&req(), ROOT_INO, b"a.txt").await.expect("lookup");

    let expected = derive_inode_number(&Identity {
        parent_ino: ROOT_INO,
        name: "a.txt",
        name_hash: hash_string("a.txt"),
        kind: IdentityKind::File,
    });
    assert_eq!(reply.ino, expected);
    assert_eq!(reply.attr.ino, expected);
    assert_eq!(reply.attr.size, 5);
    assert_eq!(reply.attr.mode, libc::S_IFREG | 0o444);
    // Attributes are seconds.
    assert_eq!(reply.attr.mtime, BASE_TIME / 1000);
    assert_eq!(
        u64::from(reply.attr.mtimensec),
        (BASE_TIME % 1000) * 1_000_000
    );
    assert_eq!(reply.generation, 0);
    assert_eq!(reply.timeouts.entry, DEFAULT_TIMEOUTS.entry);
    assert_eq!(nlookup(&fs, expected), 1);

    // Only the spelling the directory holds answers; the number derives from the folded name.
    let other_case = fs
        .lookup(&req(), ROOT_INO, b"DIR")
        .await
        .map(|reply| reply.ino);
    assert!(
        matches!(other_case, Ok(0) | Err(Errno::ENOENT)),
        "another spelling answers absent: {other_case:?}"
    );
    let dir = fs
        .lookup(&req(), ROOT_INO, b"dir")
        .await
        .expect("dir lookup");
    let expected_dir = derive_inode_number(&Identity {
        parent_ino: ROOT_INO,
        name: "dir",
        name_hash: hash_string("dir"),
        kind: IdentityKind::Dir,
    });
    assert_eq!(dir.ino, expected_dir);
    assert_eq!(dir.attr.mode, libc::S_IFDIR | 0o555);

    let again = fs
        .lookup(&req(), ROOT_INO, b"a.txt")
        .await
        .expect("second lookup");
    assert_eq!(again.ino, expected);
    assert_eq!(
        nlookup(&fs, expected),
        2,
        "a repeat lookup counts once more"
    );
}

/// Replaces the fixture state's last node block with a copy one reserved byte off, stored under
/// the block's own address as local corruption leaves it.
async fn corrupt_last_node_block(fixture: &fuse_fixture::FuseFixture) {
    let repository = fixture.repository.clone();
    let tree = fixture
        .state
        .tree(repository.clone())
        .await
        .expect("tree header");
    assert!(tree.block_count > 1, "a block the state has not loaded");
    let hashes = lore_revision::immutable::read(
        repository.clone(),
        Address::zero_context_hash(tree.hash_node),
        None,
        lore_storage::ReadOptions::default(),
    )
    .await
    .expect("node block list");
    let last = (tree.block_count as usize - 1) * size_of::<Hash>();
    let address = Address::zero_context_hash(Hash::from(&hashes[last..last + size_of::<Hash>()]));
    let (_, block) = lore_storage::load_fragment(
        repository.immutable_store(),
        repository.id,
        address,
        lore_storage::ReadOptions::default(),
        None,
    )
    .await
    .expect("node block");
    let mut corrupt = block.to_vec();
    // `NodeBlockData::reserved`, which nothing reads.
    corrupt[24] ^= 0x5a;
    repository
        .immutable_store()
        .put(
            repository.id,
            address,
            Fragment {
                flags: 0,
                size_payload: corrupt.len() as u32,
                size_content: corrupt.len() as u64,
            },
            Some(Bytes::from(corrupt)),
            true,
        )
        .await
        .expect("put");
}

#[tokio::test]
async fn listing_trusts_local_node_blocks() {
    let generated = fuse_fixture::generated_tree(700);
    let files: Vec<(&str, &[u8])> = generated
        .iter()
        .map(|(path, contents)| (path.as_str(), contents.as_slice()))
        .collect();
    let fixture = fuse_fixture::committed(&files).await;
    lore_base::runtime::LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            corrupt_last_node_block(&fixture),
        )
        .await;
    let fs = provider(fixture.repository.clone(), fixture.state.clone()).await;

    let dir = fs
        .lookup(&req(), ROOT_INO, b"d00000")
        .await
        .expect("directory lookup");
    let (records, _) = readdirplus(&fs, dir.ino, 0, 1 << 20).await;
    assert_eq!(records.len(), 702, "700 files, dot and dotdot");
}

#[tokio::test]
async fn lookup_unknown_is_enoent_and_records_negative() {
    let (_fixture, fs) = mounted(&[("a.txt", b"hello")]).await;

    let reply = fs
        .lookup(&req(), ROOT_INO, b"missing.h")
        .await
        .expect("negative entry");

    // A zero node id with a finite entry timeout is the kernel's cached ENOENT.
    assert_eq!(reply.ino, 0);
    assert!(reply.timeouts.entry > Duration::ZERO);
    let hash = hash_string("missing.h");
    let spellings = fs.spellings().take(ROOT_INO, hash).expect("recorded");
    assert!(spellings.negative);
    assert_eq!(&*spellings.seen[0], b"missing.h");
    assert_eq!(fs.inodes().len(), 1, "a miss creates no inode");
}

#[tokio::test]
async fn lookup_under_a_file_is_enotdir() {
    let (_fixture, fs) = mounted(&[("a.txt", b"hello")]).await;
    let file = fs.lookup(&req(), ROOT_INO, b"a.txt").await.expect("lookup");

    let result = fs.lookup(&req(), file.ino, b"x").await;

    assert_eq!(result.err(), Some(Errno::ENOTDIR));
}

#[tokio::test]
async fn getattr_root_is_dir() {
    let (_fixture, fs) = mounted(&[("a.txt", b"hello")]).await;

    let reply = fs.getattr(&req(), ROOT_INO).await.expect("getattr");

    assert_eq!(reply.attr.ino, ROOT_INO);
    assert_eq!(reply.attr.mode, libc::S_IFDIR | 0o555);
    assert_eq!(reply.attr.nlink, 1);
    assert_eq!(reply.timeout, DEFAULT_TIMEOUTS.attr);
    assert_eq!(fs.getattr(&req(), 424242).await.err(), Some(Errno::ENOENT));
}

#[tokio::test]
async fn readdirplus_emits_dot_dotdot_with_ino_zero_then_entries_in_hash_order() {
    let (_fixture, fs) = mounted(&[("a.txt", b"a"), ("b.txt", b"b"), ("sub/c.txt", b"c")]).await;

    let (records, emitted) = readdirplus(&fs, ROOT_INO, 0, 64 * 1024).await;

    assert_eq!(records[0].name, b".");
    assert_eq!(records[1].name, b"..");
    assert_eq!(records[0].nodeid, 0);
    assert_eq!(records[1].nodeid, 0);
    let names: Vec<&[u8]> = records[2..].iter().map(|r| r.name.as_slice()).collect();
    let snapshot = fs.served();
    let listing = fs.root_listing(&snapshot).await.expect("root listing");
    let expected: Vec<&[u8]> = (0..listing.len()).map(|i| listing.name(i)).collect();
    assert_eq!(names, expected, "entries follow the listing's hash order");
    assert_eq!(emitted, 3);
    for (position, record) in records.iter().enumerate() {
        assert_eq!(record.off, position as u64 + 1);
    }
    for record in &records[2..] {
        assert_ne!(record.nodeid, 0);
        assert_eq!(record.nodeid, record.ino);
        assert!(record.entry_valid > 0);
        let expected_kind = if record.name == b"sub" {
            lore_fuse::abi::DirentType::Dir as u32
        } else {
            lore_fuse::abi::DirentType::Reg as u32
        };
        assert_eq!(record.kind, expected_kind);
    }
}

#[tokio::test]
async fn readdirplus_increments_nlookup_per_emitted_entry_and_rollback_undoes_it() {
    let (_fixture, fs) = mounted(&[("a.txt", b"a"), ("b.txt", b"b"), ("c.txt", b"c")]).await;

    let (page, emitted) = readdirplus_page(&fs, ROOT_INO, 0, 64 * 1024).await;
    let records = decode(&page, true);
    assert_eq!(emitted, 3);
    for record in &records[2..] {
        assert_eq!(nlookup(&fs, record.nodeid), 1);
    }

    fail_reply(&fs, &page);
    for record in &records[2..] {
        assert!(
            fs.inodes().get(record.nodeid).is_none(),
            "rolled back to zero"
        );
    }
    assert_eq!(fs.inodes().len(), 1);

    // A buffer with room for `.`, `..` and exactly one entry counts exactly one lookup.
    let entry_out = std::mem::size_of::<lore_fuse::abi::EntryOut>();
    let record = |len: usize| entry_out + (24 + len).div_ceil(8) * 8;
    let capacity = record(1) + record(2) + record(5);
    let (records, emitted) = readdirplus(&fs, ROOT_INO, 0, capacity).await;
    assert_eq!(records.len(), 3);
    assert_eq!(emitted, 1);
    assert_eq!(nlookup(&fs, records[2].nodeid), 1);
    assert_eq!(
        fs.inodes().len(),
        2,
        "entries that did not fit take no inode"
    );

    // Resuming mid-listing rolls back only what that reply emitted.
    let (tail_page, emitted) = readdirplus_page(&fs, ROOT_INO, records[2].off, 64 * 1024).await;
    let tail = decode(&tail_page, true);
    assert_eq!(emitted, 2);
    fail_reply(&fs, &tail_page);
    assert_eq!(
        nlookup(&fs, records[2].nodeid),
        1,
        "earlier reply untouched"
    );
    for record in &tail {
        assert!(fs.inodes().get(record.nodeid).is_none());
    }

    let entry = fs.lookup(&req(), ROOT_INO, b"a.txt").await.expect("lookup");
    fs.reply_failed(&req(), &ReplyOutcome::Entry { ino: entry.ino });
    assert_eq!(nlookup(&fs, entry.ino), 0);
}

#[tokio::test]
async fn readdir_resumes_at_offset() {
    let files: Vec<(String, Vec<u8>)> = (0..40)
        .map(|i| (format!("file-{i:02}.txt"), vec![b'x'; i]))
        .collect();
    let borrowed: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(n, c)| (n.as_str(), c.as_slice()))
        .collect();
    let (_fixture, fs) = mounted(&borrowed).await;

    let full = readdir(&fs, ROOT_INO, 0, 64 * 1024).await;
    assert_eq!(full.len(), 42);
    assert_eq!(full[0].name, b".");
    assert_eq!(full[1].name, b"..");

    let mut resumed = Vec::new();
    let mut offset = 0;
    loop {
        let page = readdir(&fs, ROOT_INO, offset, 256).await;
        if page.is_empty() {
            break;
        }
        offset = page.last().expect("non-empty").off;
        resumed.extend(page);
    }
    let full_names: Vec<&Vec<u8>> = full.iter().map(|r| &r.name).collect();
    let resumed_names: Vec<&Vec<u8>> = resumed.iter().map(|r| &r.name).collect();
    assert_eq!(resumed_names, full_names);
    assert_eq!(fs.inodes().len(), 1, "plain READDIR creates no inodes");
}

#[tokio::test]
async fn two_streams_on_one_directory_across_a_swap_each_keep_their_own_listing() {
    let files: Vec<(String, Vec<u8>)> = (0..20)
        .map(|i| (format!("f{i:02}"), vec![b'y'; 3]))
        .collect();
    let borrowed: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(n, c)| (n.as_str(), c.as_slice()))
        .collect();
    let (_fixture, fs) = mounted(&borrowed).await;
    let other = fuse_fixture::committed(&[("zzz", b"z")]).await;

    let first_fh = fs.opendir(&req(), ROOT_INO).expect("opendir").fh;
    let first = readdir_on(&fs, ROOT_INO, first_fh, 0, 200).await;
    assert!(first.len() < 22);

    // Swap to an unrelated tree while the first stream is mid-listing.
    let tree = other
        .state
        .tree(other.repository.clone())
        .await
        .expect("tree");
    fs.publish(
        other.state.clone(),
        other.repository.clone(),
        Some(Arc::new(ListingCache::new(
            LISTING_CACHE_BYTES,
            LISTING_BLOCK_BYTES,
        ))),
        Some(&tree),
    );

    let second_fh = fs.opendir(&req(), ROOT_INO).expect("opendir").fh;
    assert_ne!(first_fh, second_fh);
    let second: Vec<Vec<u8>> = readdir_on(&fs, ROOT_INO, second_fh, 0, 64 * 1024)
        .await
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert_eq!(second, vec![b".".to_vec(), b"..".to_vec(), b"zzz".to_vec()]);

    let rest = readdir_on(
        &fs,
        ROOT_INO,
        first_fh,
        first.last().expect("page").off,
        64 * 1024,
    )
    .await;
    assert_eq!(
        first.len() + rest.len(),
        22,
        "the first stream stays on its listing"
    );
    fs.releasedir(ROOT_INO, first_fh);
    fs.releasedir(ROOT_INO, second_fh);
}

#[tokio::test]
async fn a_failed_readdirplus_reply_after_a_swap_gives_back_its_own_lookups() {
    let (_fixture, fs) = mounted(&[("a.txt", b"a"), ("b.txt", b"b")]).await;
    let other = fuse_fixture::committed(&[("zzz", b"z")]).await;

    let first_fh = fs.opendir(&req(), ROOT_INO).expect("opendir").fh;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut writer = DirentWriter::new(&mut buffer);
    let first_emitted = fs
        .readdirplus(&req(), ROOT_INO, first_fh, 0, &mut writer)
        .await
        .expect("readdirplus");
    let used = writer.finish();
    let first = buffer[..used].to_vec();
    assert_eq!(first_emitted, 2);
    let a = fs.lookup(&req(), ROOT_INO, b"a.txt").await.expect("a").ino;
    assert_eq!(nlookup(&fs, a), 2);

    let tree = other
        .state
        .tree(other.repository.clone())
        .await
        .expect("tree");
    fs.publish(
        other.state.clone(),
        other.repository.clone(),
        Some(Arc::new(ListingCache::new(
            LISTING_CACHE_BYTES,
            LISTING_BLOCK_BYTES,
        ))),
        Some(&tree),
    );
    let second_fh = fs.opendir(&req(), ROOT_INO).expect("opendir").fh;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut writer = DirentWriter::new(&mut buffer);
    fs.readdirplus(&req(), ROOT_INO, second_fh, 0, &mut writer)
        .await
        .expect("readdirplus");

    fail_reply(&fs, &first);
    assert_eq!(
        nlookup(&fs, a),
        1,
        "the rollback gives back the first reply's lookups, whatever the newer listing holds"
    );
    fs.releasedir(ROOT_INO, first_fh);
    fs.releasedir(ROOT_INO, second_fh);
}

struct InMemoryMarker;
impl InMemoryContext for InMemoryMarker {}
const IN_MEMORY_MARKER: InMemoryMarker = InMemoryMarker;

async fn add_node(
    state: &lore_revision::state::State,
    repository: Arc<RepositoryContext>,
    parent: NodeID,
    node: Node,
    name: &str,
) -> NodeID {
    let node_id = state
        .node_add(repository.clone(), parent, node, name)
        .await
        .expect("node add");
    state
        .node_mark(
            &repository,
            node_id,
            Mark::staged_and_dirty(NodeFlags::StagedAdd, NodeFlags::DirtyAdd),
        )
        .await
        .expect("mark staged");
    node_id
}

fn marked_address(marker: u64) -> Address {
    Address {
        hash: Hash::from_u64(marker),
        context: Context::default(),
    }
}

#[tokio::test]
async fn same_content_directories_under_a_path_specific_view_keep_their_own_masks() {
    let (immutable_store, mutable_store, execution) =
        fuse_fixture::test_store_create().await.expect("stores");
    let mut filter = Filter::default();
    filter
        .view
        .add_exclusion("a/x/secret.txt")
        .expect("exclusion");
    let repository = Arc::new(
        RepositoryContext::new(
            fuse_fixture::default_repository_creation_args(immutable_store, mutable_store)
                .with_filter(Arc::new(filter)),
        )
        .with_write_token(RepositoryWriteToken::in_memory(&IN_MEMORY_MARKER)),
    );

    // `a/x` and `b/x` share one content address, as a copied directory does; only the view
    // tells them apart.
    let state = lore_revision::state::State::new();
    let fs = lore_base::runtime::LORE_CONTEXT
        .scope(execution, async {
            for (top, marker) in [("a", 10u64), ("b", 11)] {
                let dir = Node {
                    address: marked_address(marker),
                    name_hash: hash_string(top),
                    ..Default::default()
                };
                let top_id = add_node(&state, repository.clone(), ROOT_NODE, dir, top).await;
                let x = Node {
                    address: marked_address(20),
                    name_hash: hash_string("x"),
                    ..Default::default()
                };
                let x_id = add_node(&state, repository.clone(), top_id, x, "x").await;
                for (name, marker) in [("keep.txt", 30u64), ("secret.txt", 31)] {
                    let file = Node {
                        flags: NodeFlags::File.bits(),
                        size: 1,
                        address: marked_address(marker),
                        name_hash: hash_string(name),
                        ..Default::default()
                    };
                    add_node(&state, repository.clone(), x_id, file, name).await;
                }
            }
            FuseFilesystem::new(repository.clone(), state.clone(), attr_context()).await
        })
        .await
        .expect("provider construction");

    let b = fs.lookup(&req(), ROOT_INO, b"b").await.expect("b");
    let bx = fs.lookup(&req(), b.ino, b"x").await.expect("b/x");
    let a = fs.lookup(&req(), ROOT_INO, b"a").await.expect("a");
    let ax = fs.lookup(&req(), a.ino, b"x").await.expect("a/x");

    // `b/x` is listed first, so a cache keyed by address alone would hand its mask to `a/x`.
    let b_secret = fs
        .lookup(&req(), bx.ino, b"secret.txt")
        .await
        .expect("lookup");
    assert_ne!(b_secret.ino, 0, "b/x/secret.txt is in the view");
    let a_secret = fs
        .lookup(&req(), ax.ino, b"secret.txt")
        .await
        .expect("lookup");
    assert_eq!(a_secret.ino, 0, "a/x/secret.txt is excluded");
    let again = fs
        .lookup(&req(), bx.ino, b"secret.txt")
        .await
        .expect("lookup");
    assert_eq!(again.ino, b_secret.ino);

    let a_names: Vec<Vec<u8>> = readdir(&fs, ax.ino, 0, 4096)
        .await
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert_eq!(
        a_names,
        vec![b".".to_vec(), b"..".to_vec(), b"keep.txt".to_vec()]
    );
    assert_eq!(readdir(&fs, bx.ino, 0, 4096).await.len(), 4);
}

#[tokio::test]
async fn opendir_replies_cache_dir_and_keep_cache() {
    let (_fixture, fs) = mounted(&[("dir/a.txt", b"a"), ("f.txt", b"f")]).await;

    let open = fs.opendir(&req(), ROOT_INO).expect("opendir");
    assert_eq!(open.flags, open_flags::CACHE_DIR | open_flags::KEEP_CACHE);
    assert_ne!(open.fh, NO_HANDLE);
    let other = fs.opendir(&req(), ROOT_INO).expect("opendir");
    assert_ne!(other.fh, open.fh, "each stream gets its own handle");
    fs.releasedir(ROOT_INO, open.fh);
    fs.releasedir(ROOT_INO, other.fh);

    let file = fs.lookup(&req(), ROOT_INO, b"f.txt").await.expect("lookup");
    assert_eq!(fs.opendir(&req(), file.ino).err(), Some(Errno::ENOTDIR));
    assert_eq!(fs.opendir(&req(), 999_999).err(), Some(Errno::ENOENT));
}

#[tokio::test]
async fn superseded_served_state_replies_zero_timeouts() {
    let (_fixture, fs) = mounted(&[("a.txt", b"a")]).await;
    let old = fs.served();
    let tree = old.state.tree(old.repository.clone()).await.expect("tree");
    fs.publish(old.state.clone(), old.repository.clone(), None, Some(&tree));
    assert!(old.superseded());

    let stale = fs
        .lookup_at(&old, ROOT_INO, b"a.txt")
        .await
        .expect("lookup");
    assert_eq!(stale.timeouts.entry, Duration::ZERO);
    assert_eq!(stale.timeouts.attr, Duration::ZERO);

    let fresh = fs.lookup(&req(), ROOT_INO, b"a.txt").await.expect("lookup");
    assert_eq!(fresh.ino, stale.ino);
    assert_eq!(fresh.timeouts.entry, DEFAULT_TIMEOUTS.entry);
}

#[tokio::test]
async fn view_excluded_child_is_enoent() {
    let fixture = fuse_fixture::committed(&[("keep.txt", b"k"), ("secret.txt", b"s")]).await;
    let repository = view_repository(&fixture, &["secret.txt"]);
    let fs = provider(repository, fixture.state.clone()).await;

    // Built outside any execution context: the listing build's filter events run under the
    // context the provider captured.
    let secret = fs
        .lookup(&req(), ROOT_INO, b"secret.txt")
        .await
        .expect("negative entry");
    assert_eq!(secret.ino, 0);
    let keep = fs
        .lookup(&req(), ROOT_INO, b"keep.txt")
        .await
        .expect("lookup");
    assert_ne!(keep.ino, 0);

    let names: Vec<Vec<u8>> = readdir(&fs, ROOT_INO, 0, 4096)
        .await
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert_eq!(
        names,
        vec![b".".to_vec(), b"..".to_vec(), b"keep.txt".to_vec()]
    );
    let (plus, emitted) = readdirplus(&fs, ROOT_INO, 0, 4096).await;
    assert_eq!(plus.len(), 3);
    assert_eq!(emitted, 1);

    let snapshot = fs.served();
    let listing = fs.root_listing(&snapshot).await.expect("listing");
    assert!(
        (0..listing.len()).any(|i| listing.entry(i).flags & entry_flags::VIEW_EXCLUDED != 0),
        "the excluded child is in the listing, only hidden"
    );
}

#[tokio::test]
async fn listing_for_directory_builds_from_the_node_its_inode_holds() {
    let (_fixture, fs) = mounted(&[
        ("top.txt", b"t"),
        ("outer/inner/leaf.txt", b"leaf"),
        ("outer/inner/other.txt", b"other"),
        ("outer/sibling.txt", b"s"),
    ])
    .await;
    // Inodes created from the root and `outer` listings; `inner`'s listing never built.
    let outer = fs.lookup(&req(), ROOT_INO, b"outer").await.expect("outer");
    let inner = fs.lookup(&req(), outer.ino, b"inner").await.expect("inner");

    let served = fs.served();
    let listing = fs
        .listing_for(&served, inner.ino)
        .await
        .expect("inner listing");

    let mut names: Vec<&[u8]> = (0..listing.len()).map(|i| listing.name(i)).collect();
    names.sort();
    assert_eq!(names, vec![&b"leaf.txt"[..], &b"other.txt"[..]]);

    let leaf = fs
        .lookup(&req(), inner.ino, b"leaf.txt")
        .await
        .expect("leaf");
    assert_eq!(leaf.attr.size, 4);
}

#[tokio::test]
async fn statfs_reports_namemax_and_tree_counts() {
    let (_fixture, fs) = mounted(&[("a.txt", b"hello"), ("b.txt", b"world!")]).await;

    let st = fs.statfs(&req()).expect("statfs");

    assert_eq!(st.namelen, 255);
    assert_eq!(st.bsize, 1 << 17);
    assert!(st.blocks >= 1, "tree size is reported");
    assert!(st.files >= 3, "node capacity is reported");
    assert_eq!(st.bfree, 0);
    assert_eq!(st.ffree, 0);
}

// ---------------------------------------------------------------------------------------------
// Task 39: OPEN, READ, RELEASE
// ---------------------------------------------------------------------------------------------

/// Deterministic, incompressible bytes, so content-defined chunking splits large files.
fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    fuse_test_util::incompressible(len, seed)
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

async fn read(
    fs: &FuseFilesystem,
    ino: u64,
    offset: u64,
    len: usize,
    fetch: bool,
) -> Result<Vec<u8>, ReadFault> {
    let mut out = vec![0u8; len];
    let n = fs.read(&req(), ino, 0, offset, &mut out, fetch).await?;
    out.truncate(n);
    Ok(out)
}

fn errno(fault: ReadFault) -> Errno {
    match fault {
        ReadFault::Errno(errno) => errno,
        ReadFault::WouldFetch => panic!("expected an errno, got WouldFetch"),
    }
}

#[tokio::test]
async fn open_is_zero_message_by_default() {
    let (_fixture, fs) = mounted(&[("a.txt", b"hello")]).await;
    let file = fs.lookup(&req(), ROOT_INO, b"a.txt").await.expect("lookup");

    assert_eq!(fs.open(&req(), file.ino, 0).err(), Some(Errno::ENOSYS));
    // READ needs no handle: the kernel sends fh 0 once OPEN answered ENOSYS.
    let got = read(&fs, file.ino, 0, 5, false)
        .await
        .map_err(errno)
        .expect("read");
    assert_eq!(got, b"hello");
}

#[tokio::test]
async fn open_replies_keep_cache_noflush_fh_zero() {
    let fixture = fuse_fixture::committed(&[("a.txt", b"hello"), ("dir/b", b"b")]).await;
    let fs = lore_base::runtime::LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            FuseFilesystem::new_with_open(
                fixture.repository.clone(),
                fixture.state.clone(),
                attr_context(),
                OpenMode::KeepCache,
            ),
        )
        .await
        .expect("provider construction");
    let file = fs.lookup(&req(), ROOT_INO, b"a.txt").await.expect("lookup");

    let open = fs
        .open(&req(), file.ino, libc::O_RDONLY as u32)
        .expect("open");

    assert_eq!(open.fh, 0);
    assert_eq!(open.flags, open_flags::KEEP_CACHE | open_flags::NOFLUSH);
    fs.release(file.ino, open.fh);
    assert_eq!(nlookup(&fs, file.ino), 1, "RELEASE touches no lookup count");

    let dir = fs.lookup(&req(), ROOT_INO, b"dir").await.expect("lookup");
    assert_eq!(fs.open(&req(), dir.ino, 0).err(), Some(Errno::EISDIR));
    assert_eq!(fs.open(&req(), 999_999, 0).err(), Some(Errno::ENOENT));
}

#[tokio::test]
async fn read_unfragmented_exact_bytes_at_unaligned_range() {
    let content = pattern(10_000);
    let (_fixture, fs) = mounted(&[("p.bin", &content)]).await;
    let file = fs.lookup(&req(), ROOT_INO, b"p.bin").await.expect("lookup");

    let got = read(&fs, file.ino, 1234, 777, false)
        .await
        .map_err(errno)
        .expect("local read");

    assert_eq!(got, &content[1234..1234 + 777]);
    let whole = read(&fs, file.ino, 0, 10_000, true)
        .await
        .map_err(errno)
        .expect("fetch read");
    assert_eq!(whole, content);
}

#[tokio::test]
async fn a_rolled_back_last_lookup_drops_the_inodes_fragment_cache_state() {
    let content = pseudo_random(2 << 20, 0x5eee);
    let (_fixture, fs) = mounted(&[("big.bin", &content)]).await;
    let file = fs
        .lookup(&req(), ROOT_INO, b"big.bin")
        .await
        .expect("lookup");
    read(&fs, file.ino, 0, 4096, false)
        .await
        .map_err(errno)
        .expect("local read");
    assert_eq!(fs.fragment_cache().inodes_held(), 1);

    fs.reply_failed(&req(), &ReplyOutcome::Entry { ino: file.ino });
    assert_eq!(nlookup(&fs, file.ino), 0);
    assert_eq!(fs.fragment_cache().inodes_held(), 0);
    assert_eq!(fs.fragment_cache().held_bytes(), 0);
}

#[tokio::test]
async fn read_fragmented_file_across_fragment_boundary() {
    let content = pseudo_random(2 << 20, 0x5eed);
    let (_fixture, fs) = mounted(&[("big.bin", &content)]).await;
    let file = fs
        .lookup(&req(), ROOT_INO, b"big.bin")
        .await
        .expect("lookup");

    // Spans of 600 KiB exceed any single leaf, so each crosses at least one boundary.
    for offset in [0usize, 100_003, 700_001, (2 << 20) - 600_000] {
        let got = read(&fs, file.ino, offset as u64, 600_000, false)
            .await
            .map_err(errno)
            .expect("local read");
        assert_eq!(got.len(), 600_000);
        assert!(
            got == content[offset..offset + 600_000],
            "bytes at {offset} differ"
        );
    }
}

#[tokio::test]
async fn read_past_eof_is_short() {
    let content = pattern(1000);
    let (_fixture, fs) = mounted(&[("p.bin", &content)]).await;
    let file = fs.lookup(&req(), ROOT_INO, b"p.bin").await.expect("lookup");

    let tail = read(&fs, file.ino, 997, 100, false)
        .await
        .map_err(errno)
        .expect("read");
    assert_eq!(tail, &content[997..]);
    let beyond = read(&fs, file.ino, 1000, 100, false)
        .await
        .map_err(errno)
        .expect("read");
    assert!(beyond.is_empty());
    let far = read(&fs, file.ino, 1 << 40, 100, false)
        .await
        .map_err(errno)
        .expect("read");
    assert!(far.is_empty());
}

#[tokio::test]
async fn read_of_directory_is_eisdir() {
    let (_fixture, fs) = mounted(&[("dir/a", b"a")]).await;
    let dir = fs.lookup(&req(), ROOT_INO, b"dir").await.expect("lookup");

    assert_eq!(
        read(&fs, dir.ino, 0, 10, false).await.err().map(errno),
        Some(Errno::EISDIR)
    );
    assert_eq!(
        read(&fs, ROOT_INO, 0, 10, true).await.err().map(errno),
        Some(Errno::EISDIR)
    );
}

#[tokio::test]
async fn read_of_remote_only_fragment_without_fetch_is_would_fetch() {
    let (_fixture, fs) = mounted(&[("a.txt", b"a")]).await;
    let snapshot = fs.served();
    let absent = Address {
        hash: Hash::from_u64(0xdead_beef),
        context: Context::default(),
    };
    let mut out = [0u8; 16];

    for strategy in [
        DEFAULT_FRAGMENT_CACHE,
        FragmentCacheStrategy::Storage,
        FragmentCacheStrategy::PerInode(LeafPolicy::Load),
        FragmentCacheStrategy::PerInode(LeafPolicy::Stored(1)),
        FragmentCacheStrategy::Global {
            budget: 1 << 20,
            decoded: true,
        },
    ] {
        fs.fragment_cache().set_strategy(strategy);
        for size in [100, 10 << 20] {
            let result = lore_base::runtime::LORE_CONTEXT
                .scope(
                    fuse_fixture::setup_test_execution(),
                    read_file(
                        &snapshot,
                        fs.fragment_cache(),
                        &RecentMisses::default(),
                        99,
                        absent,
                        size,
                        0,
                        &mut out,
                        FETCH_DEADLINE,
                        false,
                    ),
                )
                .await;
            assert!(
                matches!(result, Err(ReadFault::WouldFetch)),
                "{strategy:?} size {size}"
            );
        }
    }
}

/// Passes everything through until `stall` is set, then never answers a read.
struct StallingStore {
    inner: Arc<dyn ImmutableStore>,
    stall: AtomicBool,
}

impl std::fmt::Debug for StallingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StallingStore")
    }
}

impl StallingStore {
    async fn gate(&self) {
        if self.stall.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
    }
}

#[async_trait]
impl ImmutableStore for StallingStore {
    /// Joins the claim of the store this one wraps.
    async fn hold_for_command(
        self: Arc<Self>,
    ) -> Result<Option<lore_storage::local::store_lock::StoreGuard>, StoreError> {
        self.inner.clone().hold_for_command().await
    }

    fn is_local(&self) -> bool {
        self.inner.is_local()
    }

    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        self.gate().await;
        self.inner.clone().get(partition, address).await
    }

    async fn get_metadata(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        self.gate().await;
        self.inner.clone().get_metadata(partition, address).await
    }

    async fn query(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        results: &mut [StoreMatchResult],
    ) -> Result<(), StoreError> {
        self.gate().await;
        self.inner
            .clone()
            .query(partition, addresses, results)
            .await
    }

    async fn put(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .put(partition, address, fragment, payload, force)
            .await
    }

    async fn obliterate(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .obliterate(partition, address, stats)
            .await
    }

    async fn evict(
        self: Arc<Self>,
        max_capacity: usize,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<usize, StoreError> {
        self.inner
            .clone()
            .evict(max_capacity, sync_data, sink)
            .await
    }

    async fn compact(
        self: Arc<Self>,
        max_size: usize,
        at: Option<usize>,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, StoreError> {
        self.inner
            .clone()
            .compact(max_size, at, sync_data, sink)
            .await
    }

    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        self.inner.clone().compact_resume_at().await
    }

    fn max_query_batch(&self) -> Option<usize> {
        self.inner.max_query_batch()
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.inner.clone().flush(options).await
    }

    async fn verify(self: Arc<Self>, heal: bool) -> Result<(), StoreError> {
        self.inner.clone().verify(heal).await
    }

    async fn copy(
        self: Arc<Self>,
        source_partition: Partition,
        source_address: Address,
        destination_partition: Partition,
        destination_context: Context,
        behavior: lore_storage::immutable_store::CopyBehavior,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .copy(
                source_partition,
                source_address,
                destination_partition,
                destination_context,
                behavior,
            )
            .await
    }
}

#[tokio::test]
async fn read_deadline_elapsed_returns_eio() {
    let fixture = fuse_fixture::committed(&[("a.bin", &pattern(5000))]).await;
    let store = Arc::new(StallingStore {
        inner: fixture.repository.immutable_store(),
        stall: AtomicBool::new(false),
    });
    let repository = derived_repository(&fixture, store.clone(), None);
    let fs = provider(repository, fixture.state.clone()).await;
    let file = fs.lookup(&req(), ROOT_INO, b"a.bin").await.expect("lookup");

    store.stall.store(true, Ordering::SeqCst);
    fs.set_read_deadline(Duration::from_millis(1));
    let result = read(&fs, file.ino, 0, 100, true).await;

    assert_eq!(result.err().map(errno), Some(Errno::EIO));
}

#[tokio::test]
async fn plain_first_readdirplus_creates_no_inodes_until_a_file_is_looked_up() {
    let (_fixture, fs) = mounted(&[
        ("dir/a.txt", b"a"),
        ("dir/b.txt", b"b"),
        ("dir/sub/c", b"c"),
    ])
    .await;
    fs.set_readdirplus_plain_first(true);
    let dir = fs
        .lookup(&req(), ROOT_INO, b"dir")
        .await
        .expect("lookup")
        .ino;
    let inodes = fs.inodes().len();

    let (plain, emitted) = readdirplus(&fs, dir, 0, 64 * 1024).await;
    assert_eq!(emitted, 0, "no lookup counted");
    assert_eq!(fs.inodes().len(), inodes, "no inode created");
    let mut names: Vec<&[u8]> = plain.iter().map(|record| &record.name[..]).collect();
    names.sort_unstable();
    assert_eq!(names, [&b"."[..], b"..", b"a.txt", b"b.txt", b"sub"]);
    assert!(plain.iter().all(|record| record.nodeid == 0), "{plain:?}");

    fs.lookup(&req(), dir, b"a.txt").await.expect("lookup");
    let (plus, emitted) = readdirplus(&fs, dir, 0, 64 * 1024).await;
    assert_eq!(
        emitted, 3,
        "a looked-up file turns the directory back to READDIRPLUS"
    );
    assert!(
        plus[2..].iter().all(|record| record.nodeid != 0),
        "{plus:?}"
    );
}

/// Only a looked-up file advises its directory back to READDIRPLUS: a subdirectory's lookup does
/// not, and forgetting the directory drops the advice.
#[tokio::test]
async fn plain_first_advice_comes_from_files_and_ends_with_the_directory() {
    let (_fixture, fs) = mounted(&[("dir/a.txt", b"a"), ("dir/sub/c", b"c")]).await;
    fs.set_readdirplus_plain_first(true);
    let dir = fs
        .lookup(&req(), ROOT_INO, b"dir")
        .await
        .expect("lookup")
        .ino;

    let sub = fs.lookup(&req(), dir, b"sub").await.expect("lookup").ino;
    let (_, emitted) = readdirplus(&fs, dir, 0, 64 * 1024).await;
    assert_eq!(
        emitted, 0,
        "a subdirectory's lookup leaves the directory plain"
    );

    let file = fs.lookup(&req(), dir, b"a.txt").await.expect("lookup").ino;
    let (_, emitted) = readdirplus(&fs, dir, 0, 64 * 1024).await;
    assert_eq!(emitted, 2, "a file's lookup advises READDIRPLUS");

    fs.forget(file, 2);
    fs.forget(sub, 2);
    fs.forget(dir, 1);
    let again = fs
        .lookup(&req(), ROOT_INO, b"dir")
        .await
        .expect("lookup")
        .ino;
    let (_, emitted) = readdirplus(&fs, again, 0, 64 * 1024).await;
    assert_eq!(emitted, 0, "a forgotten directory holds no advice");
}

#[tokio::test]
async fn read_of_an_address_that_just_missed_its_deadline_fails_fast() {
    let fixture = fuse_fixture::committed(&[("a.bin", &pattern(5000))]).await;
    let store = Arc::new(StallingStore {
        inner: fixture.repository.immutable_store(),
        stall: AtomicBool::new(false),
    });
    let repository = derived_repository(&fixture, store.clone(), None);
    let fs = provider(repository, fixture.state.clone()).await;
    let file = fs.lookup(&req(), ROOT_INO, b"a.bin").await.expect("lookup");

    store.stall.store(true, Ordering::SeqCst);
    fs.set_read_deadline(Duration::from_millis(20));
    let missed = read(&fs, file.ino, 0, 100, true).await;
    assert_eq!(missed.err().map(errno), Some(Errno::EIO));

    // A retry that waited out its deadline again would take this long.
    fs.set_read_deadline(Duration::from_secs(30));
    let started = std::time::Instant::now();
    let retried = tokio::time::timeout(Duration::from_secs(5), read(&fs, file.ino, 0, 100, true))
        .await
        .expect("the retry fails without waiting out the deadline");
    assert_eq!(retried.err().map(errno), Some(Errno::EIO));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------------------------
// Task 40: bounded sequential readahead
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn sequential_reads_of_a_fragmented_file_start_one_readahead() {
    let big = pseudo_random(1 << 20, 0xabc);
    let small = pattern(4096);
    let (_fixture, fs) = mounted(&[("big.bin", &big), ("small.bin", &small)]).await;
    let file = fs
        .lookup(&req(), ROOT_INO, b"big.bin")
        .await
        .expect("lookup");
    let chunk = 128 << 10;

    for step in 0..4u64 {
        read(&fs, file.ino, step * chunk as u64, chunk, false)
            .await
            .map_err(errno)
            .expect("read");
    }
    assert_eq!(fs.readaheads_started(), 1);

    let small_file = fs
        .lookup(&req(), ROOT_INO, b"small.bin")
        .await
        .expect("lookup");
    read(&fs, small_file.ino, 0, 2048, false)
        .await
        .map_err(errno)
        .expect("read");
    read(&fs, small_file.ino, 2048, 2048, false)
        .await
        .map_err(errno)
        .expect("read");
    assert_eq!(
        fs.readaheads_started(),
        1,
        "a single-fragment file never reads ahead"
    );
}

#[tokio::test]
async fn random_reads_through_the_filesystem_start_no_readahead() {
    let big = pseudo_random(1 << 20, 0xdef);
    let (_fixture, fs) = mounted(&[("big.bin", &big)]).await;
    let file = fs
        .lookup(&req(), ROOT_INO, b"big.bin")
        .await
        .expect("lookup");

    for offset in [700_000u64, 10_000, 400_000, 0, 900_000] {
        read(&fs, file.ino, offset, 4096, false)
            .await
            .map_err(errno)
            .expect("read");
    }
    assert_eq!(fs.readaheads_started(), 0);
}

// ---------------------------------------------------------------------------------------------
// Zero allocations on the hot path
// ---------------------------------------------------------------------------------------------

struct CountingOff;
impl Drop for CountingOff {
    fn drop(&mut self) {
        lore_base::allocator::set_allocation_counting(false);
    }
}

fn counting() -> CountingOff {
    lore_base::allocator::set_allocation_counting(true);
    CountingOff
}

fn allocations() -> u64 {
    lore_base::allocator::thread_allocation_count()
}

#[tokio::test(flavor = "current_thread")]
async fn hot_requests_allocate_nothing_after_warm_up() {
    let (_fixture, fs) = mounted(&[("a.txt", b"a"), ("b.txt", b"b"), ("dir/c.txt", b"c")]).await;
    let mut buffer = vec![0u8; 64 * 1024];

    // Warm-up: build the root listing, create inodes, record spellings.
    let file = fs.lookup(&req(), ROOT_INO, b"a.txt").await.expect("lookup");
    let _ = fs.lookup(&req(), ROOT_INO, b"a.txt").await.expect("lookup");
    {
        let mut writer = DirentWriter::new(&mut buffer);
        fs.readdirplus(&req(), ROOT_INO, NO_HANDLE, 0, &mut writer)
            .await
            .expect("readdirplus");
    }
    let _ = fs.getattr(&req(), file.ino).await.expect("getattr");
    let _ = fs.statfs(&req()).expect("statfs");
    let _ = fs.open(&req(), file.ino, 0);
    fs.release(file.ino, 0);

    let _counting = counting();

    let before = allocations();
    let hit = fs.lookup(&req(), ROOT_INO, b"a.txt").await.expect("lookup");
    assert_eq!(allocations(), before, "LOOKUP hit on an existing inode");
    assert_eq!(hit.ino, file.ino);

    let before = allocations();
    let _ = fs.getattr(&req(), file.ino).await.expect("getattr");
    assert_eq!(allocations(), before, "GETATTR");

    let before = allocations();
    {
        let mut writer = DirentWriter::new(&mut buffer);
        let emitted = fs
            .readdirplus(&req(), ROOT_INO, NO_HANDLE, 0, &mut writer)
            .await
            .expect("readdirplus");
        assert_eq!(emitted, 3);
    }
    assert_eq!(allocations(), before, "READDIRPLUS from a cached listing");

    let before = allocations();
    let _ = fs.open(&req(), file.ino, 0);
    assert_eq!(allocations(), before, "OPEN");

    let before = allocations();
    fs.release(file.ino, 0);
    assert_eq!(allocations(), before, "RELEASE");

    let before = allocations();
    let _ = fs.statfs(&req()).expect("statfs");
    assert_eq!(allocations(), before, "STATFS");

    let before = allocations();
    fs.forget(file.ino, 1);
    fs.forget(file.ino, nlookup(&fs, file.ino));
    assert_eq!(allocations(), before, "FORGET");
    assert!(fs.inodes().get(file.ino).is_none());
}

/// The local forms answer only from resident listings: a cold directory is `WouldFetch`, with
/// nothing written and no lookup counted; once its listing is built they answer as the full
/// forms do.
#[tokio::test]
async fn local_lookup_and_readdir_defer_a_cold_listing_and_answer_a_resident_one() {
    let (_fixture, fs) = mounted(&[("a.txt", b"hello"), ("dir/b.txt", b"bee")]).await;
    let inodes = fs.inodes().len();
    let mut buffer = vec![0u8; 4096];

    let lookup = fs.lookup_local(&req(), ROOT_INO, b"a.txt").await;
    assert!(matches!(lookup, Err(ReadFault::WouldFetch)));
    let mut writer = DirentWriter::new(&mut buffer);
    let listed = fs
        .readdir_local(&req(), ROOT_INO, NO_HANDLE, 0, &mut writer)
        .await;
    assert!(matches!(listed, Err(ReadFault::WouldFetch)));
    assert_eq!(writer.used(), 0);
    let mut writer = DirentWriter::new(&mut buffer);
    let listed = fs
        .readdirplus_local(&req(), ROOT_INO, NO_HANDLE, 0, &mut writer)
        .await;
    assert!(matches!(listed, Err(ReadFault::WouldFetch)));
    assert_eq!(writer.used(), 0);
    assert_eq!(
        fs.inodes().len(),
        inodes,
        "a deferred request creates no inode"
    );

    let dir = fs.lookup(&req(), ROOT_INO, b"dir").await.expect("lookup");
    let lookup = fs.lookup_local(&req(), ROOT_INO, b"a.txt").await;
    assert!(matches!(lookup, Ok(ref entry) if entry.ino != 0));
    let mut writer = DirentWriter::new(&mut buffer);
    let listed = fs
        .readdirplus_local(&req(), ROOT_INO, NO_HANDLE, 0, &mut writer)
        .await;
    assert!(matches!(listed, Ok(2)), "a.txt and dir are listed");
    let lookup = fs.lookup_local(&req(), dir.ino, b"b.txt").await;
    assert!(
        matches!(lookup, Err(ReadFault::WouldFetch)),
        "dir's own listing is still cold"
    );
}

/// Attaches a node named `name` below `parent` in `fs`'s State: a file where `file`, else a
/// directory.
async fn attach_named(
    fs: &FuseFilesystem,
    repository: &Arc<RepositoryContext>,
    parent: NodeID,
    name: &str,
    file: bool,
) -> NodeID {
    let node = Node {
        flags: if file { NodeFlags::File.bits() } else { 0 },
        size: 1,
        ..Node::default()
    };
    match fs
        .state()
        .node_attach(repository.clone(), parent, node, name)
        .await
        .expect("attach")
    {
        lore_revision::state::AttachOutcome::Attached(node) => node,
        lore_revision::state::AttachOutcome::Exists(_) => panic!("{name} is held"),
    }
}

/// A directory attached since its parent's listing keeps the view state its inode took, however
/// often it is looked up again: under a view that excludes the directory and re-includes one of
/// its children, the others stay excluded.
#[tokio::test]
async fn a_directory_attached_since_the_listing_keeps_its_view_state() {
    let fixture = fuse_fixture::committed(&[("keep.txt", b"k")]).await;
    let repository = view_repository(&fixture, &["d2/", "!d2/keep.txt"]);
    let fs = provider(repository.clone(), fixture.state.clone()).await;
    fs.lookup(&req(), ROOT_INO, b"keep.txt")
        .await
        .expect("keep.txt");

    let d2 = attach_named(&fs, &repository, ROOT_NODE, "d2", false).await;
    let added = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = added.clone();
    fs.set_lookup_hook(Arc::new(move |step| {
        if step
            == (lore_revision::fs::fuse::filesystem::LookupStep::Added {
                dir: ROOT_NODE,
                node: d2,
            })
        {
            counted.fetch_add(1, Ordering::SeqCst);
        }
        Box::pin(async {})
    }));
    let first = fs.lookup(&req(), ROOT_INO, b"d2").await.expect("d2");
    assert_ne!(first.ino, 0, "d2 holds re-included content");
    attach_named(&fs, &repository, d2, "keep.txt", true).await;
    attach_named(&fs, &repository, d2, "drop.txt", true).await;
    let again = fs.lookup(&req(), ROOT_INO, b"d2").await.expect("d2");
    assert_eq!(again.ino, first.ino);
    assert_eq!(
        added.load(Ordering::SeqCst),
        2,
        "both lookups found d2 as attached since the root's listing"
    );

    let kept = fs
        .lookup(&req(), first.ino, b"keep.txt")
        .await
        .expect("keep.txt");
    assert_ne!(kept.ino, 0, "the re-included child is served");
    let dropped = fs.lookup(&req(), first.ino, b"drop.txt").await;
    assert!(
        dropped.is_err() || dropped.is_ok_and(|entry| entry.ino == 0),
        "the excluded child stays excluded"
    );
}

/// Names attached since their directory's listing that no listing serves answer absent, with
/// no inode made: a link, and one the view excludes. A name no FUSE entry carries never comes
/// this way: the State refuses `/`, NUL, `.` and `..`, and LOOKUP answers a name over 255 bytes
/// itself. The caller runs in no execution context: the event the exclusion reports goes out
/// under the one the provider captured.
#[tokio::test]
async fn names_attached_since_the_listing_that_listings_leave_out_answer_absent() {
    let fixture = fuse_fixture::committed(&[("keep.txt", b"k")]).await;
    let repository = view_repository(&fixture, &["drop.txt"]);
    let fs = provider(repository.clone(), fixture.state.clone()).await;
    fs.lookup(&req(), ROOT_INO, b"keep.txt")
        .await
        .expect("keep.txt");
    let link = Node {
        flags: NodeFlags::Link.bits(),
        mode: 0o755,
        ..Node::default()
    };
    fs.state()
        .node_attach(repository.clone(), ROOT_NODE, link, "vendor")
        .await
        .expect("attach the link");
    attach_named(&fs, &repository, ROOT_NODE, "drop.txt", true).await;
    let added = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = added.clone();
    fs.set_lookup_hook(Arc::new(move |step| {
        if matches!(
            step,
            lore_revision::fs::fuse::filesystem::LookupStep::Added { .. }
        ) {
            counted.fetch_add(1, Ordering::SeqCst);
        }
        Box::pin(async {})
    }));
    let inodes = fs.inodes().len();

    for name in ["vendor", "drop.txt"] {
        let found = fs.lookup(&req(), ROOT_INO, name.as_bytes()).await;
        assert!(
            found.is_err() || found.is_ok_and(|entry| entry.ino == 0),
            "{name} is absent"
        );
    }
    assert_eq!(
        added.load(Ordering::SeqCst),
        2,
        "each lookup read a node attached since the listing"
    );
    assert_eq!(fs.inodes().len(), inodes, "no inode was made");
}

/// The node blocks cached listings hold count against the cache's budget once each, however many
/// listings share one, and are given back when the last listing holding them leaves.
#[tokio::test]
async fn pinned_blocks_are_charged_once_and_released_with_the_last_listing() {
    let (fixture, fs) = mounted(&[("a/x.txt", b"x"), ("b/y.txt", b"y")]).await;
    let a = fs.lookup(&req(), ROOT_INO, b"a").await.expect("a").ino;
    let b = fs.lookup(&req(), ROOT_INO, b"b").await.expect("b").ino;
    fs.lookup(&req(), a, b"x.txt").await.expect("x.txt");
    fs.lookup(&req(), b, b"y.txt").await.expect("y.txt");

    let served = fs.served();
    let block = served
        .state
        .block(fixture.repository.clone(), 0)
        .await
        .expect("block 0");
    assert_eq!(served.listings.len(), 3, "the root, a and b are listed");
    assert_eq!(
        served.listings.pinned_bytes(),
        block.memory_bytes(),
        "three listings sharing one block charge it once"
    );
    let node = async |name: &str| {
        served
            .state
            .find_subnode(fixture.repository.clone(), ROOT_NODE, hash_string(name))
            .await
            .expect("node")
    };
    served.listings.remove(node("a").await);
    served.listings.remove(node("b").await);
    assert_eq!(served.listings.pinned_bytes(), block.memory_bytes());
    served.listings.remove(ROOT_NODE);
    assert_eq!(
        served.listings.pinned_bytes(),
        0,
        "the last listing gives the charge back"
    );
}

/// A block's charge is what it held when first pinned: names attached into it while a listing
/// holds it do not change what the last release gives back.
#[tokio::test]
async fn a_pinned_block_gives_back_exactly_its_charge_after_attaches() {
    let (fixture, fs) = mounted(&[("a.txt", b"a")]).await;
    // New nodes take a fresh block, which a root listing built after the first one holds.
    let first = attach_named(&fs, &fixture.repository, ROOT_NODE, "first", true).await;
    fs.served().listings.remove(ROOT_NODE);
    fs.lookup(&req(), ROOT_INO, b"a.txt").await.expect("a.txt");
    let served = fs.served();
    let stored = served
        .state
        .block(fixture.repository.clone(), 0)
        .await
        .expect("block 0");
    let block = served
        .state
        .block(
            fixture.repository.clone(),
            lore_revision::node::NodeBlock::index(first),
        )
        .await
        .expect("the fresh block");
    let charged = block.memory_bytes();
    assert_eq!(
        served.listings.pinned_bytes(),
        stored.memory_bytes() + charged,
        "the root listing pins block 0 and the fresh block"
    );

    for i in 0..64 {
        let name = format!("{i:03}-{}", "n".repeat(200));
        attach_named(&fs, &fixture.repository, ROOT_NODE, &name, true).await;
    }
    assert!(
        block.memory_bytes() > charged,
        "the attaches grew the fresh block's name table"
    );
    fs.served().listings.remove(ROOT_NODE);
    assert_eq!(
        served.listings.pinned_bytes(),
        0,
        "exactly the charge comes back"
    );
}
