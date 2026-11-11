// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_storage::Address;
use lore_storage::Context;
use lore_storage::Hash;
use lore_storage::Partition;
use lore_storage::error::StorageError;
use lore_storage::errors::AddressNotFound;
use lore_storage::options::ReadOptions;
use lore_storage::read::Keep;
use lore_storage::read_flight::*;
use lore_storage::write::FlightStore;

/// The store every flight here reads into.
const STORE: FlightStore = FlightStore(std::num::NonZeroUsize::MIN);

/// Joins the read single-flight as a caller in [`STORE`] keeping nothing of the fetch, reading
/// with `options`.
fn join(
    partition: Partition,
    address: Address,
    isolate_partitions: bool,
    options: ReadOptions,
) -> ReadFlight {
    join_read_flight(STORE, None, partition, address, isolate_partitions, options)
}

fn test_key(seed: u8) -> (Partition, Address) {
    let partition = Partition::from([seed; 16]);
    let address = Address {
        hash: Hash::from([seed; 32]),
        context: Context::from([seed; 16]),
    };
    (partition, address)
}

#[tokio::test]
async fn first_caller_is_leader_rest_are_followers() {
    let (partition, address) = test_key(1);
    let leader = match join(partition, address, false, ReadOptions::default()) {
        ReadFlight::Leader(guard) => guard,
        ReadFlight::Follower(_) => panic!("first caller must be the leader"),
    };
    for _ in 0..3 {
        match join(partition, address, false, ReadOptions::default()) {
            ReadFlight::Follower(_) => {}
            ReadFlight::Leader(_) => {
                panic!("caller joining an active flight must be a follower")
            }
        }
    }
    drop(leader);
}

#[tokio::test]
async fn followers_see_leader_error() {
    let (partition, address) = test_key(2);
    let leader = match join(partition, address, false, ReadOptions::default()) {
        ReadFlight::Leader(guard) => guard,
        ReadFlight::Follower(_) => panic!("first caller must be the leader"),
    };
    let follower_rx = match join(partition, address, false, ReadOptions::default()) {
        ReadFlight::Follower(rx) => rx,
        ReadFlight::Leader(_) => panic!("second caller must be a follower"),
    };
    let waiter = lore_base::lore_spawn!(await_flight(follower_rx));
    let expected = StorageError::from(AddressNotFound::from(address));
    leader.complete(&Err(expected.clone()));

    let observed = waiter.await.expect("await_flight task panicked");
    assert!(
        matches!(observed, Some(Err(StorageError::AddressNotFound(_)))),
        "expected follower to observe the leader's error, got {observed:?}"
    );
}

#[tokio::test]
async fn dropped_leader_without_result_promotes_a_follower() {
    let (partition, address) = test_key(3);
    let leader = match join(partition, address, false, ReadOptions::default()) {
        ReadFlight::Leader(guard) => guard,
        ReadFlight::Follower(_) => panic!("first caller must be the leader"),
    };
    let follower_rx = match join(partition, address, false, ReadOptions::default()) {
        ReadFlight::Follower(rx) => rx,
        ReadFlight::Leader(_) => panic!("second caller must be a follower"),
    };

    drop(leader);

    assert!(
        await_flight(follower_rx).await.is_none(),
        "a follower must see `None` when the leader vanishes without a result"
    );

    match join(partition, address, false, ReadOptions::default()) {
        ReadFlight::Leader(guard) => drop(guard),
        ReadFlight::Follower(_) => panic!("promoted caller must become the new leader"),
    }
}

