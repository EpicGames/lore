// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures build filesystem state directly; what these test is how the helpers read it.
#![allow(clippy::disallowed_methods)]

#[cfg(target_os = "linux")]
use std::fs::Metadata;
use std::path::Path;
use std::sync::Arc;

use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
use lore_revision::node::NodeFileMode;
use lore_revision::util::fs::*;
use lore_revision::util::path::DepthPath;
use lore_revision::util::path::RelativePath;

use crate::helper::case_insensitive;

fn temp_dir() -> lore_base::test_util::TempDir {
    lore_base::test_util::TempDir::new("lore-fs-test-")
}

/// A path of `depth` components, each named for its level.
fn nested_path(depth: usize) -> RelativePath {
    let path = (1..=depth)
        .map(|level| format!("level{level}"))
        .collect::<Vec<_>>()
        .join("/");
    std::str::FromStr::from_str(path.as_str()).expect("relative path")
}

/// Resolving a path the filesystem holds costs one lookup however deep it is: the path
/// is read whole and its components are not walked. A per-component walk reintroduced
/// here would show as a count that grows with the depth asked about.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_held_path_is_looked_up_once_however_deep() {
    use lore_revision::fs::filesystem_provider::FilesystemProvider;

    use crate::fs::filesystem_provider::TestFilesystemProvider;

    for depth in [1usize, 9] {
        let filesystem = Arc::new(TestFilesystemProvider::holding_every_path());
        let operation = FilesystemProvider::begin_operation(filesystem.as_ref())
            .await
            .expect("beginning an operation");
        let asked = nested_path(depth);

        let (resolved, info) = filesystem_path_and_info(&operation, "", &asked, None)
            .await
            .expect("a held path must resolve");

        assert_eq!(resolved.as_str(), asked.as_str());
        assert!(
            info.is_some(),
            "the lookup that settled the path answers for it"
        );
        assert_eq!(
            1,
            filesystem.file_infos(),
            "a path at depth {depth} must cost one lookup"
        );
        assert_eq!(
            0,
            filesystem.name_lookups(),
            "no component was resolved on its own"
        );
        assert_eq!(0, filesystem.directory_reads(), "no directory was read");
    }
}

/// A path the filesystem does not hold falls back to resolving a component at a time,
/// which is the only path that reads per component. It stops at the first component that
/// is not there, so that cost does not grow with the depth asked about either.
#[tokio::test]
async fn a_path_that_is_not_there_stops_at_its_first_component() {
    use lore_revision::fs::filesystem_provider::FilesystemProvider;

    use crate::fs::filesystem_provider::TestFilesystemProvider;

    for depth in [1usize, 9] {
        let filesystem = Arc::new(TestFilesystemProvider::new());
        let operation = FilesystemProvider::begin_operation(filesystem.as_ref())
            .await
            .expect("beginning an operation");

        assert!(
            filesystem_path_and_info(&operation, "", &nested_path(depth), None)
                .await
                .is_err(),
            "a path no component of which is there must not resolve"
        );
        assert_eq!(
            1,
            filesystem.name_lookups(),
            "a path at depth {depth} must stop at its first component"
        );
        assert_eq!(1, filesystem.directory_reads(), "one directory settles it");
    }
}

/// An operation over the OS filesystem, which is what the resolver reads through.
async fn os_operation(root: &Path) -> Arc<InstanceOperationImpl> {
    lore_revision::fs::filesystem_provider::FilesystemProvider::begin_operation(
        &lore_revision::fs::os::OsFilesystem::new(root),
    )
    .await
    .expect("beginning an operation over the OS filesystem")
}

#[test]
fn a_sole_variation_is_the_spelling_to_take() {
    let held = vec!["Assets".to_string()];
    assert_eq!(Some("Assets"), spelling_to_take(&held, "assets"));
}

/// The spelling asked for wins over its neighbours, which is what keeps a collision between
/// two variations from reading as an ambiguity for a caller that named one of them.
#[test]
fn the_spelling_asked_for_wins_over_a_coexisting_variation() {
    let held = vec!["Assets".to_string(), "assets".to_string()];
    assert_eq!(Some("assets"), spelling_to_take(&held, "assets"));
}

/// Several variations, none of them the one asked for, is the ambiguity the caller forks on.
#[test]
fn coexisting_variations_the_caller_did_not_name_are_no_answer() {
    let held = vec!["Assets".to_string(), "ASSETS".to_string()];
    assert_eq!(None, spelling_to_take(&held, "assets"));
}

