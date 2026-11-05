// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Changes on a multi-thread runtime whose workers yield at random scheduling points
//! ([`chaos`]): served and swap changes, and saves between bursts of checked creates, discards,
//! detaches and file metadata sets, each followed by a quiescent reclaim (C15), a provider's or the
//! sweep before a save. The steps between the points, such as the levels of a served mark's
//! descent or a save's update of its tree, interleave here as threads run them, the yields widening
//! the windows between them.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_revision::node::NodeBlock;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFileMetadata;
use lore_revision::node::NodeFileMetadataBlock;
use lore_revision::node::NodeID;
use lore_storage::hash::hash_string;

use crate::harness::catalogue::ops;
use crate::harness::chaos;
use crate::harness::fixtures;
use crate::harness::fixtures::Tree;
use crate::harness::invariants;
use crate::harness::invariants::Rules;
use crate::harness::mixes;

/// The branches of the tree the tasks change, four directories deep, two files in each
/// directory: `branches` of them.
fn paths(branches: std::ops::Range<usize>) -> Vec<String> {
    let mut paths = Vec::new();
    for branch in branches {
        let mut dir = format!("b{branch}");
        for level in 0..4 {
            for file in 0..2 {
                paths.push(format!("{dir}/f{file}"));
            }
            dir = format!("{dir}/d{level}");
        }
        paths.push(format!("{dir}/"));
    }
    paths
}

/// The tree the tasks change: branches `b0` and `b1` in block 0, `b2` and `b3` in block 1, and
/// branch `h`, whose directories lie in block 1 and whose files lie in block 0, made in a staging
/// directory before the fill and moved into place, so its pair locks take the parent's block
/// above the child's. Answers the tree and every path of the branches.
async fn build() -> (Tree, Vec<String>) {
    let (before, after) = (paths(0..2), paths(2..4));
    let high: Vec<String> = paths(4..5)
        .into_iter()
        .map(|path| path.replacen("b4", "h", 1))
        .collect();
    let high_dirs: Vec<String> = high
        .iter()
        .filter(|path| path.ends_with('/'))
        .cloned()
        .collect();
    let high_files: Vec<(String, String)> = high
        .iter()
        .filter(|path| !path.ends_with('/'))
        .map(|path| (format!("stage/{}", path.replace('/', "-")), path.clone()))
        .collect();
    let staged: Vec<&str> = before
        .iter()
        .map(String::as_str)
        .chain(high_files.iter().map(|(staged, _)| staged.as_str()))
        .collect();
    let later: Vec<&str> = after
        .iter()
        .map(String::as_str)
        .chain(high_dirs.iter().map(String::as_str))
        .collect();
    let mut tree = Tree::spread(&staged, "fill", &later).await;
    for (staged, at) in &high_files {
        let (dir, name) = at.rsplit_once('/').expect("a file below a directory");
        let (node, dir) = (tree.id(staged), tree.id(dir));
        tree.state
            .node_reattach(tree.repository.clone(), node, dir, name)
            .await
            .expect("a staged file moves into place");
        let entry = tree
            .nodes
            .iter_mut()
            .find(|(path, _)| path == staged)
            .expect("the staged path");
        entry.0.clone_from(at);
    }
    tree.commit().await;
    for (_, at) in &high_files {
        let (dir, _) = at.rsplit_once('/').expect("a file below a directory");
        assert!(
            NodeBlock::index(tree.id(dir)) > NodeBlock::index(tree.id(at)),
            "'{dir}' does not lie above its file '{at}'"
        );
    }
    let paths = before.into_iter().chain(after).chain(high).collect();
    (tree, paths)
}

/// A xorshift generator, so each task's choices repeat from its seed.
struct Choices(u64);

impl Choices {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % bound as u64) as usize
    }
}

const ROUNDS: usize = 24;
const TASKS: usize = 16;
const STEPS: usize = 400;
const SETTLING_STEPS: usize = 100;
/// How many built files a round removes, so some directories end with a built child gone, which
/// no settle may clear, yet copy-ups and flips keep files to change.
const BUILT_REMOVALS: usize = 4;

