// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use std::sync::Arc;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::filter::Filter;
use lore_revision::filter::FilterStates;
use lore_revision::fs::fuse::FuseProviderError;
use lore_revision::fs::fuse::listing::Hit;
use lore_revision::fs::fuse::listing::Listing;
use lore_revision::fs::fuse::listing::ListingEntry;
use lore_revision::fs::fuse::listing::entry_flags;
use lore_revision::fs::fuse::listing::is_representable;
use lore_revision::fs::fuse::listing_cache::LISTING_BLOCK_BYTES;
use lore_revision::fs::fuse::listing_cache::LISTING_CACHE_BYTES;
use lore_revision::fs::fuse::listing_cache::ListingCache;
use lore_revision::fs::fuse::served::Served;
use lore_revision::fs::fuse::view::filter_states_from_packed;
use lore_revision::fs::fuse::view::filter_states_from_u8;
use lore_revision::fs::fuse::view::same_view;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::InMemoryContext;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::state::Mark;
use lore_revision::state::State;
use lore_revision::util::path::RelativePath;
use lore_storage::hash::hash_string;
use lore_storage::local::immutable_store::LocalImmutableStore;

use crate::fs::fuse::fuse_fixture;
use crate::fs::fuse::fuse_fixture::RepositoryContextCreationArgsExt;

/// Builds a `Listing` the way [`Listing::build`] would, minus the async
/// State walk: sorted by `(name_hash, name bytes)`, one arena, `flags`
/// taken as given. `hash_override` lets a case hand-pick a colliding hash
/// to prove the fold-compare (not just the hash) decides a hit.
fn hand_built_listing(specs: &[(&str, u8, Option<u64>)]) -> Listing {
    let mut with_hash: Vec<(&str, u8, u64)> = specs
        .iter()
        .map(|(name, flags, hash_override)| {
            (
                *name,
                *flags,
                hash_override.unwrap_or_else(|| hash_string(name)),
            )
        })
        .collect();
    with_hash.sort_by(|a, b| {
        a.2.cmp(&b.2)
            .then_with(|| a.0.as_bytes().cmp(b.0.as_bytes()))
    });

    let mut entries = Vec::new();
    let mut names = Vec::new();
    let mut node_ids = Vec::new();
    for (index, (name, flags, name_hash)) in with_hash.into_iter().enumerate() {
        let name_offset = names.len() as u32;
        names.extend_from_slice(name.as_bytes());
        entries.push(ListingEntry {
            name_hash,
            size: 0,
            address: lore_base::types::Address::default(),
            name_offset,
            name_len: name.len() as u16,
            mode: 0,
            flags,
            filter_state: 0,
            block: 0,
            view_line: 0,
        });
        node_ids.push(index as NodeID);
    }

    Listing::from_parts(
        entries.into_boxed_slice(),
        names.into_boxed_slice(),
        node_ids.into_boxed_slice(),
    )
}

#[test]
fn find_hits_exact_and_case_variant_spelling() {
    let listing = hand_built_listing(&[("Rock.mesh", 0, None), ("paper.txt", 0, None)]);

    let exact = listing.find(b"Rock.mesh").expect("exact spelling must hit");
    assert_eq!(listing.name(exact), b"Rock.mesh");

    let variant = listing.find(b"ROCK.MESH").expect("case variant must hit");
    assert_eq!(listing.name(variant), b"Rock.mesh");
}

#[test]
fn find_miss_returns_none_without_allocating() {
    let listing = hand_built_listing(&[("Rock.mesh", 0, None)]);

    lore_base::allocator::set_allocation_counting(true);
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            lore_base::allocator::set_allocation_counting(false);
        }
    }
    let _restore = Restore;

    let before = lore_base::allocator::thread_allocation_count();
    let result = std::hint::black_box(listing.find(b"paper.txt"));
    let after = lore_base::allocator::thread_allocation_count();

    assert_eq!(result, None);
    assert_eq!(after, before, "a miss must not allocate");
}