#[test]
fn a_directory_holding_no_spelling_is_no_answer() {
    assert_eq!(None, spelling_to_take(&[], "assets"));
}

fn depth_paths(paths: &[&str]) -> Vec<DepthPath> {
    paths
        .iter()
        .map(|path| DepthPath::new((*path).to_string()))
        .collect()
}

/// A variation held beside the spelling asked for does not disturb resolving it. Where the
/// platform settles the lookup this never reads the directory; where it declines,
/// [`spelling_to_take`] answers the same. Only a case-sensitive filesystem can hold the two
/// directories this needs.
#[tokio::test]
async fn path_resolves_a_leaf_whose_case_variation_coexists() {
    let dir = temp_dir();
    if case_insensitive(dir.path()) {
        return;
    }
    let operation = os_operation(dir.path()).await;
    std::fs::create_dir(dir.path().join("Assets")).expect("create dir");
    std::fs::create_dir(dir.path().join("assets")).expect("create variation");

    let asked: RelativePath = std::str::FromStr::from_str("assets").expect("relative path");
    assert_eq!(
        "assets",
        filesystem_path(&operation, "", &asked, None)
            .await
            .expect("the spelling asked for must settle the directory")
            .as_str()
    );
}

/// The path already in the case the filesystem holds it in - the case
/// [`filesystem_path`] is built around - comes back unchanged.
#[tokio::test]
async fn path_resolves_a_path_already_in_the_case_on_disk() {
    let dir = temp_dir();
    let operation = os_operation(dir.path()).await;
    let nested = dir.path().join("Assets").join("Meshes");
    std::fs::create_dir_all(&nested).expect("create dirs");
    std::fs::write(nested.join("Rock.mesh"), b"").expect("write file");

    let asked: RelativePath =
        std::str::FromStr::from_str("Assets/Meshes/Rock.mesh").expect("relative path");
    let (resolved, info) = filesystem_path_and_info(&operation, "", &asked, None)
        .await
        .expect("the path must resolve");
    assert_eq!(resolved.as_str(), "Assets/Meshes/Rock.mesh");
    assert_eq!(
        info.is_some(),
        cfg!(target_os = "linux"),
        "the metadata comes back from the platforms that settle the path by reading it whole"
    );
}

/// A link or layer mount hands the resolver a base, and what comes back has to be relative
/// to that base rather than to the root the operation names paths from: a caller staging
/// inside a mount looks the answer up in the mount's own state, and a path with the mount
/// prefix still on it is one that state does not hold.
#[tokio::test]
async fn path_resolves_below_a_base_and_answers_relative_to_it() {
    let dir = temp_dir();
    let operation = os_operation(dir.path()).await;
    let nested = dir.path().join("Mount").join("Assets");
    std::fs::create_dir_all(&nested).expect("create dirs");
    std::fs::write(nested.join("Rock.mesh"), b"").expect("write file");

    let asked: RelativePath =
        std::str::FromStr::from_str("assets/rock.MESH").expect("relative path");
    let resolved = filesystem_path(&operation, "Mount", &asked, None)
        .await
        .expect("the path must resolve below the base");
    assert_eq!(resolved.as_str(), "Assets/Rock.mesh");
    assert_eq!(resolved.as_lowercase_str(), "assets/rock.mesh");
}

/// The lookup that settles a path already in the case on disk answers with the path as
/// asked, not with the base composed onto it. Only this shape reaches that lookup, since a
/// path in another case misses it and is settled a component at a time instead.
#[tokio::test]
async fn path_below_a_base_does_not_resolve_the_base_itself() {
    let dir = temp_dir();
    let operation = os_operation(dir.path()).await;
    let nested = dir.path().join("Mount").join("Deeper");
    std::fs::create_dir_all(&nested).expect("create dirs");
    std::fs::write(nested.join("leaf.txt"), b"").expect("write file");

    let asked: RelativePath =
        std::str::FromStr::from_str("Deeper/leaf.txt").expect("relative path");
    let resolved = filesystem_path(&operation, "Mount", &asked, None)
        .await
        .expect("the path must resolve below the base");
    assert_eq!(resolved.as_str(), "Deeper/leaf.txt");
}

