// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Every allocation from the revision tree's heap has to be released back
//! through it: a first-class rpmalloc heap has no owner thread, so rpmalloc
//! counts each thread as the owner and takes an unsynchronized free path. A
//! release that skips the heap corrupts the page's free list, which surfaces
//! far from the free itself — as an access violation somewhere else entirely.
//!
//! These tests hammer that path from several threads so a regression fails here
//! rather than in a large stage run.
#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use lore_base::allocator::HeapBox;
    use lore_base::allocator::HeapBuf;
    use lore_base::allocator::node_block_allocator;

    /// Stands in for a block payload. Real ones are 48–64 KiB; this has the
    /// same allocate-write-release shape at a size that keeps the test cheap.
    type Block = [u8; 512];

    const THREADS: usize = 8;
    const ITERATIONS: usize = 2000;

    /// Under `LORE_ALLOCATOR=system` there is no private heap and both tests
    /// exercise the global allocator instead — still valid, just not a test of
    /// the heap.
    fn allocator() -> Option<&'static (dyn std::alloc::GlobalAlloc + Sync)> {
        node_block_allocator()
    }

    #[test]
    fn concurrent_allocation_and_release() {
        thread::scope(|scope| {
            for thread_index in 0..THREADS {
                scope.spawn(move || {
                    let fill = thread_index as u8;
                    for iteration in 0..ITERATIONS {
                        let mut block = HeapBox::<Block>::new_zeroed_in(allocator());
                        assert!(block.iter().all(|byte| *byte == 0), "block not zeroed");
                        block.fill(fill);

                        let mut names = HeapBuf::new_in(allocator());
                        for _ in 0..(iteration % 8) + 1 {
                            names.extend_from_slice(&[fill; 24]);
                        }

                        assert!(block.iter().all(|byte| *byte == fill), "block corrupted");
                        assert!(names.iter().all(|byte| *byte == fill), "names corrupted");
                    }
                });
            }
        });
    }

    #[test]
    fn release_on_a_thread_that_did_not_allocate() {
        let (sender, receiver) = mpsc::channel::<(u8, HeapBox<Block>, HeapBuf)>();
        thread::scope(|scope| {
            for thread_index in 0..THREADS {
                let sender = sender.clone();
                scope.spawn(move || {
                    let fill = thread_index as u8 | 0x80;
                    for _ in 0..ITERATIONS {
                        let mut block = HeapBox::<Block>::new_zeroed_in(allocator());
                        block.fill(fill);
                        let names = HeapBuf::from_slice_in(&[fill; 64], allocator());
                        if sender.send((fill, block, names)).is_err() {
                            return;
                        }
                    }
                });
            }
            drop(sender);

            // The receiver is the only thread that drops, so every release
            // crosses a thread boundary away from the one that allocated.
            scope.spawn(move || {
                let mut received = 0;
                while let Ok((fill, block, names)) = receiver.recv() {
                    assert!(block.iter().all(|byte| *byte == fill), "block corrupted");
                    assert!(names.iter().all(|byte| *byte == fill), "names corrupted");
                    received += 1;
                }
                assert_eq!(received, THREADS * ITERATIONS);
            });
        });
    }
}
