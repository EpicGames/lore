// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures::FutureExt;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_base::types::Address;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_base::types::TypedBytes;
use lore_revision::change::NodeChangeState;
use lore_revision::filter::FilterMode;
use lore_revision::immutable::may_cache;
use lore_revision::node::NodeBlockData;
use lore_revision::node::NodeBlockFormat;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RemoteState;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::prefetch::*;
use lore_revision::state::NodeMapping;
use lore_revision::state::State;
use lore_revision::state::diff_collect_subtree;
use lore_revision::test_util::context_with_state;
use lore_revision::util::path::RelativePath;
use lore_storage::ReadOptions;
use lore_storage::hash::hash_slice;
use lore_transport::Connection;
use lore_transport::ProtocolError;
use tokio_util::sync::CancellationToken;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

use crate::fs::filesystem_provider::setup_test_execution;
use crate::helper::in_memory;
use crate::immutable::serving::Serving;
use crate::immutable::serving::ServingStorage;
use crate::immutable::serving::serving_remote;
use crate::immutable::serving::until;
use crate::paged_remote::PagedRemote;
use crate::paged_remote::Reads;
use crate::paged_remote::client_on_storage;
use crate::paged_remote::client_stores;
use crate::paged_remote::counted_client_stores;

fn spelled(paths: &[RecordedPath]) -> Vec<(String, bool)> {
    paths
        .iter()
        .map(|recorded| (recorded.path.as_str().to_string(), recorded.directory))
        .collect()
}

#[test]
fn recording_keeps_first_access_order_and_drops_repeats() {
    let (parsed, _) = parse_recording(b"b.txt\na/\na/c.txt\nb.txt\na/\n");
    assert_eq!(
        spelled(&parsed),
        vec![
            ("b.txt".to_string(), false),
            ("a".to_string(), true),
            ("a/c.txt".to_string(), false),
        ]
    );
}

#[test]
fn recording_reads_the_root_and_directories_by_their_trailing_slash() {
    let (parsed, _) = parse_recording(b"/\nEngine/\nEngine/Build.version\n");
    assert_eq!(
        spelled(&parsed),
        vec![
            (String::new(), true),
            ("Engine".to_string(), true),
            ("Engine/Build.version".to_string(), false),
        ]
    );
}

#[test]
fn recording_skips_blank_lines_and_paths_that_leave_the_tree() {
    let (parsed, _) = parse_recording(b"\r\n\nkeep.txt\r\n../outside.txt\nC:/drive.txt\n");
    assert_eq!(spelled(&parsed), vec![("keep.txt".to_string(), false)]);
}

/// A path is the line as spelled, spaces included, and paths are compared as parsed: a leading `/`
/// or a doubled separator repeats a path recorded before.
#[test]
fn recording_keeps_spaces_and_compares_paths_as_parsed() {
    let (parsed, _) = parse_recording(b" spaced .txt \n/a\na\na//b\na/b\n");
    assert_eq!(
        spelled(&parsed),
        vec![
            (" spaced .txt ".to_string(), false),
            ("a".to_string(), false),
            ("a/b".to_string(), false),
        ]
    );
}

/// A line that is not UTF-8, or names the root as a file, is skipped, and the lines around it are
/// read.
#[test]
fn recording_skips_a_line_not_utf8_or_naming_the_root_as_a_file() {
    let (parsed, _) = parse_recording(b"keep.txt\n\xff\xfe.txt\n.\nalso.txt\n");
    assert_eq!(
        spelled(&parsed),
        vec![
            ("keep.txt".to_string(), false),
            ("also.txt".to_string(), false)
        ]
    );
}

#[test]
fn a_file_and_a_directory_of_one_name_are_both_kept() {
    let (parsed, _) = parse_recording(b"a\na/\n");
    assert_eq!(
        spelled(&parsed),
        vec![("a".to_string(), false), ("a".to_string(), true)]
    );
}

