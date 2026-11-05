// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! H9: the stage's search for what was attached to a directory since its listing walking from the
//! chain's head to the head the listing saw, which holds while children are only prepended. A
//! reattach re-prepends: a served rename putting the listing's head back in front leaves what was
//! added since behind it, and the search answers that nothing was, so the stage adds the name
//! again. The raw scenarios run the stage's own listing and claim (`claim_child`) beside served
//! renames; their twins run the stage's claims as one modelled call (`STAGE_CLAIM`), which
//! re-composes `claim_child` over the same State calls. What each catches:
//! - the head scenario, a create and then a rename of the listing's head both landing between the
//!   listing and the claim; with the claim
//!   ignoring the listing's mark, or answering a stale search from its listing without the find
//!   through the chain; and where a search's answer the listing holds under its old name is left
//!   unclaimed (the stage listing without a mark is `tests/stage.rs`'s to catch);
//! - the respell scenario where the search tests the directory's count before its walk rather
//!   than after: a respell moving a child the walk has not reached yet in front of it, its name
//!   hash the same throughout, is missed; its counter without the walk's `Point::IterStep`; with
//!   the listing's head detached, so the walk covers the whole chain, where that walk's miss is
//!   never made stale;
//! - the listing scenario where the mark's count is read at the listing's end rather than with
//!   its head: a create and a rename of the head landing while the listing walks block 0, after
//!   it passed the head in block 1, are left out of the listing and hidden from the search;
//! - the sequential second claim of a hash where a miss with nothing added since answers no
//!   child rather than the listing's, with a mark and without one (a link's or a file's
//!   listing); and the claim of a renamed listed child's new name where the search's answer is
//!   left unclaimed in the listing;
//! - the twins of the head and listing scenarios where their raw scenarios fail in the State: the
//!   search ignoring a reattach, the listing's mark read late; the twins of the second claim of a
//!   hash and of the claim of a renamed listed child's new name where the mark's reattach read
//!   fails; `claim_child`'s own faults are the raw scenarios' to catch.
//!
//! No writer but the stage's own tasks runs beside a stage in production (the node module's
//! Limitations, "The stage's listing"); the scenarios, beyond the contract, hold the stage's claim
//! to the search's contract under any.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_revision::node::BLOCK_NODE_COUNT;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeID;
use lore_revision::repository::RepositoryContext;
use lore_revision::stage::DirectoryChildren;
use lore_revision::stage::claim_child;
use lore_revision::state::ChainMark;
use lore_revision::state::State;
use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::Point;
use lore_storage::hash::hash_string;

use super::Flag;
use crate::harness::catalogue::modelled::Modelling;
use crate::harness::catalogue::ops;
use crate::harness::fixtures::Tree;
use crate::harness::model::Modelled;
use crate::harness::scheduler::Check;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Op;
use crate::harness::scheduler::Scenario;

/// The stage's listing of `d`, with the mark of the chain it began from.
async fn listing(tree: &Tree) -> DirectoryChildren {
    let (listing, since) = tree
        .state
        .node_children_with_name_hash_marked(tree.repository.clone(), tree.id("d"))
        .await
        .expect("the listing");
    DirectoryChildren::new(listing, since)
}

/// The stage's claim of `name` in `d`.
async fn claim(tree: &Tree, children: &mut DirectoryChildren, name: &str) -> Option<NodeID> {
    claim_child(
        &tree.repository,
        &tree.state,
        tree.id("d"),
        children,
        hash_string(name),
    )
    .await
    .expect("the claim")
}

/// Explores `scenario`, whole or, where its whole space takes too long to run, to `preemptions`
/// preemptions with sleep sets off, as they are sound only for a whole exploration.
fn explore<C: Modelled + Send + Sync + 'static>(scenario: Scenario<C>, preemptions: Option<usize>) {
    crate::explored(
        scenario,
        Modelling::default(),
        Exploration {
            preemptions: preemptions.unwrap_or(Exploration::default().preemptions),
            sleep_sets: preemptions.is_none(),
            ..Exploration::default()
        },
    );
}