/// How long a round may take before its tasks are taken for deadlocked.
const ROUND_LIMIT: Duration = Duration::from_secs(120);

/// What the rounds did, so the test fails where its tasks stop exercising the races it is for.
#[derive(Default)]
struct Done {
    creates: AtomicUsize,
    removals: AtomicUsize,
    renames: AtomicUsize,
    directory_renames: AtomicUsize,
    settles: AtomicUsize,
    detaches: AtomicUsize,
    marks: AtomicUsize,
    copy_ups: AtomicUsize,
    swap_rewrites: AtomicUsize,
    built_removals: AtomicUsize,
    /// Clean directories found holding their built children. In practice the quiet branches'
    /// directories alone, all of them each round: a non-quiet directory ends a round dirty.
    clean_compared: AtomicUsize,
    /// Directories whose children changed, each found dirty: where the end-of-round check bites.
    changed_compared: AtomicUsize,
    refusals: AtomicUsize,
    /// Slots the reclaims freed, and nodes they retained.
    reclaimed: AtomicUsize,
    retained: AtomicUsize,
}

/// A directory of the tree, the names of its children as built, and whether it is a branch's
/// top, which the swap never removes and no rename moves, so each round keeps its branches.
struct Dir {
    id: NodeID,
    children: Vec<String>,
    /// The files among its children as built.
    files: Vec<NodeID>,
    top: bool,
    /// In branches `b2`, `b3` or `h`, which no create, rename, directory rename, built-file
    /// removal or swap detach changes, so settles can clean them while marks, copy-ups and flips
    /// race them.
    quiet: bool,
}

/// Served marks, creates, removals, renames and copy-ups, directory renames, removals of built
/// files, commit flips, swap rewrites and clean detaches, and settles, on threads. At the end of
/// each round dirty closure holds, no pending clear stands, every file a task created and did
/// not remove itself is still in the tree, since a swap's clean detach never takes a dirty node,
/// and every directory the swap took nothing from and whose children changed is dirty, as a
/// settle clears a directory only while it holds the children it was built with and any change
/// below marks it again. Marks of a branch's top and creates in it always land, and some served
/// change is refused. No swap rewrite gives local content an address. Each round ends in the
/// provider's reclaim where it pauses, retaining a third of what the round removed: it frees the
/// rest and nothing live, and keeps the retained subtrees whole.
#[test]
fn served_and_swap_changes_on_threads_keep_dirty_closure_and_created_files() {
    let (runtime, chaos) = chaos::runtime(8, 4, 0x5eed);
    let done = Arc::new(Done::default());
    for round in 0..ROUNDS {
        runtime.block_on(
            lore_base::runtime::LORE_CONTEXT.scope(fixtures::execution(), async {
                tokio::time::timeout(ROUND_LIMIT, run_round(round, done.clone()))
                    .await
                    .unwrap_or_else(|_| panic!("round {round} did not end: a deadlock"));
            }),
        );
    }
    let count = |counter: &AtomicUsize| counter.load(Ordering::Relaxed);
    let counts = [
        ("creates", count(&done.creates), 2_000),
        ("removals", count(&done.removals), 1_000),
        ("renames", count(&done.renames), 400),
        ("directory renames", count(&done.directory_renames), 100),
        ("settles", count(&done.settles), 800),
        ("detaches", count(&done.detaches), 10),
        ("marks", count(&done.marks), 2_000),
        ("copy-ups", count(&done.copy_ups), 2_000),
        ("swap rewrites", count(&done.swap_rewrites), 500),
        ("built-file removals", count(&done.built_removals), 60),
        (
            "clean directories compared",
            count(&done.clean_compared),
            200,
        ),
        (
            "changed directories compared",
            count(&done.changed_compared),
            100,
        ),
        ("served refusals", count(&done.refusals), 10),
        ("slots reclaimed", count(&done.reclaimed), 500),
        ("nodes retained", count(&done.retained), 100),
    ];
    eprintln!("{counts:?}, {} yields", chaos.yields());
    assert!(chaos.yields() > 10_000, "only {} yields", chaos.yields());
    for (what, count, least) in counts {
        assert!(count >= least, "only {count} {what}, fewer than {least}");
    }
}