#[test]
fn recording_reads_block_hints_after_a_tab_in_first_need_order_each_once() {
    let (parsed, blocks) = parse_recording(b"/\t3 1\nkeep/\t1 7 x\nkeep/a.txt\n");
    assert_eq!(
        spelled(&parsed),
        vec![
            (String::new(), true),
            ("keep".to_string(), true),
            ("keep/a.txt".to_string(), false),
        ]
    );
    assert_eq!(blocks, vec![3, 1, 7]);
}

/// A node block's content names its name table where it is in the current format and names one;
/// content of another length, a newer format or a zero name table names none.
#[test]
fn a_node_block_names_its_name_table_in_the_current_format() {
    let mut block = NodeBlockData::new_box_zeroed().expect("a zeroed node block");
    let name_table = Hash::from([7u8; 32]);
    block.version = NodeBlockFormat::Nametable as u32;
    block.name_table = name_table;
    assert_eq!(
        name_table_of(block.as_bytes()),
        Some(Address::zero_context_hash(name_table))
    );
    assert_eq!(name_table_of(&block.as_bytes()[1..]), None);

    block.version = NodeBlockFormat::NoTimestamp as u32;
    assert_eq!(name_table_of(block.as_bytes()), None);

    block.version = NodeBlockFormat::Nametable as u32;
    block.name_table = Hash::default();
    assert_eq!(name_table_of(block.as_bytes()), None);
}

#[test]
fn tree_prefetch_parses_levels_all_and_blocks() {
    assert_eq!(TreePrefetch::parse("0"), Some(TreePrefetch::Off));
    let levels = |count| TreePrefetch::Levels(NonZeroU32::new(count).expect("a level count"));
    assert_eq!(TreePrefetch::parse("2"), Some(levels(2)));
    assert_eq!(TreePrefetch::parse(" 5 "), Some(levels(5)));
    assert_eq!(TreePrefetch::parse("all"), Some(TreePrefetch::View));
    assert_eq!(TreePrefetch::parse("ALL"), Some(TreePrefetch::View));
    assert_eq!(TreePrefetch::parse("blocks"), Some(TreePrefetch::Blocks));
    assert_eq!(TreePrefetch::parse("-1"), None);
    assert_eq!(TreePrefetch::parse("deep"), None);
    assert_eq!(TreePrefetch::parse(""), None);
}

/// A file of at least `len` bytes in a temporary directory named after `test`, alive for as long as
/// the directory is: `lines` distinct file paths of 66 bytes and a line end each, then a hole up to
/// `len`.
#[allow(clippy::disallowed_methods)] // A test fixture in its own temporary directory.
fn prefetch_file(test: &str, lines: usize, len: u64) -> (TempDir, PathBuf) {
    let dir = TempDir::new(test);
    let path = dir.path().join("recording.txt");
    let text: String = (0..lines)
        .map(|line| format!("dir/{line:058}.txt\n"))
        .collect();
    std::fs::write(&path, text).expect("write the recording");
    std::fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|file| file.set_len(len.max(67 * lines as u64)))
        .expect("size the recording");
    (dir, path)
}

/// A recording of 160,000 lines of 67 bytes, 10.7 MB, past lore-io's 8 MiB whole-file limit, is
/// read whole.
#[tokio::test]
async fn a_recording_past_the_whole_file_limit_is_read() {
    let (_dir, path) = prefetch_file("a_recording_past_the_whole_file_limit_is_read-", 160_000, 0);
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let source = path.to_str().expect("a UTF-8 path");
            let plan = plan(Some(TreePrefetch::Off), Some(source)).await;
            let recording = plan.recording.expect("the recording is read");
            assert_eq!(recording.paths.len(), 160_000);
        })
        .await;
}

/// A recording whose hole takes it past the prefetch file limit is refused on its length alone,
/// before anything is read.
#[tokio::test]
async fn a_recording_past_the_prefetch_file_limit_is_refused() {
    let (_dir, path) = prefetch_file(
        "a_recording_past_the_prefetch_file_limit_is_refused-",
        1,
        PREFETCH_FILE_LIMIT as u64 + 1,
    );
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let source = path.to_str().expect("a UTF-8 path");
            let plan = plan(Some(TreePrefetch::Off), Some(source)).await;
            assert!(plan.recording.is_none());
        })
        .await;
}

