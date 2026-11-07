// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::fs::fuse::inode_number::Identity;
use lore_revision::fs::fuse::inode_number::IdentityKind;
use lore_revision::fs::fuse::inode_number::derive_inode_number;
use lore_revision::fs::fuse::inode_number::fold_eq;
use lore_revision::fs::fuse::inode_number::rehash;
use lore_revision::fs::fuse::inode_number::skip_reserved;
use lore_storage::hash::hash_string;

fn dir(parent_ino: u64, name: &str) -> Identity<'_> {
    Identity {
        parent_ino,
        name,
        name_hash: hash_string(name),
        kind: IdentityKind::Dir,
    }
}

fn file(parent_ino: u64, name: &str) -> Identity<'_> {
    Identity {
        parent_ino,
        name,
        name_hash: hash_string(name),
        kind: IdentityKind::File,
    }
}

#[test]
fn dir_number_depends_only_on_folded_path() {
    let a = derive_inode_number(&dir(1, "Source"));
    assert_eq!(a, derive_inode_number(&dir(1, "source")));
    assert_eq!(a, derive_inode_number(&dir(1, "SOURCE")));
    assert_ne!(a, derive_inode_number(&dir(2, "Source")));
    assert_ne!(a, derive_inode_number(&dir(1, "Sources")));
    // Capital sigma takes the whole-string lowercase fallback; the final
    // form must still fold the same regardless of input case.
    assert_eq!(
        derive_inode_number(&dir(1, "ΟΔΟΣ")),
        derive_inode_number(&dir(1, "οδος"))
    );
    // A directory and a file of the same name never share a number.
    assert_ne!(a, derive_inode_number(&file(1, "Source")));
}

#[test]
fn file_number_depends_only_on_parent_folded_name_and_kind() {
    let base = derive_inode_number(&file(1, "a.txt"));
    assert_eq!(base, derive_inode_number(&file(1, "A.TXT")));
    assert_ne!(base, derive_inode_number(&file(3, "a.txt")));
    assert_ne!(
        base,
        derive_inode_number(&Identity {
            parent_ino: 1,
            name: "a.txt",
            name_hash: hash_string("a.txt"),
            kind: IdentityKind::Symlink,
        })
    );
}

#[test]
fn never_yields_zero_or_one() {
    assert!(skip_reserved(0) >= 2);
    assert!(skip_reserved(1) >= 2);
    assert_eq!(skip_reserved(12345), 12345);
    for i in 0..2000u64 {
        let name = format!("n{i}");
        assert!(derive_inode_number(&dir(i, &name)) >= 2);
        assert!(derive_inode_number(&file(i, &name)) >= 2);
    }
}

#[test]
fn rehash_is_deterministic() {
    let n = derive_inode_number(&dir(1, "x"));
    assert_eq!(rehash(n, 1), rehash(n, 1));
    assert_ne!(rehash(n, 1), rehash(n, 2));
    assert_ne!(rehash(n, 1), n);
    assert_ne!(rehash(n, 1), rehash(n + 1, 1));
}

/// Names are the same exactly where their lowercases are, including the Kelvin sign, which
/// lowercases to an ASCII `k`, a final capital sigma, which lowercases to `ς`, and the dotted
/// capital I, which lowercases to two characters.
#[test]
fn fold_eq_holds_where_the_lowercases_are_equal() {
    let pairs = [
        ("Rock.MESH", "rock.mesh", true),
        ("rock.mesh", "paper.mesh", false),
        ("a", "ab", false),
        ("\u{212a}elvin", "kelvin", true),
        ("\u{00c4}pfel", "\u{00e4}pfel", true),
        (
            "\u{039f}\u{0394}\u{039f}\u{03a3}",
            "\u{03bf}\u{03b4}\u{03bf}\u{03c2}",
            true,
        ),
        (
            "\u{039f}\u{0394}\u{039f}\u{03a3}",
            "\u{03bf}\u{03b4}\u{03bf}\u{03c3}",
            false,
        ),
        ("\u{0130}", "i\u{307}", true),
        ("\u{0130}", "i", false),
    ];
    for (a, b, same) in pairs {
        assert_eq!(
            a.to_lowercase() == b.to_lowercase(),
            same,
            "{a} against {b}"
        );
        assert_eq!(fold_eq(a, b), same, "{a} against {b}");
        assert_eq!(fold_eq(b, a), same, "{b} against {a}");
    }
}
