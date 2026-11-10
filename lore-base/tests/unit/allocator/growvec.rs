// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::allocator::GrowVec;

const N: usize = 7; // small chunk for testing

#[test]
fn create_zeroed() {
    let gv: GrowVec<u32, 7> = GrowVec::new_zeroed_with_size(0);
    assert_eq!(gv.len(), 0);

    let gv: GrowVec<u32, 7> = GrowVec::new_zeroed_with_size(1);
    assert_eq!(gv.len(), 1);
    assert_eq!(gv[0], 0);

    let gv: GrowVec<u32, 7> = GrowVec::new_zeroed_with_size(N);
    assert_eq!(gv.len(), N);
    assert_eq!(gv[0], 0);

    let gv: GrowVec<u32, 7> = GrowVec::new_zeroed_with_size(N * 2);
    assert_eq!(gv.len(), N * 2);

    let gv: GrowVec<u32, 7> = GrowVec::new_zeroed_with_size(111);
    assert_eq!(gv.len(), 111);
    for i in 0..111 {
        assert_eq!(gv[i], 0);
    }
}

#[test]
fn push_basic_and_len() {
    let mut gv: GrowVec<i32, N> = GrowVec::new();
    for i in 0..N {
        gv.push(i as i32);
        assert_eq!(gv.len(), i + 1);
    }
    for i in 0..N {
        gv.push(i as i32);
        assert_eq!(gv.len(), N + i + 1);
    }
    let mut gv: GrowVec<i32, N> = GrowVec::new();
    assert_eq!(gv.len(), 0);
    for i in 0..1000 {
        gv.push(i);
        assert_eq!(gv[i as usize], i);
        assert_eq!(gv.len(), (i + 1) as usize);
    }
    assert_eq!(gv.len(), 1000);
    let v = gv.to_vec();
    assert_eq!(v, (0..1000).collect::<Vec<_>>());
}

#[test]
fn push_and_iter() {
    let mut gv: GrowVec<i32, N> = GrowVec::new();
    assert_eq!(gv.len(), 0);
    for i in 0..1000 {
        gv.push(i);
        assert_eq!(gv[i as usize], i);
    }
    assert_eq!(gv.len(), 1000);
    for (index, iter) in gv.iter().enumerate() {
        assert_eq!(*iter as usize, index);
    }
}

#[test]
fn push_at_chunk_boundaries() {
    let mut gv: GrowVec<i32, N> = GrowVec::new();
    for i in 0..N {
        gv.push(i as i32);
    }
    assert_eq!(gv.len(), N);
    assert_eq!(gv.to_vec(), (0..N as i32).collect::<Vec<_>>());

    // push one more -> create second chunk
    gv.push(N as i32);
    assert_eq!(gv.len(), N + 1);
    assert_eq!(gv.to_vec(), (0..=N as i32).collect::<Vec<_>>());
}

#[test]
fn insert_append_equals_push() {
    let mut gv: GrowVec<i32, N> = GrowVec::new();
    for i in 0..7 {
        gv.insert(i as usize, i);
    }
    let mut expected = (0..7).collect::<Vec<_>>();

    gv.insert(gv.len(), 99);
    expected.push(99);

    assert_eq!(gv.len(), expected.len());
    assert_eq!(gv.to_vec(), expected);
}

#[test]
fn insert_front() {
    let mut gv: GrowVec<i32, N> = GrowVec::new();
    for i in 0..8 {
        gv.push(i);
    } // two chunks, last is partial
    gv.insert(0, 99);

    let mut expected = vec![99];
    expected.extend(0..8);
    assert_eq!(gv.len(), 9);
    assert_eq!(gv.to_vec(), expected);
}

#[test]
fn insert_middle_same_chunk() {
    let mut gv: GrowVec<i32, N> = GrowVec::new();
    for i in 0..7 {
        gv.push(i);
    } // [0,1,2,3,4,5,6]
    // index 2 is inside first chunk when N=7
    gv.insert(2, 42);
    let expected = vec![0, 1, 42, 2, 3, 4, 5, 6];
    assert_eq!(gv.to_vec(), expected);
}