/// The blocks two lists hold differently are named in index order, the from side's first: a block
/// one side holds at an index past the other's end, or where the other holds nothing, is named
/// alone, and a block both hold alike is not named.
#[test]
fn the_differing_blocks_are_named_in_index_order_from_side_first() {
    let hash = Hash::from_u64;
    let from = [hash(1), hash(2), hash(3), Hash::default()];
    let to = [hash(1), hash(4), Hash::default(), hash(5), hash(6)];

    assert_eq!(
        differing_blocks(&from, &to).collect::<Vec<_>>(),
        [hash(2), hash(4), hash(3), hash(5), hash(6)].map(Address::zero_context_hash)
    );
}

/// Directories of the tree [`two_revisions`] commits, each of [`FILES_PER_DIRECTORY`] files: with
/// the root, more nodes than three node blocks hold.
const DIRECTORIES: usize = 4;
const FILES_PER_DIRECTORY: usize = 400;

/// The number of the next tree [`two_revisions`] commits, which every name in the tree carries.
/// Read flights join on content across tests, so each tree's name tables are its own.
static NEXT_TREE: AtomicU32 = AtomicU32::new(0);

/// A source repository holding two revisions of a tree of several node blocks, the second changing
/// the file at `changed`, the last of the last directory.
struct TwoRevisions {
    source: Arc<RepositoryContext>,
    branch: BranchId,
    first: Hash,
    second: Hash,
    changed: String,
}

async fn two_revisions(source: Arc<RepositoryContext>) -> TwoRevisions {
    let tree = NEXT_TREE.fetch_add(1, Ordering::Relaxed);
    let directory_name = |directory: usize| format!("t{tree}d{directory}");
    let file_name = |file: usize| format!("t{tree}f{file}.bin");
    let branch = in_memory::branch_id();
    let state = State::new();
    let mut last = ROOT_NODE;
    for directory in 0..DIRECTORIES {
        let name = directory_name(directory);
        let parent = in_memory::add(
            &state,
            source.clone(),
            ROOT_NODE,
            in_memory::directory(&name),
            &name,
        )
        .await;
        for file in 0..FILES_PER_DIRECTORY {
            let name = file_name(file);
            last = in_memory::add(
                &state,
                source.clone(),
                parent,
                in_memory::file(&name),
                &name,
            )
            .await;
        }
    }
    let first = in_memory::commit(&source, &state, Hash::default(), branch).await;
    let state = in_memory::revision_state(&source, first).await;
    in_memory::modify(
        &state,
        &source,
        last,
        Address::zero_context_hash(hash_slice(b"changed")),
        NodeFlags::StagedModify,
    )
    .await;
    let second = in_memory::commit(&source, &state, first, branch).await;
    TwoRevisions {
        source,
        branch,
        first,
        second,
        changed: format!(
            "{}/{}",
            directory_name(DIRECTORIES - 1),
            file_name(FILES_PER_DIRECTORY - 1)
        ),
    }
}

/// [`two_revisions`] on in-memory stores of their own.
async fn two_revisions_in_memory() -> TwoRevisions {
    let (immutable, mutable) = lore_revision::repository::create_client_memory_stores()
        .await
        .expect("in-memory stores should be creatable");
    two_revisions(in_memory::repository(immutable, mutable)).await
}

impl TwoRevisions {
    /// A client on a cold store whose remote serves the source's store as `serving` says, and the
    /// two revisions' states, read on the client.
    async fn on_a_cold_client(
        &self,
        serving: Serving,
    ) -> (
        Arc<RepositoryContext>,
        Arc<ServingStorage>,
        Arc<State>,
        Arc<State>,
    ) {
        let remote = serving_remote(&self.source, serving);
        let client = client_on_storage(
            &client_stores().await,
            Arc::new(PagedRemote::new(self.branch, self.second)),
            vec![remote.clone()],
        );
        let read = async |revision| {
            State::deserialize(client.clone(), revision)
                .await
                .expect("the client reads the revision from the remote")
        };
        let (from, to) = (read(self.first).await, read(self.second).await);
        (client, remote, from, to)
    }

