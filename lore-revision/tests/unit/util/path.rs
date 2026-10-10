// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;

use lore_revision::util::path::*;

/// What `f` returns and the allocations the calling thread made running it.
///
/// Counting stays on, since switching it off would undercount a test measuring on another thread.
fn allocations_during<T>(f: impl FnOnce() -> T) -> (T, u64) {
    lore_base::allocator::set_allocation_counting(true);
    let before = lore_base::allocator::thread_allocation_count();
    let value = std::hint::black_box(f());
    (
        value,
        lore_base::allocator::thread_allocation_count() - before,
    )
}

/// A path built from a string makes its two strings and the shared data once each, never
/// growing either string, however many names it holds.
#[test]
fn a_path_from_an_initial_string_allocates_its_two_strings_and_its_data_once() {
    let (path, allocated) = allocations_during(|| {
        RelativePath::new_from_initial_path("Content/Maps/Level01/Props.umap").expect("path")
    });
    assert_eq!(path.as_str(), "Content/Maps/Level01/Props.umap");
    assert_eq!(path.as_lowercase_str(), "content/maps/level01/props.umap");
    assert_eq!(
        allocated, 3,
        "allocations other than the two strings and the shared data"
    );
}

/// A push reserves only the bytes it appends, so a path sized for a name grows neither string
/// when the name is pushed into it.
#[test]
fn a_push_into_a_path_sized_for_it_grows_neither_string() {
    let mut path = RelativePathBuf::with_capacity("Content/Maps".len());
    let (_, allocated) = allocations_during(|| {
        path.push("Content/Maps");
    });
    assert_eq!(path.as_str(), "Content/Maps");
    assert_eq!(path.as_lowercase_str(), "content/maps");
    assert_eq!(allocated, 0);
}

#[test]
fn a_path_below_an_ancestor_is_the_names_between_them() {
    let path = RelativePath::new_from_clean_parts("thr/sub/file.txt", "");
    assert_eq!(
        path.below(&RelativePath::new_from_clean_parts("thr", "")),
        Some("sub/file.txt")
    );
    assert_eq!(path.below(&RelativePath::new()), Some("thr/sub/file.txt"));
    assert_eq!(path.below(&path), Some(""));
}

/// A name the ancestor merely shares a prefix with is not below it, so the answer is nothing
/// rather than a suffix of the wrong name.
#[test]
fn a_path_below_a_partial_name_match_is_nothing() {
    let path = RelativePath::new_from_clean_parts("third/file.txt", "");
    assert_eq!(
        path.below(&RelativePath::new_from_clean_parts("thr", "")),
        None
    );
    assert_eq!(
        path.below(&RelativePath::new_from_clean_parts(
            "third/file.txt/deeper",
            ""
        )),
        None
    );
}

#[test]
fn a_pushed_component_carries_into_the_lowercase_form() {
    let mut path = RelativePathBuf::new();
    path.push("Assets");
    path.push("MESHES");
    assert_eq!(path.as_str(), "Assets/MESHES");
    assert_eq!(path.as_lowercase_str(), "assets/meshes");
}

/// The lowercase form takes an ASCII path character for character and
/// anything else through the general mapping, which can be more characters
/// than it replaces. The capacity both are built with is a reserve, so the
/// wider one grows rather than being cut short.
#[test]
fn a_component_beyond_ascii_lowercases_through_the_general_mapping() {
    let mut path = RelativePathBuf::with_capacity("ÅNGSTRÖM".len());
    path.push("ÅNGSTRÖM");
    assert_eq!(path.as_str(), "ÅNGSTRÖM");
    assert_eq!(path.as_lowercase_str(), "ångström");

    let mut path = RelativePathBuf::with_capacity("İ".len());
    path.push("İ");
    assert_eq!(path.as_str(), "İ");
    assert_eq!(
        path.as_lowercase_str(),
        "i\u{307}",
        "one character became two"
    );
}

/// Every route into the lowercase form takes a suffix beyond ASCII through
/// the general mapping, not `push` alone. Each reserves what the suffix takes
/// in the path, which the two characters `İ` folds to outgrow.
#[test]
fn every_append_route_lowercases_beyond_ascii() {
    let base = RelativePath::new_from_clean_parts("Assets", "");

    let appended = base.append_into_buf("_İ");
    assert_eq!(appended.as_str(), "Assets_İ");
    assert_eq!(appended.as_lowercase_str(), "assets_i\u{307}");

    let pushed = base.push_into_buf("MESH_İ");
    assert_eq!(pushed.as_str(), "Assets/MESH_İ");
    assert_eq!(pushed.as_lowercase_str(), "assets/mesh_i\u{307}");

    let mut buf = base.into_buf();
    buf.append("_İ");
    assert_eq!(buf.as_str(), "Assets_İ");
    assert_eq!(buf.as_lowercase_str(), "assets_i\u{307}");
}

