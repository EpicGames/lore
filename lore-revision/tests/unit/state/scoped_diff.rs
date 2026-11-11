// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A filesystem diff scoped to a path, including one at or below a link mount, reports what the
//! diff over the whole tree reports below that path.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use lore_base::types::Address;
use lore_base::types::Hash;
use lore_revision::fs::filesystem_provider::FilesystemDiffIntent;
use lore_revision::fs::filesystem_provider::StageIntent;
use lore_revision::interface::LoreFileAction;
use lore_revision::link::LinkFlags;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::node::*;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_storage::hash::hash_string;

use crate::helper::committed_repository;
use crate::helper::in_execution;
use crate::helper::next_node;
use crate::helper::next_tree;
use crate::helper::relative_path;
use crate::helper::test_file_write;
use crate::helper::test_reported;
use crate::helper::test_scan_path_result;
use crate::helper::test_status_actions;
use crate::helper::test_status_files_at;

/// Writes `contents` to the file at `path` below `root`, with the directories above it.
fn write(root: &std::path::Path, path: &str, contents: &[u8]) {
    let file = root.join(path);
    std::fs::create_dir_all(file.parent().expect("A file has a parent"))
        .expect("Create directory failed");
    test_file_write(file.as_path(), contents);
}

/// A link to the tree `revision` of the repository `id`, exposing its node `child`.
struct Link {
    id: RepositoryId,
    revision: Hash,
    child: NodeID,
}

/// The node at `path` in `state`, adding a directory for each component it lacks.
async fn directory_at(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    path: &str,
) -> NodeID {
    let mut node = ROOT_NODE;
    let mut walked = String::new();
    for name in path.split('/').filter(|name| !name.is_empty()) {
        if !walked.is_empty() {
            walked.push('/');
        }
        walked.push_str(name);
        node = match state.find_node_link(repository.clone(), &walked).await {
            Ok(found) if found.is_valid() => found.node,
            _ => state
                .node_add(
                    repository.clone(),
                    node,
                    Node {
                        name_hash: hash_string(name),
                        ..Default::default()
                    },
                    name,
                )
                .await
                .expect("Adding a directory failed"),
        };
    }
    node
}

/// Adds to `state` a file at each of `files`, and a link at each of `links`.
async fn fill(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    files: &[&str],
    links: &[(&str, &Link)],
) {
    for path in files {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        let parent = directory_at(repository, state, parent).await;
        state
            .node_add(
                repository.clone(),
                parent,
                Node {
                    flags: NodeFlags::File.bits(),
                    name_hash: hash_string(name),
                    ..Default::default()
                },
                name,
            )
            .await
            .expect("Adding a file failed");
    }
    for (path, link) in links {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        let parent = directory_at(repository, state, parent).await;
        let node = state
            .node_add(
                repository.clone(),
                parent,
                Node {
                    flags: NodeFlags::Link.bits(),
                    name_hash: hash_string(name),
                    child: link.child,
                    address: Address {
                        hash: link.revision,
                        context: link.id.into(),
                    },
                    ..Default::default()
                },
                name,
            )
            .await
            .expect("Adding a link failed");
        state
            .link_add(
                repository.clone(),
                link.id,
                BranchId::default(),
                link.revision,
                node,
                LinkFlags::NoFlags,
            )
            .await
            .expect("Registering a link failed");
    }
}

/// Stores, as revision `number` of the repository `id`, a tree holding `files` and `links`,
/// and answers a link to its root.
async fn linked_tree(
    repository: &Arc<RepositoryContext>,
    id: RepositoryId,
    number: u64,
    files: &[&str],
    links: &[(&str, &Link)],
) -> Link {
    let linked = repository.to_link_context(id).await;
    let tree = State::new();
    fill(&linked, &tree, files, links).await;
    tree.set_revision_number(number);
    let revision = tree
        .serialize(
            linked,
            repository
                .try_write_token()
                .expect("The repository was opened for writing"),
        )
        .await
        .expect("Serializing a linked tree failed");
    Link {
        id,
        revision,
        child: ROOT_NODE,
    }
}