    /// The node block address lists of the two revisions, as the source holds them.
    async fn block_lists(&self) -> (Vec<Hash>, Vec<Hash>) {
        let list = async |revision| {
            in_memory::revision_state(&self.source, revision)
                .await
                .node_block_address_list(self.source.clone())
                .await
                .expect("the source reads the block address list")
                .as_type_slice::<Hash>()
                .to_vec()
        };
        (list(self.first).await, list(self.second).await)
    }

    /// The node block addresses the two revisions hold differently at the same index, both sides',
    /// and those they hold alike.
    async fn differing_and_shared_blocks(&self) -> (BTreeSet<Address>, BTreeSet<Address>) {
        let (from, to) = self.block_lists().await;
        let (mut differing, mut shared) = (BTreeSet::new(), BTreeSet::new());
        let address = |hash: &Hash| (!hash.is_zero()).then(|| Address::zero_context_hash(*hash));
        for (from, to) in from.iter().zip(&to) {
            if from == to {
                shared.extend(address(from));
            } else {
                differing.extend(address(from).into_iter().chain(address(to)));
            }
        }
        (differing, shared)
    }
}

/// The name tables the node blocks at `blocks` name, read from `source`'s store.
async fn name_tables_of(
    source: &Arc<RepositoryContext>,
    blocks: &BTreeSet<Address>,
) -> BTreeSet<Address> {
    let mut name_tables = BTreeSet::new();
    for &block in blocks {
        let (_, content) = lore_storage::read::load_fragment(
            source.immutable_store(),
            source.id,
            block,
            ReadOptions::default(),
            None,
        )
        .await
        .expect("the source holds the block");
        name_tables.extend(name_table_of(&content));
    }
    name_tables
}

/// A warm-up for a diff of two trees asks the remote for the node blocks whose addresses differ
/// between them, both sides', each with its name table and without its file-metadata block, and
/// for no block the two hold alike.
#[tokio::test]
async fn a_diff_warm_up_fetches_the_blocks_two_trees_differ_in_with_their_name_tables() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let revisions = two_revisions_in_memory().await;
            let (differing, shared) = revisions.differing_and_shared_blocks().await;
            assert!(!shared.is_empty(), "the two trees hold a block alike");
            let (client, remote, from, to) = revisions.on_a_cold_client(Serving::default()).await;
            let before = remote.asked();

            let warmed = warm_differing_blocks(client, from, to, CancellationToken::new())
                .await
                .expect("the warm-up");

            let asked: BTreeSet<Address> = remote.asked().difference(&before).copied().collect();
            let blocks: BTreeSet<Address> = differing.union(&shared).copied().collect();
            assert_eq!(
                asked
                    .intersection(&blocks)
                    .copied()
                    .collect::<BTreeSet<_>>(),
                differing,
                "the node blocks asked of the remote"
            );
            let name_tables = name_tables_of(&revisions.source, &differing).await;
            assert!(!name_tables.is_empty(), "the blocks name no name table");
            assert!(
                name_tables.is_subset(&asked),
                "name tables not asked of the remote: {:?}",
                name_tables.difference(&asked).collect::<Vec<_>>()
            );
            let count = differing.len() as u64;
            assert_eq!(
                (
                    warmed.blocks,
                    warmed.name_tables,
                    warmed.file_metadata,
                    warmed.failures
                ),
                (count, count, 0, 0),
                "{warmed:?}"
            );
        })
        .await;
}

/// A warm-up leaves out the blocks the local store holds, so one after another asks the remote
/// for nothing.
#[tokio::test]
async fn a_diff_warm_up_fetches_no_block_the_local_store_holds() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let revisions = two_revisions_in_memory().await;
            let (client, remote, from, to) = revisions.on_a_cold_client(Serving::default()).await;
            let warm = || {
                warm_differing_blocks(
                    client.clone(),
                    from.clone(),
                    to.clone(),
                    CancellationToken::new(),
                )
            };
            let first = warm().await.expect("the first warm-up");
            let gets = remote.gets();

            let again = warm().await.expect("the second warm-up");

            assert!(first.blocks > 0, "the first warm-up fetched no block");
            assert_eq!(again.blocks, 0, "{again:?}");
            assert_eq!(remote.gets(), gets, "the second warm-up asked the remote");
        })
        .await;
}