/// The fold has to be the one node names are hashed over, or a name matches
/// on its digest and not on its path. A character-wise fold parts from it
/// where the mapping depends on where in a word the character falls, which
/// for a final capital sigma it does.
#[test]
fn the_lowercase_form_takes_the_fold_node_names_are_hashed_over() {
    for name in [
        "",
        "Assets",
        "ROCK.MESH",
        "Stra\u{00df}e",
        "\u{0130}stanbul",
        "\u{00c5}NGSTR\u{00d6}M",
        "\u{039f}\u{0394}\u{039f}\u{03a3}",
        "\u{03a3}\u{03bf}\u{03c6}\u{03bf}\u{03c2}",
        "\u{4f60}\u{597d}",
    ] {
        let mut folded = String::new();
        push_lowercase(&mut folded, name);
        assert_eq!(folded, name.to_lowercase(), "{name:?} folds apart from it");
    }
}

/// The ASCII fold is written before the name is known to be ASCII, so a
/// component that only reaches beyond it at the end has to have that
/// speculative write taken back off what came before it.
#[test]
fn a_component_turning_non_ascii_at_its_end_keeps_what_it_was_appended_to() {
    let mut path = RelativePathBuf::new();
    path.push("Assets");
    path.push("MESH_Å");
    assert_eq!(path.as_str(), "Assets/MESH_Å");
    assert_eq!(path.as_lowercase_str(), "assets/mesh_å");
}

/// An initial path is trimmed and brought onto `/`, and only one that is not
/// already there is rewritten to establish it.
#[test]
fn an_initial_path_holds_the_canonical_separator() {
    let expect = |name: &str| {
        RelativePathBuf::new_from_initial_path(name)
            .expect("the path is relative")
            .as_str()
            .to_owned()
    };

    assert_eq!(expect("Assets/Meshes"), "Assets/Meshes");
    assert_eq!(expect("/Assets/Meshes/"), "Assets/Meshes");
    assert_eq!(expect("./Assets"), "Assets");
    assert_eq!(expect("Assets\\Meshes"), "Assets/Meshes");
    assert_eq!(expect("Assets//Meshes"), "Assets/Meshes");
    assert_eq!(
        expect("Assets///Meshes"),
        "Assets/Meshes",
        "a run of separators collapses however long it is"
    );
    assert_eq!(
        expect("Assets\\\\Meshes"),
        "Assets/Meshes",
        "a separator that doubles once rewritten collapses with it"
    );
    assert!(expect(".").is_empty());
    assert!(expect("").is_empty());
}

/// `from_str` folds a whole path rather than a component, and the lowercase
/// form it ends up with is what bounds the view of it.
#[test]
fn from_str_carries_a_lowercase_form_of_its_own_length() {
    let path = RelativePath::from_str("FÖLDER/İ").expect("the conversion is infallible");
    assert_eq!(path.as_str(), "FÖLDER/İ");
    assert_eq!(
        path.as_lowercase_str(),
        "földer/i\u{307}",
        "a fold that widens is not cut short by the length of the path"
    );
}

#[test]
fn pop_root_repeat_leaves_a_view_of_what_is_below_them() {
    let mut path = RelativePath::new_from_clean_parts("Assets/Meshes/Rock.mesh", "");
    path.pop_root_repeat(2);
    assert_eq!(path.as_str(), "Rock.mesh");
    assert_eq!(
        path.as_lowercase_str(),
        "rock.mesh",
        "the lowercase form advances with it"
    );

    path.pop_root_repeat(5);
    assert!(path.is_empty(), "advancing past the end stops at it");
}

#[test]
fn shared_component_depth_counts_whole_components() {
    assert_eq!(shared_component_depth("a/b/c", "a/b/d"), 2);
    assert_eq!(shared_component_depth("a/b/c", "a/x/c"), 1);
    assert_eq!(shared_component_depth("a/b/c", "x/b/c"), 0);
    assert_eq!(shared_component_depth("a/b/c", "a/b/c"), 3);
    // A shared string prefix that stops inside a component shares neither it
    // nor anything below it.
    assert_eq!(shared_component_depth("a/b/x", "a/bc/y"), 1);
    assert_eq!(shared_component_depth("ab/x", "a/x"), 0);
    // A path that runs out is shared as far as it goes.
    assert_eq!(shared_component_depth("a/b", "a/b/c"), 2);
    assert_eq!(shared_component_depth("", "a"), 0);
    assert_eq!(shared_component_depth("a", "a"), 1);
}