#[test]
fn insert_cross_chunk_boundary() {
    let mut gv: GrowVec<i32, N> = GrowVec::new();
    for i in 0..14 {
        gv.push(i);
    } // two full chunks: [0..6][7..13]
    // Insert at start of second chunk (index == N)
    gv.insert(N, 77);
    let mut expected = (0..N as i32).collect::<Vec<_>>();
    expected.insert(N, 77);
    expected.extend((N as i32)..((N * 2) as i32));
    // expected now: [0,1,2,3,4,5,6,77,7,8,9,10,11,12,13]
    assert_eq!(gv.len(), 15);
    assert_eq!(gv.to_vec(), expected);
}

#[test]
fn insert_creates_new_chunk_when_last_full() {
    let mut gv: GrowVec<i32, N> = GrowVec::new();
    for i in 0..(2 * N) as i32 {
        gv.push(i);
    } // two full chunks
    // Insert somewhere that forces carry across multiple chunks
    gv.insert(3, 1000);

    let mut expected = (0..(2 * N) as i32).collect::<Vec<_>>();
    expected.insert(3, 1000);
    assert_eq!(gv.len(), expected.len());
    assert_eq!(gv.to_vec(), expected);
}

#[test]
fn insert_many_matches_vec_behavior() {
    let mut gv: GrowVec<i32, N> = GrowVec::new();
    let mut vv: Vec<i32> = Vec::new();

    // seed
    for i in 0..20 {
        gv.push(i);
        vv.push(i);
    }

    // a mix of inserts front/middle/end
    let ops = [
        (0usize, -1),
        (5, -2),
        (gv.len(), -3),
        (8, -4),
        (1, -5),
        (gv.len() / 2, -6),
        (gv.len(), -7),
    ];

    for (idx, val) in ops {
        let idx_cv = if idx == gv.len() { gv.len() } else { idx };
        let idx_vv = if idx == vv.len() { vv.len() } else { idx };
        gv.insert(idx_cv, val);
        vv.insert(idx_vv, val);
    }

    assert_eq!(gv.len(), vv.len());
    assert_eq!(gv.to_vec(), vv);

    for _ in 0..1000 {
        let pos = rand::random::<u32>() as usize % gv.len();
        let val = rand::random::<i32>();

        gv.insert(pos, val);
        vv.insert(pos, val);

        assert_eq!(gv.len(), vv.len());
    }

    assert_eq!(gv.len(), vv.len());
    assert_eq!(gv.to_vec(), vv);
}

/// A vector of `len` ascending values, and the same values in a `Vec`.
fn filled(len: usize) -> (GrowVec<i32, N>, Vec<i32>) {
    let mut gv = GrowVec::new();
    let mut vv = Vec::new();
    for value in 0..len as i32 {
        gv.push(value);
        vv.push(value);
    }
    (gv, vv)
}

/// Asserts that `gv` holds what `vv` holds, in chunks holding exactly its elements.
fn assert_holds(gv: &GrowVec<i32, N>, vv: &[i32]) {
    assert_eq!(gv.len(), vv.len());
    assert_eq!(gv.is_empty(), vv.is_empty());
    assert_eq!(gv.to_vec(), vv);
    let bytes: Vec<usize> = gv.byte_segments().map(<[u8]>::len).collect();
    assert_eq!(
        bytes.iter().sum::<usize>(),
        std::mem::size_of_val(vv),
        "the chunks hold the elements"
    );
    assert!(
        bytes.iter().all(|len| *len > 0),
        "no chunk is left empty: {bytes:?}"
    );
}

#[test]
fn remove_matches_vec_at_every_index() {
    for len in [1, N - 1, N, N + 1, 2 * N, 2 * N + 3] {
        for index in 0..len {
            let (mut gv, mut vv) = filled(len);
            assert_eq!(
                gv.remove(index),
                Some(vv.remove(index)),
                "len {len} index {index}"
            );
            assert_holds(&gv, &vv);
        }
    }
}

#[test]
fn swap_remove_matches_vec_at_every_index() {
    for len in [1, N - 1, N, N + 1, 2 * N, 2 * N + 3] {
        for index in 0..len {
            let (mut gv, mut vv) = filled(len);
            assert_eq!(
                gv.swap_remove(index),
                Some(vv.swap_remove(index)),
                "len {len} index {index}"
            );
            assert_holds(&gv, &vv);
        }
    }
}