#[test]
fn find_distinguishes_hash_collision_by_fold_eq() {
    let zebra_hash = hash_string("zebra.txt");
    // "aardvark.txt" is hand-patched to collide with zebra.txt's real
    // hash, and sorts first in that shared bucket (by name bytes): a
    // `find` that only checked the hash, never the fold-compare, would
    // wrongly stop on it instead of scanning on to zebra.txt.
    let listing = hand_built_listing(&[
        ("aardvark.txt", 0, Some(zebra_hash)),
        ("zebra.txt", 0, None),
    ]);

    let zebra = listing
        .find(b"zebra.txt")
        .expect("zebra.txt must hit despite aardvark.txt sharing its hash bucket");
    assert_eq!(listing.name(zebra), b"zebra.txt");

    assert_eq!(
        listing.find(b"gamma.txt"),
        None,
        "an entirely unrelated name must miss"
    );
}

#[test]
fn find_skips_view_excluded() {
    let listing = hand_built_listing(&[("secret.txt", entry_flags::VIEW_EXCLUDED, None)]);
    assert_eq!(
        listing.find(b"secret.txt"),
        None,
        "a view-excluded entry must not be found by name"
    );
}

#[test]
fn find_rejects_non_utf8() {
    let listing = hand_built_listing(&[("valid.txt", 0, None)]);
    assert_eq!(
        listing.find(b"\xff\xfe"),
        None,
        "non-UTF-8 input must miss, not panic"
    );
}

#[test]
fn representable_name_is_accepted() {
    assert!(is_representable(b"rock.mesh"));
    assert!(is_representable(b".gitignore"));
}

#[test]
fn slash_is_rejected() {
    assert!(!is_representable(b"a/b"));
}

#[test]
fn nul_is_rejected() {
    assert!(!is_representable(b"a\0b"));
}

#[test]
fn dot_and_dotdot_are_rejected() {
    assert!(!is_representable(b"."));
    assert!(!is_representable(b".."));
}

#[test]
fn over_255_bytes_is_rejected() {
    let long = vec![b'a'; 256];
    assert!(!is_representable(&long));
    let boundary = vec![b'a'; 255];
    assert!(is_representable(&boundary));
}

#[test]
fn empty_is_rejected() {
    assert!(!is_representable(b""));
}

/// An empty listing of a fresh root block, its log installed, with one `Attached` patch for
/// `name` naming `child`'s block.
fn listing_with_a_patch(
    name: &str,
    child: &std::sync::Arc<lore_revision::node::NodeBlock>,
) -> (
    std::sync::Arc<Listing>,
    std::sync::Arc<lore_revision::node::NodeBlock>,
) {
    use lore_revision::node::ChainPatch;
    use lore_revision::node::NodeBlock;
    use lore_revision::node::PatchKind;
    let dir_block = NodeBlock::new_zeroed();
    let listing = Listing::empty(0, dir_block.clone()).expect("an empty directory");
    dir_block
        .write_for_tests()
        .chain_log_mut(0)
        .expect("the listing installed its directory's log")
        .append(ChainPatch {
            name_hash: hash_string(name),
            block: std::sync::Arc::downgrade(child),
            node: 515,
            kind: PatchKind::Attached,
        });
    (listing, dir_block)
}

#[test]
fn a_patch_names_its_node_through_the_block_it_holds() {
    let child = lore_revision::node::NodeBlock::new_zeroed();
    let (listing, _dir_block) = listing_with_a_patch("new.txt", &child);
    match listing.lookup("new.txt") {
        Hit::Added(node, block) => {
            assert_eq!(node, 515);
            assert!(std::sync::Arc::ptr_eq(&block, &child));
        }
        other => panic!("expected the patched node, got {other:?}"),
    }
    assert!(matches!(listing.lookup("other.txt"), Hit::Miss));
}

/// The name may still be live in the State with its block on the store, so a miss would be
/// wrong.
#[test]
fn a_patch_whose_block_was_dropped_answers_stale() {
    let child = lore_revision::node::NodeBlock::new_zeroed();
    let (listing, _dir_block) = listing_with_a_patch("gone.txt", &child);
    drop(child);
    assert!(matches!(listing.lookup("gone.txt"), Hit::Stale));
}

