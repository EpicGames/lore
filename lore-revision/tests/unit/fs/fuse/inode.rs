// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use lore_base::types::Address;
use lore_base::types::Hash;
use lore_fuse::abi::ForgetOne;
use lore_revision::fs::fuse::inode::ChildLookup;
use lore_revision::fs::fuse::inode::InodeEntry;
use lore_revision::fs::fuse::inode::InodeKind;
use lore_revision::fs::fuse::inode::InodeTable;
use lore_revision::fs::fuse::inode::Name;
use lore_revision::fs::fuse::inode::NewChild;
use lore_revision::fs::fuse::inode::ROOT_INO;
use lore_revision::fs::fuse::inode::claim_once;
use lore_revision::fs::fuse::inode_number::Identity;
use lore_revision::fs::fuse::inode_number::IdentityKind;
use lore_revision::fs::fuse::inode_number::derive_inode_number;
use lore_revision::fs::fuse::inode_number::rehash;
use lore_revision::node::ROOT_NODE;
use lore_storage::hash::hash_string;

#[test]
fn claim_once_wins_exactly_once() {
    let gate = AtomicBool::new(false);
    assert!(claim_once(&gate));
    assert!(!claim_once(&gate));
    assert!(!claim_once(&gate));
}

fn address(marker: u64) -> Address {
    Address {
        hash: Hash::from_u64(marker),
        context: Default::default(),
    }
}

fn file(marker: u64) -> InodeKind {
    InodeKind::File {
        address: address(marker),
        size: 10,
        executable: false,
        local: false,
    }
}

fn dir() -> InodeKind {
    InodeKind::dir(0, ROOT_NODE)
}

fn new_child(parent: u64, name: &str, name_hash: u64, kind: InodeKind) -> NewChild<'_> {
    NewChild {
        parent,
        name,
        name_hash,
        kind,
        mtime: 0,
        node: ROOT_NODE,
    }
}

fn insert(table: &InodeTable, parent: u64, name: &str, kind: InodeKind) -> u64 {
    table
        .insert_child(new_child(parent, name, hash_string(name), kind), |_| ())
        .0
}

fn lookup(table: &InodeTable, parent: u64, name: &str) -> Option<u64> {
    table.lookup_child(parent, hash_string(name), name)
}

fn find(table: &InodeTable, parent: u64, name: &str) -> Option<u64> {
    table.find_child(parent, hash_string(name), name)
}

fn nlookup(table: &InodeTable, ino: u64) -> u64 {
    table.get(ino).map_or(0, |e| e.nlookup)
}

/// A lookup whose copy of the name's index entry a RENAME over the name swapped before the
/// copy was checked counts the mover, which held the name throughout, rather than missing.
#[test]
fn a_lookup_overtaken_by_a_rename_over_the_name_counts_the_mover() {
    let table = InodeTable::new();
    let held = insert(&table, ROOT_INO, "k", file(1));
    let mover = insert(&table, ROOT_INO, "tmp", file(2));
    let fired = AtomicBool::new(false);
    let k_hash = hash_string("k");
    let _ = table.candidates_read.set(Box::new(move |table| {
        if !fired.swap(true, Ordering::SeqCst) {
            table.move_child(mover, Some(held), ROOT_INO, "k", k_hash);
        }
    }));

    let found = table.lookup_child_matching(ROOT_INO, k_hash, "k", |_, _| true);

    assert_eq!(found, ChildLookup::Counted(mover));
    assert_eq!(nlookup(&table, mover), 2);
}

/// Lookups racing renames over their name never find it without a live inode: the swap takes
/// the replaced inode out of the name's index entry and the mover into it under one lock.
#[test]
fn lookups_racing_renames_over_the_name_always_find_a_live_inode() {
    const RENAMES: u64 = 20_000;
    let table = InodeTable::new();
    let k_hash = hash_string("k");
    let mut held = insert(&table, ROOT_INO, "k", file(0));
    let done = AtomicBool::new(false);
    let missed = std::thread::scope(|scope| {
        let looking = scope.spawn(|| {
            let mut missed = 0u64;
            while !done.load(Ordering::SeqCst) {
                let found = table.lookup_child_matching(ROOT_INO, k_hash, "k", |_, _| true);
                if found == ChildLookup::Missing {
                    missed += 1;
                }
            }
            missed
        });
        for round in 1..=RENAMES {
            let mover = insert(&table, ROOT_INO, &format!("m{round}"), file(round));
            table.move_child(mover, Some(held), ROOT_INO, "k", k_hash);
            held = mover;
        }
        done.store(true, Ordering::SeqCst);
        looking.join().expect("the lookups")
    });
    assert_eq!(
        missed, 0,
        "lookups that found the name without a live inode"
    );
}