/// A path is resolved a component at a time, so an ancestor in the wrong case has to be
/// corrected as well as the leaf.
#[tokio::test]
async fn path_resolves_every_component_to_its_stored_case_variation() {
    let dir = temp_dir();
    let operation = os_operation(dir.path()).await;
    if !case_insensitive(dir.path()) {
        return;
    }
    let nested = dir.path().join("Assets").join("Meshes");
    std::fs::create_dir_all(&nested).expect("create dirs");
    std::fs::write(nested.join("Rock.mesh"), b"").expect("write file");

    let asked = std::str::FromStr::from_str("assets/MESHES/rock.MESH")
        .expect("relative path is infallible");
    let resolved = filesystem_path(&operation, "", &asked, None)
        .await
        .expect("the path must resolve");
    assert_eq!(resolved.as_str(), "Assets/Meshes/Rock.mesh");
    assert_eq!(
        resolved.as_lowercase_str(),
        "assets/meshes/rock.mesh",
        "the lowercase form answers for the case that was resolved"
    );
}

/// Shared directories are resolved once, and every path under them then uses
/// what was established rather than looking again — including when the
/// case the caller has differs from the one on disk, which is the part
/// that would go wrong if the answer were not carried over.
///
/// Spellings are matched over a directory listing rather than by asking the
/// filesystem to look the name up, so this holds on a case-sensitive one as
/// much as on a case-insensitive one and is not conditioned on which it is.
#[tokio::test]
async fn resolved_prefixes_answer_for_the_paths_under_them() {
    let dir = temp_dir();
    let operation = os_operation(dir.path()).await;
    let nested = dir.path().join("Assets").join("Meshes");
    std::fs::create_dir_all(&nested).expect("create dirs");
    std::fs::write(nested.join("Rock.mesh"), b"").expect("write file");

    // Shallowest first, as the caller has them.
    let shared = depth_paths(&["assets", "assets/meshes"]);
    let prefixes = resolve_prefixes(&operation, dir.path(), &shared).await;

    assert_eq!(prefixes.len(), 2);
    assert_eq!(prefixes.longest_prefix_of("assets"), Some((1, "Assets")));
    assert_eq!(
        prefixes.longest_prefix_of("assets/meshes"),
        Some((2, "Assets/Meshes"))
    );
    assert_eq!(
        prefixes.longest_prefix_of("assets/meshes/rock.MESH"),
        Some((2, "Assets/Meshes")),
        "a path is covered by the longest prefix above it, not by itself"
    );
    assert_eq!(prefixes.longest_prefix_of("elsewhere/file"), None);

    let asked: RelativePath =
        std::str::FromStr::from_str("assets/meshes/rock.MESH").expect("relative path");
    let (resolved, metadata) = filesystem_path_and_info(&operation, "", &asked, Some(&prefixes))
        .await
        .expect("the path must resolve");
    assert_eq!(resolved.as_str(), "Assets/Meshes/Rock.mesh");
    assert_eq!(
        resolved.as_lowercase_str(),
        "assets/meshes/rock.mesh",
        "the prefix the map answered with carries a lowercase form of its own"
    );
    assert!(
        metadata.is_none(),
        "a path settled a component at a time is never read whole"
    );
}

/// A prefix that is not there, or that several case variations answer for, is left
/// out — so a path under it resolves as it would have with no map at all,
/// rather than being resolved against a directory that was guessed.
#[tokio::test]
async fn resolved_prefixes_leave_out_what_they_cannot_settle() {
    let dir = temp_dir();
    let operation = os_operation(dir.path()).await;
    std::fs::create_dir(dir.path().join("Assets")).expect("create dir");

    let shared = depth_paths(&["absent", "absent/deeper"]);
    let prefixes = resolve_prefixes(&operation, dir.path(), &shared).await;
    assert!(prefixes.is_empty(), "nothing there resolves");

    // The path still resolves through the walk, which reports it as missing
    // in the same way it would without a map.
    let asked: RelativePath = std::str::FromStr::from_str("absent/file").expect("relative path");
    assert!(
        filesystem_path(&operation, "", &asked, Some(&prefixes))
            .await
            .is_err()
    );

    if case_insensitive(dir.path()) {
        return;
    }
    std::fs::create_dir(dir.path().join("assets")).expect("create second variation");
    let shared = depth_paths(&["ASSETS"]);
    assert!(
        resolve_prefixes(&operation, dir.path(), &shared)
            .await
            .is_empty(),
        "two case variations answer for it, so the caller has to fork and decide"
    );
}