async fn run_round(round: usize, done: Arc<Done>) {
    let (tree, paths) = build().await;
    let tree = Arc::new(tree);
    tree.state.mark_served();
    let dirs: Arc<Vec<Dir>> = Arc::new(
        tree.nodes
            .iter()
            .filter(|(path, _)| paths.iter().any(|p| p.starts_with(&format!("{path}/"))))
            .map(|(path, id)| Dir {
                id: *id,
                children: tree
                    .nodes
                    .iter()
                    .filter_map(|(child, _)| {
                        child
                            .strip_prefix(&format!("{path}/"))
                            .filter(|name| !name.contains('/'))
                            .map(str::to_string)
                    })
                    .collect(),
                files: tree
                    .nodes
                    .iter()
                    .filter(|(child, _)| {
                        child
                            .strip_prefix(&format!("{path}/"))
                            .is_some_and(|name| !name.contains('/') && name.starts_with('f'))
                    })
                    .map(|(_, id)| *id)
                    .collect(),
                top: !path.contains('/'),
                quiet: ["b2", "b3", "h"]
                    .iter()
                    .any(|branch| path == branch || path.starts_with(&format!("{branch}/"))),
            })
            .collect(),
    );
    // Every directory starts dirty, as local changes leave a tree, so the swap removes one only
    // once settles have cleaned it, bottom up.
    for dir in dirs.iter().filter(|dir| dir.children.is_empty()) {
        assert!(ops::served_mark_dir(&tree, dir.id).await);
    }
    let files: Arc<Vec<NodeID>> = Arc::new(
        paths
            .iter()
            .filter(|path| !path.ends_with('/'))
            .map(|path| tree.id(path))
            .collect(),
    );
    // The directories the swap took a child from, which it leaves as they are.
    let swapped: Arc<Mutex<BTreeSet<NodeID>>> = Arc::default();
    // Settles and the swap's detaches run one at a time, as a VFS provider runs them:
    // a directory's pending clear is one bit, which two settles of it at once would share.
    let swap_plan: Arc<tokio::sync::Mutex<()>> = Arc::default();
    let built_removals = Arc::new(AtomicUsize::new(BUILT_REMOVALS));
    let mut handles = Vec::with_capacity(TASKS);
    for task in 0..TASKS {
        let (tree, dirs, files, done, swapped, swap_plan, built_removals) = (
            tree.clone(),
            dirs.clone(),
            files.clone(),
            done.clone(),
            swapped.clone(),
            swap_plan.clone(),
            built_removals.clone(),
        );
        handles.push(lore_base::lore_spawn!(async move {
            let mut choices = Choices(0x9e37_79b9_7f4a_7c15 ^ ((round * TASKS + task) as u64 + 1));
            let mut created: BTreeMap<String, NodeID> = BTreeMap::new();
            for step in 0..STEPS {
                let dir = &dirs[choices.below(dirs.len())];
                // The last steps settle and flip, so a round ends with directories settled clean
                // while other tasks' changes still race them, for the end-of-round check.
                let choice = if step >= STEPS - SETTLING_STEPS {
                    [6, 12][choices.below(2)]
                } else {
                    choices.below(15)
                };
                match choice {
                    0 => {
                        let marked = ops::served_mark_dir(&tree, dir.id).await;
                        assert!(marked || !dir.top, "a mark of a branch's top failed");
                        if marked {
                            done.marks.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    1 | 4 | 9 if dir.quiet => {}
                    1 => {
                        let name = format!("t{task}s{step}");
                        let node = ops::served_create(&tree, dir.id, &name).await;
                        assert!(
                            node.is_some() || !dir.top,
                            "a create in a branch's top failed"
                        );
                        if let Some(node) = node {
                            done.creates.fetch_add(1, Ordering::Relaxed);
                            created.insert(name, node);
                        }
                    }
                    2 | 3 => {
                        if let Some((name, node)) = created.pop_first() {
                            if ops::served_detach(&tree, node).await {
                                done.removals.fetch_add(1, Ordering::Relaxed);
                            } else {
                                created.insert(name, node);
                            }
                        }
                    }
                    4 => {
                        if let Some((name, node)) = created.pop_first() {
                            let renamed = format!("{name}r");
                            if ops::served_rename(&tree, node, dir.id, &renamed).await {
                                done.renames.fetch_add(1, Ordering::Relaxed);
                                created.insert(renamed, node);
                            } else {
                                created.insert(name, node);
                            }
                        }
                    }
                    5 => {
                        let file = files[choices.below(files.len())];
                        if let Ok(record) = tree.state.node(tree.repository.clone(), file).await
                            && ops::served_copy_up(&tree, file, record.parent).await
                        {
                            done.copy_ups.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    6 => {
                        ops::flip(&tree, files[choices.below(files.len())]).await;
                    }
                    7 | 8 => {
                        if dir.top || dir.quiet {
                            continue;
                        }
                        let _plan = swap_plan.lock().await;
                        let parent = tree
                            .state
                            .node(tree.repository.clone(), dir.id)
                            .await
                            .map(|record| record.parent);
                        if ops::swap_detach_if_clean(&tree, dir.id).await {
                            done.detaches.fetch_add(1, Ordering::Relaxed);
                            if let Ok(parent) = parent {
                                swapped.lock().expect("the swapped set").insert(parent);
                            }
                        }
                    }
                    9 => {
                        let moved = &dirs[choices.below(dirs.len())];
                        let name = format!("m{task}s{step}");
                        if !moved.top
                            && !moved.quiet
                            && ops::served_reattach_any(&tree, moved.id, dir.id, &name).await
                                == Some(true)
                        {
                            done.directory_renames.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    14 => {
                        let file = files[choices.below(files.len())];
                        let _plan = swap_plan.lock().await;
                        if ops::swap_rewrite_if_clean(&tree, file).await {
                            done.swap_rewrites.fetch_add(1, Ordering::Relaxed);
                        }
                        // A copy-up racing the rewrite either finds it done or makes the rewrite
                        // refuse; neither leaves the committed address on local content.
                        if let Ok(held) = tree.state.node(tree.repository.clone(), file).await {
                            assert!(
                                !held.ext_flags().contains(NodeExtFlags::LocalContent)
                                    || held.address.hash.is_zero(),
                                "round {round}: swap rewrite left an address on local file {file}"
                            );
                        }
                    }
                    13 => {
                        if dir.quiet
                            || dir.files.is_empty()
                            || built_removals
                                .try_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                                    left.checked_sub(1)
                                })
                                .is_err()
                        {
                            continue;
                        }
                        let file = dir.files[choices.below(dir.files.len())];
                        if ops::served_detach(&tree, file).await {
                            done.built_removals.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    _ => {
                        let _plan = swap_plan.lock().await;
                        if ops::settle(&tree, dir.id).await == ops::Settle::Cleared {
                            done.settles.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
            created
        }));
    }
    let mut kept = Vec::new();
    for handle in handles {
        kept.extend(handle.await.expect("a task finishes").into_values());
    }
    let retained = mixes::retained_drawn(&tree, round as u64);
    let reclaimed = ops::served_sweep(&tree, &retained).await;
    done.reclaimed.fetch_add(
        usize::try_from(reclaimed.file_count + reclaimed.directory_count).expect("a count"),
        Ordering::Relaxed,
    );
    done.retained.fetch_add(retained.len(), Ordering::Relaxed);
    let report = invariants::check(
        &tree.state,
        &tree.repository,
        Rules {
            dirty_closure: true,
            unique_names: true,
            no_ext_flags: false,
            no_builds: false,
        },
    )
    .await
    .unwrap_or_else(|violations| panic!("round {round} invariants: {violations:#?}"));
    let left = invariants::swept(&tree.state, &tree.repository, &report, |node| {
        retained.contains(&node)
    })
    .await;
    assert!(left.is_empty(), "round {round}: the reclaim left {left:#?}");
    for node in &retained {
        assert!(
            report.unreleased.contains(node),
            "round {round}: the reclaim freed {node}, which the provider retains"
        );
    }
    for node in kept {
        assert!(
            report.nodes.contains_key(&node),
            "round {round}: created file {node} is no longer in the tree"
        );
    }
    // A swap's rewrite never replaces local content: a file holding it has no address.
    for (id, node) in &report.nodes {
        if node.is_file() && node.ext_flags().contains(NodeExtFlags::LocalContent) {
            assert!(
                node.address.hash.is_zero(),
                "round {round}: local file {id} carries an address"
            );
        }
    }
    done.refusals.fetch_add(
        usize::try_from(tree.state.served_refusals()).expect("a count"),
        Ordering::Relaxed,
    );
    let swapped = swapped.lock().expect("the swapped set");
    for dir in dirs.iter() {
        let Some(record) = report.nodes.get(&dir.id) else {
            continue;
        };
        if swapped.contains(&dir.id) {
            continue;
        }
        let path = &report.paths[&dir.id];
        let mut held: Vec<(NodeID, u64)> = report
            .nodes
            .iter()
            .filter(|(_, node)| node.parent == dir.id)
            .map(|(id, node)| (*id, node.name_hash))
            .collect();
        let built_path = tree
            .nodes
            .iter()
            .find(|(_, id)| *id == dir.id)
            .map(|(path, _)| path.as_str())
            .expect("a directory of the tree");
        let mut built: Vec<(NodeID, u64)> = dir
            .children
            .iter()
            .map(|name| (tree.id(&format!("{built_path}/{name}")), hash_string(name)))
            .collect();
        held.sort_unstable();
        built.sort_unstable();
        // A directory whose children changed is where the check bites: no settle may have
        // cleared it, and any change below marked it again.
        if held != built {
            done.changed_compared.fetch_add(1, Ordering::Relaxed);
            assert!(
                record.is_dirty(),
                "round {round}: clean '{path}' does not hold the children it was built with: \
                 {held:?}, built {built:?}"
            );
        } else if !record.is_dirty() {
            done.clean_compared.fetch_add(1, Ordering::Relaxed);
        }
    }
}

const SAVE_ROUNDS: usize = 12;
const SAVE_TASKS: usize = 6;
const SAVE_STEPS: usize = 300;
/// The bursts of each round's steps, a save after each.
const SAVE_BURSTS: usize = 10;

/// What the save rounds did, so the test fails where they stop racing saves against grabs.
#[derive(Default)]
struct Saved {
    saves: AtomicUsize,
    creates: AtomicUsize,
    discards: AtomicUsize,
    filler_discards: AtomicUsize,
    metadata_sets: AtomicUsize,
    fresh_blocks: AtomicUsize,
    /// Files detached, which the sweep before the next save frees.
    detaches: AtomicUsize,
    reclaimed: AtomicUsize,
}

/// Creates, discards and detaches on threads in bursts, taking slots from fresh blocks and freeing
/// slots in full ones, and after each burst, with no writer beside them, the sweep and a save, as a
/// stage and a commit run them: the sweep frees every file detached since the last, and nothing
/// live; each save updates the tree in place and threads the blocks that freed a slot while full
/// behind the unused chain's head. At each round's end every block with room is reachable by a
/// grab, and a save holds every node the State does. Each task changes its own directory alone,
/// and one task frees the filler's files, so no slot a discard frees is one a concurrent task
/// still reaches (the exclusive tier's frees). Tasks set file metadata on their files too, so
/// saves write file metadata blocks and update the tree's list of them. Edits during a save are
/// the H4 scenarios'.
#[test]
fn saves_between_bursts_of_creates_and_discards_keep_the_unused_chain_and_every_node() {
    let (runtime, chaos) = chaos::runtime(8, 4, 0x5a7e);
    let done = Arc::new(Saved::default());
    for round in 0..SAVE_ROUNDS {
        runtime.block_on(
            lore_base::runtime::LORE_CONTEXT.scope(fixtures::execution(), async {
                tokio::time::timeout(ROUND_LIMIT, save_round(round, done.clone()))
                    .await
                    .unwrap_or_else(|_| panic!("round {round} did not end: a deadlock"));
            }),
        );
    }
    let count = |counter: &AtomicUsize| counter.load(Ordering::Relaxed);
    let counts = [
        ("saves", count(&done.saves), 100),
        ("creates", count(&done.creates), 5_000),
        ("discards", count(&done.discards), 1_000),
        ("filler discards", count(&done.filler_discards), 100),
        ("metadata sets", count(&done.metadata_sets), 500),
        ("fresh blocks", count(&done.fresh_blocks), SAVE_ROUNDS),
        ("detaches", count(&done.detaches), 300),
        ("slots reclaimed", count(&done.reclaimed), 300),
    ];
    eprintln!("{counts:?}, {} yields", chaos.yields());
    for (what, count, least) in counts {
        assert!(count >= least, "only {count} {what}, fewer than {least}");
    }
}

async fn save_round(round: usize, done: Arc<Saved>) {
    let dirs: Vec<String> = (0..SAVE_TASKS).map(|task| format!("d{task}/")).collect();
    let dirs: Vec<&str> = dirs.iter().map(String::as_str).collect();
    let tree = Arc::new(Tree::spread(&dirs, "fill", &[]).await);
    let fill: Arc<Vec<NodeID>> = Arc::new(
        tree.state
            .node_children(tree.repository.clone(), tree.id("fill"))
            .await
            .expect("the filler's children"),
    );
    let next_fill = Arc::new(AtomicUsize::new(0));
    let mut tasks: Vec<(Choices, Vec<NodeID>)> = (0..SAVE_TASKS)
        .map(|task| {
            let seed = 0x5a7e_d00d ^ ((round * SAVE_TASKS + task) as u64 + 1);
            (Choices(seed), Vec::new())
        })
        .collect();
    for burst in 0..SAVE_BURSTS {
        let mut handles = Vec::with_capacity(SAVE_TASKS);
        for (task, (mut choices, mut created)) in tasks.drain(..).enumerate() {
            let (tree, fill, next_fill, done) =
                (tree.clone(), fill.clone(), next_fill.clone(), done.clone());
            handles.push(lore_base::lore_spawn!(async move {
                let steps =
                    burst * SAVE_STEPS / SAVE_BURSTS..(burst + 1) * SAVE_STEPS / SAVE_BURSTS;
                for step in steps {
                    save_step(
                        &tree,
                        (task, step),
                        &mut choices,
                        &mut created,
                        (&fill, &next_fill),
                        &done,
                    )
                    .await;
                }
                (choices, created)
            }));
        }
        for handle in handles {
            tasks.push(handle.await.expect("a task finishes"));
        }
        let detached = done.detaches.load(Ordering::Relaxed);
        let reclaimed = ops::sweep(&tree).await;
        let reclaimed =
            usize::try_from(reclaimed.file_count + reclaimed.directory_count).expect("a count");
        let before = done.reclaimed.fetch_add(reclaimed, Ordering::Relaxed);
        assert_eq!(
            before + reclaimed,
            detached,
            "round {round} burst {burst}: the sweep frees every file detached since the last"
        );
        let report = invariants::check(&tree.state, &tree.repository, Rules::default())
            .await
            .unwrap_or_else(|violations| panic!("round {round} invariants: {violations:#?}"));
        let left = invariants::swept(&tree.state, &tree.repository, &report, |_| false).await;
        assert!(
            left.is_empty(),
            "round {round} burst {burst}: the sweep left {left:#?}"
        );
        tree.save().await;
        done.saves.fetch_add(1, Ordering::Relaxed);
    }
    done.fresh_blocks
        .fetch_add(tree.state.block_count() - 1, Ordering::Relaxed);
    // Every block in memory, so the check walks the whole chain: a clean block reads back as it
    // was, and one awaiting a save's threading stays registered, so in memory.
    let mut held = Vec::new();
    for index in 0..tree.state.block_count() {
        held.push(
            tree.state
                .block(tree.repository.clone(), index)
                .await
                .expect("the block loads"),
        );
    }
    let live = invariants::check(&tree.state, &tree.repository, Rules::default())
        .await
        .unwrap_or_else(|violations| panic!("round {round} invariants: {violations:#?}"));
    let next = tree.saved().await;
    let saved = invariants::check(&next, &tree.repository, Rules::default())
        .await
        .unwrap_or_else(|violations| panic!("round {round} saved invariants: {violations:#?}"));
    let paths =
        |report: &invariants::Report| report.paths.values().cloned().collect::<BTreeSet<_>>();
    assert_eq!(
        paths(&saved),
        paths(&live),
        "round {round}: the save holds every node the State does"
    );
}

/// One step of save round task `task`: a create in its own directory, a discard or a metadata set
/// of a file it created, or, for task 0, a discard of the filler's next file, for the others a
/// detach of a file it created.
async fn save_step(
    tree: &Tree,
    (task, step): (usize, usize),
    choices: &mut Choices,
    created: &mut Vec<NodeID>,
    (fill, next_fill): (&[NodeID], &AtomicUsize),
    done: &Saved,
) {
    match choices.below(10) {
        0..=5 => {
            let dir = tree.id(&format!("d{task}"));
            let name = format!("s{step}");
            let answered = ops::checked_create(tree, dir, &name).await;
            let node = ops::attached(answered)
                .unwrap_or_else(|| panic!("'d{task}/{name}' attaches: {answered:?}"));
            done.creates.fetch_add(1, Ordering::Relaxed);
            created.push(node);
        }
        6..=7 => {
            if !created.is_empty() {
                let node = created.swap_remove(choices.below(created.len()));
                discard(tree, node).await;
                done.discards.fetch_add(1, Ordering::Relaxed);
            }
        }
        8 => {
            if !created.is_empty() {
                let node = created[choices.below(created.len())];
                set_metadata(tree, node, step).await;
                done.metadata_sets.fetch_add(1, Ordering::Relaxed);
            }
        }
        _ if task == 0 => {
            let at = next_fill.fetch_add(1, Ordering::Relaxed);
            if let Some(node) = fill.get(at) {
                discard(tree, *node).await;
                done.filler_discards.fetch_add(1, Ordering::Relaxed);
            }
        }
        _ => {
            if !created.is_empty() {
                let node = created.swap_remove(choices.below(created.len()));
                assert!(ops::unserved_detach(tree, node).await, "the detach");
                done.detaches.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Sets the file metadata of `node`'s slot, as a metadata set writes it.
async fn set_metadata(tree: &Tree, node: NodeID, step: usize) {
    let slot = lore_revision::node::node_to_file_metadata(node);
    let index = NodeFileMetadataBlock::index(slot);
    let block = tree
        .state
        .block_file_metadata(tree.repository.clone(), index)
        .await
        .expect("the file metadata block");
    let dirtied = {
        let mut writer = block.write();
        let mut value = [1u8; 32];
        value[..8].copy_from_slice(&(step as u64).to_le_bytes());
        writer.node(NodeFileMetadata::index(slot)).metadata = value.into();
        writer.mark_dirty()
    };
    if dirtied {
        tree.state.block_file_metadata_modified(block, index);
    }
}

async fn discard(tree: &Tree, node: NodeID) {
    lore_revision::state::node_discard_fused(tree.state.clone(), tree.repository.clone(), node)
        .await
        .expect("the discard");
}