/// A copy of the committed tree of `repository` holding `files` and `links`.
async fn parent_tree(
    repository: &Arc<RepositoryContext>,
    files: &[&str],
    links: &[(&str, &Link)],
) -> Arc<State> {
    let tree = State::deserialize(repository.clone(), next_tree(repository).await.revision())
        .await
        .expect("Deserializing the committed tree failed");
    fill(repository, &tree, files, links).await;
    tree
}

/// The trees a diff compares, as the files and links copies of the committed tree hold.
struct Trees<'a> {
    from_files: &'a [&'a str],
    from_links: &'a [(&'a str, &'a Link)],
    current_files: &'a [&'a str],
    current_links: &'a [(&'a str, &'a Link)],
}

/// The changes a diff of the working tree with `intent` against fresh copies of `trees` reports,
/// scoped to `path` where given, as sorted (action, path) pairs. The copies are fresh because a
/// marking diff marks the from tree.
async fn reported(
    repository: &Arc<RepositoryContext>,
    trees: &Trees<'_>,
    path: Option<&str>,
    intent: FilesystemDiffIntent,
) -> Vec<(String, String)> {
    let changes = test_scan_path_result(
        repository.clone(),
        parent_tree(repository, trees.from_files, trees.from_links).await,
        parent_tree(repository, trees.current_files, trees.current_links).await,
        path.map(relative_path),
        intent,
    )
    .await
    .expect("Failed to diff filesystem");
    let mut reported = test_reported(&changes);
    reported.sort();
    reported
}

/// The entries of `changes` at and below `path`.
fn under(changes: &[(impl AsRef<str>, impl AsRef<str>)], path: &str) -> Vec<(String, String)> {
    let below = format!("{path}/");
    changes
        .iter()
        .map(|(action, changed)| (action.as_ref().to_string(), changed.as_ref().to_string()))
        .filter(|(_, changed)| changed == path || changed.starts_with(&below))
        .collect()
}

/// A tree a case links to: the files and links it holds, stored as a revision of the repository
/// `repository` names among the case's, exposing its node at `child` where given, and pinned to
/// a revision no store holds where `unreadable`.
struct Spec {
    repository: u8,
    files: &'static [&'static str],
    links: Vec<(&'static str, Spec)>,
    child: Option<&'static str>,
    unreadable: bool,
}

fn spec(repository: u8, files: &'static [&'static str]) -> Spec {
    Spec {
        repository,
        files,
        links: Vec::new(),
        child: None,
        unreadable: false,
    }
}

impl Spec {
    fn link(mut self, path: &'static str, spec: Spec) -> Self {
        self.links.push((path, spec));
        self
    }

    fn exposing(mut self, child: &'static str) -> Self {
        self.child = Some(child);
        self
    }

    fn unreadable(mut self) -> Self {
        self.unreadable = true;
        self
    }
}

/// Stores `spec` and answers a link to it, each of the case's repositories taking one id and each
/// tree stored in it the next revision number.
fn store<'a>(
    repository: &'a Arc<RepositoryContext>,
    spec: &'a Spec,
    ids: &'a mut HashMap<u8, (RepositoryId, u64)>,
) -> Pin<Box<dyn Future<Output = Link> + Send + 'a>> {
    Box::pin(async move {
        let mut nested = Vec::new();
        for (path, inner) in &spec.links {
            nested.push((*path, store(repository, inner, ids).await));
        }
        let entry = ids
            .entry(spec.repository)
            .or_insert_with(|| (RepositoryId::from(uuid::Uuid::now_v7()), 0));
        entry.1 += 1;
        let (id, number) = *entry;
        let links: Vec<(&str, &Link)> = nested.iter().map(|(path, link)| (*path, link)).collect();
        let mut link = linked_tree(repository, id, number, spec.files, &links).await;
        if let Some(child) = spec.child {
            link.child = linked_node(repository, &link, child).await;
        }
        if spec.unreadable {
            link.revision = Hash::from([7; 32]);
        }
        link
    })
}