/// A warm-up comparing the blocks with the local store a few at a time fetches those of each
/// comparison the store lacks, and goes on past one whose blocks the store holds every one of.
#[tokio::test]
async fn a_diff_warm_up_compared_block_by_block_fetches_every_block_the_store_lacks() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let revisions = two_revisions_in_memory().await;
            let (differing, _) = revisions.differing_and_shared_blocks().await;
            let (from_list, to_list) = revisions.block_lists().await;
            let first = (0..to_list.len())
                .find(|&index| from_list.get(index) != to_list.get(index))
                .expect("a block differs");
            let held = Address::zero_context_hash(from_list[first]);
            let (client, remote, from, to) = revisions.on_a_cold_client(Serving::default()).await;
            warm_blocks(
                client.clone(),
                from.clone(),
                Some(&[first]),
                Arc::new(|| false),
            )
            .await
            .expect("the client fetches the first block that differs");
            let before = remote.asked();

            let warmed =
                warm_differing_blocks_in_chunks(client, from, to, CancellationToken::new(), 1)
                    .await
                    .expect("the warm-up");

            let lacking: BTreeSet<Address> = differing
                .iter()
                .copied()
                .filter(|&block| block != held)
                .collect();
            let asked: BTreeSet<Address> = remote.asked().difference(&before).copied().collect();
            assert_eq!(
                asked
                    .intersection(&differing)
                    .copied()
                    .collect::<BTreeSet<_>>(),
                lacking,
                "the node blocks asked of the remote"
            );
            assert_eq!(
                (warmed.blocks, warmed.failures),
                (lacking.len() as u64, 0),
                "{warmed:?}"
            );
        })
        .await;
}

/// A warm-up stopped before it starts reads nothing: it asks the remote neither for a block nor
/// for the block address lists.
#[tokio::test]
async fn a_stopped_diff_warm_up_reads_nothing() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let revisions = two_revisions_in_memory().await;
            let (client, remote, from, to) = revisions.on_a_cold_client(Serving::default()).await;
            let gets = remote.gets();
            let stopped = CancellationToken::new();
            stopped.cancel();

            let warmed = warm_differing_blocks(client, from, to, stopped)
                .await
                .expect("the stopped warm-up");

            assert_eq!(warmed.blocks, 0, "{warmed:?}");
            assert_eq!(remote.gets(), gets, "the stopped warm-up asked the remote");
        })
        .await;
}

/// A warm-up stopped while it fetches a node block ends once that fetch does, leaving the block's
/// name table unfetched.
#[tokio::test]
async fn a_diff_warm_up_stopped_with_a_block_in_flight_leaves_its_name_table_unfetched() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let revisions = two_revisions_in_memory().await;
            let (from_list, to_list) = revisions.block_lists().await;
            let differing: Vec<usize> = (0..to_list.len())
                .filter(|&index| from_list.get(index) != to_list.get(index))
                .collect();
            let &[.., last] = &differing[..] else {
                panic!("no block differs");
            };
            let released = Arc::new(tokio::sync::Notify::new());
            let (client, remote, from, to) = revisions
                .on_a_cold_client(Serving::holding(to_list[last], released.clone()))
                .await;
            let warm = async |state: &Arc<State>, blocks: &[usize]| {
                warm_blocks(
                    client.clone(),
                    state.clone(),
                    Some(blocks),
                    Arc::new(|| false),
                )
                .await
                .expect("the client fetches the blocks");
            };
            warm(&from, &differing).await;
            warm(&to, &differing[..differing.len() - 1]).await;
            let held = Address::zero_context_hash(to_list[last]);
            let stopped = CancellationToken::new();
            let warm_up = warm_differing_blocks(client.clone(), from, to, stopped.clone());
            let stop = async {
                until(|| remote.asked().contains(&held)).await;
                stopped.cancel();
                released.notify_one();
            };

            let (warmed, ()) = Box::pin(tokio::time::timeout(Duration::from_secs(30), async {
                tokio::join!(warm_up, stop)
            }))
            .await
            .expect("the warm-up asks for the held block and ends");

            let warmed = warmed.expect("the stopped warm-up");
            assert_eq!(
                (warmed.blocks, warmed.name_tables, warmed.failures),
                (1, 0, 0),
                "{warmed:?}"
            );
        })
        .await;
}