/// [`explore`], asserting `raced` counted a run.
fn explore_racing<C: Modelled + Send + Sync + 'static>(
    scenario: Scenario<C>,
    preemptions: Option<usize>,
    (raced, race): (&AtomicUsize, &str),
) {
    explore(scenario, preemptions);
    assert!(raced.load(Ordering::Relaxed) > 0, "no run had {race}");
}

/// The check counting in `raced` each run whose claim `raced_in` its context says raced.
fn counting_raced<C: 'static>(
    raced: &Arc<AtomicUsize>,
    raced_in: fn(&C) -> &OnceLock<bool>,
) -> Check<C> {
    let raced = raced.clone();
    Box::new(move |c: Arc<C>| {
        if raced_in(&c).get() == Some(&true) {
            raced.fetch_add(1, Ordering::Relaxed);
        }
        Box::pin(async {})
    })
}

/// Adds file `name` below `dir`, clean.
async fn add_file(tree: &Tree, dir: NodeID, name: &str) -> NodeID {
    tree.state
        .node_add(tree.repository.clone(), dir, ops::file(name), name)
        .await
        .expect("a file adds")
}

/// Adds files below `d` until block 0 is full; the one in its last slot but one, and its name.
/// The chain runs from the file in the last slot to it; a file `head` added after goes to
/// block 1.
async fn fill_block_0(tree: &Tree) -> (NodeID, String) {
    let d = tree.id("d");
    let mut index = 0;
    let mut previous = None;
    loop {
        let name = format!("more{index}");
        let id = add_file(tree, d, &name).await;
        assert_eq!(NodeBlock::index(id), 0, "the files fill block 0 first");
        if Node::index(id) == BLOCK_NODE_COUNT - 1 {
            return previous.expect("block 0 holds more than one file");
        }
        previous = Some((id, name));
        index += 1;
    }
}

/// Adds `head` below `d`, in block 1, heading its chain.
async fn add_head(tree: &Tree) -> NodeID {
    let head = add_file(tree, tree.id("d"), "head").await;
    assert_eq!(
        NodeBlock::index(head),
        1,
        "the chain's head lies in block 1"
    );
    head
}

struct HeadContext {
    tree: Tree,
    /// The listing's head, which the setup made the chain's.
    head: NodeID,
    /// The node the create of `d/a` made, once it returned.
    created: OnceLock<NodeID>,
    created_flag: Flag,
    /// Whether the rename of the head returned, having moved it.
    renamed: Flag,
    /// Whether the claim began after the create and then the rename, `a` not in its listing.
    raced: OnceLock<bool>,
}

impl HeadContext {
    /// The tree [`head_and_other`] builds, nothing landed yet.
    async fn new() -> Self {
        let (tree, head, _) = head_and_other().await;
        Self {
            tree,
            head,
            created: OnceLock::new(),
            created_flag: Flag::new("created"),
            renamed: Flag::new("renamed"),
            raced: OnceLock::new(),
        }
    }

    /// Whether `d`'s chain holds the head in front of the node the create made: the rename came
    /// after the create.
    async fn head_first(&self) -> bool {
        let order = (self.tree.state)
            .node_children(self.tree.repository.clone(), self.tree.id("d"))
            .await
            .expect("d's children");
        let at = |node| order.iter().position(|&id| id == node);
        (self.created.get()).is_some_and(|&created| at(self.head) < at(created))
    }

    /// The served rename of the head within `d`, noted where it moved the head.
    async fn rename_head(&self) {
        if ops::served_rename(&self.tree, self.head, self.tree.id("d"), "renamed").await {
            self.renamed.set();
        }
    }
}

impl Modelled for HeadContext {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }
}

