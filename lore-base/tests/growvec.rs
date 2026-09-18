// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
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
}