/// A child the build left out may hold any name, so the listing shows none absent.
#[test]
fn a_build_that_left_a_child_out_shows_no_name_absent() {
    let dir_block = lore_revision::node::NodeBlock::new_zeroed();
    let listing = Listing::empty(0, dir_block).expect("an empty directory");
    let state = State::new();
    let absent = |listing: &Listing| listing.lookup_to_attach(&state, "free.txt").1;
    assert!(absent(&listing).is_some(), "nothing holds the name");
    let Ok(mut listing) = std::sync::Arc::try_unwrap(listing) else {
        panic!("the test holds the only reference");
    };
    listing.skipped_staged = 1;
    assert!(absent(&listing).is_none(), "a staged delete was left out");
    listing.skipped_staged = 0;
    listing.skipped_unrepresentable = 1;
    assert!(
        absent(&listing).is_none(),
        "an unrepresentable name was left out"
    );
    listing.skipped_unrepresentable = 0;
    listing.skipped_invalid = 1;
    assert!(
        absent(&listing).is_none(),
        "a child whose name its block could not give back was left out"
    );
}

/// The newest patch for the name since the build decides: an attach holds it, a detach frees
/// it.
#[test]
fn a_name_attached_since_the_build_is_not_absent_until_detached() {
    use lore_revision::node::ChainPatch;
    use lore_revision::node::PatchKind;
    let child = lore_revision::node::NodeBlock::new_zeroed();
    let (listing, dir_block) = listing_with_a_patch("new.txt", &child);
    let state = State::new();
    assert!(
        matches!(
            listing.lookup_to_attach(&state, "new.txt"),
            (Hit::Added(515, _), None)
        ),
        "the attach holds the name"
    );
    assert!(
        matches!(
            listing.lookup_to_attach(&state, "other.txt"),
            (Hit::Miss, Some(_))
        ),
        "another name is free"
    );
    dir_block
        .write_for_tests()
        .chain_log_mut(0)
        .expect("the directory's log")
        .append(ChainPatch {
            name_hash: hash_string("new.txt"),
            block: std::sync::Arc::downgrade(&child),
            node: 515,
            kind: PatchKind::Detached,
        });
    let (hit, absent) = listing.lookup_to_attach(&state, "new.txt");
    assert!(matches!(hit, Hit::Miss));
    let absent = absent.expect("the detach freed the name");
    assert_eq!(absent.at(), 2, "the position after both patches");
}

struct InMemoryMarker;
impl InMemoryContext for InMemoryMarker {}
const IN_MEMORY_MARKER: InMemoryMarker = InMemoryMarker;

/// What a provider serves over `state`/`repository`, for a `Listing::build` call.
fn test_served(state: Arc<State>, repository: Arc<RepositoryContext>) -> Served {
    Served::new(
        state,
        repository,
        Arc::new(ListingCache::new(LISTING_CACHE_BYTES, LISTING_BLOCK_BYTES)),
    )
}