/// A lookup counts a live candidate that matches even behind one that does not: a stale
/// inode ahead of the mover at a name answers a mismatch only where nothing else matches.
#[test]
fn a_lookup_counts_a_matching_candidate_behind_a_mismatched_one() {
    let table = InodeTable::new();
    let stale = insert(&table, ROOT_INO, "k", file(1));
    let mover = insert(&table, ROOT_INO, "tmp", file(2));
    let k_hash = hash_string("k");
    table.move_child(mover, None, ROOT_INO, "k", k_hash);

    let found = table.lookup_child_matching(ROOT_INO, k_hash, "k", |entry, _| entry.ino == mover);
    let none = table.lookup_child_matching(ROOT_INO, k_hash, "k", |_, _| false);

    assert_eq!(found, ChildLookup::Counted(mover));
    assert_eq!(none, ChildLookup::Mismatch(stale));
}

/// [`InodeTable::find_child`] overtaken the same way finds the mover.
#[test]
fn a_find_overtaken_by_a_rename_over_the_name_finds_the_mover() {
    let table = InodeTable::new();
    let held = insert(&table, ROOT_INO, "k", file(1));
    let mover = insert(&table, ROOT_INO, "tmp", file(2));
    let fired = AtomicBool::new(false);
    let _ = table.candidates_read.set(Box::new(move |table| {
        if !fired.swap(true, Ordering::SeqCst) {
            table.move_child(mover, Some(held), ROOT_INO, "k", hash_string("k"));
        }
    }));

    assert_eq!(find(&table, ROOT_INO, "k"), Some(mover));
}

/// [`InodeTable::lookup_child`] overtaken the same way counts the mover.
#[test]
fn a_counted_lookup_overtaken_by_a_rename_over_the_name_counts_the_mover() {
    let table = InodeTable::new();
    let held = insert(&table, ROOT_INO, "k", file(1));
    let mover = insert(&table, ROOT_INO, "tmp", file(2));
    let fired = AtomicBool::new(false);
    let _ = table.candidates_read.set(Box::new(move |table| {
        if !fired.swap(true, Ordering::SeqCst) {
            table.move_child(mover, Some(held), ROOT_INO, "k", hash_string("k"));
        }
    }));

    assert_eq!(lookup(&table, ROOT_INO, "k"), Some(mover));
    assert_eq!(nlookup(&table, mover), 2);
}

#[test]
fn lookup_or_insert_increments_nlookup_and_indexes_child() {
    let table = InodeTable::new();
    assert_eq!(table.len(), 1);
    assert_eq!(nlookup(&table, ROOT_INO), 1);

    let ino = insert(&table, ROOT_INO, "Foo", file(1));
    assert!(ino >= 2);
    assert_eq!(nlookup(&table, ino), 1);
    {
        let entry = table.get(ino).expect("inserted");
        assert_eq!(entry.parent, ROOT_INO);
        assert_eq!(&*entry.name, b"Foo");
        assert_eq!(entry.ino, ino);
    }
    assert_eq!(find(&table, ROOT_INO, "foo"), Some(ino));
    assert_eq!(nlookup(&table, ino), 1, "find_child does not count");
    assert_eq!(lookup(&table, ROOT_INO, "FOO"), Some(ino));
    assert_eq!(nlookup(&table, ino), 2);
    assert_eq!(insert(&table, ROOT_INO, "foo", file(1)), ino);
    assert_eq!(nlookup(&table, ino), 3);
    assert_eq!(lookup(&table, ROOT_INO, "bar"), None);
    assert_eq!(table.len(), 2);
}

#[test]
fn forget_to_zero_removes_entry_and_child_index() {
    let table = InodeTable::new();
    let ino = insert(&table, ROOT_INO, "a", file(1));
    assert_eq!(lookup(&table, ROOT_INO, "a"), Some(ino));
    assert!(!table.forget(ino, 1), "one lookup remains");
    assert_eq!(nlookup(&table, ino), 1);
    assert_eq!(find(&table, ROOT_INO, "a"), Some(ino));
    assert!(table.forget(ino, 1), "the last lookup removes the entry");
    assert!(table.get(ino).is_none());
    assert_eq!(find(&table, ROOT_INO, "a"), None);
    assert!(table.children.is_empty());
    assert_eq!(table.len(), 1);
    // Forgetting more than was counted, or an unknown inode, is harmless.
    assert!(!table.forget(ino, 5));
    assert!(!table.forget(ROOT_INO, 1));
    assert!(table.get(ROOT_INO).is_some(), "root is never removed");
}