/// A warm-up stopped while it connects for the blocks the store lacks gives the connect up and
/// ends, without waiting for the remote to answer.
#[tokio::test]
async fn a_diff_warm_up_stopped_while_it_connects_gives_the_connect_up() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let revisions = two_revisions_in_memory().await;
            let read = async |revision| {
                let state = in_memory::revision_state(&revisions.source, revision).await;
                state
                    .node_block_address_list(revisions.source.clone())
                    .await
                    .expect("the source reads the block address list");
                state
            };
            let (from, to) = (read(revisions.first).await, read(revisions.second).await);
            let polled = Arc::new(AtomicBool::new(false));
            let connect = {
                let polled = polled.clone();
                async move {
                    polled.store(true, Ordering::SeqCst);
                    std::future::pending::<Result<Arc<Connection>, ProtocolError>>().await
                }
                .boxed()
                .shared()
            };
            let connecting = context_with_state(RemoteState::Pending(connect)).await;
            let stopped = CancellationToken::new();
            let warm_up = warm_differing_blocks(connecting, from, to, stopped.clone());
            let stop = async {
                until(|| polled.load(Ordering::SeqCst)).await;
                stopped.cancel();
            };

            let (warmed, ()) = Box::pin(tokio::time::timeout(Duration::from_secs(30), async {
                tokio::join!(warm_up, stop)
            }))
            .await
            .expect("the warm-up connects and, stopped, ends");

            let warmed = warmed.expect("the stopped warm-up");
            assert_eq!(warmed.blocks, 0, "{warmed:?}");
        })
        .await;
}

/// A warm-up in a repository without a remote, which cannot fetch what its store lacks, reads
/// nothing: it asks its store neither for a block nor whether it holds one.
#[tokio::test]
async fn a_diff_warm_up_without_a_remote_reads_nothing() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (stores, counted) = counted_client_stores().await;
            let source = in_memory::repository(stores.immutable.clone(), stores.mutable.clone());
            let revisions = two_revisions(source.clone()).await;
            let from = in_memory::revision_state(&source, revisions.first).await;
            let to = in_memory::revision_state(&source, revisions.second).await;
            counted.take_reads();

            let warmed = warm_differing_blocks(source, from, to, CancellationToken::new())
                .await
                .expect("the warm-up");

            assert_eq!(warmed.blocks, 0, "{warmed:?}");
            assert_eq!(
                counted.take_reads(),
                Reads {
                    payloads: 0,
                    queries: 0
                }
            );
        })
        .await;
}

/// A diff fetches ahead only in a context that can fetch what its store lacks and store it: not a
/// server's, which has no remote, nor one reading the remote alone, which stores nothing.
#[tokio::test]
async fn only_a_context_reading_its_store_with_a_remote_may_fetch_ahead() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let revisions = two_revisions_in_memory().await;
            let (client, _, _, _) = revisions.on_a_cold_client(Serving::default()).await;

            assert!(may_cache(&client));
            assert!(!may_cache(&client.to_server_context(client.id)));
            assert!(!may_cache(
                &client
                    .to_filter_context(Arc::default())
                    .reading_remote_only()
            ));
            assert!(!may_cache(&revisions.source));
        })
        .await;
}