/// A repository over freshly minted in-memory stores, for constructing nodes
/// `state.node_add` accepts but no real filesystem write ever could (a link,
/// or a bare `.` name) -- the same technique `commit_in_memory.rs` uses.
async fn in_memory_repository(
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Arc<RepositoryContext> {
    let immutable_store = LocalImmutableStore::new(
        None,
        lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
    )
    .await
    .expect("in-memory immutable store");
    Arc::new(
        RepositoryContext::new(fuse_fixture::default_repository_creation_args(
            immutable_store,
            mutable_store,
        ))
        .with_write_token(RepositoryWriteToken::in_memory(&IN_MEMORY_MARKER)),
    )
}

/// Adds `node` under `parent` as `name` and marks it staged, exactly as a real
/// write would leave it -- `commit_in_memory.rs`'s helper of the same name.
async fn add(
    state: &State,
    repository: Arc<RepositoryContext>,
    parent: NodeID,
    node: Node,
    name: &str,
) -> NodeID {
    let node_id = state
        .node_add(repository.clone(), parent, node, name)
        .await
        .expect("adding the node must succeed");
    state
        .node_mark(
            &repository,
            node_id,
            Mark::staged_and_dirty(NodeFlags::StagedAdd, NodeFlags::DirtyAdd),
        )
        .await
        .expect("marking the addition must succeed");
    node_id
}

fn placeholder_address(marker: u64) -> Address {
    Address {
        hash: Hash::from_u64(marker),
        context: Context::from(uuid::Uuid::now_v7()),
    }
}

/// A repository over the same stores, path, id and instance id as `fixture`'s,
/// but with `globs` loaded into the view slot instead of an empty default.
///
/// Inlines what `tests/helper.rs`'s `test_filter_context` does, since that
/// helper takes a `&TestRepository` and `fuse_fixture::committed` does not
/// hand one back (it consumes it building `FuseFixture`). A leading `!`
/// re-includes, exactly as it does in an authored view file.
fn view_repository(fixture: &fuse_fixture::FuseFixture, globs: &[&str]) -> Arc<RepositoryContext> {
    let mut filter = Filter::default();
    for glob in globs {
        match glob.strip_prefix('!') {
            Some(inclusion) => filter
                .view
                .add_inclusion(inclusion)
                .expect("filter inclusion"),
            None => filter.view.add_exclusion(glob).expect("filter exclusion"),
        }
    }
    let mutable_store = fixture
        .repository
        .try_mutable_store_arc()
        .expect("fixture repository must hold a write token");
    Arc::new(RepositoryContext::new(
        fuse_fixture::default_repository_creation_args(
            fixture.repository.immutable_store(),
            mutable_store,
        )
        .with_path(&fixture.path)
        .with_id(fixture.repository.id)
        .with_instance_id(fixture.repository.instance_id)
        .with_filter(Arc::new(filter)),
    ))
}

#[tokio::test]
async fn listing_is_sorted_by_name_hash_and_holds_every_child() {
    let fixture = fuse_fixture::committed(&[
        ("zebra.txt", b"z"),
        ("apple.txt", b"a"),
        ("mango.txt", b"m"),
        ("dir/nested.txt", b"n"),
    ])
    .await;

    let served = test_served(fixture.state.clone(), fixture.repository.clone());
    let listing = Listing::build(&served, ROOT_NODE, FilterStates::ROOT, &RelativePath::new())
        .await
        .expect("build must succeed for a plain directory");

    assert_eq!(listing.len(), 4, "every top-level child must be present");

    let mut names: Vec<Vec<u8>> = (0..listing.len())
        .map(|i| listing.name(i).to_vec())
        .collect();
    names.sort();
    let mut expected = vec![
        b"apple.txt".to_vec(),
        b"dir".to_vec(),
        b"mango.txt".to_vec(),
        b"zebra.txt".to_vec(),
    ];
    expected.sort();
    assert_eq!(names, expected, "the listing must hold every child by name");

    for pair in listing.entries.windows(2) {
        assert!(
            pair[0].name_hash <= pair[1].name_hash,
            "entries must be sorted by name_hash"
        );
    }
}

#[tokio::test]
async fn listing_skips_names_with_slash_nul_dot_dotdot_or_over_255_bytes_and_counts_them() {
    // `is_representable`'s own unit tests cover all five categories directly.
    // A State refuses `/`, NUL, `.` and `..` in a name it adds or reads, but
    // holds a name of up to `MAX_NODE_NAME_LEN` bytes, past the 255 a FUSE
    // entry carries: 128 `é`s, 256 bytes in UTF-8, are a name a Windows client
    // commits. This test runs that one reachable category through `build`.
    let (_immutable, mutable, execution) = fuse_fixture::test_store_create()
        .await
        .expect("test store creation must succeed");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let repository = in_memory_repository(mutable).await;
        let state = State::new();

        let long_name = "é".repeat(128);
        let long = Node {
            flags: NodeFlags::File.bits(),
            mode: 0o644,
            size: 1,
            address: placeholder_address(1),
            name_hash: hash_string(&long_name),
            ..Default::default()
        };
        add(&state, repository.clone(), ROOT_NODE, long, &long_name).await;

        let ok = Node {
            flags: NodeFlags::File.bits(),
            mode: 0o644,
            size: 2,
            address: placeholder_address(2),
            name_hash: hash_string("a.txt"),
            ..Default::default()
        };
        add(&state, repository.clone(), ROOT_NODE, ok, "a.txt").await;

        let served = test_served(state, repository);
        let listing = Listing::build(&served, ROOT_NODE, FilterStates::ROOT, &RelativePath::new())
            .await
            .expect("build must succeed, skipping the unrepresentable name");

        assert_eq!(
            listing.skipped_unrepresentable, 1,
            "the 256-byte child must be skipped and counted"
        );
        assert_eq!(listing.len(), 1, "the representable child must remain");
        assert_eq!(listing.name(0), b"a.txt");
    }))
    .await
    .expect("task failed");
}