/// The map is an answer about the filesystem as it was when it was built. A
/// caller that renames while it stages - which `StageCaseChange::Keep` does,
/// including to the directories these prefixes name - must not be given one,
/// and this is what that would look like: the path still resolves, to the
/// case variation that is no longer there.
#[tokio::test]
async fn a_resolved_prefix_does_not_survive_the_directory_being_renamed() {
    let dir = temp_dir();
    let operation = os_operation(dir.path()).await;
    std::fs::create_dir(dir.path().join("Assets")).expect("create dir");
    std::fs::write(dir.path().join("Assets").join("rock.mesh"), b"").expect("write file");

    let prefixes = resolve_prefixes(&operation, dir.path(), &depth_paths(&["Assets"])).await;
    assert_eq!(prefixes.longest_prefix_of("Assets"), Some((1, "Assets")));

    std::fs::rename(dir.path().join("Assets"), dir.path().join("ASSETS")).expect("rename");

    let asked: RelativePath =
        std::str::FromStr::from_str("Assets/rock.mesh").expect("relative path");
    let afresh = filesystem_path(&operation, "", &asked, None).await.ok();
    assert_eq!(
        afresh.as_ref().map(RelativePath::as_str),
        Some("ASSETS/rock.mesh"),
        "resolving afresh finds the directory under the name it now has"
    );
    let mapped = filesystem_path(&operation, "", &asked, Some(&prefixes))
        .await
        .ok();
    assert_ne!(
        mapped.as_ref().map(RelativePath::as_str),
        Some("ASSETS/rock.mesh"),
        "the map still answers with the name the directory had"
    );
}

/// What the map is allowed to change is how long resolving takes, never what
/// it resolves to. Every shape that reaches it has to come back the same
/// either way, because everything downstream - the tree node the path is
/// compared against, and what a case change means for it - reads the result
/// and nothing else.
#[tokio::test]
async fn a_resolved_prefix_answers_exactly_as_the_walk_would() {
    let dir = temp_dir();
    let operation = os_operation(dir.path()).await;
    let nested = dir.path().join("Assets").join("Meshes");
    std::fs::create_dir_all(&nested).expect("create dirs");
    std::fs::write(nested.join("Rock.mesh"), b"").expect("write file");
    std::fs::create_dir(dir.path().join("Assets").join("Empty")).expect("create dir");

    let shared = depth_paths(&[
        "Assets",
        "assets",
        "Assets/Meshes",
        "assets/meshes",
        "absent",
    ]);
    let prefixes = resolve_prefixes(&operation, dir.path(), &shared).await;

    for asked in [
        // Given exactly as the filesystem holds it.
        "Assets/Meshes/Rock.mesh",
        // Given in another case, at the leaf, at an ancestor, and at both.
        "Assets/Meshes/rock.MESH",
        "assets/meshes/Rock.mesh",
        "ASSETS/MESHES/ROCK.MESH",
        // Not there at all, below a prefix that is and one that is not.
        "Assets/Meshes/absent.mesh",
        "absent/deeper/absent.mesh",
        // A directory rather than a file, and a single component.
        "Assets/Empty",
        "Assets",
    ] {
        let asked: RelativePath = std::str::FromStr::from_str(asked).expect("relative path");
        assert_eq!(
            filesystem_path(&operation, "", &asked, Some(&prefixes))
                .await
                .ok(),
            filesystem_path(&operation, "", &asked, None).await.ok(),
            "{asked} must resolve the same with the map as without it"
        );
    }
}

const EXEC: u16 = NodeFileMode::Executable.bits();

#[test]
fn an_observed_bit_replaces_the_stored_one() {
    assert_eq!(EXEC, mode_from_observed(true, Some(true), 0));
    assert_eq!(0, mode_from_observed(true, Some(false), EXEC));
}

#[test]
fn an_unobserved_bit_keeps_the_stored_one() {
    assert_eq!(EXEC, mode_from_observed(true, None, EXEC));
    assert_eq!(0, mode_from_observed(true, None, 0));
}

#[test]
fn only_a_file_carries_a_mode() {
    assert_eq!(0, mode_from_observed(false, Some(true), EXEC));
    assert_eq!(0, mode_from_observed(false, None, EXEC));
}