/// The steps a path takes to reach canonical form, each of which leaves a
/// path already there as it is.
#[test]
fn clean_brings_a_path_onto_the_canonical_form() {
    assert_eq!("abc/def", clean("abc/def".to_owned()));
    assert_eq!("abc/def", clean(r"\\?\abc\def".to_owned()));
    assert_eq!("abc/def", clean(r"\\.\abc\def".to_owned()));
    assert_eq!("abc/def", clean("abc//def".to_owned()));
    assert_eq!("abc/def", clean("abc////def".to_owned()));
    assert_eq!("abc/def", clean("abc/./def".to_owned()));
    assert_eq!(
        "abc/def",
        clean("abc/././def".to_owned()),
        "a `.` left adjacent to the next by removing one is removed with it"
    );
    assert_eq!("abc/def", clean("./abc/def".to_owned()));
    assert_eq!("abc/def", clean("././abc/def".to_owned()));
    assert_eq!("abc/def", clean("abc/def/.".to_owned()));
    assert_eq!("", clean(String::new()));
}

/// Only a component that is `..` entire is a step up. A name that merely
/// begins with two periods, or is made of them, is a name.
#[test]
fn clean_steps_up_for_a_parent_and_not_for_a_name() {
    assert_eq!("abc/.../def", clean("abc/.../def".to_owned()));
    assert_eq!("abc/..name/def", clean("abc/..name/def".to_owned()));
    assert_eq!("abc/name../def", clean("abc/name../def".to_owned()));
    assert_eq!("abc/def", clean("abc/ghi/../def".to_owned()));
}

#[test]
fn test_clean_path_with_leading_parent() {
    assert_eq!("../../def", clean("../../abc/../def".to_owned()));
}

#[test]
fn test_clean_path_with_double_parent() {
    assert_eq!("abc/jkl", clean("abc/def/ghi/../../jkl".to_owned()));
}

#[test]
fn test_clean_path_with_parent_after_period() {
    assert_eq!("abc/ghi", clean("abc/def/./../ghi".to_owned()));
}

#[test]
#[cfg(target_os = "windows")]
fn test_clean_path_with_parent_after_drive() {
    assert_eq!("C:/abc", clean("C:\\..\\abc".to_owned()));
    assert_eq!("C:/abc", clean("C:/../abc".to_owned()));
}

#[cfg(not(target_os = "windows"))]
mod is_path_inside_repository {
    use std::path::Path;

    use lore_revision::util::path::is_path_inside_repository;

    #[test]
    fn child_at_root() {
        assert!(is_path_inside_repository(Path::new("/a/b"), "/a/b/x.txt"));
    }

    #[test]
    fn nested_child() {
        assert!(is_path_inside_repository(
            Path::new("/a/b"),
            "/a/b/c/d/x.txt",
        ));
    }

    #[test]
    fn sibling_directory_is_outside() {
        assert!(!is_path_inside_repository(Path::new("/a/b"), "/a/c/x.txt",));
    }

    #[test]
    fn repo_equals_candidate() {
        assert!(is_path_inside_repository(Path::new("/a/b"), "/a/b"));
    }

    #[test]
    fn traversal_escaping_is_outside() {
        // /a/b/../../tmp/x.txt cleans to /tmp/x.txt, which is outside /a/b.
        assert!(!is_path_inside_repository(
            Path::new("/a/b"),
            "/a/b/../../tmp/x.txt",
        ));
    }

    #[test]
    fn traversal_returning_is_inside() {
        // /a/b/sub/../x.txt cleans to /a/b/x.txt.
        assert!(is_path_inside_repository(
            Path::new("/a/b"),
            "/a/b/sub/../x.txt",
        ));
    }

    #[test]
    fn empty_candidate_is_inside() {
        // new_from_user_path treats "" / "." as the repo root itself.
        assert!(is_path_inside_repository(Path::new("/a/b"), ""));
        assert!(is_path_inside_repository(Path::new("/a/b"), "."));
    }

    #[test]
    fn case_insensitive() {
        // new_from_user_path lowercases both sides before comparing.
        assert!(is_path_inside_repository(Path::new("/A/B"), "/a/b/x.txt",));
    }

    /// The dot directory is inside the root for this question, however spelled, even though the
    /// tree holds it nowhere: the write dispatch it picks is the safe default.
    #[test]
    fn the_dot_directory_is_inside() {
        assert!(is_path_inside_repository(
            Path::new("/a/b"),
            "/a/b/.lore/config.toml"
        ));
        assert!(is_path_inside_repository(Path::new("/a/b"), "/a/b/.URC"));
    }
}