/// The stage's listing of `d` and its claim of `a`, beside a checked create of `d/a` and a
/// served rename of the listing's head within `d`: a claim beginning after the create returned
/// answers `a`; one beginning after the rename answers the head under its new name, and claims
/// it in the listing, which holds it under the old one.
#[test]
fn a_claim_after_a_create_and_a_rename_of_the_listing_head_finds_the_created() {
    let raced = Arc::new(AtomicUsize::new(0));
    let scenario = Scenario::<HeadContext> {
        name: "h9: a claim beside a create and a rename of the listing's head",
        setup: Box::new(|| Box::pin(HeadContext::new())),
        ops: vec![
            Box::new(|c: Arc<HeadContext>| {
                Box::pin(async move {
                    let mut children = listing(&c.tree).await;
                    let listed = children.holds(hash_string("a")).is_some();
                    hooks::reached(Point::Named("listed")).await;
                    let created = c.created_flag.get();
                    let renamed = c.renamed.get();
                    let head_first = created && renamed && c.head_first().await;
                    c.raced
                        .set(created && renamed && head_first && !listed)
                        .expect("one claim");
                    let claimed = claim(&c.tree, &mut children, "a").await;
                    if created {
                        assert_eq!(
                            claimed,
                            c.created.get().copied(),
                            "a claim beginning after 'd/a' was created answers it"
                        );
                    }
                    if renamed {
                        assert_eq!(
                            claim(&c.tree, &mut children, "renamed").await,
                            Some(c.head),
                            "a claim beginning after the rename answers the head"
                        );
                        assert!(
                            !children.unclaimed().any(|id| id == c.head),
                            "the head the claim answered is claimed in the listing"
                        );
                    }
                })
            }),
            Box::new(|c: Arc<HeadContext>| {
                Box::pin(async move {
                    assert!(create_a(&c.tree, &c.created).await, "the create attaches");
                    c.created_flag.set();
                })
            }),
            Box::new(|c: Arc<HeadContext>| Box::pin(async move { c.rename_head().await })),
        ],
        cancellable: vec![],
        check: counting_raced(&raced, |c: &HeadContext| &c.raced),
    };
    explore_racing(
        scenario,
        Some(3),
        (
            &raced,
            "the claim begin after the create and then the rename, 'a' not in its listing",
        ),
    );
}

struct RespellContext {
    tree: Tree,
    /// The listing of `d` the setup took, with its mark.
    listing: (Vec<(NodeID, u64)>, Option<ChainMark>),
    /// The child added since the listing, in block 0's last slot but one, which the respell
    /// moves; its name.
    moved: (NodeID, String),
    /// Whether the respell returned, having moved it.
    respelled: Flag,
    /// Whether the respell returned while the claim was under way.
    raced: OnceLock<bool>,
}

impl Modelled for RespellContext {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }
}

/// The stage's claim of a child added since its listing, in block 0 second behind the chain's
/// head in block 1 (the child right behind it is the walk's next where it enters block 0, met
/// however a respell moves it), beside a served respell of it, which changes its name's case
/// only and moves it to the head: its name hash stands throughout, so the claim answers it, the
/// respell landing before the search, during its walk (where it goes on into block 0 past the
/// head, the child not yet reached) or after.
#[test]
fn a_claim_beside_a_respell_of_a_child_added_since_finds_it() {
    respell_beside_a_claim(false);
}

/// [`a_claim_beside_a_respell_of_a_child_added_since_finds_it`] with the listing's head
/// detached, so the search's walk covers the whole chain: a respell during it makes its miss
/// stale all the same.
#[test]
fn a_claim_beside_a_respell_after_the_listing_head_left_finds_it() {
    respell_beside_a_claim(true);
}