/// An unlink holds no `Metadata` across the removal it awaits: the stat it decides by is reduced
/// to the kind, and the permissions a retry needs are read in a boxed future of their own.
#[cfg(target_os = "linux")]
#[test]
fn an_unlink_holds_no_metadata() {
    let path = Path::new("unlinked");
    let query = lore_io::IoDriver::global().metadata(path);
    let removal = unlink(path);

    assert!(
        size_of_val(&removal) < size_of_val(&query) + size_of::<Metadata>(),
        "an unlink holds {} bytes, a metadata query {}",
        size_of_val(&removal),
        size_of_val(&query)
    );
}

/// A directory holding entries is kept as it stands: clearing its read-only flag could not let it
/// be removed, and would leave it writable by everyone.
#[cfg(unix)]
#[tokio::test]
async fn an_unlink_leaves_a_directory_holding_entries_as_it_stands() {
    use std::os::unix::fs::PermissionsExt;

    let temp = temp_dir();
    let directory = temp.path().join("held");
    std::fs::create_dir(&directory).expect("create the directory");
    std::fs::write(directory.join("entry"), b"held").expect("write the entry");
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755))
        .expect("set the directory's mode");

    assert!(
        unlink(&directory).await.is_err(),
        "a directory holding entries is not removed"
    );
    let mode = std::fs::metadata(&directory)
        .expect("the directory stays")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755, "the directory keeps its mode");
}

/// A file whose user bit already carries the executable bit asked for is left as it is, group
/// and other bits included, whether its mode is read from the file or given. Setting a mode moves
/// the file's change time even when the mode stays the same, so a matching bit must leave the
/// time where it was. Clearing the bit then clears it for user, group and others and moves the
/// time, which shows the clock had advanced far enough for a change to be seen.
#[cfg(unix)]
#[tokio::test]
async fn a_file_whose_user_bit_already_matches_is_left_as_it_is() {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    lore_base::runtime::LORE_CONTEXT
        .scope(
            crate::fs::filesystem_provider::setup_test_execution(),
            async {
                let temp = temp_dir();
                let path = temp.path().join("tool");
                std::fs::write(&path, b"tool").expect("write the file");
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o744))
                    .expect("set the file's mode");
                let before = std::fs::metadata(&path).expect("stat the file");
                wait_for_change_time_past(temp.path(), &before);

                set_executable(&path, None, true)
                    .await
                    .expect("set the bit");
                set_executable(&path, Some(&before), true)
                    .await
                    .expect("set the bit");
                let kept = std::fs::metadata(&path).expect("stat the file");
                assert_eq!(kept.mode() & 0o777, 0o744, "the group and other bits stay");
                assert_eq!(
                    (before.ctime(), before.ctime_nsec()),
                    (kept.ctime(), kept.ctime_nsec()),
                    "a matching user bit leaves the file untouched"
                );

                set_executable(&path, Some(&before), false)
                    .await
                    .expect("clear the bit");
                let changed = std::fs::metadata(&path).expect("stat the file");
                assert_eq!(
                    changed.mode() & 0o777,
                    0o644,
                    "the bit is cleared for everyone"
                );
                assert_ne!(
                    (before.ctime(), before.ctime_nsec()),
                    (changed.ctime(), changed.ctime_nsec()),
                    "a differing user bit is set"
                );
            },
        )
        .await;
}

/// Setting the executable bit of a path that holds nothing fails rather than passing unnoticed.
#[cfg(unix)]
#[tokio::test]
async fn setting_the_executable_bit_of_a_missing_file_fails() {
    lore_base::runtime::LORE_CONTEXT
        .scope(
            crate::fs::filesystem_provider::setup_test_execution(),
            async {
                let temp = temp_dir();
                assert!(
                    set_executable(temp.path().join("absent"), None, true)
                        .await
                        .is_err()
                );
            },
        )
        .await;
}

/// An unlink removes a file and an empty directory, and answers a path that holds nothing as
/// already removed.
#[tokio::test]
async fn an_unlink_removes_a_file_and_an_empty_directory() {
    lore_base::runtime::LORE_CONTEXT
        .scope(
            crate::fs::filesystem_provider::setup_test_execution(),
            async {
                let temp = temp_dir();
                let file = temp.path().join("file");
                let directory = temp.path().join("directory");
                std::fs::write(&file, b"removed").expect("write the file");
                std::fs::create_dir(&directory).expect("create the directory");

                unlink(&file).await.expect("the file is removed");
                unlink(&directory).await.expect("the directory is removed");
                unlink(temp.path().join("absent"))
                    .await
                    .expect("a path holding nothing is removed");

                assert!(!file.exists(), "the file is gone");
                assert!(!directory.exists(), "the directory is gone");
            },
        )
        .await;
}