/// What one side of a case's diff holds beyond the committed tree.
struct Side {
    files: &'static [&'static str],
    links: Vec<(&'static str, Spec)>,
}

fn side(files: &'static [&'static str], links: Vec<(&'static str, Spec)>) -> Side {
    Side { files, links }
}

/// A diff of a working tree holding `disk` against trees holding `from` and `current`, scoped to
/// each of `paths`, which reports what `report` and `marking` list at and below each path for a
/// diff that reports and one that marks what it finds.
struct Case {
    name: &'static str,
    disk: &'static [&'static str],
    from: Side,
    current: Side,
    paths: &'static [&'static str],
    report: &'static [(&'static str, &'static str)],
    marking: &'static [(&'static str, &'static str)],
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "a link only the from tree holds is compared against its linked tree",
            disk: &["vendor/src/a.txt"],
            from: side(&[], vec![("vendor", spec(1, &["src/a.txt"]))]),
            current: side(&[], vec![]),
            paths: &["vendor", "vendor/src"],
            report: &[("M", "vendor/src/a.txt")],
            marking: &[("M", "vendor/src/a.txt")],
        },
        Case {
            name: "what is under a link only the current tree holds belongs to the link",
            disk: &["vendor/src/a.txt"],
            from: side(&[], vec![]),
            current: side(&[], vec![("vendor", spec(1, &["src/a.txt"]))]),
            paths: &["vendor", "vendor/src"],
            report: &[],
            marking: &[],
        },
        Case {
            name: "a link only the current tree holds over a from directory clears the mounts",
            disk: &["vendor/lib/b.txt", "vendor/lib/inner/c.txt"],
            from: side(&["vendor/lib/b.txt"], vec![]),
            current: side(
                &[],
                vec![(
                    "vendor",
                    spec(1, &["lib/b.txt"]).link("lib/inner", spec(2, &["c.txt"])),
                )],
            ),
            paths: &["vendor", "vendor/lib", "vendor/lib/inner/c.txt"],
            report: &[
                ("A", "vendor/lib/inner"),
                ("A", "vendor/lib/inner/c.txt"),
                ("M", "vendor/lib/b.txt"),
            ],
            marking: &[
                ("A", "vendor/lib/inner"),
                ("A", "vendor/lib/inner/c.txt"),
                ("M", "vendor/lib/b.txt"),
            ],
        },
        Case {
            name: "a missing mount of a link exposing a directory keeps its link",
            disk: &[],
            from: side(
                &[],
                vec![("vendor", spec(1, &["src/lib/a.txt"]).exposing("src"))],
            ),
            current: side(
                &[],
                vec![("vendor", spec(1, &["src/lib/a.txt"]).exposing("src"))],
            ),
            paths: &["vendor", "vendor/lib"],
            report: &[],
            marking: &[],
        },
        Case {
            name: "a missing mount of an unreadable link keeps its link unread",
            disk: &[],
            from: side(&[], vec![("vendor", spec(1, &[]).unreadable())]),
            current: side(&[], vec![("vendor", spec(1, &[]).unreadable())]),
            paths: &["vendor", "vendor/src"],
            report: &[],
            marking: &[],
        },
        Case {
            name: "a mount the working tree holds as a file is compared as the link",
            disk: &["vendor"],
            from: side(&[], vec![("vendor", spec(1, &["src/a.txt"]))]),
            current: side(&[], vec![("vendor", spec(1, &["src/a.txt"]))]),
            paths: &["vendor", "vendor/src"],
            report: &[("A", "vendor"), ("D", "vendor")],
            marking: &[("A", "vendor"), ("D", "vendor")],
        },
        Case {
            name: "a link added in the linked tree between pins is left to its repository",
            disk: &["vendor/lib/b.txt", "vendor/lib/inner/c.txt"],
            from: side(&[], vec![("vendor", spec(1, &["lib/b.txt"]))]),
            current: side(
                &[],
                vec![(
                    "vendor",
                    spec(1, &["lib/b.txt"]).link("lib/inner", spec(2, &["c.txt"])),
                )],
            ),
            paths: &["vendor", "vendor/lib"],
            report: &[("M", "vendor/lib/b.txt")],
            marking: &[("M", "vendor/lib/b.txt")],
        },
        Case {
            name: "below links both trees hold, the mounts follow the last link crossed",
            disk: &["vendor/lib/b.txt", "vendor/lib/inner/n.txt"],
            from: side(&[], vec![("vendor", spec(1, &["lib/b.txt"]))]),
            current: side(
                &[],
                vec![(
                    "vendor",
                    spec(1, &[]).link("lib", spec(2, &[]).link("inner", spec(3, &["n.txt"]))),
                )],
            ),
            paths: &["vendor/lib"],
            report: &[
                ("A", "vendor/lib/inner"),
                ("A", "vendor/lib/inner/n.txt"),
                ("M", "vendor/lib/b.txt"),
            ],
            marking: &[
                ("A", "vendor/lib/b.txt"),
                ("A", "vendor/lib/inner"),
                ("A", "vendor/lib/inner/n.txt"),
            ],
        },
        Case {
            name: "a from link shallower than the current tree's is decided first",
            disk: &["vendor/lib/b.txt"],
            from: side(&[], vec![("vendor", spec(1, &["lib/b.txt"]))]),
            current: side(&[], vec![("vendor/lib", spec(2, &["b.txt"]))]),
            paths: &["vendor", "vendor/lib", "vendor/lib/b.txt"],
            report: &[("M", "vendor/lib/b.txt")],
            marking: &[("M", "vendor/lib/b.txt")],
        },
        Case {
            name: "a current link shallower than the from tree's is decided first",
            disk: &["vendor/lib/b.txt"],
            from: side(&[], vec![("vendor/lib", spec(2, &["b.txt"]))]),
            current: side(&[], vec![("vendor", spec(1, &["lib/b.txt"]))]),
            paths: &["vendor", "vendor/lib", "vendor/lib/b.txt"],
            report: &[("M", "vendor/lib/b.txt")],
            marking: &[("M", "vendor/lib/b.txt")],
        },
        Case {
            name: "a file at a link only the current tree holds is new",
            disk: &["vendor"],
            from: side(&[], vec![]),
            current: side(&[], vec![("vendor", spec(1, &["src/a.txt"]))]),
            paths: &["vendor", "vendor/src"],
            report: &[("A", "vendor")],
            marking: &[("A", "vendor")],
        },
        Case {
            name: "a link only the current tree holds over a missing from directory",
            disk: &[],
            from: side(&["vendor/lib/b.txt"], vec![]),
            current: side(&[], vec![("vendor", spec(1, &["lib/b.txt"]))]),
            paths: &["vendor", "vendor/lib"],
            report: &[("D", "vendor/lib"), ("D", "vendor/lib/b.txt")],
            marking: &[("D", "vendor/lib"), ("D", "vendor/lib/b.txt")],
        },
        Case {
            name: "a link only the current tree holds over a from directory held as a file",
            disk: &["vendor"],
            from: side(&["vendor/lib/b.txt"], vec![]),
            current: side(&[], vec![("vendor", spec(1, &["lib/b.txt"]))]),
            paths: &["vendor", "vendor/lib"],
            report: &[
                ("A", "vendor"),
                ("D", "vendor"),
                ("D", "vendor/lib"),
                ("D", "vendor/lib/b.txt"),
            ],
            marking: &[
                ("A", "vendor"),
                ("D", "vendor"),
                ("D", "vendor/lib"),
                ("D", "vendor/lib/b.txt"),
            ],
        },
        Case {
            name: "a from file at a link only the current tree holds over a directory",
            disk: &["vendor/x.txt", "vendor/dir/y.txt"],
            from: side(&["vendor"], vec![]),
            current: side(&[], vec![("vendor", spec(1, &["lib/b.txt"]))]),
            paths: &["vendor", "vendor/x.txt", "vendor/dir/y.txt"],
            report: &[
                ("A", "vendor"),
                ("A", "vendor/dir"),
                ("A", "vendor/dir/y.txt"),
                ("A", "vendor/x.txt"),
                ("D", "vendor"),
            ],
            marking: &[("A", "vendor"), ("D", "vendor")],
        },
        Case {
            name: "a from file below a link only the current tree holds, over a nested link",
            disk: &["vendor/lib/b.txt", "vendor/lib/inner/c.txt"],
            from: side(&["vendor/lib"], vec![]),
            current: side(
                &[],
                vec![(
                    "vendor",
                    spec(1, &["lib/b.txt"]).link("lib/inner", spec(2, &["c.txt"])),
                )],
            ),
            paths: &["vendor/lib/inner/c.txt"],
            report: &[("A", "vendor/lib/inner"), ("A", "vendor/lib/inner/c.txt")],
            marking: &[],
        },
        Case {
            name: "a from file the working tree holds as a nested repository is left to it",
            disk: &["a/.lore/x", "a/b.txt"],
            from: side(&["a"], vec![]),
            current: side(&["a"], vec![]),
            paths: &["a"],
            report: &[("A", "a"), ("D", "a")],
            marking: &[("A", "a"), ("D", "a")],
        },
        Case {
            name: "a file in a linked tree the working tree holds as a directory",
            disk: &["vendor/lib/x.txt"],
            from: side(&[], vec![("vendor", spec(1, &["lib"]))]),
            current: side(&[], vec![("vendor", spec(1, &["lib"]))]),
            paths: &["vendor/lib", "vendor/lib/x.txt"],
            report: &[
                ("A", "vendor/lib"),
                ("A", "vendor/lib/x.txt"),
                ("D", "vendor/lib"),
            ],
            marking: &[("A", "vendor/lib"), ("D", "vendor/lib")],
        },
        Case {
            name: "a from file the working tree holds as a directory, with no link",
            disk: &["a/b.txt", "a/sub/c.txt"],
            from: side(&["a"], vec![]),
            current: side(&["a"], vec![]),
            paths: &["a", "a/b.txt", "a/sub/c.txt"],
            report: &[
                ("A", "a"),
                ("A", "a/b.txt"),
                ("A", "a/sub"),
                ("A", "a/sub/c.txt"),
                ("D", "a"),
            ],
            marking: &[("A", "a"), ("D", "a")],
        },
    ]
}

