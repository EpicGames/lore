// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `linked`: the linked states a provider serving links enters, numbered from 1 into chunks that
//! only grow, each twice the one before.

use std::sync::Arc;

use lore_base::types::Hash;
use lore_revision::fs::linked::LinkedStates;
use lore_revision::fs::linked::locate;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;

use crate::fs::filesystem_provider::test_store_create;
use crate::helper::in_memory;

/// A repository context for the states a test enters.
async fn repository() -> Arc<RepositoryContext> {
    let (immutable, mutable, _execution) = test_store_create().await.expect("making test stores");
    in_memory::repository(immutable, mutable)
}

/// Each number has a slot, at the edges of the first chunk, the second and the last, and none
/// does past the last chunk; 0 has none.
#[test]
fn each_number_has_a_slot_up_to_the_last_chunk() {
    assert_eq!(locate(0), None);
    assert_eq!(locate(1), Some((0, 0)));
    assert_eq!(locate(32), Some((0, 31)));
    assert_eq!(locate(33), Some((1, 0)));
    assert_eq!(locate(96), Some((1, 63)));
    assert_eq!(locate(97), Some((2, 0)));
    // The 27 chunks hold 2^5 + 2^6 + ... + 2^31 numbers.
    let last = u32::MAX - 31;
    assert_eq!(locate(last), Some((26, (1 << 31) - 1)));
    assert_eq!(locate(last + 1), None);
    assert_eq!(locate(u32::MAX), None);
}

/// States entered past the first two chunks are numbered from 1 in the order entered, and each
/// reads back by its number and its key. A key entered again answers its first number, and the
/// state given with it is left.
#[tokio::test]
async fn each_state_entered_reads_back_by_its_number() {
    let repository = repository().await;
    let linked = LinkedStates::default();
    let id = RepositoryId::from(uuid::Uuid::now_v7());
    let mut states = Vec::new();
    for at in 1..=100u32 {
        let state = State::new();
        let index = linked
            .insert(
                (id, Hash::from_u64(at.into())),
                (repository.clone(), state.clone()),
            )
            .expect("the state is entered");
        assert_eq!(index, at);
        states.push(state);
    }
    for (index, state) in (1..).zip(&states) {
        assert_eq!(
            linked.index_of(id, Hash::from_u64(index.into())),
            Some(index)
        );
        let (_, read) = linked.get(index).expect("the state entered");
        assert!(Arc::ptr_eq(read, state), "state {index} reads back");
    }
    assert!(linked.get(0).is_none());
    assert!(linked.get(101).is_none());
    assert_eq!(linked.index_of(id, Hash::from_u64(101)), None);

    let again = linked
        .insert((id, Hash::from_u64(40)), (repository.clone(), State::new()))
        .expect("the key entered again");
    assert_eq!(again, 40);
    let (_, kept) = linked.get(40).expect("the state entered");
    assert!(Arc::ptr_eq(kept, &states[39]), "the first state stays");
    assert!(linked.get(101).is_none(), "no number is taken");
}

/// Callers entering one key at once all answer one number, and one of their states is kept.
#[tokio::test(flavor = "multi_thread")]
async fn a_key_entered_by_callers_at_once_takes_one_number() {
    const CALLERS: usize = 8;

    let repository = repository().await;
    let linked = Arc::new(LinkedStates::default());
    let key = (RepositoryId::from(uuid::Uuid::now_v7()), Hash::from_u64(7));
    let start = Arc::new(std::sync::Barrier::new(CALLERS));
    let callers: Vec<_> = (0..CALLERS)
        .map(|_| {
            let (linked, repository, start) = (linked.clone(), repository.clone(), start.clone());
            std::thread::spawn(move || {
                let state = State::new();
                start.wait();
                let index = linked
                    .insert(key, (repository, state.clone()))
                    .expect("the state is entered");
                (index, state)
            })
        })
        .collect();
    let answered: Vec<(u32, Arc<State>)> = callers
        .into_iter()
        .map(|caller| caller.join().expect("the caller ends"))
        .collect();
    assert!(
        answered.iter().all(|&(index, _)| index == 1),
        "{:?}",
        answered.iter().map(|(index, _)| index).collect::<Vec<_>>()
    );
    let (_, kept) = linked.get(1).expect("the state entered");
    assert_eq!(
        answered
            .iter()
            .filter(|(_, state)| Arc::ptr_eq(state, kept))
            .count(),
        1,
        "one caller's state is kept"
    );
    assert!(linked.get(2).is_none(), "no other number is taken");
}