/// A recursive unlink removes a directory with all it holds, and a file on its own.
#[tokio::test]
async fn a_recursive_unlink_removes_a_tree_and_a_file() {
    lore_base::runtime::LORE_CONTEXT
        .scope(
            crate::fs::filesystem_provider::setup_test_execution(),
            async {
                let temp = temp_dir();
                let tree = temp.path().join("tree");
                let file = temp.path().join("file");
                std::fs::create_dir_all(tree.join("nested")).expect("create the tree");
                std::fs::write(tree.join("nested").join("leaf"), b"removed")
                    .expect("write the leaf");
                std::fs::write(&file, b"removed").expect("write the file");

                unlink_recursive(&tree).await.expect("the tree is removed");
                unlink_recursive(&file).await.expect("the file is removed");

                assert!(!tree.exists(), "the tree is gone");
                assert!(!file.exists(), "the file is gone");
            },
        )
        .await;
}

/// A directory whose mode keeps its entries from being removed is made writable by its owner and
/// removed by the retry. A privileged process, which the mode does not keep, has no retry to
/// show.
#[cfg(unix)]
#[tokio::test]
async fn a_recursive_unlink_retries_a_directory_its_mode_keeps() {
    lore_base::runtime::LORE_CONTEXT
        .scope(
            crate::fs::filesystem_provider::setup_test_execution(),
            async {
                let temp = temp_dir();
                let directory = temp.path().join("kept");
                std::fs::create_dir(&directory).expect("create the directory");
                std::fs::write(directory.join("entry"), b"kept").expect("write the entry");
                let _locked = LockedDirectory::new(&directory, 0o555);
                if !mode_keeps_entries(&directory) {
                    return;
                }

                unlink_recursive(&directory)
                    .await
                    .expect("the retry removes the directory");

                assert!(!directory.exists(), "the directory is gone");
            },
        )
        .await;
}

/// A tree whose retry still fails is left as the retry made it: its top directory writable by
/// its owner alone, and the directory below it as it was, nothing made writable by others.
#[cfg(unix)]
#[tokio::test]
async fn a_recursive_unlink_retry_adds_only_the_owners_write_permission() {
    use std::os::unix::fs::PermissionsExt;

    lore_base::runtime::LORE_CONTEXT
        .scope(
            crate::fs::filesystem_provider::setup_test_execution(),
            async {
                let temp = temp_dir();
                let top = temp.path().join("top");
                let below = top.join("below");
                std::fs::create_dir_all(&below).expect("create the tree");
                std::fs::write(below.join("entry"), b"kept").expect("write the entry");
                let _below = LockedDirectory::new(&below, 0o555);
                let _top = LockedDirectory::new(&top, 0o555);
                if !mode_keeps_entries(&below) {
                    return;
                }

                assert!(
                    unlink_recursive(&top).await.is_err(),
                    "the entry below keeps the tree"
                );

                let mode = |path: &Path| {
                    std::fs::metadata(path)
                        .expect("the directory stays")
                        .permissions()
                        .mode()
                        & 0o777
                };
                assert_eq!(
                    0o755,
                    mode(&top),
                    "the top directory gains its owner's write"
                );
                assert_eq!(0o555, mode(&below), "the directory below keeps its mode");
            },
        )
        .await;
}

/// A symbolic link standing where a directory was is removed as a link, by an unlink and a
/// recursive unlink alike, and the directory it stands for keeps its entry and its mode.
#[cfg(unix)]
#[tokio::test]
async fn an_unlink_removes_a_symbolic_link_to_a_directory_and_leaves_the_directory() {
    use std::os::unix::fs::PermissionsExt;

    lore_base::runtime::LORE_CONTEXT
        .scope(
            crate::fs::filesystem_provider::setup_test_execution(),
            async {
                let temp = temp_dir();
                let target = temp.path().join("target");
                let link = temp.path().join("link");
                let recursive_link = temp.path().join("recursive-link");
                std::fs::create_dir(&target).expect("create the directory");
                std::fs::write(target.join("entry"), b"kept").expect("write the entry");
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
                    .expect("set the directory's mode");
                std::os::unix::fs::symlink(&target, &link).expect("create the link");
                std::os::unix::fs::symlink(&target, &recursive_link)
                    .expect("create the second link");

                unlink(&link).await.expect("the link is removed");
                unlink_recursive(&recursive_link)
                    .await
                    .expect("the second link is removed");

                assert!(
                    std::fs::symlink_metadata(&link).is_err(),
                    "the link is gone"
                );
                assert!(
                    std::fs::symlink_metadata(&recursive_link).is_err(),
                    "the second link is gone"
                );
                assert_eq!(
                    Some(b"kept".to_vec()),
                    std::fs::read(target.join("entry")).ok(),
                    "the directory keeps its entry"
                );
                let mode = std::fs::metadata(&target)
                    .expect("the directory stays")
                    .permissions()
                    .mode();
                assert_eq!(0o755, mode & 0o777, "the directory keeps its mode");
            },
        )
        .await;
}