/// A diff scoped to a path, including one at or below a link mount, reports what the diff over
/// the whole tree reports at and below that path, for a diff that reports and one that marks what it finds, as
/// `lore file diff` and `lore status --scan` run them.
#[tokio::test]
async fn a_scoped_diff_reports_what_the_whole_tree_does_below_its_path() {
    in_execution(async {
        for case in cases() {
            let (repository, tempdir) = committed_repository("", &[]).await;
            let mut ids = HashMap::new();
            let mut from_links = Vec::new();
            for (path, spec) in &case.from.links {
                from_links.push((*path, store(&repository, spec, &mut ids).await));
            }
            let mut current_links = Vec::new();
            for (path, spec) in &case.current.links {
                current_links.push((*path, store(&repository, spec, &mut ids).await));
            }
            for path in case.disk {
                write(&tempdir, path, path.as_bytes());
            }
            let from_links: Vec<_> = from_links.iter().map(|(p, l)| (*p, l)).collect();
            let current_links: Vec<_> = current_links.iter().map(|(p, l)| (*p, l)).collect();
            let trees = Trees {
                from_files: case.from.files,
                from_links: &from_links,
                current_files: case.current.files,
                current_links: &current_links,
            };
            for (intent, expected) in [
                (FilesystemDiffIntent::Report, case.report),
                (FilesystemDiffIntent::MarkDirty, case.marking),
            ] {
                let whole_tree = reported(&repository, &trees, None, intent).await;
                for path in case.paths {
                    let scoped = reported(&repository, &trees, Some(path), intent).await;
                    let name = case.name;
                    assert_eq!(
                        scoped,
                        under(&whole_tree, path),
                        "{name}: the diff with {intent:?} scoped to {path} reported other than \
                         the diff over the whole tree"
                    );
                    assert_eq!(
                        scoped,
                        under(expected, path),
                        "{name}: the diff with {intent:?} scoped to {path} reported other than \
                         expected"
                    );
                }
            }
        }
    })
    .await;
}