#[cfg(not(target_os = "windows"))]
mod repository_relative_path {
    use std::path::Path;

    use lore_revision::util::path::repository_relative_path;

    fn held(candidate: &str) -> Option<String> {
        repository_relative_path(Path::new("/a/b"), candidate).map(|path| path.to_string())
    }

    #[test]
    fn a_path_under_the_root_is_held_relative_to_it() {
        assert_eq!(held("/a/b/c/x.txt").as_deref(), Some("c/x.txt"));
    }

    #[test]
    fn a_path_outside_the_root_is_held_nowhere() {
        assert_eq!(held("/a/c/x.txt"), None);
        assert_eq!(held("/a/b/../../tmp/x.txt"), None);
    }

    #[test]
    fn the_dot_directory_is_held_nowhere() {
        assert_eq!(held("/a/b/.lore"), None);
        assert_eq!(held("/a/b/.lore/config"), None);
        assert_eq!(held("/a/b/.urc/config"), None);
    }

    /// The root is matched whatever case it is named in, so the dot directory is too: on a
    /// filesystem that folds case, `.LORE` is the directory `.lore` names.
    #[test]
    fn the_dot_directory_is_held_nowhere_in_any_case() {
        assert_eq!(held("/a/b/.LORE/config"), None);
        assert_eq!(held("/a/b/.Lore"), None);
        assert_eq!(held("/a/b/.URC/config"), None);
    }

    /// A name that merely begins with the dot directory is ordinary content. A dot directory
    /// further down is a nested repository's own, which this tree holds nowhere either: no node
    /// carries the name at any depth.
    #[test]
    fn a_name_that_merely_begins_with_the_dot_directory_is_held() {
        assert_eq!(held("/a/b/.lorebak/x").as_deref(), Some(".lorebak/x"));
        assert_eq!(held("/a/b/sub/.lore/x"), None);
        assert_eq!(held("/a/b/sub/.URC"), None);
    }
}

/// The lowercase view has to narrow with the one it mirrors, or a filter would fold a
/// parent against a name the path no longer holds.
#[test]
fn a_parent_path_narrows_both_views() {
    let nested = RelativePath::new_from_initial_path("A/B/C").unwrap();
    let parent = nested.parent_path();
    assert_eq!(parent.as_str(), "A/B");
    assert_eq!(parent.as_lowercase_str(), "a/b");

    let single = RelativePath::new_from_initial_path("D").unwrap();
    let single_parent = single.parent_path();
    assert!(single_parent.is_empty());
    assert_eq!(single_parent.as_lowercase_str(), "");

    let empty = RelativePath::new();
    assert!(empty.parent_path().is_empty());

    let grandparent = nested.parent_path().parent_path();
    assert_eq!(grandparent.as_str(), "A");
    assert_eq!(grandparent.as_lowercase_str(), "a");
}

#[cfg(not(target_os = "windows"))]
mod a_user_path_through_the_dot_directory {
    use std::path::Path;

    use lore_revision::util::path::RelativePath;

    /// No node carries the name at any depth or in any ASCII case, so a user path through it is
    /// an invalid path, the answer every verb already gives a path outside the root.
    #[test]
    fn is_refused_in_any_case_and_at_any_depth() {
        for candidate in [
            "/a/b/.lore",
            "/a/b/.LORE/id",
            "/a/b/.urc/config.toml",
            "/a/b/sub/.Urc/x",
        ] {
            assert!(
                RelativePath::new_from_user_path(Path::new("/a/b"), candidate).is_err(),
                "{candidate} must be refused"
            );
        }
    }

    /// A mount hands paths in by geometry and leaves the question to the resolver, so the
    /// conversion it uses holds the path and refuses only what lies outside the root.
    #[test]
    fn a_mount_path_through_it_is_held_by_geometry() {
        let held = RelativePath::new_from_mount_path(Path::new("/a/b"), "/a/b/.lore/id")
            .expect("a mount path under the root is held");
        assert_eq!(held.as_str(), ".lore/id");
        assert!(RelativePath::new_from_mount_path(Path::new("/a/b"), "/a/c/x").is_err());
    }

    #[test]
    fn a_name_that_merely_begins_with_it_is_accepted() {
        for candidate in ["/a/b/.loreignore", "/a/b/.urcignore", "/a/b/.lorebak/x"] {
            assert!(
                RelativePath::new_from_user_path(Path::new("/a/b"), candidate).is_ok(),
                "{candidate} must be accepted"
            );
        }
    }
}
