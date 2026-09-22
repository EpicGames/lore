// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! What a two-view walk emits, held against the set of paths each view materializes.
//!
//! The walk decides which files a view change puts on disk and which it takes off, and both tests
//! here state that set without it: every node of both revisions is measured against its own view
//! with [`Filter::excludes_tree`], one whole path at a time, and the change set is required to
//! carry the one set to the other exactly -- nothing missing, nothing touched twice, nothing
//! touched that neither the revisions nor the views moved.
//!
//! Two shapes of input. A hand-written matrix names one subject per revision movement and asserts
//! each under every view movement, so a failure says which case broke. A generator draws trees and
//! view pairs from a seed and reaches what the matrix does not think to ask; a failure names the
//! seed, which is the whole of what a re-run needs.
//!
//! Neither covers a node changing type between the revisions. A retype emits a delete and an add at
//! one path, so path order does not say which the working tree takes first, and asserting on the
//! pair needs a tie-break the rest of the matrix does without. `diff_views.rs` pins it instead,
//! imposing that order: once as the pair itself, and once into a directory a view excludes, where
//! the delete is the whole change.

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Hash;
    use lore_revision::change::FileAction;
    use lore_revision::change::NodeChange;
    use lore_revision::filter::Filter;
    use lore_revision::filter::FilterMode;
    use lore_revision::lore::RepositoryId;
    use lore_revision::node::ROOT_NODE;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::state;
    use lore_revision::state::State;
    use lore_revision::util::path::RelativePath;
    use rand::Rng;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    include!("helper.rs");

    /// The matrix's subjects, the file ones under `file` and the directory ones under `tree`. No
    /// view rule names either of those two directories, so what a walk does to a subject is decided
    /// at the subject itself rather than above it.
    const UNCHANGED_FILE: &str = "file/unchanged.txt";
    const MODIFIED_FILE: &str = "file/modified.txt";
    const ADDED_FILE: &str = "file/added.txt";
    const DELETED_FILE: &str = "file/deleted.txt";

    const UNCHANGED_TREE: &str = "tree/unchanged";
    const MODIFIED_TREE: &str = "tree/modified";
    const ADDED_TREE: &str = "tree/added";
    const DELETED_TREE: &str = "tree/deleted";

    /// The one file each directory subject holds, so a directory entering or leaving the view
    /// reads as the directory's own record and the one below it.
    const UNCHANGED_TREE_FILE: &str = "tree/unchanged/inner.txt";
    const MODIFIED_TREE_FILE: &str = "tree/modified/inner.txt";
    const ADDED_TREE_FILE: &str = "tree/added/inner.txt";
    const DELETED_TREE_FILE: &str = "tree/deleted/inner.txt";

    /// A record a walk emits, as the action letter against the path.
    type Record = (&'static str, &'static str);

    /// One subject of the routing matrix: a path the two revisions move one way and the two views
    /// another, with what a walk must emit for it under each view movement.
    ///
    /// A subject neither view admits is not a column. It stands on disk under neither view, so no
    /// walk has anything to say about it, and that is asserted once over the whole matrix rather
    /// than as a column of empty rows.
    struct Case {
        /// The subject, and the root of every path a record about it can name.
        subject: &'static str,
        /// What the two revisions do to it, named for the failure message.
        movement: &'static str,
        /// Both views admit it, so what is emitted is what the revisions did.
        admitted: &'static [Record],
        /// The from view admits it and the to view does not, so it leaves the working tree.
        leaving: &'static [Record],
        /// The to view admits it and the from view did not, so it arrives in the working tree.
        entering: &'static [Record],
    }

    /// The routing matrix: every revision movement against every view movement, for a file and for
    /// a directory.
    ///
    /// Read down a column and the view movement is fixed, so what differs between rows is what the
    /// revisions did. Read across a row and the revisions are fixed, so what differs is the views.
    /// The two axes are independent on purpose: a subject the older revision does not hold has
    /// nothing for the from view to admit, and one the newer does not hold has nothing for the to
    /// view to, which is what the empty cells state.
    ///
    /// Every subject is excluded at its own path, which is what keeps this one rule per row. It is
    /// also what keeps the content-equality prune out of reach here: a subject is routed by the
    /// to-side verdict before a walk asks whether the views can differ *below* an unchanged
    /// directory. That question is `diff_views.rs`'s
    /// (`views_that_cover_a_content_equal_subtree_do_not_enter_it` and
    /// `views_re_including_different_files_below_one_directory_diverge`) and the generator's below.
    const MATRIX: &[Case] = &[
        Case {
            subject: UNCHANGED_FILE,
            movement: "a file both revisions hold identically",
            admitted: &[],
            leaving: &[("D", UNCHANGED_FILE)],
            entering: &[("A", UNCHANGED_FILE)],
        },
        Case {
            subject: MODIFIED_FILE,
            movement: "a file the newer revision rewrites",
            admitted: &[("M", MODIFIED_FILE)],
            leaving: &[("D", MODIFIED_FILE)],
            entering: &[("A", MODIFIED_FILE)],
        },
        Case {
            subject: ADDED_FILE,
            movement: "a file the newer revision adds",
            admitted: &[("A", ADDED_FILE)],
            leaving: &[],
            entering: &[("A", ADDED_FILE)],
        },
        Case {
            subject: DELETED_FILE,
            movement: "a file the newer revision deletes",
            admitted: &[("D", DELETED_FILE)],
            leaving: &[("D", DELETED_FILE)],
            entering: &[],
        },
        Case {
            subject: UNCHANGED_TREE,
            movement: "a directory both revisions hold identically",
            admitted: &[],
            leaving: &[("D", UNCHANGED_TREE), ("D", UNCHANGED_TREE_FILE)],
            entering: &[("A", UNCHANGED_TREE), ("A", UNCHANGED_TREE_FILE)],
        },
        Case {
            subject: MODIFIED_TREE,
            movement: "a directory whose one file the newer revision rewrites",
            admitted: &[("M", MODIFIED_TREE_FILE)],
            leaving: &[("D", MODIFIED_TREE), ("D", MODIFIED_TREE_FILE)],
            entering: &[("A", MODIFIED_TREE), ("A", MODIFIED_TREE_FILE)],
        },
        Case {
            subject: ADDED_TREE,
            movement: "a directory the newer revision adds",
            admitted: &[("A", ADDED_TREE), ("A", ADDED_TREE_FILE)],
            leaving: &[],
            entering: &[("A", ADDED_TREE), ("A", ADDED_TREE_FILE)],
        },
        Case {
            subject: DELETED_TREE,
            movement: "a directory the newer revision deletes",
            admitted: &[("D", DELETED_TREE), ("D", DELETED_TREE_FILE)],
            leaving: &[("D", DELETED_TREE), ("D", DELETED_TREE_FILE)],
            entering: &[],
        },
    ];

    /// How many trees the generator draws, each walked under every view pair.
    const SEEDS: u64 = 8;
    /// How many top-level directories a generated tree holds, and how many files at most sit
    /// directly in any one directory. Small enough that a counterexample is readable, wide enough
    /// that one view rule names more than one thing.
    const TOP_LEVEL: u32 = 4;
    const MAX_FILES: u32 = 4;
    /// The extension the generator gives some of its files, which the unanchored view rule names.
    /// A name rule can match at any depth anywhere, so its presence alone leaves the walk no
    /// subtree to prune on content equality; the files carrying it are what the rule then routes.
    const TEMPORARY: &str = "tmp";

    /// What a path holds, which is what two materialized sets are compared on.
    ///
    /// A file carries its content and its executable bit, which together are what a walk calls a
    /// modification: [`NodeChangeState::differs_from`] compares the content hash and that one bit.
    ///
    /// A directory carries neither. Its address is the hash of what it holds, so two views agreeing
    /// on a directory while disagreeing below it would compare unequal on an address the walk
    /// correctly never emits a record for.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Held {
        Directory,
        File(Hash, u16),
    }

    /// The executable bit of `mode`, which is the whole of the mode a change is measured on.
    fn executable(mode: u16) -> u16 {
        mode & lore_revision::node::NodeFileMode::Executable.bits()
    }

    /// A repository holding a tree in two revisions, over the stores its contexts are built on.
    struct Fixture {
        instance: TestRepository,
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        older: Arc<State>,
        newer: Arc<State>,
    }

    impl Fixture {
        /// `tree` committed twice: once as it stands, once with its mutations applied.
        async fn build(
            immutable_store: Arc<dyn lore_storage::ImmutableStore>,
            mutable_store: Arc<dyn lore_storage::MutableStore>,
            tree: &Tree,
        ) -> Self {
            let instance = test_repository_create(
                immutable_store.clone(),
                mutable_store.clone(),
                RepositoryId::from(uuid::Uuid::now_v7()),
            )
            .await;

            for path in &tree.files {
                write_file(&instance, path, 1);
            }
            let older = test_commit_tree(&instance, "First").await;

            for path in &tree.modified {
                write_file(&instance, path, 2);
            }
            for path in &tree.added {
                write_file(&instance, path, 1);
            }
            for path in &tree.deleted {
                std::fs::remove_file(instance.path.join(path)).expect("Remove file failed");
            }
            for path in &tree.executable {
                make_executable(&instance.path.join(path));
            }
            for path in &tree.emptied {
                std::fs::remove_dir(instance.path.join(path)).expect("Remove directory failed");
            }
            let newer = test_commit_tree(&instance, "Second").await;

            Self {
                instance,
                immutable_store,
                mutable_store,
                older,
                newer,
            }
        }

        /// A context over the repository whose view holds `rules`.
        fn view(&self, rules: &[&str]) -> Arc<RepositoryContext> {
            test_view_context(
                &self.instance,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
                rules,
            )
        }

        /// What a walk of the whole tree between the two revisions reports, read the way every
        /// caller in the product reads it: coalesced and sorted.
        ///
        /// Asked with [`FilterMode::View`], which is the mode a view change carries: the two sides
        /// share one ignore slot, so it cannot tell them apart, and here it holds nothing at all.
        async fn changes(
            &self,
            from: Arc<RepositoryContext>,
            to: Arc<RepositoryContext>,
        ) -> Vec<NodeChange> {
            state::diff_collect(
                from,
                self.older.clone(),
                to,
                self.newer.clone(),
                None,
                FilterMode::View,
            )
            .await
            .expect("Failed to diff the two revisions")
        }

        /// The nodes of both revisions, each measured against the view its own side holds.
        async fn materialized(
            &self,
            from: &Arc<RepositoryContext>,
            to: &Arc<RepositoryContext>,
        ) -> (BTreeMap<String, Held>, BTreeMap<String, Held>) {
            let repository = self.instance.repository.clone();
            (
                in_view(&nodes_in(&repository, &self.older).await, &from.filter),
                in_view(&nodes_in(&repository, &self.newer).await, &to.filter),
            )
        }
    }

    /// Writes `path` under the instance holding its own name and `version`, creating the
    /// directories above it.
    ///
    /// The content names the path so no two files anywhere in a tree hold the same bytes: two that
    /// did, one deleted and one added, would be coalesced into a move, which is a fifth routing
    /// this states nothing about.
    fn write_file(instance: &TestRepository, path: &str, version: u32) {
        let full = instance.path.join(path);
        if let Some(directory) = full.parent() {
            std::fs::create_dir_all(directory).expect("Create directory failed");
        }
        test_file_write(full.as_path(), format!("{path} v{version}").as_bytes());
    }

    /// Marks `path` executable, which is the one mode bit a node records.
    #[cfg(unix)]
    fn make_executable(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("Set permissions failed");
    }

    /// No mode to set where the filesystem records none.
    #[cfg(not(unix))]
    fn make_executable(_path: &std::path::Path) {}

    /// Every node `state` holds, by path.
    ///
    /// Walks the tree rather than asking a filtered walk for it. This is what the change set is
    /// measured against, so it has to be arrived at without the code under test.
    async fn nodes_in(
        repository: &Arc<RepositoryContext>,
        state: &Arc<State>,
    ) -> BTreeMap<String, Held> {
        let mut nodes = BTreeMap::new();
        let mut pending = vec![(ROOT_NODE, String::new())];
        while let Some((parent, prefix)) = pending.pop() {
            let children = state
                .node_children(repository.clone(), parent)
                .await
                .expect("Failed to read the children of a node");
            for child in children {
                let node = state
                    .node(repository.clone(), child)
                    .await
                    .expect("Failed to read a node");
                let name = state
                    .node_name_clone(repository.clone(), child)
                    .await
                    .expect("Failed to read a node name");
                let path = match prefix.is_empty() {
                    true => name,
                    false => format!("{prefix}/{name}"),
                };
                match node.is_directory() {
                    true => {
                        nodes.insert(path.clone(), Held::Directory);
                        pending.push((child, path));
                    }
                    false => {
                        nodes.insert(path, Held::File(node.address.hash, executable(node.mode)));
                    }
                }
            }
        }
        nodes
    }

    /// The nodes of `nodes` that `filter` leaves in view, asked one whole path at a time.
    fn in_view(nodes: &BTreeMap<String, Held>, filter: &Filter) -> BTreeMap<String, Held> {
        nodes
            .iter()
            .filter(|(path, held)| {
                let path = RelativePath::new_from_initial_path(path).expect("Valid path");
                !filter.excludes_tree(&path, matches!(held, Held::Directory), FilterMode::View)
            })
            .map(|(path, held)| (path.clone(), *held))
            .collect()
    }

    /// `before` with every change applied, which is what the working tree holds once a sync
    /// realizes them, beside every record that was not the tree's to take.
    ///
    /// A record naming a path the tree does not hold, or leaving it holding what it already did,
    /// is work a view change did not need: the set it produces can still be right while the walk
    /// emitted more than the change set it should have.
    ///
    /// A delete removes the path it names and nothing below it, which is what a walk emits for the
    /// directories these trees hold: `add_hierarchy_delete` spells out a record per node. A link
    /// mount is the exception — that walk returns at a link (`state.rs:10044`), so one delete
    /// carries the whole mount — and no tree here holds one.
    fn applied(
        before: &BTreeMap<String, Held>,
        changes: &[NodeChange],
    ) -> (BTreeMap<String, Held>, Vec<String>) {
        let mut after = before.clone();
        let mut redundant = Vec::new();
        for change in changes {
            let side = change.resolved_side();
            let path = side.mapping.path.as_str().to_string();
            let held = match side.flags.is_directory() {
                true => Held::Directory,
                false => Held::File(side.address.hash, executable(side.mode)),
            };
            match change.action {
                FileAction::Add => {
                    if after.insert(path.clone(), held).is_some() {
                        redundant.push(format!("add of a path already held: {path}"));
                    }
                }
                FileAction::Delete => {
                    if after.remove(&path).is_none() {
                        redundant.push(format!("delete of a path not held: {path}"));
                    }
                }
                FileAction::Keep => match after.insert(path.clone(), held) {
                    None => redundant.push(format!("modify of a path not held: {path}")),
                    Some(previous) if previous == held => {
                        redundant.push(format!("modify leaving the path as it was: {path}"));
                    }
                    Some(_) => {}
                },
                FileAction::Move => {
                    match change
                        .move_source()
                        .map(|source| source.as_str().to_string())
                    {
                        Some(source) => {
                            if after.remove(&source).is_none() {
                                redundant.push(format!("move from a path not held: {source}"));
                            }
                        }
                        None => redundant.push(format!("move with no source: {path}")),
                    }
                    after.insert(path, held);
                }
                action => redundant.push(format!("{action:?} of {path}")),
            }
        }
        (after, redundant)
    }

    /// Where two materialized sets disagree, as the path against what each holds there.
    ///
    /// The sets are the whole of a tree, so naming them both states a counterexample in thousands
    /// of characters the reader has to diff by eye. What went wrong is a handful of paths.
    fn difference(
        carried: &BTreeMap<String, Held>,
        expected: &BTreeMap<String, Held>,
    ) -> Vec<String> {
        carried
            .keys()
            .chain(expected.keys())
            .filter(|path| carried.get(*path) != expected.get(*path))
            .map(|path| {
                format!(
                    "{path}: carried {:?}, expected {:?}",
                    carried.get(path),
                    expected.get(path)
                )
            })
            .collect()
    }

    /// The records naming `subject` or something below it, in the order the walk reported them.
    fn records_for(changes: &[(String, String)], subject: &str) -> Vec<(String, String)> {
        changes
            .iter()
            .filter(|(_, path)| below(path, subject))
            .cloned()
            .collect()
    }

    /// Whether `path` is `subject` or sits below it.
    fn below(path: &str, subject: &str) -> bool {
        path == subject || path.starts_with(&format!("{subject}/"))
    }

    /// The expected records as the walk reports them.
    fn expected(records: &[Record]) -> Vec<(String, String)> {
        records
            .iter()
            .map(|(action, path)| (action.to_string(), path.to_string()))
            .collect()
    }

    /// One rule per matrix subject, naming the subject itself. A path rule rather than a
    /// directory-only one, so it names the subject whichever kind of node stands there, and a view
    /// holding them all moves the subjects and nothing else.
    fn subject_rules() -> Vec<String> {
        MATRIX
            .iter()
            .map(|case| format!("/{}", case.subject))
            .collect()
    }

    /// Every revision movement against every view movement, for a file and for a directory.
    ///
    /// The four walks are one apiece for the view movements, so each subject is read out of the
    /// same walk its neighbours are: a case that emitted something for the wrong subject fails as
    /// a stray record rather than passing unseen.
    #[tokio::test]
    async fn the_routing_matrix_holds_for_every_subject() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::build(immutable_store, mutable_store, &Tree::matrix()).await;
                let rules = subject_rules();
                let rules: Vec<&str> = rules.iter().map(String::as_str).collect();

                let admitted =
                    test_reported(&fixture.changes(fixture.view(&[]), fixture.view(&[])).await);
                let leaving = test_reported(
                    &fixture
                        .changes(fixture.view(&[]), fixture.view(&rules))
                        .await,
                );
                let entering = test_reported(
                    &fixture
                        .changes(fixture.view(&rules), fixture.view(&[]))
                        .await,
                );
                let neither = test_reported(
                    &fixture
                        .changes(fixture.view(&rules), fixture.view(&rules))
                        .await,
                );

                assert_eq!(
                    neither,
                    Vec::<(String, String)>::new(),
                    "a subject neither view admits stands on disk under neither, so a walk has \
                     nothing to say about it however the revisions moved it"
                );

                for case in MATRIX {
                    for (movement, changes, records) in [
                        ("both views admit it", &admitted, case.admitted),
                        ("the to view drops it", &leaving, case.leaving),
                        ("the from view dropped it", &entering, case.entering),
                    ] {
                        assert_eq!(
                            records_for(changes, case.subject),
                            expected(records),
                            "{}, where {movement}",
                            case.movement
                        );
                    }
                }

                for (movement, changes) in [
                    ("both views admit them", &admitted),
                    ("the to view drops them", &leaving),
                    ("the from view dropped them", &entering),
                ] {
                    let stray: Vec<&(String, String)> = changes
                        .iter()
                        .filter(|(_, path)| MATRIX.iter().all(|case| !below(path, case.subject)))
                        .collect();
                    assert!(
                        stray.is_empty(),
                        "a walk where {movement} named something no subject holds: {stray:?}"
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }

    /// A tree in two revisions: what the first holds, and what the second does to it.
    struct Tree {
        /// Every file of the first revision.
        files: Vec<String>,
        /// The files the second revision rewrites, adds and removes.
        modified: Vec<String>,
        added: Vec<String>,
        deleted: Vec<String>,
        /// The files the second revision marks executable, leaving their content as it stands:
        /// the executable bit is a modification in its own right.
        executable: Vec<String>,
        /// The directories the second revision does not hold, removed once what they held is.
        emptied: Vec<String>,
    }

    /// The paths as a tree holds them, for a set written out as literals.
    fn owned(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| path.to_string()).collect()
    }

    impl Tree {
        /// The matrix's tree: one subject per revision movement, as a file and as a directory.
        fn matrix() -> Self {
            Self {
                files: owned(&[
                    UNCHANGED_FILE,
                    MODIFIED_FILE,
                    DELETED_FILE,
                    UNCHANGED_TREE_FILE,
                    MODIFIED_TREE_FILE,
                    DELETED_TREE_FILE,
                ]),
                modified: owned(&[MODIFIED_FILE, MODIFIED_TREE_FILE]),
                added: owned(&[ADDED_FILE, ADDED_TREE_FILE]),
                deleted: owned(&[DELETED_FILE, DELETED_TREE_FILE]),
                executable: Vec::new(),
                emptied: owned(&[DELETED_TREE]),
            }
        }

        /// The tree `seed` draws, and the mutations that make the second revision differ from it.
        fn drawn(seed: u64) -> (Self, StdRng) {
            let mut rng = StdRng::seed_from_u64(seed);
            let mut files = Vec::new();

            for index in 0..rng.random_range(0..MAX_FILES) {
                files.push(generated_file(&mut rng, "", index));
            }
            for top in 0..TOP_LEVEL {
                let directory = format!("d{top}");
                for index in 0..rng.random_range(1..=MAX_FILES) {
                    files.push(generated_file(&mut rng, &directory, index));
                }
                for nested in 0..rng.random_range(0..3u32) {
                    let child = format!("{directory}/s{nested}");
                    for index in 0..rng.random_range(1..=MAX_FILES) {
                        files.push(generated_file(&mut rng, &child, index));
                    }
                }
            }

            let mut modified = Vec::new();
            let mut deleted = Vec::new();
            let mut executable = Vec::new();
            for path in &files {
                match rng.random_range(0..10u32) {
                    0 | 1 => modified.push(path.clone()),
                    2 => deleted.push(path.clone()),
                    3 => executable.push(path.clone()),
                    _ => {}
                }
            }
            let mut drawn = Self {
                files,
                modified,
                added: Vec::new(),
                deleted,
                executable,
                emptied: Vec::new(),
            };
            let directories = drawn.directories();
            drawn.added = (0..rng.random_range(1..=3u32))
                .map(|index| {
                    let directory = directories[rng.random_range(0..directories.len())];
                    format!("{directory}/added{index}.uasset")
                })
                .collect();

            (drawn, rng)
        }

        /// Every directory the tree's files sit in, ordered, so a rule can be drawn against one the
        /// tree holds.
        ///
        /// Derived rather than carried beside the files: every directory a generated tree holds has
        /// a file directly in it, so the two would be the same list kept in two places.
        fn directories(&self) -> Vec<&str> {
            self.files
                .iter()
                .filter_map(|path| path.rsplit_once('/').map(|(parent, _name)| parent))
                .collect::<BTreeSet<&str>>()
                .into_iter()
                .collect()
        }

        /// The view pairs this tree is walked under, each as the two sides' rules.
        ///
        /// The four shapes a view change takes in the field, and one drawn from the tree itself:
        /// narrowing from everything, widening to everything, two views differing in a
        /// re-inclusion alone, and a rule no prefix bounds, which is what leaves the walk no
        /// subtree it can prune on content equality.
        fn view_pairs(&self, rng: &mut StdRng) -> Vec<(String, Vec<String>, Vec<String>)> {
            let directories = self.directories();
            let directory = directories[rng.random_range(0..directories.len())];
            let within = self
                .files
                .iter()
                .find(|path| path.starts_with(&format!("{directory}/")))
                .expect("every generated directory holds at least one file");
            let reincluded = format!("!/{within}");

            vec![
                (
                    "narrowing from everything".to_string(),
                    vec![],
                    vec![format!("/{directory}")],
                ),
                (
                    "widening to everything".to_string(),
                    vec![format!("/{directory}")],
                    vec![],
                ),
                (
                    "a re-inclusion alone".to_string(),
                    vec![format!("/{directory}"), reincluded],
                    vec![format!("/{directory}")],
                ),
                (
                    "an unanchored exclusion".to_string(),
                    vec![],
                    vec![format!("*.{TEMPORARY}")],
                ),
                (
                    "drawn from the tree".to_string(),
                    self.rules(&directories, rng),
                    self.rules(&directories, rng),
                ),
            ]
        }

        /// One to three rules naming directories and files this tree holds.
        fn rules(&self, directories: &[&str], rng: &mut StdRng) -> Vec<String> {
            (0..rng.random_range(1..=3u32))
                .map(|_| match rng.random_bool(0.5) {
                    true => format!("/{}", directories[rng.random_range(0..directories.len())]),
                    false => format!("/{}", self.files[rng.random_range(0..self.files.len())]),
                })
                .collect()
        }
    }

    /// A file in `directory`, some of them carrying the extension the unanchored rule names.
    fn generated_file(rng: &mut StdRng, directory: &str, index: u32) -> String {
        let extension = match rng.random_bool(0.25) {
            true => TEMPORARY,
            false => "uasset",
        };
        match directory.is_empty() {
            true => format!("f{index}.{extension}"),
            false => format!("{directory}/f{index}.{extension}"),
        }
    }

    /// Asserts that the second revision records the mode changes the tree asks for.
    ///
    /// Without this the mode half of every comparison below could be comparing one value with
    /// itself and say so nowhere.
    #[cfg(unix)]
    async fn mode_changes_are_recorded(fixture: &Fixture, tree: &Tree, seed: u64) {
        if tree.executable.is_empty() {
            return;
        }
        let repository = fixture.instance.repository.clone();
        let older = nodes_in(&repository, &fixture.older).await;
        let newer = nodes_in(&repository, &fixture.newer).await;
        assert!(
            tree.executable
                .iter()
                .any(|path| older.get(path) != newer.get(path)),
            "seed {seed}: the second revision carries none of the {} mode changes the tree makes, \
             so nothing here states what one costs",
            tree.executable.len()
        );
    }

    /// No mode to record where the filesystem records none.
    #[cfg(not(unix))]
    async fn mode_changes_are_recorded(_fixture: &Fixture, _tree: &Tree, _seed: u64) {}

    /// Every generated tree, under every view pair, carries the set one view materializes to the
    /// set the other does -- and emits no record that was not the working tree's to take.
    ///
    /// This is the only thing here that states the set exactly. The matrix names the cases someone
    /// thought of; the walk has to be right about the ones nobody did.
    ///
    /// Every pair below moves something, so the count this ends on cannot fall to zero as the
    /// generator stands. It is asserted for the generator that replaces this one.
    #[tokio::test]
    async fn a_generated_view_change_carries_the_materialized_set_exactly() {
        for seed in 0..SEEDS {
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("Failed to create stores");

            runtime()
                .spawn(LORE_CONTEXT.scope(execution, async move {
                    let (tree, mut rng) = Tree::drawn(seed);
                    let fixture = Fixture::build(immutable_store, mutable_store, &tree).await;
                    mode_changes_are_recorded(&fixture, &tree, seed).await;
                    let mut moved = 0;

                    for (label, from_rules, to_rules) in tree.view_pairs(&mut rng) {
                        let from_rules: Vec<&str> = from_rules.iter().map(String::as_str).collect();
                        let to_rules: Vec<&str> = to_rules.iter().map(String::as_str).collect();
                        let from = fixture.view(&from_rules);
                        let to = fixture.view(&to_rules);

                        let (before, after) = fixture.materialized(&from, &to).await;
                        let changes = fixture.changes(from, to).await;
                        let (carried, redundant) = applied(&before, &changes);
                        moved += usize::from(before != after);

                        assert!(
                            redundant.is_empty(),
                            "seed {seed}, {label}: {redundant:?}, from {from_rules:?} to \
                             {to_rules:?}"
                        );
                        let disagreed = difference(&carried, &after);
                        assert!(
                            disagreed.is_empty(),
                            "seed {seed}, {label}: the change set must leave the tree holding \
                             what the to view materializes of the newer revision, from \
                             {from_rules:?} to {to_rules:?}: {disagreed:?}"
                        );
                    }

                    assert!(
                        moved > 0,
                        "seed {seed} drew a tree its view pairs materialize identically, which \
                         states nothing about a view change"
                    );
                }))
                .await
                .expect("Test task failed");
        }
    }
}