/// A staging diff scoped to a path the tree holds as a file and the working tree as a directory
/// stages the replacement and what the directory holds, as the diff over the whole tree does,
/// for a file in the parent tree and one in a linked tree.
#[tokio::test]
async fn a_staging_diff_scoped_to_a_file_replaced_by_a_directory_stages_its_content() {
    in_execution(async {
        let (repository, tempdir) = committed_repository("", &[]).await;
        write(&tempdir, "a/b.txt", b"b");
        write(&tempdir, "a/sub/c.txt", b"c");
        write(&tempdir, "vendor/lib/x.txt", b"x");
        let link = link_to(&repository, &["lib"]).await;
        let mounted = [("vendor", &link)];
        let trees = Trees {
            from_files: &["a"],
            from_links: &mounted,
            current_files: &["a"],
            current_links: &mounted,
        };
        let intent = FilesystemDiffIntent::Stage(StageIntent::default());
        let whole_tree = reported(&repository, &trees, None, intent).await;
        for (path, staged) in [
            (
                "a",
                &[
                    ("A", "a"),
                    ("A", "a/b.txt"),
                    ("A", "a/sub"),
                    ("A", "a/sub/c.txt"),
                    ("D", "a"),
                ][..],
            ),
            (
                "vendor/lib",
                &[
                    ("A", "vendor/lib"),
                    ("A", "vendor/lib/x.txt"),
                    ("D", "vendor/lib"),
                ][..],
            ),
        ] {
            let scoped = reported(&repository, &trees, Some(path), intent).await;
            assert_eq!(scoped, under(&whole_tree, path), "{path}");
            assert_eq!(scoped, under(staged, path), "{path}");
        }
    })
    .await;
}

