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

/// Whether the filesystem under the temporary directory holds one case variation of a name
/// and answers lookups in any other. Windows and macOS do by default and Linux does not, but a
/// mount can be either on any of them, so the tests below ask rather than assume — and the
/// two behaviours are different enough that a test written for one is not a test of the
/// other.
fn case_insensitive(dir: &Path) -> bool {
    let probe = dir.join("CaseProbe");
    std::fs::write(&probe, b"").expect("write probe");
    let insensitive = std::fs::metadata(dir.join("caseprobe")).is_ok();
    std::fs::remove_file(&probe).expect("remove probe");
    insensitive
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

/// An unlink keeps what it reads of the path's metadata, not the metadata, across the
/// removal it awaits.
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

/// A file whose mode already carries the bit asked for is left as it is, whether its mode is
/// read from the file or given. Setting a mode moves the file's change time even when the mode
/// stays the same, so a matching mode must leave the time where it was. Asking for the other
/// bit then moves it, which shows the clock had advanced far enough for a change to be seen.
#[cfg(unix)]
#[tokio::test]
async fn a_mode_that_already_matches_is_left_as_it_is() {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    lore_base::runtime::LORE_CONTEXT
        .scope(
            crate::fs::filesystem_provider::setup_test_execution(),
            async {
                let temp = temp_dir();
                let path = temp.path().join("plain");
                std::fs::write(&path, b"plain").expect("write the file");
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
                    .expect("set the file's mode");
                let before = std::fs::metadata(&path).expect("stat the file");
                wait_for_change_time_past(temp.path(), &before);

                metadata_set_executable(&path, None, false).await;
                metadata_set_executable(&path, Some(&before), false).await;
                let kept = std::fs::metadata(&path).expect("stat the file");
                assert_eq!(
                    (before.ctime(), before.ctime_nsec()),
                    (kept.ctime(), kept.ctime_nsec()),
                    "a matching mode leaves the file untouched"
                );

                metadata_set_executable(&path, Some(&before), true).await;
                let changed = std::fs::metadata(&path).expect("stat the file");
                assert_eq!(changed.mode() & 0o777, 0o755, "the bit is set for everyone");
                assert_ne!(
                    (before.ctime(), before.ctime_nsec()),
                    (changed.ctime(), changed.ctime_nsec()),
                    "a mode that differs is set"
                );
            },
        )
        .await;
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