#[tokio::test]
async fn listing_refuses_link_nodes() {
    let (_immutable, mutable, execution) = fuse_fixture::test_store_create()
        .await
        .expect("test store creation must succeed");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let repository = in_memory_repository(mutable).await;
        let state = State::new();

        let link = Node {
            flags: NodeFlags::Link.bits(),
            mode: 0o755,
            name_hash: hash_string("vendor"),
            child: 0,
            address: placeholder_address(0x5a),
            ..Default::default()
        };
        add(&state, repository.clone(), ROOT_NODE, link, "vendor").await;

        let served = test_served(state, repository);
        let result =
            Listing::build(&served, ROOT_NODE, FilterStates::ROOT, &RelativePath::new()).await;

        assert!(
            matches!(result, Err(FuseProviderError::NotSupported(_))),
            "a link child must fail the build with NotSupported"
        );
    }))
    .await
    .expect("task failed");
}

#[tokio::test]
async fn entry_records_dir_flag_mode_size_address_and_node_id() {
    let fixture = fuse_fixture::committed(&[
        ("top.txt", b"hello world"),
        ("dir/child.txt", b"nested contents"),
    ])
    .await;

    let served = test_served(fixture.state.clone(), fixture.repository.clone());
    let listing = Listing::build(&served, ROOT_NODE, FilterStates::ROOT, &RelativePath::new())
        .await
        .expect("build must succeed");

    assert_eq!(listing.len(), 2);

    for i in 0..listing.len() {
        let entry = listing.entry(i);
        let node_id = listing.node_ids[i];
        let node = fixture
            .state
            .node(fixture.repository.clone(), node_id)
            .await
            .expect("node must read back for a listed child");

        assert_eq!(entry.mode, node.mode, "entry mode must match the node's");
        assert_eq!(
            entry.address, node.address,
            "entry address must match the node's full address (hash and context)"
        );
        assert_eq!(
            entry.flags & entry_flags::IS_DIR != 0,
            node.is_directory(),
            "IS_DIR must reflect the node's own directory-ness"
        );
        if !node.is_directory() {
            assert_eq!(entry.size, node.size, "entry size must match a file node's");
        }
    }
}

#[tokio::test]
async fn root_listing_build_uses_root_node() {
    let fixture = fuse_fixture::committed(&[("only.txt", b"root child")]).await;

    let served = test_served(fixture.state.clone(), fixture.repository.clone());
    let listing = Listing::build(&served, ROOT_NODE, FilterStates::ROOT, &RelativePath::new())
        .await
        .expect("build must succeed against the root node");

    assert_eq!(listing.len(), 1, "the root's own child must be listed");
    assert_eq!(listing.name(0), b"only.txt");
}

// `child_filter` reports a hit through `LoreEvent::send`, which reads the
// task-local execution context -- absent under a bare `#[tokio::test]` once
// `fuse_fixture::committed`'s own internal scope has returned. The other
// tests never trip this because `FilterStates::ROOT` over an unfiltered
// default `Filter` never actually excludes anything, so `emit` never sends.
// These three actually exclude something, so they need the same
// `lore_spawn!(LORE_CONTEXT.scope(...))` wrapping `commit_in_memory.rs` uses.
#[tokio::test]
async fn view_excluded_children_are_flagged() {
    lore_spawn!(
        LORE_CONTEXT.scope(fuse_fixture::setup_test_execution(), async move {
            let fixture =
                fuse_fixture::committed(&[("keep.txt", b"k"), ("secret.txt", b"s")]).await;

            let repository = view_repository(&fixture, &["secret.txt"]);
            let served = test_served(fixture.state.clone(), repository);
            let listing =
                Listing::build(&served, ROOT_NODE, FilterStates::ROOT, &RelativePath::new())
                    .await
                    .expect("build must succeed under a restrictive view");

            assert_eq!(
                listing.len(),
                2,
                "an excluded child is still listed, just flagged"
            );

            let secret = (0..listing.len())
                .find(|&i| listing.name(i) == b"secret.txt")
                .expect("secret.txt must be listed");
            let keep = (0..listing.len())
                .find(|&i| listing.name(i) == b"keep.txt")
                .expect("keep.txt must be listed");

            assert_ne!(
                listing.entry(secret).flags & entry_flags::VIEW_EXCLUDED,
                0,
                "secret.txt must be flagged view-excluded"
            );
            assert!(
                filter_states_from_u8(listing.entry(secret).filter_state)
                    .view
                    .excluded(),
                "secret.txt's packed filter_state must decode as view-excluded"
            );
            assert_eq!(
                listing.entry(keep).flags & entry_flags::VIEW_EXCLUDED,
                0,
                "keep.txt must not be flagged view-excluded"
            );
        })
    )
    .await
    .expect("task failed");
}