/// A path the tree holds as a file and the working tree as a symbolic link to a directory, which
/// the walk over its parent skips, reads as the file deleted.
#[cfg(unix)]
#[tokio::test]
async fn a_diff_scoped_to_a_file_replaced_by_a_symbolic_link_reads_it_deleted() {
    in_execution(async {
        let (repository, tempdir) = committed_repository("", &[]).await;
        write(&tempdir, "elsewhere/b.txt", b"b");
        std::os::unix::fs::symlink(tempdir.join("elsewhere"), tempdir.join("a"))
            .expect("Creating the symbolic link failed");
        let trees = Trees {
            from_files: &["a"],
            from_links: &[],
            current_files: &["a"],
            current_links: &[],
        };
        for intent in [
            FilesystemDiffIntent::Report,
            FilesystemDiffIntent::MarkDirty,
        ] {
            let whole_tree = reported(&repository, &trees, None, intent).await;
            let scoped = reported(&repository, &trees, Some("a"), intent).await;
            assert_eq!(scoped, under(&whole_tree, "a"), "{intent:?}");
            assert_eq!(scoped, [("D".to_string(), "a".to_string())], "{intent:?}");
        }
    })
    .await;
}

/// A mount the working tree holds as a symbolic link, which the listing above a path leaves out,
/// keeps its link below it, as the walk over the whole tree keeps a mount it does not list.
#[cfg(unix)]
#[tokio::test]
async fn a_diff_scoped_below_a_mount_held_as_a_symbolic_link_keeps_the_link() {
    in_execution(async {
        let (repository, tempdir) = committed_repository("", &[]).await;
        let link = link_to(&repository, &["src/a.txt"]).await;
        write(&tempdir, "elsewhere/src/a.txt", b"a");
        std::os::unix::fs::symlink(tempdir.join("elsewhere"), tempdir.join("vendor"))
            .expect("Creating the symbolic link failed");

        let mounted = [("vendor", &link)];
        let trees = Trees {
            from_files: &[],
            from_links: &mounted,
            current_files: &[],
            current_links: &mounted,
        };
        for intent in [
            FilesystemDiffIntent::Report,
            FilesystemDiffIntent::MarkDirty,
        ] {
            let whole_tree = reported(&repository, &trees, None, intent).await;
            let scoped = reported(&repository, &trees, Some("vendor/src"), intent).await;
            assert_eq!(scoped, under(&whole_tree, "vendor/src"), "{intent:?}");
            assert_eq!(
                scoped,
                [],
                "{intent:?}: the scoped diff reported below the link"
            );
        }
    })
    .await;
}