#[test]
fn batch_forget_applies_each() {
    let table = InodeTable::new();
    let a = insert(&table, ROOT_INO, "a", file(1));
    let b = insert(&table, ROOT_INO, "b", dir());
    let c = insert(&table, ROOT_INO, "c", file(3));
    assert!(table.add_lookups(c, 2));
    table.forget_batch(&[
        ForgetOne {
            nodeid: a,
            nlookup: 1,
        },
        ForgetOne {
            nodeid: b,
            nlookup: 1,
        },
        ForgetOne {
            nodeid: c,
            nlookup: 2,
        },
    ]);
    assert!(table.get(a).is_none());
    assert!(table.get(b).is_none());
    assert_eq!(nlookup(&table, c), 1);
    assert_eq!(table.len(), 2);
}

#[test]
fn rollback_restores_nlookup() {
    let table = InodeTable::new();
    let ino = insert(&table, ROOT_INO, "a", file(1));
    assert!(table.add_lookups(ino, 2));
    assert_eq!(nlookup(&table, ino), 3);
    table.rollback(ino, 2);
    assert_eq!(nlookup(&table, ino), 1);
    // Rolling back the lookup that created the inode removes it again.
    table.rollback(ino, 1);
    assert!(table.get(ino).is_none());
    assert_eq!(find(&table, ROOT_INO, "a"), None);
    assert!(
        !table.add_lookups(ino, 1),
        "a removed inode cannot gain lookups"
    );
}

/// Each thread holds kernel-style counts on inodes it looked up and
/// checks that an inode it holds is always present and still indexed.
#[test]
fn lookup_racing_forget_never_loses_positive_entry() {
    const THREADS: u64 = 16;
    const OPS: u64 = 10_000;
    const NAMES: [&str; 6] = ["a", "B", "c", "D", "e", "F"];
    let table = InodeTable::new();
    let parent = insert(&table, ROOT_INO, "dir", dir());

    std::thread::scope(|scope| {
        for t in 0..THREADS {
            let table = &table;
            scope.spawn(move || {
                let mut rng = 0x9e37_79b9_7f4a_7c15u64 ^ (t + 1).wrapping_mul(0xff51_afd7);
                let mut next = || {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    rng
                };
                let mut held: [(u64, u64); NAMES.len()] = [(0, 0); NAMES.len()];
                for _ in 0..OPS {
                    let r = next();
                    let slot = (r % NAMES.len() as u64) as usize;
                    let name = NAMES[slot];
                    match (r >> 8) % 4 {
                        0 | 1 => {
                            let ino = lookup(table, parent, name)
                                .unwrap_or_else(|| insert(table, parent, name, file(slot as u64)));
                            if held[slot].1 > 0 {
                                assert_eq!(held[slot].0, ino, "number is stable");
                            }
                            held[slot] = (ino, held[slot].1 + 1);
                        }
                        2 => {
                            if held[slot].1 > 0 {
                                let n = 1 + (r >> 16) % held[slot].1;
                                table.forget(held[slot].0, n);
                                held[slot].1 -= n;
                            }
                        }
                        _ => {
                            if held[slot].1 > 0 {
                                let (ino, count) = held[slot];
                                assert!(nlookup(table, ino) >= count, "positive entry lost");
                                assert_eq!(find(table, parent, name), Some(ino));
                            }
                        }
                    }
                }
                for (ino, count) in held {
                    if count > 0 {
                        table.forget(ino, count);
                    }
                }
            });
        }
    });

    table.forget(parent, 1);
    assert_eq!(table.check_all_zero(), Ok(()));
    assert_eq!(table.len(), 1);
    assert!(table.children.is_empty());
}