/// A symbolic link naming nothing is removed, not taken for a path holding nothing.
#[cfg(unix)]
#[tokio::test]
async fn an_unlink_removes_a_dangling_symbolic_link() {
    lore_base::runtime::LORE_CONTEXT
        .scope(
            crate::fs::filesystem_provider::setup_test_execution(),
            async {
                let temp = temp_dir();
                let link = temp.path().join("dangling");
                std::os::unix::fs::symlink(temp.path().join("gone"), &link)
                    .expect("create the link");

                unlink(&link).await.expect("the link is removed");

                assert!(
                    std::fs::symlink_metadata(&link).is_err(),
                    "the link is gone"
                );
            },
        )
        .await;
}

/// A file whose directory's mode keeps it is refused and keeps its own mode: a file's own
/// permissions never decide its removal on Unix, so nothing is retried and nothing is made
/// writable.
#[cfg(unix)]
#[tokio::test]
async fn an_unlink_leaves_the_mode_of_a_file_its_directory_keeps() {
    use std::os::unix::fs::PermissionsExt;

    lore_base::runtime::LORE_CONTEXT
        .scope(
            crate::fs::filesystem_provider::setup_test_execution(),
            async {
                let temp = temp_dir();
                let directory = temp.path().join("kept");
                let file = directory.join("file");
                std::fs::create_dir(&directory).expect("create the directory");
                std::fs::write(&file, b"kept").expect("write the file");
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644))
                    .expect("set the file's mode");
                let _locked = LockedDirectory::new(&directory, 0o555);
                if !mode_keeps_entries(&directory) {
                    return;
                }

                assert!(unlink(&file).await.is_err(), "the file is not removed");
                let mode = std::fs::metadata(&file)
                    .expect("the file stays")
                    .permissions()
                    .mode();
                assert_eq!(0o644, mode & 0o777, "the file keeps its mode");
            },
        )
        .await;
}

/// A directory a test sets to a mode that keeps its entries, given back its owner's full access
/// when dropped, so the scratch space holding it is removed however the test ends.
#[cfg(unix)]
struct LockedDirectory(std::path::PathBuf);

#[cfg(unix)]
impl LockedDirectory {
    fn new(directory: &Path, mode: u32) -> Self {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(mode))
            .expect("set the directory's mode");
        LockedDirectory(directory.to_path_buf())
    }
}