/// A link of `repository` to a stored tree holding `files`.
async fn link_to(repository: &Arc<RepositoryContext>, files: &[&str]) -> Link {
    linked_tree(
        repository,
        RepositoryId::from(uuid::Uuid::now_v7()),
        1,
        files,
        &[],
    )
    .await
}

/// The node at `path` in the tree `link` links to.
async fn linked_node(repository: &Arc<RepositoryContext>, link: &Link, path: &str) -> NodeID {
    let linked = repository.to_link_context(link.id).await;
    State::deserialize(linked.clone(), link.revision)
        .await
        .expect("Deserializing the linked tree failed")
        .find_node_link(linked, path)
        .await
        .expect("The linked tree holds the path")
        .node
}

/// A marking diff scoped to a path below a mount that the linked tree lacks adds what it finds
/// to the linked tree, and leaves the link itself as it was, for a link exposing its
/// linked tree's root and one exposing a directory of it.
#[tokio::test]
async fn a_marking_diff_scoped_below_a_mount_to_a_new_path_leaves_the_link_alone() {
    in_execution(async {
        let (repository, tempdir) = committed_repository("", &[]).await;
        let root = link_to(&repository, &["src/a.txt"]).await;
        let sub = Link {
            child: linked_node(&repository, &root, "src").await,
            ..link_to(&repository, &["src/a.txt"]).await
        };
        write(&tempdir, "vendor/x.txt", b"x");
        write(&tempdir, "vendor/newdir/x.txt", b"x");

        for link in [&root, &sub] {
            let mounted = [("vendor", link)];
            for (path, added) in [
                ("vendor/newdir", "vendor/newdir/x.txt"),
                ("vendor/newdir/x.txt", "vendor/newdir/x.txt"),
                ("vendor/x.txt", "vendor/x.txt"),
            ] {
                let from = parent_tree(&repository, &[], &mounted).await;
                let link_node = from
                    .find_node_link(repository.clone(), "vendor")
                    .await
                    .expect("The tree holds the link")
                    .node;
                let before = from
                    .node(repository.clone(), link_node)
                    .await
                    .expect("The link node reads");
                let changes = test_scan_path_result(
                    repository.clone(),
                    from.clone(),
                    parent_tree(&repository, &[], &mounted).await,
                    Some(relative_path(path)),
                    FilesystemDiffIntent::MarkDirty,
                )
                .await
                .unwrap_or_else(|err| panic!("The diff scoped to {path} failed: {err}"));
                assert_eq!(
                    test_reported(&changes),
                    [("A".to_string(), added.to_string())],
                    "the diff scoped to {path} reported other than the new file"
                );
                assert_eq!(
                    changes[0].to.mapping.repository.id, link.id,
                    "the diff scoped to {path} added the new file outside the linked tree"
                );
                let after = from
                    .node(repository.clone(), link_node)
                    .await
                    .expect("The link node reads");
                assert_eq!(
                    (after.child, after.flags),
                    (before.child, before.flags),
                    "the diff scoped to {path} changed the link node"
                );
            }
        }
    })
    .await;
}