#[test]
fn existing_inode_lookup_allocates_nothing() {
    let table = InodeTable::new();
    let ino = insert(&table, ROOT_INO, "Rock.mesh", file(1));
    let hash = hash_string("rock.MESH");

    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            lore_base::allocator::set_allocation_counting(false);
        }
    }
    lore_base::allocator::set_allocation_counting(true);
    let _restore = Restore;

    let before = lore_base::allocator::thread_allocation_count();
    let found = std::hint::black_box(table.lookup_child(ROOT_INO, hash, "rock.MESH"));
    let (again, ()) = table.insert_child(new_child(ROOT_INO, "Rock.mesh", hash, file(1)), |_| ());
    let counted = table.add_lookups(ino, 1);
    let attr_size = table.get(ino).map(|e| e.nlookup);
    table.forget(ino, 1);
    let after = lore_base::allocator::thread_allocation_count();

    assert_eq!(found, Some(ino));
    assert_eq!(again, ino);
    assert!(counted);
    assert_eq!(attr_size, Some(4));
    assert_eq!(after, before, "an existing-inode lookup must not allocate");
}

/// A revival refused once the orphan is indexed by name is undone: the orphan stays one,
/// with the lookups it held, and no name finds it.
#[test]
fn a_revival_refused_once_indexed_is_undone() {
    let table = InodeTable::new();
    let ino = insert(&table, ROOT_INO, "held.txt", file(1));
    table.orphan(ino);
    let held = nlookup(&table, ino);
    let asked = std::cell::Cell::new(0);
    let revived = table.insert_child_if(
        new_child(ROOT_INO, "held.txt", hash_string("held.txt"), file(1)),
        || {
            asked.set(asked.get() + 1);
            asked.get() == 1
        },
        |_| (),
    );
    assert!(revived.is_none(), "the second answer refuses");
    assert_eq!(asked.get(), 2, "asked before indexing and after");
    assert!(table.get(ino).is_some_and(|entry| entry.orphaned));
    assert_eq!(nlookup(&table, ino), held);
    assert_eq!(find(&table, ROOT_INO, "held.txt"), None);
}

/// An admission withdrawn once the entry is indexed by name, as a swap beginning then
/// withdraws it, leaves no entry for any lookup to count.
#[test]
fn an_admission_withdrawn_once_indexed_leaves_no_entry() {
    let table = InodeTable::new();
    let asked = std::cell::Cell::new(0);
    let hash = hash_string("late.txt");
    let inserted = table.insert_child_if(
        new_child(ROOT_INO, "late.txt", hash, file(1)),
        || {
            asked.set(asked.get() + 1);
            asked.get() == 1
        },
        |_| (),
    );
    assert!(inserted.is_none(), "the second answer refuses");
    assert_eq!(asked.get(), 2);
    assert_eq!(table.len(), 1, "only the root stays");
    assert!(table.children.is_empty(), "no name index stays");
    assert_eq!(lookup(&table, ROOT_INO, "late.txt"), None);
}

/// Every shard passes through a full table on the way, where an entry-API call reserves room
/// and grows the table even for a key it then finds.
#[test]
fn existing_inode_lookup_and_forget_allocate_nothing_in_a_full_shard() {
    let table = InodeTable::new();
    let names: Vec<String> = (0..4096).map(|i| format!("file-{i}")).collect();

    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            lore_base::allocator::set_allocation_counting(false);
        }
    }
    lore_base::allocator::set_allocation_counting(true);
    let _restore = Restore;

    for name in &names {
        let hash = hash_string(name);
        let ino = insert(&table, ROOT_INO, name, file(1));
        let before = lore_base::allocator::thread_allocation_count();
        let (again, ()) = table.insert_child(new_child(ROOT_INO, name, hash, file(1)), |_| ());
        let kept = table.forget(ino, 1);
        let gone = table.forget(ino, 1);
        let after = lore_base::allocator::thread_allocation_count();
        assert_eq!(again, ino);
        assert!(!kept && gone);
        assert_eq!(
            after, before,
            "{name}: a warm lookup and its forgets must not allocate"
        );
        insert(&table, ROOT_INO, name, file(1));
    }
}

#[test]
fn all_counts_zero_check_passes_after_forgets() {
    let table = InodeTable::new();
    let a = insert(&table, ROOT_INO, "a", dir());
    let b = insert(&table, a, "b", file(2));
    assert_eq!(lookup(&table, a, "B"), Some(b));
    let mut outstanding = table.check_all_zero().expect_err("counts outstanding");
    outstanding.sort_unstable();
    let mut expected = vec![a, b];
    expected.sort_unstable();
    assert_eq!(outstanding, expected);
    table.forget(b, 2);
    table.forget(a, 1);
    assert_eq!(table.check_all_zero(), Ok(()));
    assert_eq!(table.len(), 1);
}