#[tokio::test]
async fn nested_directory_inherits_filter_state() {
    lore_spawn!(
        LORE_CONTEXT.scope(fuse_fixture::setup_test_execution(), async move {
            let fixture =
                fuse_fixture::committed(&[("dir/keep.txt", b"k"), ("dir/secret.txt", b"s")]).await;

            // A bare name excludes any node called "dir" at any depth; "dir"
            // itself is the only such node here.
            let repository = view_repository(&fixture, &["dir"]);
            let served = test_served(fixture.state.clone(), repository);

            let root_listing =
                Listing::build(&served, ROOT_NODE, FilterStates::ROOT, &RelativePath::new())
                    .await
                    .expect("root build must succeed");

            let dir_index = (0..root_listing.len())
                .find(|&i| root_listing.name(i) == b"dir")
                .expect("dir must still be listed even though the view excludes it");
            let dir_entry = root_listing.entry(dir_index);
            assert_ne!(
                dir_entry.flags & entry_flags::VIEW_EXCLUDED,
                0,
                "dir itself must be flagged view-excluded"
            );

            // Threads the parent's packed filter_state back into FilterStates
            // for the nested build, exactly as a real lookup chain would from
            // a cached entry.
            let dir_filter_state = filter_states_from_u8(dir_entry.filter_state);
            let dir_node_id = root_listing.node_ids[dir_index];
            let dir_path = RelativePath::new().join("dir");

            let nested_listing = Listing::build(&served, dir_node_id, dir_filter_state, &dir_path)
                .await
                .expect("nested build must succeed under an excluded parent");

            assert_eq!(
                nested_listing.len(),
                2,
                "both children must still be listed"
            );
            for i in 0..nested_listing.len() {
                assert_ne!(
                    nested_listing.entry(i).flags & entry_flags::VIEW_EXCLUDED,
                    0,
                    "a child of an excluded directory must inherit the exclusion with no rule \
                     of its own"
                );
            }
        })
    )
    .await
    .expect("task failed");
}

/// A listing below an excluded directory, built from the directory's entry as a lookup chain
/// builds one, takes no line before the one that excluded the directory: a re-inclusion authored
/// before it lost there and re-includes nothing below, as an ordinary clone's walk leaves it.
#[tokio::test]
async fn a_nested_listing_resumes_from_the_line_that_excluded_its_directory() {
    lore_spawn!(
        LORE_CONTEXT.scope(fuse_fixture::setup_test_execution(), async move {
            let fixture = fuse_fixture::committed(&[
                ("assets/keep.txt", b"k"),
                ("assets/plain/p1.txt", b"p"),
            ])
            .await;
            let repository = view_repository(&fixture, &["**", "!assets/**", "assets/plain/"]);
            let served = test_served(fixture.state.clone(), repository);

            let mut listing =
                Listing::build(&served, ROOT_NODE, FilterStates::ROOT, &RelativePath::new())
                    .await
                    .expect("the root's listing");
            let mut path = RelativePath::new();
            for name in ["assets", "plain"] {
                let index = (0..listing.len())
                    .find(|&i| listing.name(i) == name.as_bytes())
                    .unwrap_or_else(|| panic!("{name} is listed"));
                let entry = listing.entry(index);
                let states = filter_states_from_packed(entry.filter_state, entry.view_line);
                path = path.join(name);
                listing = Listing::build(&served, listing.node_ids[index], states, &path)
                    .await
                    .unwrap_or_else(|error| panic!("{name}'s listing: {error}"));
            }
            let p1 = (0..listing.len())
                .find(|&i| listing.name(i) == b"p1.txt")
                .expect("p1.txt is listed");
            assert_ne!(
                listing.entry(p1).flags & entry_flags::VIEW_EXCLUDED,
                0,
                "the view's `assets/plain/` excludes p1.txt, whatever `!assets/**` above it says"
            );
        })
    )
    .await
    .expect("task failed");
}