/// A status scan of several new paths below a link mount, which share a new directory, reports
/// them and leaves the link in the staged tree as it was.
#[tokio::test]
async fn a_status_scan_of_new_paths_below_a_mount_leaves_the_link_alone() {
    in_execution(async {
        let (repository, tempdir) = committed_repository("", &[]).await;
        let link = link_to(&repository, &["src/a.txt"]).await;
        write(&tempdir, "vendor/src/a.txt", b"a");
        write(&tempdir, "vendor/new/a.txt", b"a");
        write(&tempdir, "vendor/new/b.txt", b"b");
        let staged = parent_tree(&repository, &[], &[("vendor", &link)]).await;
        let signature = staged
            .serialize(
                repository.clone(),
                repository
                    .try_write_token()
                    .expect("The repository was opened for writing"),
            )
            .await
            .expect("Serializing the staged tree failed");
        lore_revision::instance::store_staged_anchor(&repository, signature)
            .await
            .expect("Storing the staged anchor failed");
        let before = next_node(&repository, "vendor").await;

        let reported = test_status_files_at(
            repository.clone(),
            Some(vec![
                relative_path("vendor/new/a.txt"),
                relative_path("vendor/new/b.txt"),
            ]),
            true,
        )
        .await;
        assert_eq!(
            test_status_actions(reported),
            [
                ("vendor/new/a.txt".to_string(), LoreFileAction::Add),
                ("vendor/new/b.txt".to_string(), LoreFileAction::Add),
            ],
            "the status scan reported other than the new files"
        );

        let after = next_node(&repository, "vendor").await;
        assert_eq!(
            (after.child, after.flags, after.address.hash),
            (before.child, before.flags, before.address.hash),
            "the status scan changed the link"
        );
    })
    .await;
}

/// A status scan of several new paths below a file the staged tree holds and the working tree
/// holds as a directory leaves the file in the staged tree, as a scan of each path alone does.
#[tokio::test]
async fn a_status_scan_of_new_paths_below_a_staged_file_keeps_the_file() {
    in_execution(async {
        let (repository, tempdir) = committed_repository("", &["a".to_string()]).await;
        #[allow(clippy::disallowed_methods)]
        std::fs::remove_file(tempdir.join("a")).expect("Removing the file failed");
        write(&tempdir, "a/x.txt", b"x");
        write(&tempdir, "a/y.txt", b"y");

        test_status_files_at(
            repository.clone(),
            Some(vec![relative_path("a/x.txt"), relative_path("a/y.txt")]),
            true,
        )
        .await;

        assert!(
            next_node(&repository, "a").await.is_file(),
            "the status scan replaced the staged file"
        );
    })
    .await;
}

/// A path below a mount whose linked tree cannot be read fails the diff, as the diff over the
/// whole tree fails on crossing the link, rather than reading as a path the tree does not hold.
#[tokio::test]
async fn a_diff_scoped_below_a_mount_whose_linked_tree_is_missing_fails_as_the_whole_tree_does() {
    in_execution(async {
        let (repository, tempdir) = committed_repository("", &[]).await;
        write(&tempdir, "vendor/src/a.txt", b"a");
        let missing = Link {
            revision: Hash::from([7; 32]),
            ..link_to(&repository, &[]).await
        };
        let mounted = [("vendor", &missing)];
        for path in [None, Some("vendor/src")] {
            let result = test_scan_path_result(
                repository.clone(),
                parent_tree(&repository, &[], &mounted).await,
                parent_tree(&repository, &[], &mounted).await,
                path.map(relative_path),
                FilesystemDiffIntent::Report,
            )
            .await;
            assert!(
                matches!(&result, Err(err) if err.is_not_found()),
                "the diff scoped to {path:?} over a link to a missing tree answered {result:?}"
            );
        }
    })
    .await;
}