fn identity_dir(parent_ino: u64, name: &str) -> Identity<'_> {
    Identity {
        parent_ino,
        name,
        name_hash: hash_string(name),
        kind: IdentityKind::Dir,
    }
}

#[test]
fn two_tables_derive_identical_numbers_for_identical_trees() {
    let first = InodeTable::new();
    let second = InodeTable::new();
    let a1 = insert(&first, ROOT_INO, "Src", dir());
    let x1 = insert(&first, a1, "main.rs", file(5));
    let y1 = insert(&first, ROOT_INO, "README", file(6));
    // Different insertion order and spelling.
    let y2 = insert(&second, ROOT_INO, "readme", file(6));
    let a2 = insert(&second, ROOT_INO, "src", dir());
    let x2 = insert(&second, a2, "MAIN.rs", file(5));
    assert_eq!((a1, x1, y1), (a2, x2, y2));
    assert_eq!(a1, derive_inode_number(&identity_dir(ROOT_INO, "src")));
}

#[test]
fn an_orphan_is_revived_only_for_the_same_content() {
    let table = InodeTable::new();
    let f = insert(&table, ROOT_INO, "f", file(1));
    table.orphan(f);
    assert!(table.get(f).is_some_and(|e| e.orphaned));
    assert_eq!(find(&table, ROOT_INO, "f"), None);
    assert_eq!(lookup(&table, ROOT_INO, "f"), None);

    // Other content at the path is another file: the orphan's pages stay its own.
    let g = insert(&table, ROOT_INO, "f", file(2));
    assert_ne!(g, f);
    assert_eq!(g, rehash(f, 1));
    assert_eq!(nlookup(&table, f), 1);

    // The same content again revives the orphan under its old number.
    table.orphan(g);
    assert_eq!(insert(&table, ROOT_INO, "F", file(1)), f);
    assert!(table.get(f).is_some_and(|e| !e.orphaned));
    assert_eq!(nlookup(&table, f), 2);
    assert_eq!(find(&table, ROOT_INO, "f"), Some(f));

    // Forgotten entirely, the same identity derives the same number.
    table.orphan(f);
    table.forget(f, 2);
    assert!(table.get(f).is_none());
    assert_eq!(insert(&table, ROOT_INO, "f", file(1)), f);

    // A directory orphan is never revived: the new directory rehashes.
    let d = insert(&table, ROOT_INO, "d", dir());
    table.orphan(d);
    let d2 = insert(&table, ROOT_INO, "d", dir());
    assert_eq!(d2, rehash(d, 1));
    assert!(table.get(d).is_some_and(|e| e.orphaned));
}

/// A local file's orphan is never revived, though a new local file at its path holds the same
/// zero address: the orphan holds a host file its path no longer reaches.
#[test]
fn a_local_file_orphan_is_never_revived() {
    let local = || InodeKind::File {
        address: Address::default(),
        size: 0,
        executable: false,
        local: true,
    };
    let table = InodeTable::new();
    let f = insert(&table, ROOT_INO, "f", local());
    table.orphan(f);
    let g = insert(&table, ROOT_INO, "f", local());
    assert_ne!(g, f, "the new local file takes another number");
    assert!(table.get(f).is_some_and(|e| e.orphaned));
    assert_eq!(nlookup(&table, f), 1);
}

#[test]
fn live_collision_with_different_identity_rehashes() {
    let table = InodeTable::new();
    let wanted = derive_inode_number(&identity_dir(ROOT_INO, "target"));
    // Occupy the derived number with an unrelated live identity.
    table.by_ino.insert(
        wanted,
        InodeEntry {
            ino: wanted,
            parent: 77,
            name_hash: hash_string("other"),
            name: Name::from_slice(b"other"),
            kind: dir(),
            nlookup: 1,
            mtime: 0,
            orphaned: false,
            read_cursor: AtomicU64::new(0),
            readahead_started: AtomicBool::new(false),
            access_recorded: AtomicBool::new(false),
            write_generation: AtomicU64::new(0),
            incarnation: 0,
            host_epoch: 0,
            renames: AtomicU32::new(0),
            rename_changes: AtomicU64::new(0),
            host_file: 0,
            node: ROOT_NODE,
        },
    );
    let ino = insert(&table, ROOT_INO, "target", dir());
    assert_eq!(ino, rehash(wanted, 1));
    assert_eq!(insert(&table, ROOT_INO, "TARGET", dir()), ino);
    assert_eq!(nlookup(&table, ino), 2);
    assert_eq!(nlookup(&table, wanted), 1, "the occupant is untouched");
}