#[tokio::test]
async fn view_change_yields_new_listing_cache() {
    lore_spawn!(
        LORE_CONTEXT.scope(fuse_fixture::setup_test_execution(), async move {
            let fixture = fuse_fixture::committed(&[("secret.txt", b"s")]).await;

            let repository_a = view_repository(&fixture, &["secret.txt"]);
            let repository_b = view_repository(&fixture, &[]);

            assert!(
                !same_view(&repository_a, &repository_b),
                "two distinct Filter instances must not count as the same view, even over one \
                 fixture"
            );

            let cache_a = Arc::new(ListingCache::new(LISTING_CACHE_BYTES, LISTING_BLOCK_BYTES));
            let served_a =
                Served::new(fixture.state.clone(), repository_a.clone(), cache_a.clone());
            let listing_a = Listing::build(
                &served_a,
                ROOT_NODE,
                FilterStates::ROOT,
                &RelativePath::new(),
            )
            .await
            .expect("build under view A must succeed");
            cache_a.insert(ROOT_NODE, listing_a.clone());

            // A remount whose view changed must not carry the old cache
            // forward: a hit for the same directory would otherwise hand back
            // view A's exclusion bits under view B's mount. `same_view` is
            // what a real caller would gate this decision on.
            let cache_b = if same_view(&repository_a, &repository_b) {
                cache_a.clone()
            } else {
                Arc::new(ListingCache::new(LISTING_CACHE_BYTES, LISTING_BLOCK_BYTES))
            };
            assert!(
                cache_b.is_empty(),
                "a genuinely new view must start from an empty cache"
            );

            let served_b =
                Served::new(fixture.state.clone(), repository_b.clone(), cache_b.clone());
            let listing_b = Listing::build(
                &served_b,
                ROOT_NODE,
                FilterStates::ROOT,
                &RelativePath::new(),
            )
            .await
            .expect("build under view B must succeed");
            cache_b.insert(ROOT_NODE, listing_b.clone());

            let secret_a = (0..listing_a.len())
                .find(|&i| listing_a.name(i) == b"secret.txt")
                .expect("secret.txt listed under view A");
            let secret_b = (0..listing_b.len())
                .find(|&i| listing_b.name(i) == b"secret.txt")
                .expect("secret.txt listed under view B");

            assert_ne!(
                listing_a.entry(secret_a).flags & entry_flags::VIEW_EXCLUDED,
                0,
                "view A excludes secret.txt"
            );
            assert_eq!(
                listing_b.entry(secret_b).flags & entry_flags::VIEW_EXCLUDED,
                0,
                "view B does not exclude secret.txt"
            );

            assert!(
                Arc::ptr_eq(
                    &cache_a.get(ROOT_NODE).expect("view A cache hit"),
                    &listing_a
                ),
                "view A's cache must hand back view A's listing"
            );
            assert!(
                Arc::ptr_eq(
                    &cache_b.get(ROOT_NODE).expect("view B cache hit"),
                    &listing_b
                ),
                "view B's cache must hand back view B's listing, independent of cache A"
            );
        })
    )
    .await
    .expect("task failed");
}

#[tokio::test]
async fn listing_build_skips_discarded_nodes() {
    let (_immutable, mutable, execution) = fuse_fixture::test_store_create()
        .await
        .expect("test store creation must succeed");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let repository = in_memory_repository(mutable).await;
        let state = State::new();

        let keep = Node {
            flags: NodeFlags::File.bits(),
            mode: 0o644,
            size: 1,
            address: placeholder_address(1),
            name_hash: hash_string("keep.txt"),
            ..Default::default()
        };
        add(&state, repository.clone(), ROOT_NODE, keep, "keep.txt").await;

        let gone = Node {
            flags: NodeFlags::File.bits(),
            mode: 0o644,
            size: 2,
            address: placeholder_address(2),
            name_hash: hash_string("gone.txt"),
            ..Default::default()
        };
        let gone_id = add(&state, repository.clone(), ROOT_NODE, gone, "gone.txt").await;

        // Discarded but still in the chain: what a walk that read the node's predecessor
        // before the detach meets.
        let block = state
            .block(repository.clone(), NodeBlock::index(gone_id))
            .await
            .expect("the node's block must load");
        block.write_for_tests().node(Node::index(gone_id)).flags |= NodeFlags::Discarded.bits();
        drop(block);

        let served = test_served(state, repository);
        let listing = Listing::build(&served, ROOT_NODE, FilterStates::ROOT, &RelativePath::new())
            .await
            .expect("build must succeed with a discarded node still in the chain");

        assert_eq!(listing.len(), 1, "the discarded node must not be listed");
        assert_eq!(listing.name(0), b"keep.txt");
    }))
    .await
    .expect("task failed");
}
