// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Timing for a commit that rehashes a directory holding many children.
//!
//! A commit rehashes every directory holding a change, and each child of a rehashed directory
//! contributes to its address. Each commit here changes one file in a directory of many and is
//! made on the revision read afresh. The lookup of the changed file, ahead of the timing, reads
//! the blocks it passes without their name tables; the timed commit reads the other blocks and
//! every name table from the store.
//!
//! Run with:
//!     `cargo test -p lore-revision --release --test unit -- commit::rehash_bench --ignored --nocapture`

use std::time::Duration;
use std::time::Instant;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::State;

use crate::helper::in_memory;
use crate::helper::in_memory::add;
use crate::helper::in_memory::branch_id;
use crate::helper::in_memory::commit;
use crate::helper::in_memory::directory;
use crate::helper::in_memory::file;
use crate::helper::in_memory::revision_state;
use crate::helper::test_store_create;

/// The directory every commit rehashes.
const WIDE: &str = "wide";
/// How many commits are timed for each directory size.
const ROUNDS: u32 = 5;

/// Workload multiplier, from `LORE_BENCH_SCALE`.
fn scale() -> u32 {
    std::env::var("LORE_BENCH_SCALE")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(1)
}

fn child(index: u32) -> String {
    format!("File{index:07}.bin")
}

#[tokio::test]
#[ignore = "benchmark: run on demand with --ignored"]
async fn commit_rehashing_a_wide_directory() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");

    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        for children in [1_000, 10_000, 100_000].map(|count| count * scale()) {
            let repository = in_memory::repository(immutable_store.clone(), mutable_store.clone());
            let branch = branch_id();
            let state = State::new();
            let parent = add(&state, repository.clone(), ROOT_NODE, directory(WIDE), WIDE).await;
            for index in 0..children {
                let name = child(index);
                add(&state, repository.clone(), parent, file(&name), &name).await;
            }
            let mut revision = commit(&repository, &state, Hash::default(), branch).await;

            let changed = format!("{WIDE}/{}", child(children / 2));
            let mut total = Duration::ZERO;
            // The first commit is not timed, so the store is as warm for the first timed commit as
            // for the last.
            for round in 0..=ROUNDS {
                let state = revision_state(&repository, revision).await;
                let node = state
                    .find_node_link(repository.clone(), &changed)
                    .await
                    .expect("Failed to find the changed file")
                    .node;
                let version = u64::from(round) + 1;
                state
                    .node_modify_staged(
                        &repository,
                        node,
                        &lore_revision::state::Content {
                            mode: 0o644,
                            size: version,
                            address: Address::zero_context_hash(Hash::from_u64(version)),
                        },
                    )
                    .await
                    .expect("Failed to modify and stage the file");
                let start = Instant::now();
                revision = commit(&repository, &state, revision, branch).await;
                if round > 0 {
                    total += start.elapsed();
                }
            }
            println!(
                "  {children:>8} children {:>9.3} ms/commit",
                total.as_secs_f64() * 1_000.0 / f64::from(ROUNDS)
            );
        }
    }))
    .await
    .expect("Test task failed");
}