#[cfg(unix)]
impl Drop for LockedDirectory {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;

        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

/// Whether the mode of `directory` keeps this process from adding an entry to it, which it does
/// not for a privileged process.
#[cfg(unix)]
fn mode_keeps_entries(directory: &Path) -> bool {
    let probe = directory.join("probe");
    if std::fs::write(&probe, b"probe").is_err() {
        return true;
    }
    std::fs::remove_file(&probe).expect("remove the probe");
    false
}

/// A read-only empty directory, which Windows refuses to remove, is made writable and removed by
/// the retry.
#[cfg(windows)]
#[tokio::test]
async fn an_unlink_retries_a_read_only_empty_directory() {
    lore_base::runtime::LORE_CONTEXT
        .scope(
            crate::fs::filesystem_provider::setup_test_execution(),
            async {
                let temp = temp_dir();
                let directory = temp.path().join("read-only");
                std::fs::create_dir(&directory).expect("create the directory");
                let mut permissions = std::fs::metadata(&directory)
                    .expect("stat the directory")
                    .permissions();
                permissions.set_readonly(true);
                std::fs::set_permissions(&directory, permissions)
                    .expect("make the directory read-only");

                unlink(&directory)
                    .await
                    .expect("the retry removes the directory");

                assert!(!directory.exists(), "the directory is gone");
            },
        )
        .await;
}

/// A junction, live or naming a directory that is gone, is removed as a link by an unlink and a
/// recursive unlink alike, and the directory a live one stands for keeps its entry.
#[cfg(windows)]
#[tokio::test]
async fn an_unlink_removes_a_junction_and_leaves_the_directory() {
    lore_base::runtime::LORE_CONTEXT
        .scope(
            crate::fs::filesystem_provider::setup_test_execution(),
            async {
                let temp = temp_dir();
                let target = temp.path().join("target");
                let gone = temp.path().join("gone");
                std::fs::create_dir(&target).expect("create the directory");
                std::fs::write(target.join("entry"), b"kept").expect("write the entry");
                std::fs::create_dir(&gone).expect("create the directory to remove");
                let links = ["live", "recursive-live", "dangling", "recursive-dangling"]
                    .map(|name| temp.path().join(name));
                for (link, to) in links.iter().zip([&target, &target, &gone, &gone]) {
                    junction(link, to);
                }
                std::fs::remove_dir(&gone).expect("remove the directory two junctions name");

                unlink(&links[0])
                    .await
                    .expect("the live junction is removed");
                unlink_recursive(&links[1])
                    .await
                    .expect("the second live junction is removed");
                unlink(&links[2])
                    .await
                    .expect("the dangling junction is removed");
                unlink_recursive(&links[3])
                    .await
                    .expect("the second dangling junction is removed");

                for link in &links {
                    assert!(
                        std::fs::symlink_metadata(link).is_err(),
                        "{} is gone",
                        link.display()
                    );
                }
                assert_eq!(
                    Some(b"kept".to_vec()),
                    std::fs::read(target.join("entry")).ok(),
                    "the directory keeps its entry"
                );
            },
        )
        .await;
}

/// A junction whose own read-only attribute refuses its removal is removed by the retry, which
/// clears the junction's attribute, and the read-only directory it stands for stays read-only
/// with its entry.
#[cfg(windows)]
#[tokio::test]
async fn an_unlink_removes_a_read_only_junction_and_leaves_its_target_read_only() {
    lore_base::runtime::LORE_CONTEXT
        .scope(
            crate::fs::filesystem_provider::setup_test_execution(),
            async {
                let temp = temp_dir();
                let target = temp.path().join("target");
                let link = temp.path().join("junction");
                std::fs::create_dir(&target).expect("create the directory");
                std::fs::write(target.join("entry"), b"kept").expect("write the entry");
                junction(&link, &target);
                let marked = std::process::Command::new("cmd")
                    .args(["/C", "attrib", "+r"])
                    .arg(&link)
                    .arg("/L")
                    .output()
                    .expect("run attrib");
                assert!(marked.status.success(), "attrib +r /L failed: {marked:?}");
                let mut permissions = std::fs::metadata(&target)
                    .expect("stat the directory")
                    .permissions();
                permissions.set_readonly(true);
                std::fs::set_permissions(&target, permissions)
                    .expect("make the directory read-only");

                unlink(&link).await.expect("the retry removes the junction");

                assert!(
                    std::fs::symlink_metadata(&link).is_err(),
                    "the junction is gone"
                );
                assert!(
                    std::fs::metadata(&target)
                        .expect("the directory stays")
                        .permissions()
                        .readonly(),
                    "the directory stays read-only"
                );
                assert_eq!(
                    Some(b"kept".to_vec()),
                    std::fs::read(target.join("entry")).ok(),
                    "the directory keeps its entry"
                );
            },
        )
        .await;
}

/// Creates a junction at `link` to the directory `target`, which Windows does without a privilege.
#[cfg(windows)]
fn junction(link: &Path, target: &Path) {
    let created = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .expect("run mklink");
    assert!(created.status.success(), "mklink /J failed: {created:?}");
}

/// Stamps a probe in `dir` until the filesystem gives it a later change time than `metadata`
/// carries, so that a change made from then on moves the time a file carries.
#[cfg(unix)]
fn wait_for_change_time_past(dir: &Path, metadata: &std::fs::Metadata) {
    use std::os::unix::fs::MetadataExt;

    let probe = dir.join("probe");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        std::fs::write(&probe, b"probe").expect("stamp the probe");
        let stamped = std::fs::metadata(&probe).expect("stat the probe");
        if (stamped.ctime(), stamped.ctime_nsec()) > (metadata.ctime(), metadata.ctime_nsec()) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the filesystem clock must advance"
        );
    }
}