/// The walk of a whole-tree diff reads each node block its trees differ in, which is what makes
/// fetching them ahead of it worthwhile: run alone on a cold client, it asks the remote for every
/// one of them.
#[tokio::test]
async fn a_whole_tree_walk_reads_every_block_its_trees_differ_in() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let revisions = two_revisions_in_memory().await;
            let (differing, _) = revisions.differing_and_shared_blocks().await;
            let (client, remote, from, to) = revisions.on_a_cold_client(Serving::default()).await;
            let side = |state: Arc<State>| NodeChangeState {
                mapping: NodeMapping {
                    repository: client.clone(),
                    state,
                    path: RelativePath::new(),
                    node: ROOT_NODE,
                },
                observed: None,
                flags: NodeFlags::NoFlags,
                address: Address::default(),
                mode: 0,
            };

            let changes = diff_collect_subtree(
                side(from),
                side(to),
                RelativePath::new(),
                lore_revision::state::diff::DiffFlags::empty(),
                FilterMode::View,
            )
            .await
            .expect("the walk");

            assert_eq!(
                changes
                    .iter()
                    .map(|change| change.path().as_str())
                    .collect::<Vec<_>>(),
                [revisions.changed.as_str()]
            );
            let asked = remote.asked();
            assert!(
                differing.is_subset(&asked),
                "blocks the walk did not read: {:?}",
                differing.difference(&asked).collect::<Vec<_>>()
            );
        })
        .await;
}

/// A whole-tree diff fetches the blocks its trees differ in ahead of its walk: while the remote
/// holds the first get of the to side's first block that differs, which the walk reads before it
/// reaches the changed file, it is asked for the to side's last block that differs, which holds
/// the changed file. The diff then ends with the change, its warm-up holding nothing of the client.
#[tokio::test]
async fn a_whole_tree_diff_fetches_the_blocks_its_trees_differ_in_ahead_of_its_walk() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let revisions = two_revisions_in_memory().await;
            let (from_list, to_list) = revisions.block_lists().await;
            let differing: Vec<usize> = (0..to_list.len())
                .filter(|&index| from_list.get(index) != to_list.get(index))
                .collect();
            let [first, .., last] = differing[..] else {
                panic!("fewer than two blocks differ: {differing:?}");
            };
            let released = Arc::new(tokio::sync::Notify::new());
            let (client, remote, from, to) = revisions
                .on_a_cold_client(Serving::holding(to_list[first], released.clone()))
                .await;
            let ahead = Address::zero_context_hash(to_list[last]);
            assert!(
                !remote.asked().contains(&ahead),
                "{ahead} is asked of the remote"
            );
            let holders = Arc::strong_count(&client);

            let diff = lore_revision::state::diff_collect(
                client.clone(),
                from.clone(),
                client.clone(),
                to.clone(),
                None,
                FilterMode::View,
            );
            let release = async {
                until(|| remote.asked().contains(&ahead)).await;
                released.notify_one();
            };
            let (changes, ()) = Box::pin(tokio::time::timeout(Duration::from_secs(30), async {
                tokio::join!(diff, release)
            }))
            .await
            .expect("the remote is asked for the changed file's block while the walk waits");

            let changes = changes.expect("the diff");
            assert_eq!(
                changes
                    .iter()
                    .map(|change| change.path().as_str())
                    .collect::<Vec<_>>(),
                [revisions.changed.as_str()]
            );
            drop(changes);
            assert_eq!(
                Arc::strong_count(&client),
                holders,
                "the diff left the client held"
            );
        })
        .await;
}

/// A diff warm-up dropped rather than finished is aborted with the fetches it has in flight, and
/// leaves nothing holding the client.
#[tokio::test]
async fn a_dropped_diff_warm_up_aborts_its_fetches() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let revisions = two_revisions_in_memory().await;
            let (from_list, to_list) = revisions.block_lists().await;
            let first = (0..to_list.len())
                .find(|&index| from_list.get(index) != to_list.get(index))
                .expect("a block differs");
            let (client, remote, from, to) = revisions
                .on_a_cold_client(Serving::holding(
                    to_list[first],
                    Arc::new(tokio::sync::Notify::new()),
                ))
                .await;
            let held = Address::zero_context_hash(to_list[first]);
            let holders = Arc::strong_count(&client);
            let warm_up = DifferingBlocksWarmUp::start(client.clone(), from, to);
            Box::pin(tokio::time::timeout(
                Duration::from_secs(30),
                until(|| remote.asked().contains(&held)),
            ))
            .await
            .expect("the warm-up asks for the held block");

            drop(warm_up);

            Box::pin(tokio::time::timeout(
                Duration::from_secs(30),
                until(|| Arc::strong_count(&client) == holders),
            ))
            .await
            .expect("the aborted warm-up leaves nothing holding the client");
        })
        .await;
}