#[tokio::test]
async fn a_cancelled_follower_releases_its_receiver() {
    let (partition, address) = test_key(4);
    let leader = match join(partition, address, false, ReadOptions::default()) {
        ReadFlight::Leader(guard) => guard,
        ReadFlight::Follower(_) => panic!("first caller must be the leader"),
    };
    let follower_rx = match join(partition, address, false, ReadOptions::default()) {
        ReadFlight::Follower(rx) => rx,
        ReadFlight::Leader(_) => panic!("second caller must be a follower"),
    };

    let follower_task = lore_base::lore_spawn!(await_flight(follower_rx));
    assert!(leader.has_followers());
    follower_task.abort();
    let _ = follower_task.await;

    assert!(
        !leader.has_followers(),
        "the cancelled follower released its receiver"
    );
    drop(leader);
}

/// A read of the same content under another context or partition follows the flight where the
/// store does not isolate partitions, and is told the leader's address is not its own, where a
/// read of the leader's address is told it is.
#[tokio::test]
async fn a_read_of_the_same_content_under_another_address_follows() {
    let (partition, address) = test_key(5);
    let leader = match join(partition, address, false, ReadOptions::default()) {
        ReadFlight::Leader(guard) => guard,
        ReadFlight::Follower(_) => panic!("first caller must be the leader"),
    };
    let other_partition = Partition::from([6; 16]);
    let other_address = Address {
        context: Context::from([6; 16]),
        ..address
    };
    match join(
        other_partition,
        other_address,
        false,
        ReadOptions::default(),
    ) {
        ReadFlight::Follower(follower) => assert!(!follower.same_address()),
        ReadFlight::Leader(_) => panic!("a read of the same content must follow"),
    }
    match join(partition, address, false, ReadOptions::default()) {
        ReadFlight::Follower(follower) => assert!(follower.same_address()),
        ReadFlight::Leader(_) => panic!("a read of the same address must follow"),
    }
    drop(leader);
}

/// Where the store isolates partitions, a read of the same content in another partition leads a
/// flight of its own, and one under another context in the same partition follows.
#[tokio::test]
async fn an_isolating_store_keys_reads_by_partition() {
    let (partition, address) = test_key(7);
    let leader = match join(partition, address, true, ReadOptions::default()) {
        ReadFlight::Leader(guard) => guard,
        ReadFlight::Follower(_) => panic!("first caller must be the leader"),
    };
    let other_partition = Partition::from([8; 16]);
    let other_leader = match join(other_partition, address, true, ReadOptions::default()) {
        ReadFlight::Leader(guard) => guard,
        ReadFlight::Follower(_) => panic!("another partition must lead its own flight"),
    };
    let other_context = Address {
        context: Context::from([8; 16]),
        ..address
    };
    assert!(matches!(
        join(partition, other_context, true, ReadOptions::default()),
        ReadFlight::Follower(_)
    ));
    drop(other_leader);
    drop(leader);
}

/// A read that trusts the remote and one that does not lead flights of their own for the same
/// content, and each follows only a flight of its own trust.
#[tokio::test]
async fn reads_are_keyed_by_trust() {
    let (partition, address) = test_key(31);
    let untrusting = ReadOptions::default();
    let trusting = untrusting.trusting_remote();
    let untrusting_leader = match join(partition, address, false, untrusting) {
        ReadFlight::Leader(guard) => guard,
        ReadFlight::Follower(_) => panic!("first caller must be the leader"),
    };
    let trusting_leader = match join(partition, address, false, trusting) {
        ReadFlight::Leader(guard) => guard,
        ReadFlight::Follower(_) => panic!("a trusting read must lead its own flight"),
    };
    for options in [untrusting, trusting] {
        assert!(matches!(
            join(partition, address, false, options),
            ReadFlight::Follower(_)
        ));
    }
    drop(trusting_leader);
    drop(untrusting_leader);
}