fn respell_beside_a_claim(detach_listing_head: bool) {
    let raced = Arc::new(AtomicUsize::new(0));
    let seen = raced.clone();
    let scenario = Scenario::<RespellContext> {
        name: if detach_listing_head {
            "h9: a claim beside a respell, the listing's head detached"
        } else {
            "h9: a claim beside a respell of a child added since the listing"
        },
        setup: Box::new(move || {
            Box::pin(async move {
                let tree = Tree::new(&["d/h"]).await;
                let listing = tree
                    .state
                    .node_children_with_name_hash_marked(tree.repository.clone(), tree.id("d"))
                    .await
                    .expect("the listing");
                let moved = fill_block_0(&tree).await;
                add_head(&tree).await;
                if detach_listing_head {
                    assert!(
                        ops::unserved_detach(&tree, tree.id("d/h")).await,
                        "the listing's head detaches"
                    );
                }
                RespellContext {
                    tree,
                    listing,
                    moved,
                    respelled: Flag::new("respelled"),
                    raced: OnceLock::new(),
                }
            })
        }),
        ops: vec![
            Box::new(|c: Arc<RespellContext>| {
                Box::pin(async move {
                    let before = c.respelled.get();
                    let (listing, since) = c.listing.clone();
                    let mut children = DirectoryChildren::new(listing, since);
                    let claimed = claim(&c.tree, &mut children, &c.moved.1).await;
                    c.raced
                        .set(!before && c.respelled.get())
                        .expect("one claim");
                    assert_eq!(
                        claimed,
                        Some(c.moved.0),
                        "the claim answers the child its name stood for throughout"
                    );
                })
            }),
            Box::new(|c: Arc<RespellContext>| {
                Box::pin(async move {
                    let respelled = c.moved.1.to_uppercase();
                    if ops::served_rename(&c.tree, c.moved.0, c.tree.id("d"), &respelled).await {
                        c.respelled.set();
                    }
                })
            }),
        ],
        cancellable: vec![],
        check: Box::new(move |c: Arc<RespellContext>| {
            let raced = seen.clone();
            Box::pin(async move {
                assert!(c.respelled.get(), "the respell moved the child");
                if c.raced.get() == Some(&true) {
                    raced.fetch_add(1, Ordering::Relaxed);
                }
            })
        }),
    };
    explore_racing(
        scenario,
        None,
        (&raced, "the respell land while the claim was under way"),
    );
}

struct ListingContext {
    tree: Tree,
    /// The chain's head, in block 1.
    head: NodeID,
    /// The node the create of `d/a` made, once it returned.
    created: OnceLock<NodeID>,
    /// Whether the create, then the rename of the head, returned.
    done: Flag,
    /// Whether the create and the rename both landed after the listing began, `a` left out of it.
    raced: OnceLock<bool>,
}

impl ListingContext {
    /// A tree whose `d`'s chain crosses blocks: its head in block 1, then `h` in block 0, which
    /// the files of another directory fill.
    async fn new() -> Self {
        let tree = Tree::spread(&["d/h"], "fill", &[]).await;
        let head = add_head(&tree).await;
        Self {
            tree,
            head,
            created: OnceLock::new(),
            done: Flag::new("done"),
            raced: OnceLock::new(),
        }
    }
}

impl Modelled for ListingContext {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }
}

/// The stage's listing of `d`, its chain across two blocks, and its claim of `a`, beside a
/// checked create of `d/a` followed by a served rename of the chain's head: a claim beginning
/// after both returned answers `a`, though the listing, having passed the head before they
/// landed, left `a` out.
#[test]
fn a_claim_after_a_create_and_a_rename_landing_during_the_listing_finds_the_created() {
    let raced = Arc::new(AtomicUsize::new(0));
    let scenario = Scenario::<ListingContext> {
        name: "h9: a claim after a create and a rename landing during the listing",
        setup: Box::new(|| Box::pin(ListingContext::new())),
        ops: vec![
            Box::new(|c: Arc<ListingContext>| {
                Box::pin(async move {
                    let before = c.done.get();
                    let mut children = listing(&c.tree).await;
                    let listed = children.holds(hash_string("a")).is_some();
                    let done = c.done.get();
                    c.raced.set(!before && done && !listed).expect("one claim");
                    let claimed = claim(&c.tree, &mut children, "a").await;
                    if done {
                        assert_eq!(
                            claimed,
                            c.created.get().copied(),
                            "a claim beginning after 'd/a' was created answers it"
                        );
                    }
                })
            }),
            Box::new(|c: Arc<ListingContext>| {
                Box::pin(async move {
                    assert!(create_a(&c.tree, &c.created).await, "the create attaches");
                    assert!(
                        ops::served_rename(&c.tree, c.head, c.tree.id("d"), "renamed").await,
                        "the rename moves the head"
                    );
                    c.done.set();
                })
            }),
        ],
        cancellable: vec![],
        check: counting_raced(&raced, |c: &ListingContext| &c.raced),
    };
    explore_racing(
        scenario,
        None,
        (
            &raced,
            "the create and the rename land during the listing, 'a' left out of it",
        ),
    );
}