/// A removal at an index not below the length answers `None` and leaves the vector as it is.
#[test]
fn a_removal_past_the_end_answers_none_and_changes_nothing() {
    for len in [0, 1, N, N + 1] {
        let (mut gv, vv) = filled(len);
        for index in [len, len + 1, usize::MAX] {
            assert_eq!(gv.remove(index), None, "len {len} index {index}");
            assert_eq!(gv.swap_remove(index), None, "len {len} index {index}");
            assert_holds(&gv, &vv);
        }
    }
}

#[test]
fn removing_every_element_frees_every_chunk_and_the_vector_grows_again() {
    for front in [true, false] {
        let (mut gv, mut vv) = filled(2 * N + 1);
        while !vv.is_empty() {
            if front {
                assert_eq!(gv.remove(0), Some(vv.remove(0)));
            } else {
                let last = vv.len() - 1;
                assert_eq!(gv.swap_remove(last), Some(vv.swap_remove(last)));
            }
            assert_holds(&gv, &vv);
        }
        assert_eq!(gv.byte_segments().count(), 0, "no chunk is left");
        gv.push(7);
        assert_eq!(gv.to_vec(), vec![7]);
    }
}

#[test]
fn removals_mixed_with_inserts_match_vec() {
    let (mut gv, mut vv) = filled(3 * N);
    for _ in 0..4000 {
        let choice = rand::random::<u32>() % 3;
        if choice == 0 || vv.is_empty() {
            let index = rand::random::<u32>() as usize % (vv.len() + 1);
            let value = rand::random::<i32>();
            gv.insert(index, value);
            vv.insert(index, value);
        } else {
            let index = rand::random::<u32>() as usize % vv.len();
            if choice == 1 {
                assert_eq!(gv.remove(index), Some(vv.remove(index)));
            } else {
                assert_eq!(gv.swap_remove(index), Some(vv.swap_remove(index)));
            }
        }
        assert_holds(&gv, &vv);
    }
}

/// Two references into one chunk, alive at the same time. The second must not invalidate the
/// first, which it did while the iterator borrowed the whole chunk for every element.
#[test]
fn iter_mut_yields_references_that_outlive_each_other() {
    let mut gv: GrowVec<u64, N> = GrowVec::new();
    for value in 0..4 {
        gv.push(value);
    }

    {
        let mut iter = gv.iter_mut();
        let first = iter.next().expect("first element");
        let second = iter.next().expect("second element");
        *first += 10;
        *second += 20;
        assert_eq!(*first, 10);
        assert_eq!(*second, 21);
    }

    // Both writes landed in the vector, not in copies of its elements.
    assert_eq!(gv.to_vec(), vec![10, 21, 2, 3]);
}

#[test]
fn zst_support() {
    let mut gv: GrowVec<(), N> = GrowVec::new();
    for _ in 0..10 {
        gv.push(());
    }
    assert_eq!(gv.len(), 10);
    gv.insert(5, ());
    assert_eq!(gv.len(), 11);
    assert_eq!(gv.iter().count(), 11);

    // A zero-sized element has no bytes, so its references cannot conflict.
    assert_eq!(gv.iter_mut().count(), 11);

    // Cloning allocates a chunk per chunk, so it takes the zero-sized path too.
    let clone = gv.clone();
    assert_eq!(clone.len(), 11);
    assert_eq!(clone.iter().count(), 11);

    // SAFETY: A zero-sized chunk holds no bytes, so no read can observe uninitialised memory.
    let unzeroed: GrowVec<(), N> = unsafe { GrowVec::new_unzeroed_with_size(11) };
    assert_eq!(unzeroed.len(), 11);
    assert_eq!(unzeroed.iter().count(), 11);
}

#[test]
fn clone() {
    let mut gv: GrowVec<usize, N> = GrowVec::new();
    // At least one non-full chunk
    for i in 0..(N * 2 + N / 2) {
        gv.push(i);
    }
    let gv_clone = gv.clone();
    assert_eq!(gv.len(), gv_clone.len());
    assert_eq!(gv.to_vec(), gv_clone.to_vec());
}