/// A leader no follower joins allocates nothing: the channel its outcome would go out on is the
/// first follower's to create.
#[test]
fn a_leader_without_followers_allocates_nothing() {
    let partition = Partition::from([9; 16]);
    let addresses: Vec<Address> = (0..64u8)
        .map(|i| Address {
            hash: Hash::from([0x90 ^ i; 32]),
            context: Context::from([9; 16]),
        })
        .collect();
    let failed = Err(StorageError::from(AddressNotFound::from(addresses[0])));
    let lead = |address: Address| match join(partition, address, false, ReadOptions::default()) {
        ReadFlight::Leader(guard) => guard.complete(&failed),
        ReadFlight::Follower(_) => panic!("no flight for this content is in flight"),
    };
    addresses.iter().copied().for_each(lead);

    lore_base::allocator::set_allocation_counting(true);
    let before = lore_base::allocator::thread_allocation_count();
    addresses.iter().copied().for_each(lead);
    let allocations = lore_base::allocator::thread_allocation_count() - before;
    lore_base::allocator::set_allocation_counting(false);
    assert_eq!(allocations, 0, "leaders allocated");
}

/// The first follower of a flight creates its channel, and a later follower subscribes to it
/// without allocating.
#[test]
fn a_second_follower_subscribes_without_allocating() {
    let (partition, address) = test_key(10);
    let leader = match join(partition, address, false, ReadOptions::default()) {
        ReadFlight::Leader(guard) => guard,
        ReadFlight::Follower(_) => panic!("first caller must be the leader"),
    };
    let first = match join(partition, address, false, ReadOptions::default()) {
        ReadFlight::Follower(follower) => follower,
        ReadFlight::Leader(_) => panic!("second caller must be a follower"),
    };
    assert!(leader.has_followers());

    lore_base::allocator::set_allocation_counting(true);
    let before = lore_base::allocator::thread_allocation_count();
    let second = join(partition, address, false, ReadOptions::default());
    let allocations = lore_base::allocator::thread_allocation_count() - before;
    lore_base::allocator::set_allocation_counting(false);
    assert!(matches!(second, ReadFlight::Follower(_)));
    assert_eq!(allocations, 0, "the second follower allocated");
    drop((first, second, leader));
}

/// A follower at the leader's address in the leader's store has the leader keep what it keeps,
/// merged into what the leader keeps there with no allocation, and one at another address keeps
/// its own; one joining once the leader took what it keeps is kept where that covers what it
/// keeps, and keeps its own otherwise.
#[test]
fn a_follower_in_the_leaders_store_merges_what_it_keeps_until_the_leader_takes_it() {
    let (partition, address) = test_key(11);
    let other_address = Address {
        context: Context::from([12; 16]),
        ..address
    };
    let leader = match join_read_flight(
        STORE,
        None,
        partition,
        address,
        false,
        ReadOptions::default(),
    ) {
        ReadFlight::Leader(guard) => guard,
        ReadFlight::Follower(_) => panic!("first caller must be the leader"),
    };
    let follow = |address, keep| match join_read_flight(
        STORE,
        Some(keep),
        partition,
        address,
        false,
        ReadOptions::default(),
    ) {
        ReadFlight::Follower(follower) => follower,
        ReadFlight::Leader(_) => panic!("a read of the content in flight led"),
    };
    let elsewhere = follow(other_address, Keep::Store);
    assert!(
        !elsewhere.kept(),
        "a follower at another address keeps its own"
    );
    let priority = follow(address, Keep::Priority);
    lore_base::allocator::set_allocation_counting(true);
    let before = lore_base::allocator::thread_allocation_count();
    let storing = follow(address, Keep::Store);
    let allocations = lore_base::allocator::thread_allocation_count() - before;
    lore_base::allocator::set_allocation_counting(false);
    assert_eq!(allocations, 0, "a follower merging what it keeps allocated");
    assert!(priority.kept() && storing.kept());

    assert_eq!(leader.take_keep(), Some(Keep::Store));
    let covered = follow(address, Keep::Priority);
    assert!(
        covered.kept(),
        "what the leader took covers a late follower keeping less"
    );
    let late = follow(address, Keep::Replace);
    assert!(
        !late.kept(),
        "a follower joining once the leader took what it keeps, keeping more, keeps its own"
    );
    drop((elsewhere, priority, storing, covered, late, leader));
}