/// A tree whose directory `d` holds `x` and `h`, the chain's head, and the other of the two,
/// with its name.
async fn head_and_other() -> (Tree, NodeID, (NodeID, &'static str)) {
    let tree = Tree::new(&["d/x", "d/h"]).await;
    let head = tree
        .state
        .node_children(tree.repository.clone(), tree.id("d"))
        .await
        .expect("d's children")[0];
    let other = if head == tree.id("d/x") {
        (tree.id("d/h"), "h")
    } else {
        (tree.id("d/x"), "x")
    };
    (tree, head, other)
}

/// The stage's claims of two names sharing a hash, differing in case, as the file system can
/// list beside each other: the second, finding the listed child claimed and nothing added since,
/// answers the child the first claimed, from the listing.
#[tokio::test]
async fn a_second_claim_of_a_hash_answers_the_listed_child_the_first_claimed() {
    crate::in_context(async {
        let (tree, _, (other, name)) = head_and_other().await;
        let mut children = listing(&tree).await;
        assert_eq!(claim(&tree, &mut children, name).await, Some(other));
        assert_eq!(
            claim(&tree, &mut children, &name.to_uppercase()).await,
            Some(other),
            "the listing answers the claimed child for the name's other case"
        );
    })
    .await;
}

/// The stage's claim of a listed child's new name after a rename moved it to the head, ahead of
/// the listing's head: the search's walk meets it, and the claim takes it out of the listing,
/// which holds it under its old name, so the pass over what nothing claimed leaves it.
#[tokio::test]
async fn a_claim_of_a_renamed_listed_childs_new_name_claims_it_in_the_listing() {
    crate::in_context(async {
        let (tree, _, (other, _)) = head_and_other().await;
        let mut children = listing(&tree).await;
        assert!(
            ops::served_rename(&tree, other, tree.id("d"), "renamed").await,
            "the rename moves the child"
        );
        assert_eq!(claim(&tree, &mut children, "renamed").await, Some(other));
        assert!(
            !children.unclaimed().any(|id| id == other),
            "the renamed child is claimed in the listing"
        );
    })
    .await;
}

/// The stage's claims of two names sharing a hash in a listing without a mark, as a link's or a
/// file's has: the second answers the child the first claimed, from the listing alone.
#[tokio::test]
async fn a_listing_without_a_mark_answers_a_second_claim_from_itself() {
    crate::in_context(async {
        let (tree, _, (other, name)) = head_and_other().await;
        let (listed, _) = tree
            .state
            .node_children_with_name_hash_marked(tree.repository.clone(), tree.id("d"))
            .await
            .expect("the listing");
        let mut children = DirectoryChildren::new(listed, None);
        assert_eq!(claim(&tree, &mut children, name).await, Some(other));
        assert_eq!(
            claim(&tree, &mut children, &name.to_uppercase()).await,
            Some(other)
        );
    })
    .await;
}

/// A checked create of `d/a`: whether it attached, its node kept in `created` where it did. Beside
/// a stage's claims that added the name first, beyond the contract, it finds the name held.
async fn create_a(tree: &Tree, created: &OnceLock<NodeID>) -> bool {
    let Some(id) = ops::attached(ops::checked_create(tree, tree.id("d"), "a").await) else {
        return false;
    };
    created.set(id).expect("one create");
    true
}

/// [`a_claim_after_a_create_and_a_rename_of_the_listing_head_finds_the_created`] through the
/// stage's claims (`STAGE_CLAIM`), modelled: claims of `a` and of `renamed` in one call, the model
/// holding each read by the call's own mark to the directory within it. A claim beginning after
/// the create answers `a`; one beginning after the rename, landing between the listing and the
/// claims or at the stage's window after the claim of `a`, answers the head and claims it in the
/// listing too. The create and the rename find what the claims added first.
#[test]
fn stage_claims_after_a_create_and_a_rename_of_the_listing_head_find_the_created() {
    let (raced, staged) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let counted = staged.clone();
    let scenario = Scenario::<HeadContext> {
        name: "h9: stage claims beside a create and a rename of the listing's head",
        setup: Box::new(|| Box::pin(HeadContext::new())),
        ops: vec![
            Box::new(move |c: Arc<HeadContext>| {
                let staged = counted.clone();
                Box::pin(async move {
                    let before = (c.created_flag.get(), c.renamed.get());
                    let (listed, first) = (OnceLock::new(), OnceLock::new());
                    let between = async {
                        let landed = (c.created_flag.get(), c.renamed.get());
                        let head_first = landed == (true, true) && c.head_first().await;
                        listed.set((landed, head_first)).expect("one listing");
                    };
                    let after = |index: usize, _| {
                        if index == 0 {
                            first.set(c.renamed.get()).expect("one first claim");
                        }
                        async {}
                    };
                    let entries = [("a", true), ("renamed", true)];
                    let claimed = ops::stage_claim_windowed(
                        &c.tree,
                        c.tree.id("d"),
                        &entries,
                        between,
                        after,
                    );
                    let claimed = claimed.await.expect("the claims run");
                    let ((created, renamed), head_first) =
                        *listed.get().expect("the listing's window ran");
                    let renamed_first = *first.get().expect("the first claim's window ran");
                    let a = c.created.get().copied();
                    let unlisted = a.is_some_and(|a| !claimed.listed.contains(&a));
                    c.raced
                        .set(
                            before == (false, false)
                                && created
                                && renamed
                                && head_first
                                && unlisted,
                        )
                        .expect("one claim");
                    // The rename landed at the stage's window after `a`'s claim from the listing,
                    // the one place between the two windows where the claims let a writer in.
                    if !renamed && renamed_first && claimed.listed.contains(&claimed.nodes[0]) {
                        staged.fetch_add(1, Ordering::Relaxed);
                    }
                    if created {
                        assert_eq!(
                            Some(claimed.nodes[0]),
                            a,
                            "a claim beginning after 'd/a' was created answers it"
                        );
                    }
                    if renamed_first {
                        assert_eq!(
                            claimed.nodes[1], c.head,
                            "a claim beginning after the rename answers the head"
                        );
                        assert!(
                            !claimed.unclaimed.contains(&c.head),
                            "the head the claim answered is claimed in the listing"
                        );
                    }
                })
            }),
            Box::new(|c: Arc<HeadContext>| {
                Box::pin(async move {
                    if create_a(&c.tree, &c.created).await {
                        c.created_flag.set();
                    }
                })
            }),
            Box::new(|c: Arc<HeadContext>| Box::pin(async move { c.rename_head().await })),
        ],
        cancellable: vec![],
        check: counting_raced(&raced, |c: &HeadContext| &c.raced),
    };
    explore_racing(
        scenario,
        Some(2),
        (
            &raced,
            "the create and then the rename land between the listing and the claims, 'a' not in it",
        ),
    );
    assert!(
        staged.load(Ordering::Relaxed) > 0,
        "no run had the rename land at the stage's window between the claims"
    );
}

/// [`a_claim_after_a_create_and_a_rename_landing_during_the_listing_finds_the_created`] through the
/// stage's claims (`STAGE_CLAIM`), modelled: the listing and the claim of `a` in one call, its
/// chain across two blocks; a claim beginning after the create and the rename returned answers `a`,
/// though the listing, having passed the head before they landed, left it out; within five
/// preemptions, the race needing one, though the claims' own add of `a` multiplies the
/// interleavings. The claims' window follows the scheduling point after the listing, so its race
/// counts the writes landing during the listing or after it; a mark whose count is read at the
/// listing's end fails only where they land during it.
#[test]
fn stage_claims_after_a_create_and_a_rename_landing_during_the_listing_find_the_created() {
    let raced = Arc::new(AtomicUsize::new(0));
    let scenario = Scenario::<ListingContext> {
        name: "h9: stage claims after a create and a rename landing during the listing",
        setup: Box::new(|| Box::pin(ListingContext::new())),
        ops: vec![
            Box::new(|c: Arc<ListingContext>| {
                Box::pin(async move {
                    let before = c.done.get();
                    let done = OnceLock::new();
                    let between = async {
                        done.set(c.done.get()).expect("one listing");
                    };
                    let claimed =
                        ops::stage_claim_waiting(&c.tree, c.tree.id("d"), &[("a", true)], between);
                    let claimed = claimed.await.expect("the claims run");
                    let done = *done.get().expect("the listing's window ran");
                    let a = c.created.get().copied();
                    let unlisted = a.is_some_and(|a| !claimed.listed.contains(&a));
                    c.raced.set(!before && done && unlisted).expect("one claim");
                    if done {
                        assert_eq!(
                            Some(claimed.nodes[0]),
                            a,
                            "a claim beginning after 'd/a' was created answers it"
                        );
                    }
                })
            }),
            Box::new(|c: Arc<ListingContext>| {
                Box::pin(async move {
                    if create_a(&c.tree, &c.created).await {
                        assert!(
                            ops::served_rename(&c.tree, c.head, c.tree.id("d"), "renamed").await,
                            "the rename moves the head"
                        );
                        c.done.set();
                    }
                })
            }),
        ],
        cancellable: vec![],
        check: counting_raced(&raced, |c: &ListingContext| &c.raced),
    };
    explore_racing(
        scenario,
        Some(5),
        (
            &raced,
            "the create and the rename land after the listing began, 'a' left out of it",
        ),
    );
}

/// The tree [`head_and_other`] builds, with the child of `d` that does not head its chain and its
/// name.
struct OtherContext {
    tree: Tree,
    other: (NodeID, &'static str),
}

impl Modelled for OtherContext {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }
}

/// One task's stage claims in `d` of the tree [`head_and_other`] builds, modelled: `claims` makes
/// them and asserts what they answered.
fn sequential_stage_claims(name: &'static str, claims: Op<OtherContext>) {
    let scenario = Scenario::<OtherContext> {
        name,
        setup: Box::new(|| {
            Box::pin(async {
                let (tree, _, other) = head_and_other().await;
                OtherContext { tree, other }
            })
        }),
        ops: vec![claims],
        cancellable: vec![],
        check: Box::new(|_| Box::pin(async {})),
    };
    explore(scenario, None);
}

/// [`a_second_claim_of_a_hash_answers_the_listed_child_the_first_claimed`] through the stage's
/// claims, modelled: the claim of the other case of a listed name answers the child the first
/// claimed, from the listing, adding nothing.
#[test]
fn stage_claims_of_two_names_sharing_a_hash_answer_the_listed_child_twice() {
    sequential_stage_claims(
        "h9: stage claims of two names sharing a hash",
        Box::new(|c: Arc<OtherContext>| {
            Box::pin(async move {
                let (other, name) = c.other;
                let upper = name.to_uppercase();
                let entries = [(name, true), (upper.as_str(), true)];
                let claimed = ops::stage_claim(&c.tree, c.tree.id("d"), &entries).await;
                assert_eq!(claimed.expect("the claims run").nodes, [other, other]);
            })
        }),
    );
}

/// [`a_claim_of_a_renamed_listed_childs_new_name_claims_it_in_the_listing`] through the stage's
/// claims, modelled: a served rename in the claims' window, after their listing, moves the listed
/// child to the head under a new name; the claim of that name finds it ahead of the mark's head
/// and, the mark finding the reattach, claims it in the listing too.
#[test]
fn a_stage_claim_of_a_renamed_listed_childs_new_name_claims_it_in_the_listing() {
    sequential_stage_claims(
        "h9: a stage claim of a renamed listed child's new name",
        Box::new(|c: Arc<OtherContext>| {
            Box::pin(async move {
                let (other, d) = (c.other.0, c.tree.id("d"));
                let renamed = async {
                    assert!(
                        ops::served_rename(&c.tree, other, d, "renamed").await,
                        "the rename moves the child"
                    );
                };
                let claimed = ops::stage_claim_waiting(&c.tree, d, &[("renamed", true)], renamed);
                let claimed = claimed.await.expect("the claims run");
                assert_eq!(claimed.nodes, [other]);
                assert!(
                    !claimed.unclaimed.contains(&other),
                    "the renamed child is claimed in the listing"
                );
            })
        }),
    );
}
