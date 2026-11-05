// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The model's rules on a call's outcome, judged as the replay passes the call's end: an error only
//! where a read failed in it, a drop only of a kind its production caller may drop or where its run
//! admits one, a self-report only where its run admits one, counted, a `NeedLoad`
//! only where it met a block out of memory, its guards closed within it, and its answer held to its
//! own records and to its State: an attach's node published by it, a held name answered with no
//! publish and its holder live at some `seq` of its interval, a reattach's one move, a detach's
//! unlink, a discard's frees as it counts them, a served mark's target admitting a served change at
//! some `seq` of its interval, a done answer leaving on its target what its op noted, once its own
//! last write there applied or, with none, at some `seq` of its interval, a move's or a rename's
//! its one `Move` or `Rename` as called; a call left part way, dropped or failed, no more than its
//! done answer makes, a move's or a rename's one `Move` or `Rename` as called, a detach's one
//! unlink, of its target; each refusal's condition holding at some `seq` of its interval, whether a
//! record or a read outside every section decided it, and a refusal its section's record alone
//! decides, an in-place change declined or a delete's level refused, made with that record among
//! the call's own; whatever it answered, each node it passed over as taken gone at some `seq` from
//! where it found it, and each find it looked up held from where it began: a node found carrying
//! the name at some `seq`, a miss where no one node carried it throughout, either node of a
//! replacing reattach counting (C7); each walk of a directory's children it made held to the
//! directory from its head read, C6 among its rules, no node yielded twice unless it said it may
//! repeat; and each read by a listing's `ChainMark` held to the marked listing it names, from the
//! listing's head read: a node a search found carrying the name, a miss over the nodes that joined
//! since, and, under the gate, over a count moved past the mark's while its head stood, a stale
//! search and a reattach found by the count, a reattach not found, under the gate, by it as the
//! read began, a read by the mark of the call's own marked listing so within its own interval, by
//! the prepends the call counted by then; each name read held to its one name record, a comparison
//! by it, and a path read to its walk up its parents and the names it read; and each has-children
//! read held to its directory from its listing's head read: a child found carrying the flags a
//! node of the directory at some `seq` since, carrying them then or later, none found a violation
//! where a child carried them there throughout, unless a move within the directory repositioned it
//! since; and each dirty-paths collection held to what it named, one hop at a time from its root,
//! each dirty link it named to the paths the revision of the pin it read holds below it, and to its
//! root from its head read: no node left out that stood in place, named and dirty below dirty
//! directories throughout, nor a path below such a dirty link.

use std::collections::BTreeMap;

use lore_base::types::Hash;
use lore_revision::node::BLOCK_NODE_COUNT;
use lore_revision::node::INVALID_NODE;
use lore_revision::node::Node;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::NodeIDExt;
use lore_revision::node::NodeLink;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::AddedSince;
use lore_revision::state::Content;
use lore_revision::state::REATTACH_CHANGES;
use lore_revision::state::node::hooks::log::Arm;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::GuardEvent;
use lore_revision::state::node::hooks::log::Name;
use lore_revision::state::node::hooks::log::Reason;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;

use super::Admits;
use super::Model;
use super::Replay;
use super::Slot;
use super::Snapshot;
use super::StateModel;
use super::Status;
use super::Unchained;
use super::Violation;
use super::fields::Fields;
use crate::harness::catalogue;
use crate::harness::catalogue::call::Answer;
use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Leaves;
use crate::harness::catalogue::call::Lookup;
use crate::harness::catalogue::call::Outcome;
use crate::harness::catalogue::call::Over;
use crate::harness::catalogue::call::Refusal;
use crate::harness::catalogue::call::Repeats;
use crate::harness::catalogue::call::SelfReported;
use crate::harness::catalogue::call::Spawn;
use crate::harness::catalogue::call::Walk;
use crate::harness::catalogue::call::Yielded;
use crate::harness::catalogue::call::path_spelling;
use crate::harness::catalogue::call::spelling;
use crate::harness::catalogue::call::spelling_below;
use crate::harness::catalogue::ops::Flagged;
use crate::harness::faults::INJECTED_AS;

/// What a call's records tell of its answer, as the replay goes.
#[derive(Clone, Debug, Default)]
pub(super) struct Answered {
    /// Its publishes, and whether one published the node it answered.
    published: u32,
    published_answer: bool,
    /// Its unlinks, and whether one unlinked its target.
    unlinked: u32,
    unlinked_target: bool,
    /// Its moves and renames, and the last of them: its node, the directory it left it in, the name
    /// hash and the spelling it gave it, the spelling where the record names one.
    moves: u32,
    renames: u32,
    moved: Option<(NodeID, NodeID, u64, Option<u64>)>,
    /// The files and the directories its discards freed, in their frees' sections and in their
    /// marks', which free a file below a directory and a node with nothing below it; by the `File`
    /// flag, links counted as directories, as `DiscardCounts` counts them.
    freed: (u64, u64),
    /// Its guards armed and not yet finished or dropped.
    armed: u32,
    /// Which of its claims held at some `seq` of its interval ([`claims`]): its answer's, bit 0,
    /// then each node it passed over as taken's, from the `seq` it found it at, then each
    /// lookup's, from the `seq` it began at, a read by its own listing's mark to the `seq` it
    /// ended at, then each walk's checks' ([`walk_checks`]), from its head read or its start, then,
    /// for a search since a listing's mark answering none, the mark's head kept's, from its begin,
    /// then, for each search since its own listing's mark answering none, the mark's head kept's,
    /// within the search, then, for a resolution, each hop's after its first, made from no link,
    /// from where the hop began.
    pub(super) seen: u32,
    /// Whether its answer's claim held once its last own write on its target applied, where it
    /// wrote one there ([`Calls::after_writes`](super::calls::Calls::after_writes)).
    pub(super) after_write: Option<bool>,
    /// Whether it met a block not in memory where it reads only those ([`Arm::NeedLoad`]).
    need_load: bool,
    /// The reasons of the refusals its sections recorded, in order.
    refused: Vec<Reason>,
    /// Each `Head` it made of a directory it walks, a has-children read lists, or a dirty-paths
    /// collection lists, in order.
    heads: Vec<HeadRead>,
    /// Each parent word it read, of a node, and each name it read through the State's name reads,
    /// in order, for a name or a path read ([`Effect::Parent`], [`Effect::Named`]).
    parents: Vec<(NodeID, NodeID)>,
    names: Vec<(NodeID, Name)>,
    /// A resolution's hops so far, and the `seq` of its last, after which it read the pin of a link
    /// it crossed.
    hopped: usize,
    crossed_at: Option<u64>,
    /// Each child a dirty-paths collection named, in order, and whether its root, as the model held
    /// it at its head read, was a dirty add with no child but `Discarded` ones, which its walk
    /// passes.
    namings: Vec<Naming>,
    root_empty_add: bool,
    /// Whether the listing it names, for a read by a listing's mark, was a marked listing of its
    /// directory on its State whose head read the replay had met as it began, its mark judging
    /// its claims ([`ByMark`]).
    pub(super) marked: bool,
}

/// A child a dirty-paths collection named: below its call's head read `head`, by its index, the
/// last before it, which read `under`; by `name`; and what the model held as it read the name: its
/// words, whether it stood in `under` or had joined where it stood since `under`'s head read, as a
/// child listed may move after, and, for a link a `Word` wrote since that head read, after which
/// the collection read its words, each pin it held since, its current one last, one of which it
/// read; none where the words are those it read.
#[derive(Clone, Debug)]
struct Naming {
    node: NodeID,
    name: Name,
    head: Option<usize>,
    under: NodeID,
    held: Option<Node>,
    in_place: bool,
    pins: Vec<NodeLink>,
}

/// A directory's chain as a walk's, a has-children read's or a dirty-paths collection's listing's
/// `Head` read it: the directory, the record's `seq`, the count, the node heading the chain then, by
/// the model, and for a collection how many children it had named before it. A marked listing's is
/// its `ChainMark`'s, as a stage's claim's listing's is ([`lists_marked`]).
#[derive(Clone, Copy, Debug)]
struct HeadRead {
    dir: NodeID,
    at: u64,
    changes: u32,
    head: Option<NodeID>,
    named: usize,
}

/// What a read by a listing's mark is judged by: the `seq` of the marked listing's head read, the
/// count it read with the prepends the mark's holder counted, and the node heading the chain then.
#[derive(Clone, Copy, Debug)]
pub(super) struct ByMark {
    since: u64,
    base: u32,
    head: Option<NodeID>,
}

/// What `call`, a read by the mark of `listing`, is judged by, where it reads by a mark and
/// `listing` is a marked listing of the read's directory on its State that read the directory's
/// head, by what its records told (`answered`).
pub(super) fn by_mark_of(call: &Call, (listing, answered): (&Call, &Answered)) -> Option<ByMark> {
    if !reads_by_mark(call)
        || !std::ptr::eq(listing.kind, &catalogue::LISTED_MARKED)
        || listing.state != call.state
    {
        return None;
    }
    let walk = (listing.walks.first()).filter(|walk| Some(walk.dir) == call.target)?;
    head_read(walk, answered).map(|read| ByMark::of(read, call.prepends))
}

impl ByMark {
    /// The mark a listing's head read `read` made, with `prepends` of its holder's own counted.
    fn of(read: &HeadRead, prepends: u32) -> Self {
        Self {
            since: read.at,
            base: read.changes.wrapping_add(prepends),
            head: read.head,
        }
    }
}

/// Whether `call` is a name or a path read, its answer judged by its parent and name records.
fn reads_names(call: &Call) -> bool {
    matches!(
        call.outcome,
        Outcome::Answer(Answer::Name(_) | Answer::NameIs(_) | Answer::Path(_))
    )
}

/// Whether `call` is a dirty-paths collection, its paths judged by its head and name records.
fn collects(call: &Call) -> bool {
    std::ptr::eq(call.kind, &catalogue::DIRTY_PATHS)
}

/// Whether `call` resolves a path entering links on the way ([`Call::names`]).
fn resolves(call: &Call) -> bool {
    std::ptr::eq(call.kind, &catalogue::FIND_LINKED)
}

/// The link a resolution crossed: the node its last hop found with names of its path left, which
/// it entered, as it walks on through any other node.
fn crossed(call: &Call) -> Option<NodeID> {
    if !resolves(call) || call.lookups.len() >= call.names.len() {
        return None;
    }
    let Lookup::Find { found, .. } = *call.lookups.last()? else {
        return None;
    };
    found
}

/// Whether `lookup` is a find that found `answer`: the node, or none.
fn finds(lookup: &Lookup, answer: Option<NodeID>) -> bool {
    matches!(*lookup, Lookup::Find { found, .. } if found == answer)
}

/// How many hops of a resolution ([`resolves`]) but its first made from a node the hop before it
/// found, each held to be no link ([`Past::Unlinked`]).
fn unlinked_hops(call: &Call) -> usize {
    if resolves(call) {
        call.lookups.len().saturating_sub(1)
    } else {
        0
    }
}

/// Each resolution's hops ([`resolves`]), as its State recorded them in its own steps
/// ([`Call::holds`]), a lookup each, begun at its interval's start or just after the hop before it.
/// The records come in `(state, seq)` order, so each call's hops in its order.
pub(super) fn hops_looked_up(calls: &mut [Call], records: &[Record], spawns: &[Spawn]) {
    // Each resolving call, with the `seq` of the last of its hops found so far.
    let mut resolving: Vec<(usize, Option<u64>)> = (0..calls.len())
        .filter(|&at| resolves(&calls[at]))
        .map(|at| (at, None))
        .collect();
    if resolving.is_empty() {
        return;
    }
    for record in records {
        let Effect::Hop { dir, name, found } = record.effect else {
            continue;
        };
        for (at, last) in &mut resolving {
            let call = &mut calls[*at];
            if call.holds(record, spawns) {
                let from = last.map_or(call.begin, |seq| seq + 1);
                call.lookups.push(Lookup::Find {
                    dir,
                    name,
                    found,
                    from,
                });
                *last = Some(record.seq);
                // A record is one call's at most, as the collector holds it.
                break;
            }
        }
    }
}

/// Whether `call` reads by a listing's mark ([`Call::listing`]).
fn reads_by_mark(call: &Call) -> bool {
    std::ptr::eq(call.kind, &catalogue::ADDED_SINCE)
        || std::ptr::eq(call.kind, &catalogue::REATTACHED_SINCE)
}

/// Whether `call` lists with a mark, its head read keeping the chain's head as its `ChainMark`
/// does: a marked listing, or a stage's claim, which reads by the mark of its own listing.
fn lists_marked(call: &Call) -> bool {
    std::ptr::eq(call.kind, &catalogue::LISTED_MARKED)
        || std::ptr::eq(call.kind, &catalogue::STAGE_CLAIM)
}

/// The directory `lookup`, a read by the mark of its call's own marked listing, reads, and the
/// prepends to it the call counted in the mark by then; none for a find.
fn own_mark_of(lookup: &Lookup) -> Option<(NodeID, u32)> {
    match *lookup {
        Lookup::Since { dir, prepends, .. } | Lookup::Reattached { dir, prepends, .. } => {
            Some((dir, prepends))
        }
        Lookup::Find { .. } => None,
    }
}

/// The head read of `call`'s own marked listing of `dir`, its first walk of the directory's, where
/// it lists with a mark ([`lists_marked`]) and its records told the read.
fn own_listing<'a>(call: &Call, dir: NodeID, answered: &'a Answered) -> Option<&'a HeadRead> {
    if !lists_marked(call) {
        return None;
    }
    let walk = call.walks.iter().find(|walk| walk.dir == dir)?;
    head_read(walk, answered)
}

/// The mark `lookup`, a read by the mark of `call`'s own listing, is judged by, and its directory:
/// as the listing's head read made it, with the prepends the call counted by the read; none before
/// that read.
fn own_mark(call: &Call, lookup: &Lookup, answered: &Answered) -> Option<(ByMark, NodeID)> {
    let (dir, prepends) = own_mark_of(lookup)?;
    let listed = own_listing(call, dir, answered)?;
    Some((ByMark::of(listed, prepends), dir))
}

/// Whether `lookup` is a search since its call's own listing's mark answering none, which claims
/// the mark's head kept within it ([`Past::OwnKept`]).
fn keeps_own_head(lookup: &Lookup) -> bool {
    matches!(
        lookup,
        Lookup::Since {
            searched: AddedSince::Absent,
            ..
        }
    )
}

/// Whether `past` is judged under the gate alone: a search's mark's head kept and a mark found not
/// reattached, each by a count read with no lock, which is the model's as the read began only
/// there.
fn gated_only(past: Past) -> bool {
    matches!(
        past,
        Past::Kept
            | Past::OwnKept(_)
            | Past::Lookup(Lookup::Reattached {
                reattached: false,
                ..
            })
    )
}

/// Whether `call` is a search since a listing's mark answering none, which claims its mark's head
/// kept ([`Past::Kept`]).
fn keeps_head(call: &Call) -> bool {
    std::ptr::eq(call.kind, &catalogue::ADDED_SINCE)
        && call.outcome == Outcome::Answer(Answer::Since(None))
}

/// The most claims a call may make past its answer's, a bit of [`Answered::seen`] each after its
/// answer's: the nodes it passed over as taken, then what it looked up, then its walks' checks
/// ([`walk_checks`]), then a search's mark's head kept, then its searches' own marks' heads kept,
/// then a resolution's hops from no link ([`past`]).
const MOST_CLAIMS: usize = u32::BITS as usize - 1;

/// What a call's answer claims of its State at some `seq` of its interval: a condition the call read
/// before it answered, which another task may change after, or what its done answer leaves on its
/// target, held once its own write there applied; a node it passed over as taken, gone at some
/// `seq` from where it found it; what a find it looked up answered, or a read by the mark of its
/// own listing, within that read; or what a walk of a directory's children it made yielded and
/// passed over. A claim a walk up the parents decides, one level at a time, is read as the walk
/// reads it: over every node the walk may have met by then ([`Claim::holds`]).
#[derive(Clone, Copy, Debug)]
pub(super) enum Claim {
    /// A served mark's target admits a served change below it: not `Discarded`, and the root, or
    /// dirty with no clear pending.
    Admits(NodeID),
    /// A held name's holder, or a node a find or a search since a listing's mark found, carries the
    /// name in the directory: in its chain, not `Discarded`.
    Holds {
        holder: NodeID,
        parent: NodeID,
        name: u64,
    },
    /// Either node, or a directory above either, gone: `Discarded`, out of every chain, or freed.
    Gone([Option<NodeID>; 2]),
    /// The node away from its place: gone, or under another parent or by another name hash.
    Away {
        node: NodeID,
        parent: NodeID,
        name: u64,
    },
    /// A live child of `dir` carrying the hash `name`, other than the node moving there and the
    /// holder it replaces.
    Named {
        dir: NodeID,
        name: u64,
        other: [Option<NodeID>; 2],
    },
    /// The node dirty.
    Dirty(NodeID),
    /// A live child of the directory not staged for deletion.
    Occupied(NodeID),
    /// The node a directory without local content, away from the place it moves to: under `to`
    /// by the hash `name`, where a respell moves any directory.
    NotLocal { node: NodeID, to: NodeID, name: u64 },
    /// The node dead, not a file, or staged for deletion, by its own words.
    Unmodifiable(NodeID),
    /// The node dead or not staged for deletion, or its parent dead or staged for deletion.
    Unrestorable(NodeID),
    /// The directory staged for deletion, or freed: a parent `Discarded` at the add's publish,
    /// whose slot a grab took before the add read it back.
    Unaddable(NodeID),
    /// The node, its parent or the destination dead or staged for deletion; the destination no
    /// directory; the node under it by the hash `name` already; or the destination the node
    /// itself or below it, up its parents.
    Unmovable { node: NodeID, to: NodeID, name: u64 },
    /// The destination no directory, or the node itself or below it, up its parents.
    Inside { node: NodeID, to: NodeID },
    /// No node carried the hash `name` in `dir` at every read so far: seen once the nodes that
    /// carried it at the first read, kept in the claim's reach, each stopped carrying it, the node
    /// a replacing reattach put there carrying on for the one it replaced (C7).
    Missed { dir: NodeID, name: u64 },
    /// What the call's walk, by its index, claims by `check`.
    Walked { walk: usize, check: WalkCheck },
    /// No node that joined the directory, or was repositioned in it, after a listing's mark at
    /// `since` carried the hash `name` there at every read so far: a miss over those nodes alone,
    /// as a search since the mark walks only what lies ahead of the mark's head.
    NotAdded { dir: NodeID, name: u64, since: u64 },
    /// The directory's count moved by a reattach past `base`, its listing's mark's count with the
    /// prepends its holder counted; or, where `or_gone`, the node no longer a directory. The mark
    /// holds the directory's block, so no load restarts the count while it lives.
    Moved {
        dir: NodeID,
        base: u32,
        or_gone: bool,
    },
    /// The directory's count not moved by a reattach past `base`; as a count only grows, read so
    /// where it is read from or at no read.
    Unmoved { dir: NodeID, base: u32 },
    /// A node of the directory at some read so far carrying the flags `flagged` names at a read
    /// since, both from the listing's head read: the nodes a has-children read's listing may have
    /// held, kept in the claim's reach, as it reads each child's words after its listing. By id, so
    /// a slot freed and reused within the read would count, which no caller's contract allows.
    Carried { dir: NodeID, flagged: Flagged },
    /// No child of the directory carried the flags `flagged` names at every read so far from the
    /// listing's head read, at `listed`, but one a `Move` within it repositioned since, which the
    /// listing may pass over: those carrying them at the first read kept in the claim's reach,
    /// each seen once it stopped.
    Uncarried {
        dir: NodeID,
        flagged: Flagged,
        listed: u64,
    },
    /// No node a dirty-paths collection below `root` reports, standing in place, named and dirty
    /// below dirty directories at every read so far from the root's head read, at `listed`
    /// ([`collected_path`]), was left out of its paths, nor a path a dirty link standing so holds
    /// below it in the revision its pin then names ([`left_out`]): those left out at the first read
    /// kept in the claim's reach, each seen once it stopped standing or left nothing out.
    Collected { root: NodeID, listed: u64 },
    /// The mark's head, `head`, out of the directory's chain, or the directory's count not moved by
    /// a reattach past `base`: a search since the mark meets a head still chained, and answers
    /// stale where the count moved by its walk's end.
    Kept {
        dir: NodeID,
        head: Option<NodeID>,
        base: u32,
    },
    /// What a done answer leaves on its target ([`Leaves`]), `to`, `name` and `spelling` the call's
    /// destination, name and spelling, which a place and a name are read by.
    Leaves {
        node: NodeID,
        leaves: Leaves,
        to: Option<NodeID>,
        name: Option<u64>,
        spelling: Option<u64>,
    },
    /// A resolution crossing the link `link` answered as the revision the link's pin names
    /// resolves the names of its path from `rest` on below the node the pin exposes
    /// ([`linked_holds`]).
    Linked { link: NodeID, rest: usize },
    /// What `lookup`, a read by the mark of the call's own marked listing, claims, resolved once
    /// the listing's head read is known, by the mark it made and the prepends the call counted by
    /// the read ([`read`]).
    OwnRead(Lookup),
    /// For `lookup`, a search since the call's own listing's mark answering none, the mark's head
    /// kept ([`Self::Kept`]), resolved as [`Self::OwnRead`] is.
    OwnKept(Lookup),
    /// The node no link: a resolution enters a link it finds with names of its path left, and hops
    /// on only from a directory, or a file, where its find misses. A node's kind stands while its
    /// slot is used.
    Unlinked(NodeID),
}

/// Every claim `call` makes, each with its bit in [`Answered::seen`] and the `seq` it is read from:
/// its answer's, bit 0, from its begin; then those past it ([`past`]), each read to its call's end
/// but a read by the mark of its own listing, read to its own ([`Claim::bound`]). A walk's claims,
/// a has-children read's answer's and a dirty-paths collection's are read from its head read, the
/// collection's its root's, once its `Head` is known, a read by its own listing's mark by that
/// listing's, and a resolution's answer through a link from its last hop ([`read`]). A read by a
/// listing's mark claims by `mark`, and claims nothing without one.
pub(super) fn claims(
    call: &Call,
    mark: Option<ByMark>,
) -> impl Iterator<Item = (u32, u64, Claim)> + '_ {
    let past = past(call).filter_map(move |(bit, past)| {
        Some(match past {
            Past::Taken(node, seq) => (bit, seq, Claim::Gone([Some(node), None])),
            Past::Lookup(lookup) => (bit, lookup.begun(), Claim::looked_up(lookup)),
            Past::Walk(walk, check) => (bit, call.begin, Claim::Walked { walk, check }),
            Past::Kept => {
                let ByMark { head, base, .. } = mark?;
                let dir = call.target?;
                (bit, call.begin, Claim::Kept { dir, head, base })
            }
            Past::OwnKept(lookup) => (bit, lookup.begun(), Claim::OwnKept(lookup)),
            Past::Unlinked(hop) => {
                let lookup = call.lookups[hop];
                let Lookup::Find { dir, .. } = lookup else {
                    return None;
                };
                (bit, lookup.begun(), Claim::Unlinked(dir))
            }
        })
    });
    let answer = Claim::of(call).or_else(|| mark.and_then(|mark| Claim::by_mark(call, mark)));
    answer
        .map(|claim| (0, call.begin, claim))
        .into_iter()
        .chain(past)
}

/// A call's claim past its answer's.
#[derive(Clone, Copy)]
enum Past {
    /// A node it passed over as taken, at the `seq` it found it at.
    Taken(NodeID, u64),
    /// What it looked up.
    Lookup(Lookup),
    /// What its walk, by its index, yielded or passed over, by the check.
    Walk(usize, WalkCheck),
    /// For a search since a listing's mark answering none, the mark's head out of the directory's
    /// chain or the count not moved by a reattach past the mark's, at some read: judged under the
    /// gate alone, where the count the search loads with no lock at its walk's end is the model's
    /// as the call ends, no less than as it began.
    Kept,
    /// For a search since its call's own listing's mark answering none, as [`Past::Kept`] for a
    /// search by a listing's mark, within the search.
    OwnKept(Lookup),
    /// For a resolution, the node its hop, by its index, made from no link ([`Claim::Unlinked`]).
    Unlinked(usize),
}

/// What a walk of a directory's children claims of it from its head read to its call's end, by the
/// children iterators' rules (`StateNodeChildrenIterator`), each read once its `Head` is known
/// ([`read`]); or a walk over a link, of the revision its pin names.
#[derive(Clone, Copy, Debug)]
pub(super) enum WalkCheck {
    /// Each node it yielded stood in the directory, kept by its caller, with the record, the
    /// name's hash and the name it read for it at some `seq` of that span.
    Yields,
    /// It yielded every node in the directory that its caller kept throughout that span, but one a
    /// `Move` within the directory repositioned after its head read, whose `seq` it carries, where
    /// the walk may have repeated. Where no counted change of the directory followed its head read,
    /// no node joined, left or moved, so this is C6: every node of the chain then, not `Discarded`,
    /// its caller kept throughout, unless a record since marked it `Discarded` in place.
    Throughout(u64),
    /// Over a link, it yielded the children, as a set, that a load of the revision a pin the link
    /// held at some `seq` of its call names holds below the node the pin exposes, kept by its
    /// caller, with the record, the name's hash and the name it read for each: the read loads
    /// that State afresh, private to it, so the revision's snapshot is what it walked.
    Pinned,
    /// Over a node that takes no children, the node a file, or a link, at some `seq` of its call.
    Childless,
}

/// What a walk claims, by what it walked ([`Walk::over`]): a chain's yields and what it passed
/// over; a link's pinned children; and of a node that takes no children, its kind, its walk
/// yielding none ([`judged`]).
fn walk_checks(over: Over) -> &'static [WalkCheck] {
    match over {
        Over::Chain => &[WalkCheck::Yields, WalkCheck::Throughout(0)],
        Over::Link => &[WalkCheck::Pinned],
        Over::Leaf => &[WalkCheck::Childless],
    }
}

/// The claims `call` makes past its answer's, each with its bit: each node it passed over as taken,
/// then each lookup, then each walk's checks, then, for a search since a listing's mark answering
/// none, its mark's head kept, then, for each search since its own listing's mark answering none,
/// the mark's head kept, then, for a resolution, each hop but its first made from no link,
/// [`MOST_CLAIMS`] at most.
fn past(call: &Call) -> impl Iterator<Item = (u32, Past)> + '_ {
    let taken = call.taken.iter().map(|&(node, seq)| Past::Taken(node, seq));
    let lookups = call.lookups.iter().copied().map(Past::Lookup);
    let walks = call.walks.iter().enumerate().flat_map(|(index, walk)| {
        (walk_checks(walk.over).iter()).map(move |&check| Past::Walk(index, check))
    });
    let kept = keeps_head(call).then_some(Past::Kept);
    let own_kept = (call.lookups.iter().copied())
        .filter(keeps_own_head)
        .map(Past::OwnKept);
    let unlinked = (1..unlinked_hops(call) + 1).map(Past::Unlinked);
    let past = taken
        .chain(lookups)
        .chain(walks)
        .chain(kept)
        .chain(own_kept)
        .chain(unlinked);
    (1..).zip(past.take(MOST_CLAIMS))
}

/// The `seq` `claim` of `call` is read from and the claim as read, by what its records told so far:
/// `from`, but for a walk's claim its head read's `seq`, for a has-children read's its listing's
/// first head read of the directory, and for a collection's its first, of its root, none before
/// it; but for a walk over a link or a node that takes no children, which reads no head, from the
/// walk's start; for a resolution's answer through a link, from its last hop, once known; and for
/// a read by the mark of the call's own listing, from `from` by the mark its listing's head read
/// made, none before that read.
pub(super) fn read(
    call: &Call,
    claim: Claim,
    from: u64,
    answered: &Answered,
) -> Option<(u64, Claim)> {
    let listing = |dir| {
        (answered.heads.iter())
            .find(|head| head.dir == dir)
            .map(|head| head.at)
    };
    match claim {
        Claim::Walked {
            walk,
            check: WalkCheck::Pinned | WalkCheck::Childless,
        } => Some((call.walks[walk].from, claim)),
        Claim::Walked { walk, check } => head_read(&call.walks[walk], answered).map(|head| {
            let check = match check {
                WalkCheck::Throughout(_) => WalkCheck::Throughout(head.at),
                check => check,
            };
            (head.at, Claim::Walked { walk, check })
        }),
        Claim::Carried { dir, .. } => listing(dir).map(|listed| (listed, claim)),
        Claim::Uncarried { dir, flagged, .. } => listing(dir).map(|listed| {
            let claim = Claim::Uncarried {
                dir,
                flagged,
                listed,
            };
            (listed, claim)
        }),
        Claim::Collected { root, .. } => {
            listing(root).map(|listed| (listed, Claim::Collected { root, listed }))
        }
        // The product reads the link's pin after the hop that found it recorded, under the link's
        // block's read lock, which every repin before that hop holds as it writes.
        Claim::Linked { .. } => answered.crossed_at.map(|at| (at, claim)),
        Claim::OwnRead(lookup) => {
            let (mark, _) = own_mark(call, &lookup, answered)?;
            Claim::by_own_mark(lookup, mark).map(|claim| (from, claim))
        }
        Claim::OwnKept(lookup) => {
            let (ByMark { head, base, .. }, dir) = own_mark(call, &lookup, answered)?;
            Some((from, Claim::Kept { dir, head, base }))
        }
        claim => Some((from, claim)),
    }
}

/// `walk`'s read of its directory's head: its call's first `Head` of the directory from where the
/// walk began.
fn head_read<'a>(walk: &Walk, answered: &'a Answered) -> Option<&'a HeadRead> {
    (answered.heads.iter()).find(|head| head.dir == walk.dir && head.at >= walk.from)
}

impl Claim {
    /// What `call`'s answer claims, if anything.
    pub(super) fn of(call: &Call) -> Option<Self> {
        if let (Some(link), Outcome::Answer(_)) = (crossed(call), &call.outcome) {
            let rest = call.lookups.len();
            return Some(Self::Linked { link, rest });
        }
        let target = call.target;
        match call.outcome {
            Outcome::Done(Answer::None) if std::ptr::eq(call.kind, &catalogue::SERVED_MARK) => {
                target.map(Self::Admits)
            }
            Outcome::Done(Answer::Held(holder)) => {
                target.zip(call.name).map(|(parent, name)| Self::Holds {
                    holder,
                    parent,
                    name,
                })
            }
            // A served rename's one `Move` put its node where it named.
            Outcome::Done(Answer::Moved { .. }) => {
                target.zip(call.destination).map(|(node, to)| Self::Leaves {
                    node,
                    leaves: Leaves::Placed,
                    to: Some(to),
                    name: call.name,
                    spelling: call.spelling,
                })
            }
            Outcome::Done(_) => target.zip(call.leaves).map(|(node, leaves)| Self::Leaves {
                node,
                leaves,
                to: call.destination,
                name: call.name,
                spelling: call.spelling,
            }),
            Outcome::Refused(reason) => Self::refused(reason, call),
            // Read from the listing's head read ([`read`]).
            Outcome::Answer(Answer::Has(has)) => {
                target.zip(Flagged::of(call.kind)).map(|(dir, flagged)| {
                    if has {
                        Self::Carried { dir, flagged }
                    } else {
                        Self::Uncarried {
                            dir,
                            flagged,
                            listed: 0,
                        }
                    }
                })
            }
            // Read from its root's head read, as a has-children read's.
            Outcome::Answer(Answer::Collected) => {
                target.map(|root| Self::Collected { root, listed: 0 })
            }
            _ => None,
        }
    }

    /// What `call`'s answer, a read by a listing's mark, claims, judged by `mark`.
    fn by_mark(call: &Call, mark: ByMark) -> Option<Self> {
        let Outcome::Answer(answer) = call.outcome else {
            return None;
        };
        Self::answered_by_mark((call.target?, call.name), answer, mark)
    }

    /// What `lookup`, a read by the mark of its call's own listing, claims, judged by `mark`: as a
    /// read by a listing's mark answering as it did.
    fn by_own_mark(lookup: Lookup, mark: ByMark) -> Option<Self> {
        match lookup {
            Lookup::Since {
                dir,
                name,
                searched,
                ..
            } => Self::answered_by_mark((dir, Some(name)), Answer::since(searched), mark),
            Lookup::Reattached {
                dir, reattached, ..
            } => Self::answered_by_mark((dir, None), Answer::Reattached(reattached), mark),
            Lookup::Find { .. } => None,
        }
    }

    /// What a read by a listing's mark of `dir`, searching for the hash `name` where it searches,
    /// claims by answering `answer`, judged by `mark`.
    fn answered_by_mark(
        (dir, name): (NodeID, Option<u64>),
        answer: Answer,
        mark: ByMark,
    ) -> Option<Self> {
        let ByMark { since, base, .. } = mark;
        Some(match answer {
            Answer::Since(Some(holder)) => Self::Holds {
                holder,
                parent: dir,
                name: name?,
            },
            Answer::Since(None) => Self::NotAdded {
                dir,
                name: name?,
                since,
            },
            Answer::Stale => Self::Moved {
                dir,
                base,
                or_gone: true,
            },
            Answer::Reattached(true) => Self::Moved {
                dir,
                base,
                or_gone: false,
            },
            Answer::Reattached(false) => Self::Unmoved { dir, base },
            _ => return None,
        })
    }

    /// What `lookup` claims: a node found carried the hash in the directory, as a held name's
    /// holder does; a miss, that no node carried it there throughout; a read by the mark of the
    /// call's own listing, what that read claims once its listing's head read is known.
    fn looked_up(lookup: Lookup) -> Self {
        match lookup {
            Lookup::Find {
                dir,
                name,
                found: Some(holder),
                ..
            } => Self::Holds {
                holder,
                parent: dir,
                name,
            },
            Lookup::Find {
                dir,
                name,
                found: None,
                ..
            } => Self::Missed { dir, name },
            Lookup::Since { .. } | Lookup::Reattached { .. } => Self::OwnRead(lookup),
        }
    }

    /// The `seq` the claim is read by, where it is bounded short of its call's end: a read by the
    /// mark of the call's own listing, by its own end.
    pub(super) fn bound(self) -> Option<u64> {
        match self {
            Self::OwnRead(lookup) | Self::OwnKept(lookup) => lookup.ended(),
            _ => None,
        }
    }

    /// Whether a refusal for `reason` claims a condition the model reads: all but those a
    /// section's record decides, judged there, and the settle's but its `NotFound`, which wait
    /// until the settle resolves and retries.
    fn claimed(reason: Refusal) -> bool {
        matches!(
            reason,
            Refusal::NotFound
                | Refusal::Exists
                | Refusal::IfClean
                | Refusal::NotEmpty
                | Refusal::NotLocal
                | Refusal::Modify
                | Refusal::Restore
                | Refusal::Add
                | Refusal::Move
                | Refusal::Subtree
        )
    }

    /// The condition `call`'s refusal for `reason` claims, where it claims one ([`Self::claimed`])
    /// and the call carries what it is read by.
    fn refused(reason: Refusal, call: &Call) -> Option<Self> {
        let (target, destination) = (call.target?, call.destination);
        Some(match reason {
            Refusal::NotFound => match call.place {
                Some((parent, name)) => Self::Away {
                    node: target,
                    parent,
                    name,
                },
                None => Self::Gone([Some(target), destination]),
            },
            Refusal::Exists => Self::Named {
                dir: destination.unwrap_or(target),
                name: call.name?,
                other: [destination.map(|_| target), call.holder],
            },
            Refusal::IfClean => Self::Dirty(target),
            Refusal::NotEmpty => Self::Occupied(target),
            Refusal::NotLocal => Self::NotLocal {
                node: target,
                to: destination?,
                name: call.name?,
            },
            Refusal::Modify => Self::Unmodifiable(target),
            Refusal::Restore => Self::Unrestorable(target),
            Refusal::Add => Self::Unaddable(target),
            Refusal::Move => Self::Unmovable {
                node: target,
                to: destination?,
                name: call.name?,
            },
            Refusal::Subtree => Self::Inside {
                node: target,
                to: destination?,
            },
            _ => return None,
        })
    }

    /// Whether the claim holds of `state` as the model has it. `reach` is what the claim keeps
    /// between reads. A claim a walk up the parents decides keeps the nodes the walk may have met so
    /// far, closed here up their parent words as `state` has them: a walk reads one level at a
    /// time, so a decision over several reads need not match any one `seq`, only each level as it
    /// was when read. A miss keeps the nodes carrying the name since its first read; a walk of a
    /// directory's children, what holds its claim back ([`walk_holds`]); a has-children read that
    /// found a child, the nodes of the directory since its head read, and one that found none,
    /// those carrying the flags there since it; a dirty-paths collection, the nodes it left out
    /// since its root's head read. A walk over a link and a collection's dirty links read the
    /// snapshots of the revisions loaded in the run, `revisions`, by revision.
    pub(super) fn holds(
        self,
        call: &Call,
        (state, revisions): (&StateModel, &BTreeMap<Hash, Snapshot>),
        reach: &mut Vec<NodeID>,
    ) -> bool {
        match self {
            Self::Walked { walk, check } => {
                walk_holds(&call.walks[walk], check, (state, revisions), reach)
            }
            Self::Linked { link, rest } => linked_holds(call, (state, revisions), (link, rest)),
            Self::Unlinked(node) => !state.slot(node).is_some_and(|slot| slot.record.is_link()),
            Self::OwnRead(_) | Self::OwnKept(_) => unreachable!("resolved by read() first"),
            Self::Admits(node) => state.slot(node).is_some_and(|slot| {
                !slot.is_discarded()
                    && (node == ROOT_NODE || (slot.record.is_dirty() && !slot.clear_pending))
            }),
            Self::Holds {
                holder,
                parent,
                name,
            } => carries(state, holder, (parent, name)),
            Self::Gone(nodes) => state.reaches_gone(nodes.into_iter().flatten(), reach),
            Self::Away { node, parent, name } => {
                state.slot(node).is_some_and(|slot| {
                    (slot.record.parent, slot.record.name_hash) != (parent, name)
                }) || state.reaches_gone([node].into_iter(), reach)
            }
            Self::Named { dir, name, other } => state.children(dir).any(|(child, word)| {
                !other.contains(&Some(child)) && word.name_hash == name && !word.is_discarded()
            }),
            Self::Missed { dir, name } => missed(state, (dir, name), reach, |_| true),
            Self::NotAdded { dir, name, since } => missed(state, (dir, name), reach, |slot| {
                slot.joined_at.is_some_and(|at| at > since)
                    || slot.repositioned_at.is_some_and(|at| at > since)
            }),
            Self::Carried { dir, flagged } => {
                let carries =
                    |node| (state.slot(node)).is_some_and(|slot| flagged.carries(&slot.record));
                let held = || {
                    (state.children(dir).map(|(child, _)| child))
                        .filter(|&child| state.slot(child).is_some_and(|slot| live_in(slot, dir)))
                };
                if held().any(carries) || reach.iter().any(|&node| carries(node)) {
                    return true;
                }
                for child in held() {
                    if !reach.contains(&child) {
                        reach.push(child);
                    }
                }
                false
            }
            Self::Uncarried {
                dir,
                flagged,
                listed,
            } => {
                let standing = |node: NodeID| {
                    state.slot(node).is_some_and(|slot| {
                        live_in(slot, dir)
                            && flagged.carries(&slot.record)
                            && slot.repositioned_at.is_none_or(|at| at < listed)
                    })
                };
                if reach.is_empty() {
                    reach.extend(
                        state
                            .children(dir)
                            .map(|(child, _)| child)
                            .filter(|&child| standing(child)),
                    );
                } else {
                    reach.retain(|&node| standing(node));
                }
                reach.is_empty()
            }
            Self::Collected { root, listed } => {
                let scope = (root, listed);
                if reach.is_empty() {
                    if left_out((state, revisions), call, scope, root) {
                        reach.push(root);
                    }
                    gather((state, revisions), call, scope, root, reach);
                } else {
                    reach.retain(|&node| left_out((state, revisions), call, scope, node));
                }
                reach.is_empty()
            }
            Self::Moved { dir, base, or_gone } => state.slot(dir).map_or(or_gone, |slot| {
                moved_past(slot, base) || (or_gone && !slot.record.is_directory())
            }),
            Self::Unmoved { dir, base } => !moved(state, dir, base),
            Self::Kept { dir, head, base } => {
                let chained = |head| {
                    (state.slot(head)).is_some_and(|slot| {
                        slot.status == Status::Chained && slot.record.parent == dir
                    })
                };
                !head.is_some_and(chained) || !moved(state, dir, base)
            }
            Self::Dirty(node) => state.slot(node).is_some_and(|slot| slot.record.is_dirty()),
            Self::Occupied(dir) => state
                .children(dir)
                .any(|(_, word)| !word.is_discarded() && !word.is_staged_delete()),
            Self::NotLocal { node, to, name } => state.slot(node).is_some_and(|slot| {
                !slot.is_discarded()
                    && slot.record.is_directory()
                    && slot.record.ext_flags & NodeExtFlags::LocalContent.bits() == 0
                    && (slot.record.parent, slot.record.name_hash) != (to, name)
            }),
            Self::Unmodifiable(node) => {
                state.dead(node)
                    || state.slot(node).is_some_and(|slot| {
                        !slot.record.is_file() || slot.record.is_staged_delete()
                    })
            }
            Self::Unrestorable(node) => {
                state.dead(node)
                    || state.slot(node).is_some_and(|slot| {
                        !slot.record.is_staged_delete() || state.refuses_child(slot.record.parent)
                    })
            }
            Self::Unaddable(dir) => state
                .slot(dir)
                .is_none_or(|slot| slot.status == Status::Free || slot.record.is_staged_delete()),
            Self::Unmovable { node, to, name } => {
                state.refuses_child(node)
                    || state.slot(node).is_some_and(|slot| {
                        state.refuses_child(slot.record.parent)
                            || (slot.record.parent, slot.record.name_hash) == (to, name)
                    })
                    || state.refuses_child(to)
                    || state.refuses_move(node, to, reach)
            }
            Self::Inside { node, to } => state.refuses_move(node, to, reach),
            Self::Leaves {
                node,
                leaves,
                to,
                name,
                spelling,
            } => state.slot(node).is_some_and(|slot| {
                let word = &slot.record;
                let same_content = |given: Content| {
                    (word.mode, word.size, word.address.hash)
                        == (given.mode, given.size, given.address.hash)
                };
                // A name the model has not read is not held against the spelling.
                let spelled = spelling.is_none_or(|spelled| {
                    slot.name
                        .as_ref()
                        .is_none_or(|held| catalogue::call::spelling(held.as_bytes()) == spelled)
                });
                match leaves {
                    Leaves::Content(given) => same_content(given),
                    Leaves::Flipped(given) => same_content(given) && !word.is_dirty(),
                    Leaves::Size(size) => word.size == size,
                    Leaves::Dirty => word.is_dirty_modify(),
                    Leaves::Clean => !word.is_dirty(),
                    Leaves::Unstaged => !word.is_staged(),
                    Leaves::StagedDelete => word.is_staged_delete() && word.is_dirty_delete(),
                    Leaves::Restored => {
                        word.is_staged_modify()
                            && word.is_dirty_modify()
                            && !word.is_staged_delete()
                    }
                    Leaves::Pinned { hash, child } => {
                        word.address.hash == hash && child.is_none_or(|child| word.child == child)
                    }
                    Leaves::Placed => {
                        Some(word.parent) == to
                            && name.is_none_or(|name| word.name_hash == name)
                            && spelled
                    }
                    Leaves::Named => Some(word.name_hash) == name && spelled,
                    Leaves::LocalCopy => {
                        word.is_dirty() && word.ext_flags & NodeExtFlags::LocalContent.bits() != 0
                    }
                }
            }),
        }
    }
}

impl StateModel {
    /// `reach` closed from `starts` up the parent words as the model has them now, `starts` where it
    /// is empty: each node a walk up from them, reading one level at a time, may meet, from any
    /// `seq` read so far.
    fn reach_up(&self, starts: impl Iterator<Item = NodeID>, reach: &mut Vec<NodeID>) {
        if reach.is_empty() {
            reach.extend(starts);
        }
        let mut at = 0;
        while at < reach.len() && reach.len() <= self.blocks.len() * BLOCK_NODE_COUNT {
            let node = reach[at];
            at += 1;
            if node == ROOT_NODE {
                continue;
            }
            if let Some(parent) = self.slot(node).map(|slot| slot.record.parent)
                && !reach.contains(&parent)
            {
                reach.push(parent);
            }
        }
    }

    /// Whether a walk up from `starts`, closing `reach`, meets a node gone by itself: `Discarded`,
    /// out of its parent's chain, or freed; never the root.
    fn reaches_gone(&self, starts: impl Iterator<Item = NodeID>, reach: &mut Vec<NodeID>) -> bool {
        self.reach_up(starts, reach);
        reach.iter().any(|&node| {
            node != ROOT_NODE
                && self
                    .slot(node)
                    .is_none_or(|slot| slot.status != Status::Chained || slot.is_discarded())
        })
    }

    /// Whether `node` is dead by its own words, as a check reading them finds it: no image, or
    /// `Discarded`, as a freed slot's words are too; never the root.
    fn dead(&self, node: NodeID) -> bool {
        node != ROOT_NODE && self.slot(node).is_none_or(Slot::is_discarded)
    }

    /// Whether `node` refuses a child by its own words: dead, or staged for deletion.
    fn refuses_child(&self, node: NodeID) -> bool {
        self.dead(node)
            || self
                .slot(node)
                .is_some_and(|slot| slot.record.is_staged_delete())
    }

    /// Whether `to` takes no move of `node`: no directory, or `node` itself or below it, as a
    /// subtree check walks up the parent words from `to`, closing `reach`.
    fn refuses_move(&self, node: NodeID, to: NodeID, reach: &mut Vec<NodeID>) -> bool {
        if self
            .slot(to)
            .is_some_and(|slot| !slot.record.is_directory())
        {
            return true;
        }
        self.reach_up([to].into_iter(), reach);
        reach.contains(&node)
    }

    /// The children in `dir`'s chain, each with its words.
    fn children(&self, dir: NodeID) -> impl Iterator<Item = (NodeID, &Node)> + '_ {
        self.chains
            .get(&dir)
            .into_iter()
            .flatten()
            .filter_map(|&child| self.slot(child).map(|slot| (child, &slot.record)))
    }
}

impl Replay<'_> {
    /// What the record, its call's, tells of the call's answer, before the record applies; and its
    /// guards, each finished or dropped within the call, dropped armed only where it did not
    /// answer done.
    pub(super) fn answering(&mut self, call: &Call, answered: &mut Answered) {
        match &self.record.effect {
            Effect::Publish { node, .. } => {
                answered.published += 1;
                answered.published_answer |=
                    matches!(call.outcome, Outcome::Done(Answer::Node(answer)) if answer == *node);
            }
            Effect::Detach { node, .. } => {
                answered.unlinked += 1;
                answered.unlinked_target |= call.target == Some(*node);
            }
            Effect::Move {
                node,
                to,
                name_hash,
                name,
                ..
            } => {
                answered.moves += 1;
                let spelled = name.as_ref().map(|name| spelling(name.as_bytes()));
                answered.moved = Some((*node, *to, *name_hash, spelled));
            }
            Effect::Rename {
                node,
                parent,
                name_hash,
                name,
                ..
            } => {
                answered.renames += 1;
                let spelled = Some(spelling(name.as_bytes()));
                answered.moved = Some((*node, *parent, *name_hash, spelled));
            }
            Effect::Free { node, .. }
                if matches!(
                    self.record.section,
                    Section::DiscardFree | Section::DiscardMarks
                ) =>
            {
                match self.state.slot(*node) {
                    Some(slot) if slot.record.flags & NodeFlags::File.bits() != 0 => {
                        answered.freed.0 += 1;
                    }
                    Some(_) => answered.freed.1 += 1,
                    None => {}
                }
            }
            Effect::Arm(Arm::NeedLoad) => answered.need_load = true,
            Effect::Refused { reason, .. } => answered.refused.push(*reason),
            Effect::Hop { .. } if resolves(call) => {
                answered.hopped += 1;
                if answered.hopped == call.lookups.len() {
                    answered.crossed_at = Some(self.record.seq);
                }
            }
            // Kept only where a name or a path read's answer is judged by them.
            Effect::Parent { node, parent } if reads_names(call) => {
                answered.parents.push((*node, *parent));
            }
            Effect::Named { node, name } if reads_names(call) => {
                answered.names.push((*node, name.clone()));
            }
            Effect::Named { node, name } if collects(call) => {
                let (under, listed) =
                    (answered.heads.last()).map_or((INVALID_NODE, 0), |last| (last.dir, last.at));
                let slot = self.state.slot(*node);
                let unwritten =
                    slot.is_some_and(|slot| slot.written_at.is_none_or(|written| written < listed));
                let pins = match slot {
                    Some(slot) if slot.record.is_link() && !unwritten => {
                        let current = slot.record.linked_node();
                        let since = self.state.pins_since(*node, listed);
                        since.chain(std::iter::once(current)).collect()
                    }
                    _ => Vec::new(),
                };
                answered.namings.push(Naming {
                    node: *node,
                    name: name.clone(),
                    head: answered.heads.len().checked_sub(1),
                    under,
                    held: slot.map(|slot| slot.record),
                    in_place: slot.is_some_and(|slot| {
                        slot.record.parent == under
                            || slot.joined_at.is_some_and(|joined| joined > listed)
                    }),
                    pins,
                });
            }
            Effect::Head { node, changes, .. }
                if call.walks.iter().any(|walk| walk.dir == *node)
                    || (call.target == Some(*node) && Flagged::of(call.kind).is_some())
                    || collects(call) =>
            {
                // Whether a collection's root is a dirty add its walk finds empty, at its head read.
                if collects(call) && answered.heads.is_empty() {
                    answered.root_empty_add = empty_add(self.state, *node);
                }
                // The chain's head only a marked listing's mark keeps.
                let head = lists_marked(call)
                    .then(|| {
                        self.state
                            .chains
                            .get(node)
                            .and_then(|chain| chain.front().copied())
                    })
                    .flatten();
                answered.heads.push(HeadRead {
                    dir: *node,
                    at: self.record.seq,
                    changes: *changes,
                    head,
                    named: answered.namings.len(),
                });
            }
            Effect::Guard { event } => match event {
                GuardEvent::Armed => answered.armed += 1,
                GuardEvent::Finished | GuardEvent::DroppedArmed if answered.armed == 0 => {
                    self.fail(format!(
                        "a guard {event:?} with none armed in its call of '{}'",
                        call.kind.name
                    ));
                }
                GuardEvent::Finished => answered.armed -= 1,
                GuardEvent::DroppedArmed => {
                    answered.armed -= 1;
                    if matches!(call.outcome, Outcome::Done(_)) {
                        self.fail(format!(
                            "a guard dropped armed in a call of '{}' that answered done",
                            call.kind.name
                        ));
                    }
                }
            },
            _ => {}
        }
    }
}

impl Model {
    /// The replay at `at`, a State's `seq`, before the record there applies: where it passes to a
    /// new `seq`, each open claim read on the model as the `seq` before left it, then the calls
    /// begun by `at`, each claim read as the call begins, then the calls ended by it, each judged;
    /// a claim only from the `seq` it is read from on. Records come in `(state, seq)` order, so a
    /// State's calls still open as its records end end with the next State's first record, or
    /// with the replay ([`Self::end_calls`]).
    pub(super) fn reach(&mut self, at: (u64, u64)) {
        let Some(calls) = self.calls.as_mut() else {
            return;
        };
        if !calls.passes(at) {
            return;
        }
        let (states, revisions) = (&self.states, &self.sources);
        let holds = |call: &Call, claim: Claim, reach: &mut Vec<NodeID>| {
            states
                .get(&call.state)
                .is_some_and(|state| claim.holds(call, (state, revisions), reach))
        };
        calls.after_writes(holds);
        calls.look(at, holds);
        // The sweeps begun and ended here, their plans read once the borrows above end: each
        // begun before it ends, all of one State's records before `at` applied.
        let mut sweeps: Vec<(bool, Call)> = Vec::new();
        while let Some(index) = calls.begun(at) {
            calls.watch(index, at, holds);
            let call = calls.judging(index).0;
            if super::sweeps::sweeps(call.kind) {
                sweeps.push((true, call.clone()));
            }
        }
        while let Some(index) = calls.ended(at) {
            calls.unwatch(index);
            let (call, unlinks, answered) = calls.judging(index);
            if super::sweeps::sweeps(call.kind) {
                sweeps.push((false, call.clone()));
            }
            if let Some(what) = judged(
                (call, unlinks),
                answered,
                (self.admits, revisions),
                &mut self.self_reported,
            ) {
                self.violations.push(Violation {
                    state: call.state,
                    seq: call.end,
                    sub: 0,
                    what,
                    write_set: false,
                });
            }
        }
        for (begun, call) in sweeps {
            if begun {
                self.sweep_begun(&call);
            } else {
                self.sweep_ended(&call);
            }
        }
    }

    /// Every call the replay left open, ended where the replay ends.
    pub(super) fn end_calls(&mut self) {
        self.reach((u64::MAX, u64::MAX));
    }
}

/// What is wrong with a resolution's hops ([`resolves`]), whatever it answered, each its State
/// recorded and each a lookup of its own: more hops than its path has names; a hop not from its
/// target or the node the hop before it found, past one that found nothing among them, or of
/// another name than its path's there.
fn hops_judged(call: &Call) -> Option<String> {
    if !resolves(call) {
        return None;
    }
    let name = call.kind.name;
    let hops = call.lookups.len();
    if hops > call.names.len() {
        return Some(format!(
            "a call of '{name}' made more hops than its path has names: {hops} for {}",
            call.names.len()
        ));
    }
    let mut at = call.target;
    for (index, (lookup, &named)) in call.lookups.iter().zip(&call.names).enumerate() {
        let Lookup::Find {
            dir,
            name: hash,
            found,
            ..
        } = *lookup
        else {
            return Some(format!(
                "a call of '{name}' noted {lookup:?} among its hops, which only find"
            ));
        };
        if Some(dir) != at || hash != named {
            return Some(format!(
                "a call of '{name}' made hop {index} for {hash:#x} in {dir}, not for its path's \
                 name there in {at:?}"
            ));
        }
        at = found;
    }
    None
}

/// What is wrong with a resolution's answer ([`resolves`]), its hops walking its path
/// ([`hops_judged`]): with names left past the node its last hop found, the link it crossed, an
/// answer that no revision a pin of the link held from its last hop gives ([`Claim::Linked`]);
/// else an answer other than its last hop's node.
fn resolution_judged(call: &Call, answered: &Answered) -> Option<String> {
    let name = call.kind.name;
    match (crossed(call), &call.outcome) {
        (Some(link), Outcome::Answer(Answer::Found(None) | Answer::Linked { .. })) => {
            (answered.seen & 1 == 0).then(|| {
                format!(
                    "a call of '{name}' answered {:?} through the link {link}, which no revision a \
                     pin of it held from its last hop on gives below the node the pin exposes",
                    call.outcome
                )
            })
        }
        (Some(link), outcome) => Some(format!(
            "a call of '{name}' answered {outcome:?} with names of its path left past the link \
             {link}"
        )),
        (None, Outcome::Answer(Answer::Found(answer)))
            if !(call.lookups.last()).is_some_and(|last| finds(last, *answer)) =>
        {
            Some(format!(
                "a call of '{name}' answered {answer:?}, not what its last hop found"
            ))
        }
        (None, Outcome::Answer(Answer::Linked { .. })) => Some(format!(
            "a call of '{name}' answered {:?}, crossing no link",
            call.outcome
        )),
        _ => None,
    }
}

/// Whether a resolution crossing the link `link`, with the names of its path from `rest` on left,
/// answered as the revision the pin `state` has the link holding names resolves them below the
/// node the pin exposes, from the snapshots of the revisions loaded in the run, `revisions`: the
/// node there, in that revision of the repository the pin names, or none ([`resolved`]).
fn linked_holds(
    call: &Call,
    (state, revisions): (&StateModel, &BTreeMap<Hash, Snapshot>),
    (link, rest): (NodeID, usize),
) -> bool {
    let Some((pin, snapshot)) = pinned(state, link, revisions) else {
        return false;
    };
    resolved(snapshot, pin, &call.names[rest..])
        .is_some_and(|answer| call.outcome == Outcome::Answer(answer))
}

/// The pin `state` has the link `link` holding and the snapshot of the revision it names, from the
/// snapshots of the revisions loaded in the run, `revisions`; none where `link` is no link.
fn pinned<'a>(
    state: &StateModel,
    link: NodeID,
    revisions: &'a BTreeMap<Hash, Snapshot>,
) -> Option<(NodeLink, &'a Snapshot)> {
    let pin = (state.slot(link))
        .filter(|slot| slot.record.is_link())
        .map(|slot| slot.record.linked_node())?;
    Some((pin, revisions.get(&pin.revision)?))
}

/// What a resolution of the name hashes `path` from the node `pin` exposes in `snapshot`, the
/// revision it names, answers, as the product walks a linked State: each name's live child in
/// turn, the last its answer, with the pin's repository and revision; none where a name is missing
/// or a file or a link stands where a directory would, as at the node the pin exposes. `None`
/// where the walk finds a link with names left, which the product follows into a third State, or
/// leaves the snapshot: not modelled, as no fixture holds either.
fn resolved(snapshot: &Snapshot, pin: NodeLink, path: &[u64]) -> Option<Answer> {
    let mut node = pin.node;
    for (index, &name) in path.iter().enumerate() {
        if snapshot.slot(node)?.0.is_link() {
            return (index == 0).then_some(Answer::Found(None));
        }
        let mut found = None;
        for child in snapshot.children(node)? {
            let (child, record, _) = child?;
            if !record.is_discarded() && record.name_hash == name {
                found = Some(child);
                break;
            }
        }
        match found {
            Some(child) => node = child,
            None => return Some(Answer::Found(None)),
        }
    }
    Some(Answer::Linked {
        node,
        repository: pin.repository,
        revision: pin.revision,
    })
}

/// What is wrong with `call`'s outcome, by what its records told and what its run `admits`, if
/// anything, where `unlinks` says its done answer is an unlink of its target; its self-reported
/// outcome counted in `counted`. Whatever it answered, a resolution's hops walk its path
/// ([`hops_judged`]), each after its first made from no link at some `seq` from where it began,
/// each node it passed over as taken is gone at some `seq` from where it found it, each lookup
/// holds from where it began, each walk of a chain read its directory's head, each walk yielded no
/// node twice unless it said it may repeat and passed over no live child for its name, a walk of a
/// node that takes no children yielded none, each walk holds from its head read to its call's end,
/// or over a link, by a pin it held, a read by a listing's mark names a marked listing of its
/// directory, and a read by the mark of its own listing has its own marked listing of the
/// directory reading its head; under the gate, a search since the mark answering none met its
/// mark's head out of the chain or the count not moved past the mark's, and a read by its own
/// listing's mark finding no reattach met the count not moved past the mark's.
fn judged(
    (call, unlinks): (&Call, bool),
    answered: &Answered,
    (admits, revisions): (Admits, &BTreeMap<Hash, Snapshot>),
    counted: &mut BTreeMap<SelfReported, usize>,
) -> Option<String> {
    let name = call.kind.name;
    if answered.armed != 0 {
        return Some(format!(
            "a call of '{name}' ended with {} guard(s) armed",
            answered.armed
        ));
    }
    if let Some(fault) = hops_judged(call) {
        return Some(fault);
    }
    let (taken, lookups) = (call.taken.len(), call.lookups.len());
    let walks: usize = (call.walks.iter())
        .map(|walk| walk_checks(walk.over).len())
        .sum();
    let own_kept = call.lookups.iter().filter(|lookup| keeps_own_head(lookup));
    let kept = usize::from(keeps_head(call)) + own_kept.count();
    let unlinked = unlinked_hops(call);
    let claims = taken + lookups + walks + kept + unlinked;
    if claims > MOST_CLAIMS {
        return Some(format!(
            "a call of '{name}' made {claims} claims past its answer's ({taken} nodes passed over \
             as taken, {lookups} lookups, {walks} by its walks, {kept} marks' heads kept, \
             {unlinked} hops from no link), more than the {MOST_CLAIMS} the model watches"
        ));
    }
    if let Some((walk, node)) = call
        .walks
        .iter()
        .find_map(|walk| repeated(walk).map(|node| (walk, node)))
    {
        return Some(format!(
            "a walk of {} in a call of '{name}' yielded {node} twice, {}",
            walk.dir,
            match walk.repeats {
                Repeats::Dropped => "where it drops its repeats",
                _ => "reporting no repeat",
            }
        ));
    }
    let headless = |walk: &&Walk| walk.over == Over::Chain && head_read(walk, answered).is_none();
    if let Some(walk) = call.walks.iter().find(headless) {
        return Some(format!(
            "a walk of {} in a call of '{name}' read no head of its directory",
            walk.dir
        ));
    }
    let leaf = |walk: &&Walk| walk.over == Over::Leaf && !walk.yielded.is_empty();
    if let Some(walk) = call.walks.iter().find(leaf) {
        return Some(format!(
            "a walk of {} in a call of '{name}' yielded {} children of a node that takes none",
            walk.dir,
            walk.yielded.len()
        ));
    }
    if reads_by_mark(call) && !answered.marked {
        return Some(format!(
            "a call of '{name}' read by a mark no marked listing of its directory made"
        ));
    }
    if let Some((dir, _)) = (call.lookups.iter())
        .filter_map(own_mark_of)
        .find(|&(dir, _)| own_listing(call, dir, answered).is_none())
    {
        return Some(format!(
            "a call of '{name}' read by its own mark of {dir}, no marked listing of the directory \
             in it reading its head"
        ));
    }
    if let Some(walk) = call.walks.iter().find(|walk| walk.skipped != 0) {
        return Some(format!(
            "a walk of {} in a call of '{name}' passed over {} live children for a name it could \
             not read",
            walk.dir, walk.skipped
        ));
    }
    answer_judged((call, unlinks), answered, (admits, revisions), counted).or_else(|| {
        past(call)
            .find(|&(bit, past)| {
                answered.seen & (1 << bit) == 0 && (admits.gated || !gated_only(past))
            })
            .map(|(_, past)| match past {
                Past::Taken(node, seq) => format!(
                    "a call of '{name}' passed over {node} as taken at {seq}, gone at no seq of \
                     its interval from there"
                ),
                Past::Lookup(Lookup::Find {
                    dir,
                    name: hash,
                    found: Some(node),
                    from,
                }) => format!(
                    "a find of {hash:#x} in {dir} in a call of '{name}' answered {node}, carrying \
                     it in the directory at no seq of its interval from {from}"
                ),
                Past::Lookup(Lookup::Find {
                    dir,
                    name: hash,
                    from,
                    ..
                }) => format!(
                    "a find of {hash:#x} in {dir} in a call of '{name}' missed, a node carrying \
                     it in the directory throughout its interval from {from}"
                ),
                Past::Lookup(Lookup::Since {
                    dir,
                    name: hash,
                    searched,
                    from,
                    to,
                    ..
                }) => {
                    let said = match searched {
                        AddedSince::Found(node) => {
                            format!("{node}, carrying the name at no seq of its interval")
                        }
                        AddedSince::Absent => "none, a node that joined it since the mark \
                                               carrying the name throughout its interval"
                            .to_string(),
                        AddedSince::Stale => "stale, its count moving by less than a reattach \
                                              past the mark's, and it a directory throughout its \
                                              interval"
                            .to_string(),
                    };
                    format!(
                        "a search of {hash:#x} in {dir} since its own listing's mark in a call of \
                         '{name}' answered {said}, [{from}, {to})"
                    )
                }
                Past::Lookup(Lookup::Reattached {
                    dir,
                    reattached,
                    from,
                    to,
                    ..
                }) => format!(
                    "a read of its own listing's mark of {dir} in a call of '{name}' answered {}, \
                     [{from}, {to})",
                    if reattached {
                        "reattached, its count moving by less than a reattach past the mark's \
                         throughout its interval"
                    } else {
                        "not reattached, its count moved by a reattach past the mark's as it \
                         began"
                    }
                ),
                Past::Walk(walk, check) => {
                    let dir = call.walks[walk].dir;
                    match check {
                        WalkCheck::Yields => format!(
                            "a walk of {dir} in a call of '{name}' yielded a node standing in the \
                             directory, kept by its caller, with the record, the name's hash and \
                             the name it read at no seq from its head read"
                        ),
                        WalkCheck::Throughout(_) => format!(
                            "a walk of {dir} in a call of '{name}' left out a node in the \
                             directory, kept by its caller, throughout from its head read"
                        ),
                        WalkCheck::Pinned => format!(
                            "a walk of the link {dir} in a call of '{name}' yielded other than \
                             the children below the node a pin it held at some seq of its \
                             interval exposes, in the revision the pin names"
                        ),
                        WalkCheck::Childless => format!(
                            "a walk of {dir} in a call of '{name}' walked a node that takes no \
                             children, which was neither a file nor a link at any seq of its \
                             interval"
                        ),
                    }
                }
                Past::Kept => format!(
                    "a search of {} since its listing's mark in a call of '{name}' answered none, \
                     its count moved by a reattach past the mark's as it began, the mark's head in \
                     its chain throughout",
                    targeted(call)
                ),
                Past::OwnKept(Lookup::Since {
                    dir,
                    name: hash,
                    from,
                    to,
                    ..
                }) => format!(
                    "a search of {hash:#x} in {dir} since its own listing's mark in a call of \
                     '{name}' answered none, its count moved by a reattach past the mark's as it \
                     began, the mark's head in its chain throughout its interval, [{from}, {to})"
                ),
                Past::OwnKept(lookup) => {
                    unreachable!("a head kept claimed by {lookup:?}, a search answering none alone")
                }
                Past::Unlinked(hop) => format!(
                    "a call of '{name}' made hop {hop} from a link, names of its path left, not \
                     entering it: {:?}",
                    call.lookups[hop]
                ),
            })
    })
}

/// Whether `walk` held by `check` by the reads so far, `reach` keeping between reads only what holds
/// it back, the walk's yields fixed: its yields not seen standing yet, by index, each seen at a read
/// of its own; the nodes in its directory its caller kept at every read it did not yield. An empty
/// reach is the first read, a claim seen then kept no longer. A walk over a link reads `revisions`
/// ([`pinned_holds`]).
fn walk_holds(
    walk: &Walk,
    check: WalkCheck,
    (state, revisions): (&StateModel, &BTreeMap<Hash, Snapshot>),
    reach: &mut Vec<NodeID>,
) -> bool {
    let first = reach.is_empty();
    match check {
        WalkCheck::Pinned => pinned_holds(walk, state, revisions),
        WalkCheck::Childless => (state.slot(walk.dir))
            .is_some_and(|slot| slot.record.is_file() || slot.record.is_link()),
        WalkCheck::Yields => {
            let unseen = |at: usize| !stood(state, walk, &walk.yielded[at]);
            if first {
                reach.extend(
                    (0..walk.yielded.len())
                        .filter(|&at| unseen(at))
                        .map(|at| at as NodeID),
                );
            } else {
                reach.retain(|&at| unseen(at as usize));
            }
            reach.is_empty()
        }
        // The State's chain of a directory a free reached since its detach may hold what the
        // model's no longer does.
        WalkCheck::Throughout(_) if state.slot(walk.dir).is_some_and(|slot| slot.chain_freed) => {
            true
        }
        WalkCheck::Throughout(head) => {
            let live = |node: NodeID| {
                (state.slot(node))
                    .is_some_and(|slot| live_in(slot, walk.dir) && walk.filter.keeps(&slot.record))
            };
            if first {
                let sorted = (walk.yielded.len() > SCANNED_YIELDS).then(|| sorted_yields(walk));
                let yielded = |node: NodeID| match &sorted {
                    Some(sorted) => sorted.binary_search(&node).is_ok(),
                    None => walk.yielded.iter().any(|yielded| yielded.node == node),
                };
                let children = state.children(walk.dir).map(|(child, _)| child);
                reach.extend(children.filter(|&child| live(child) && !yielded(child)));
            } else {
                reach.retain(|&node| live(node));
            }
            // Passed over where a `Move` within the directory repositioned it behind the walk.
            reach.iter().all(|&node| {
                walk.repeats.may_pass_over()
                    && (state.slot(node))
                        .and_then(|slot| slot.repositioned_at)
                        .is_some_and(|at| at > head)
            })
        }
    }
}

/// The most yields of a walk its rules scan for a node; past it they search a sorted copy.
const SCANNED_YIELDS: usize = 32;

/// The nodes `walk` yielded, sorted.
fn sorted_yields(walk: &Walk) -> Vec<NodeID> {
    let mut nodes: Vec<NodeID> = walk.yielded.iter().map(|yielded| yielded.node).collect();
    nodes.sort_unstable();
    nodes
}

/// Whether `walk`, over a link, yielded the children a load of the revision the link's pin names
/// holds below the node the pin exposes, by the pin `state` has the link holding, from the
/// snapshots of the revisions loaded in the run, `revisions`: each kept by its caller and not
/// `Discarded` yielded, read as the revision holds it ([`read_as`]), and no other, its yields
/// distinct as [`judged`] holds a walk reporting no repeat to. The pinned directories the suite's
/// links expose hold a few children, so each is looked for among the yields. Not modelled, as no
/// fixture pins one: a pin exposing a link, which the product follows into a third State, and a
/// pin of no revision, whose load is empty; both answer false here.
fn pinned_holds(walk: &Walk, state: &StateModel, revisions: &BTreeMap<Hash, Snapshot>) -> bool {
    let Some(children) =
        pinned(state, walk.dir, revisions).and_then(|(pin, snapshot)| snapshot.children(pin.node))
    else {
        return false;
    };
    let mut kept = 0;
    for child in children {
        let Some((node, record, name)) = child else {
            return false;
        };
        if record.is_discarded() || !walk.filter.keeps(record) {
            continue;
        }
        let yielded = walk.yielded.iter().find(|yielded| yielded.node == node);
        if !yielded.is_some_and(|yielded| read_as(yielded, record, name)) {
            return false;
        }
        kept += 1;
    }
    kept == walk.yielded.len()
}

/// Whether the node `yielded` names stood in `walk`'s directory as the model has it: a live child
/// of it its caller keeps, read as the model holds it ([`read_as`]).
fn stood(state: &StateModel, walk: &Walk, yielded: &Yielded) -> bool {
    state.slot(yielded.node).is_some_and(|slot| {
        live_in(slot, walk.dir)
            && walk.filter.keeps(&slot.record)
            && read_as(yielded, &slot.record, slot.name.as_ref())
    })
}

/// Whether a walk read `yielded` as a node with `record` and `name` is: its words those read, but a
/// directory's head, which is the chain's, its name's hash the one read, and its name the one read
/// where `name` is known, each where the walk read it.
fn read_as(yielded: &Yielded, record: &Node, name: Option<&Name>) -> bool {
    (yielded.record.as_ref())
        .is_none_or(|read| Fields::changed((record, false), (read, false)).is_empty())
        && (yielded.name).is_none_or(|hash| record.name_hash == hash)
        && (yielded.spelling)
            .is_none_or(|read| name.is_none_or(|name| spelling(name.as_bytes()) == read))
}

/// Whether `slot` is a live child of `dir`: in its chain, not `Discarded`.
fn live_in(slot: &Slot, dir: NodeID) -> bool {
    slot.status == Status::Chained && !slot.is_discarded() && slot.record.parent == dir
}

/// Whether a collection reports `record`: a dirty file, a dirty delete, or a dirty add
/// [`reported_add`] keeps.
fn reportable(record: &Node) -> bool {
    (record.is_file() && record.is_dirty())
        || reported_add(record)
        || (record.is_directory() && record.is_dirty_delete())
}

/// Whether a collection reports `record`, a directory, as a dirty add: one staged, or heading no
/// chain, as `marked_directory_carried_by_children` (state.rs) leaves the children of one only
/// marked to carry it.
fn reported_add(record: &Node) -> bool {
    record.is_directory()
        && record.is_dirty_add()
        && (record.is_staged() || record.child().is_none())
}

/// Whether a collection descends into `record`: a directory, dirty and no dirty delete.
fn descends(record: &Node) -> bool {
    record.is_directory() && record.is_dirty() && !record.is_dirty_delete()
}

/// The spelling of `node`'s path below `root`, `node` not `root` itself, by the names the model
/// holds, where it stands for a collection from `root`'s head read, at `listed`: it and each node
/// above it below `root` live in its parent, joined there and repositioned there by no move since;
/// each directory between them one the collection descends into ([`descends`]) and respelled by
/// no rename since, as the collection reads each name at a read of its own.
fn collected_path(state: &StateModel, (root, listed): (NodeID, u64), node: NodeID) -> Option<u64> {
    if !node.is_valid_node_id() {
        return None;
    }
    let since = |at: Option<u64>| at.is_some_and(|at| at > listed);
    let slot = state.slot(node)?;
    let parent = slot.record.parent;
    if !live_in(slot, parent) || since(slot.joined_at) || since(slot.repositioned_at) {
        return None;
    }
    let name = slot.name.as_ref()?.as_bytes();
    if parent == root {
        return Some(spelling(name));
    }
    let above = state.slot(parent)?;
    if !descends(&above.record) || since(above.renamed_at) {
        return None;
    }
    let below = collected_path(state, (root, listed), parent)?;
    Some(spelling_below(Some(below), name))
}

/// Whether `node` stands for `call`, a collection, from `scope`, its root and its root's head read,
/// at a path it did not report: a node it reports ([`reportable`]) at its [`collected_path`], or its
/// root, but the State's, an [`empty_add`], at the empty path; or a dirty link there, which it
/// follows, with a path the revision its pin names holds below it ([`linked_paths`]) unreported,
/// or a pin the model does not follow.
fn left_out(
    (state, revisions): (&StateModel, &BTreeMap<Hash, Snapshot>),
    call: &Call,
    scope: (NodeID, u64),
    node: NodeID,
) -> bool {
    let unreported = |path: u64| call.paths.binary_search(&path).is_err();
    let (root, _) = scope;
    if node == root {
        return root != ROOT_NODE && empty_add(state, root) && unreported(spelling(b""));
    }
    let Some(record) = state.slot(node).map(|slot| &slot.record) else {
        return false;
    };
    if record.is_link() && record.is_dirty() {
        let pin = record.linked_node();
        return collected_path(state, scope, node).is_some_and(|path| {
            (revisions.get(&pin.revision))
                .and_then(|snapshot| linked_paths(snapshot, pin.node, path))
                .is_none_or(|paths| paths.into_iter().any(unreported))
        });
    }
    reportable(record) && collected_path(state, scope, node).is_some_and(unreported)
}

/// The spellings of the paths a collection of `snapshot`, a dirty link's pinned revision, reports
/// from `node`, the node the pin exposes, below the link's path `below`, sorted, as the product
/// walks a linked State: none for a file; the link's own path for a dirty-add directory with no
/// child but `Discarded` ones; else each dirty child not `Discarded`, a file, a dirty add
/// [`reported_add`] keeps and a dirty delete reported, a dirty directory but a delete descended
/// into. `None` where the pinned
/// node is a link or the walk meets a dirty one, the product following either into a third State,
/// or a node, a name or a chain the snapshot does not hold: not modelled, as no fixture holds one.
fn linked_paths(snapshot: &Snapshot, node: NodeID, below: u64) -> Option<Vec<u64>> {
    let (pinned, _) = snapshot.slot(node)?;
    let mut paths = Vec::new();
    let mut stack = vec![(node, below)];
    while let Some((dir, path)) = stack.pop() {
        let mut empty = true;
        for child in snapshot.children(dir)? {
            let (child, record, name) = child?;
            if record.is_discarded() {
                continue;
            }
            empty = false;
            if !record.is_dirty() {
                continue;
            }
            let spelled = spelling_below(Some(path), name?.as_bytes());
            if record.is_file() {
                paths.push(spelled);
            } else if record.is_link() {
                return None;
            } else {
                if reported_add(record) {
                    paths.push(spelled);
                }
                if record.is_dirty_delete() {
                    paths.push(spelled);
                } else {
                    stack.push((child, spelled));
                }
            }
        }
        if dir == node && empty && pinned.is_directory() && pinned.is_dirty_add() {
            paths.push(below);
        }
    }
    paths.sort_unstable();
    Some(paths)
}

/// Whether `dir` is a dirty-add directory with no child but `Discarded` ones, which a walk passes:
/// a collection's empty root, reported at its own path unless the State's root.
fn empty_add(state: &StateModel, dir: NodeID) -> bool {
    (state.slot(dir)).is_some_and(|slot| slot.record.is_directory() && slot.record.is_dirty_add())
        && state.children(dir).all(|(_, word)| word.is_discarded())
}

/// Each node below `dir` that [`left_out`] finds, into `reach`, descending as a collection does:
/// into each directory it descends into ([`descends`]) that stands for it.
fn gather(
    (state, revisions): (&StateModel, &BTreeMap<Hash, Snapshot>),
    call: &Call,
    scope: (NodeID, u64),
    dir: NodeID,
    reach: &mut Vec<NodeID>,
) {
    for (child, record) in state.children(dir) {
        if left_out((state, revisions), call, scope, child) {
            reach.push(child);
        }
        if descends(record) && collected_path(state, scope, child).is_some() {
            gather((state, revisions), call, scope, child, reach);
        }
    }
}

/// A node `walk` yielded twice, where it reported no repeat.
fn repeated(walk: &Walk) -> Option<NodeID> {
    if walk.repeats == Repeats::May {
        return None;
    }
    let yielded = &walk.yielded;
    if yielded.len() > SCANNED_YIELDS {
        let sorted = sorted_yields(walk);
        return (sorted.windows(2))
            .find(|pair| pair[0] == pair[1])
            .map(|pair| pair[0]);
    }
    (yielded.iter().enumerate())
        .find(|&(at, this)| {
            yielded[..at]
                .iter()
                .any(|earlier| earlier.node == this.node)
        })
        .map(|(_, this)| this.node)
}

/// Whether no node `counts` carried the hash `name` in `dir` at every read so far, `reach` keeping
/// the nodes carrying it since the first read, an empty reach being the first read: seen once each
/// stopped carrying it, the node a replacing reattach put there carrying on for the one it
/// replaced (C7).
fn missed(
    state: &StateModel,
    (dir, name): (NodeID, u64),
    reach: &mut Vec<NodeID>,
    counts: impl Fn(&Slot) -> bool,
) -> bool {
    let carrying = || {
        let counted =
            |slot: &Slot| live_in(slot, dir) && slot.record.name_hash == name && counts(slot);
        (state.children(dir))
            .filter(move |&(child, _)| state.slot(child).is_some_and(counted))
            .map(|(child, _)| child)
    };
    if reach.is_empty() {
        reach.extend(carrying());
    } else {
        let mut replaced = false;
        reach.retain(|&node| {
            replaced |= state
                .slot(node)
                .is_some_and(|slot| slot.status == Status::Unchained(Unchained::Replaced));
            carries(state, node, (dir, name))
        });
        if replaced {
            for node in carrying() {
                if !reach.contains(&node) {
                    reach.push(node);
                }
            }
        }
    }
    reach.is_empty()
}

/// Whether directory `dir`'s count moved by a reattach past `changes`.
fn moved(state: &StateModel, dir: NodeID, changes: u32) -> bool {
    (state.slot(dir)).is_some_and(|slot| moved_past(slot, changes))
}

/// Whether `slot`'s count moved by a reattach past `changes`.
fn moved_past(slot: &Slot, changes: u32) -> bool {
    slot.changes.wrapping_sub(changes) >= REATTACH_CHANGES
}

/// What is wrong with a name read's answer `read`, by its call's own records: the spelling of the
/// name its one name record of its target read; a skip, which only a name the read path refuses
/// makes, as no fixture holds one.
fn name_judged(call: &Call, answered: &Answered, read: Option<u64>) -> Option<String> {
    let name = call.kind.name;
    let Some(read) = read else {
        return Some(format!(
            "a call of '{name}' skipped the name of {}, though no fixture holds a name the read \
             path refuses",
            targeted(call)
        ));
    };
    match &answered.names[..] {
        [(node, named)] if Some(*node) == call.target && spelling(named.as_bytes()) == read => None,
        names => Some(format!(
            "a call of '{name}' answered the name spelled {read:#x} of {}, its name reads \
             {names:?}",
            targeted(call)
        )),
    }
}

/// What is wrong with a comparison by name answering `is`, by its call's own records: whether its
/// one name record of its target read the name compared.
fn name_is_judged(call: &Call, answered: &Answered, is: bool) -> Option<String> {
    match &answered.names[..] {
        [(node, named)]
            if Some(*node) == call.target
                && (Some(spelling(named.as_bytes())) == call.spelling) == is =>
        {
            None
        }
        names => Some(format!(
            "a call of '{}' answered {is} for {} spelled {:?}, its name reads {names:?}",
            call.kind.name,
            targeted(call),
            call.spelling
        )),
    }
}

/// What is wrong with a collection's paths, by its call's own records, which rebuild its walk
/// ([`walked`]). Each named file reported, each named directory it did not descend into reported,
/// as a dirty delete, and no path more times than the nodes it named spell it, its empty root's
/// once where it named none, which the State's root never is. Under the gate, where nothing lands
/// between a child's words and its name: each named child dirty and in its directory as it read
/// the name, each named directory descended into but a dirty delete and reported as a dirty add or
/// a dirty delete, and its empty root reported as a dirty add; the gate alone catches a plain dirty
/// directory reported, a clean child named, or a dirty delete descended into. Told apart by
/// nothing, as no fixture makes them: a slot freed and grabbed again within the collection, a
/// directory pushed and then freed or made a file before its pop, which reads no head, a link
/// freed between its words and its name, and in a run of no gate a directory named twice whose
/// later naming, a dirty delete, pushed nothing. Each dirty link it named it followed: where the
/// model's words of it at the naming are those it read, each path the revision its pin names
/// holds below it ([`linked_paths`], `revisions` holding the snapshot of each) reported and
/// counted among those the namings spell, a pin the model does not follow a violation; for a link
/// named outside the gate with a write since its directory's head read, which it followed by one
/// of the pins the link held since and reported every path of, each path up to the most times such
/// a pin gives it counted, none such a violation, a pin of a revision no load read none it read,
/// and one the model does not follow leaving the paths no naming spells unjudged.
fn collected_judged(
    call: &Call,
    answered: &Answered,
    (gated, revisions): (bool, &BTreeMap<Hash, Snapshot>),
) -> Option<String> {
    let name = call.kind.name;
    let root = call.target?;
    let reported = |path: u64| call.paths.binary_search(&path).is_ok();
    let Some(first) = answered.heads.first() else {
        // A root no directory as read lists nothing.
        return (!call.paths.is_empty() || !answered.namings.is_empty())
            .then(|| format!("a call of '{name}' collected below {root}, reading no head of it"));
    };
    if first.dir != root {
        return Some(format!(
            "a call of '{name}' read the head of {} before its root's, {root}",
            first.dir
        ));
    }
    let namings = &answered.namings;
    let (descended, spelled) = match walked(name, answered, gated) {
        Ok(walk) => walk,
        Err(fault) => return Some(fault),
    };
    for ((naming, &path), &descended) in namings.iter().zip(&spelled).zip(&descended) {
        let node = naming.node;
        let Some(held) = naming.held else {
            continue;
        };
        if held.is_file() && !reported(path) {
            return Some(format!(
                "a call of '{name}' named {node}, a file, at a path it did not report"
            ));
        }
        if held.is_directory() && !descended && !reported(path) {
            return Some(format!(
                "a call of '{name}' named {node}, a directory it did not descend into, at a path \
                 it did not report"
            ));
        }
    }
    let (mut linked, mut unjudged) = (Vec::new(), false);
    for (naming, &path) in namings.iter().zip(&spelled) {
        let Some(held) = naming.held.filter(Node::is_link) else {
            continue;
        };
        if !gated && !naming.pins.is_empty() {
            // It named the link dirty, so followed a pin it held since, a pin of a revision no
            // load read none, and reported every path of it: each path counts up to the most
            // times such a pin gives it.
            let (mut most, mut followed) = (BTreeMap::<u64, usize>::new(), false);
            for pin in &naming.pins {
                let Some(snapshot) = revisions.get(&pin.revision) else {
                    continue;
                };
                let Some(paths) = linked_paths(snapshot, pin.node, path) else {
                    (unjudged, followed) = (true, true);
                    break;
                };
                if !paths.iter().all(|&path| reported(path)) {
                    continue;
                }
                followed = true;
                for same in paths.chunk_by(|one, other| one == other) {
                    let count = most.entry(same[0]).or_default();
                    *count = (*count).max(same.len());
                }
            }
            if !followed {
                return Some(format!(
                    "a call of '{name}' named the dirty link {}, leaving out a path each pin it held \
                     since its directory's head read holds below it",
                    naming.node
                ));
            }
            linked.extend(
                (most.into_iter()).flat_map(|(path, count)| std::iter::repeat_n(path, count)),
            );
            continue;
        }
        if !held.is_dirty() {
            continue;
        }
        let (node, pin) = (naming.node, held.linked_node());
        let Some(paths) = (revisions.get(&pin.revision))
            .and_then(|snapshot| linked_paths(snapshot, pin.node, path))
        else {
            return Some(format!(
                "a call of '{name}' named the dirty link {node}, pinning a revision the model does \
                 not follow below it"
            ));
        };
        if !paths.iter().all(|&path| reported(path)) {
            return Some(format!(
                "a call of '{name}' named the dirty link {node}, leaving out a path its pinned \
                 revision holds below it"
            ));
        }
        linked.extend(paths);
    }
    linked.sort_unstable();
    let (empty, root_empty) = (spelling(b""), namings.is_empty() && root != ROOT_NODE);
    for same in call.paths.chunk_by(|one, other| one == other) {
        let path = same[0];
        let below = linked.partition_point(|&linked| linked <= path)
            - linked.partition_point(|&linked| linked < path);
        let spells = (spelled.iter().zip(namings))
            .filter(|&(&spells, naming)| {
                spells == path && naming.held.is_none_or(|held| !held.is_link())
            })
            .count()
            + usize::from(root_empty && path == empty)
            + below;
        if same.len() > spells && !(unjudged && spells == 0) {
            return Some(if spells == 0 {
                format!("a call of '{name}' reported a path no child it named spells")
            } else {
                format!(
                    "a call of '{name}' reported a path more times than the children it named \
                     spell it"
                )
            });
        }
    }
    if !gated {
        return None;
    }
    for ((naming, &path), &descended) in namings.iter().zip(&spelled).zip(&descended) {
        let (node, under) = (naming.node, naming.under);
        let Some(held) = naming.held else {
            continue;
        };
        if !held.is_dirty() {
            return Some(format!(
                "a call of '{name}' named {node}, the model holding it clean as it read the name"
            ));
        }
        if !naming.in_place {
            return Some(format!(
                "a call of '{name}' named {node} below {under}, the model holding it elsewhere \
                 since before {under}'s head read"
            ));
        }
        if !held.is_directory() {
            continue;
        }
        let (deleted, added) = (held.is_dirty_delete(), reported_add(&held));
        // A dirty delete pushes nothing, so no head is bound to one under the gate.
        let fault = match (descended, reported(path)) {
            (false, _) if !deleted => {
                format!("did not descend into {node}, which the model held no dirty delete")
            }
            (_, true) if held.is_dirty_add() && !added => {
                format!("reported {node}, a dirty-add directory its children carry")
            }
            (_, true) if !added && !deleted => format!(
                "reported {node}, a directory the model held neither a dirty add nor a dirty \
                 delete"
            ),
            (_, false) if added || deleted => format!(
                "did not report {node}, a directory the model held a dirty add or a dirty delete"
            ),
            _ => continue,
        };
        return Some(format!("a call of '{name}' {fault}"));
    }
    match (root_empty, reported(empty), answered.root_empty_add) {
        (true, true, false) => Some(format!(
            "a call of '{name}' reported its empty root {root}, which the model held no dirty add \
             with no children"
        )),
        (true, false, true) => Some(format!(
            "a call of '{name}' did not report its empty root {root}, which the model held a \
             dirty add with no children"
        )),
        _ => None,
    }
}

/// The walk a collection's records rebuild, or what is wrong with it: for each naming, whether
/// the collection descended into it, a head read past its root's bound to it, the latest naming
/// before that head not bound yet, as the walk's stack pops the last pushed first, and under the
/// gate only a directory and no dirty delete, which alone it pushes; and each naming's path, its
/// names spelled down from the root, the root's the empty path. Its root's head read first.
fn walked(name: &str, answered: &Answered, gated: bool) -> Result<(Vec<bool>, Vec<u64>), String> {
    let namings = &answered.namings;
    let pushes = |naming: &Naming| {
        !gated
            || naming
                .held
                .is_some_and(|held| held.is_directory() && !held.is_dirty_delete())
    };
    let mut descended = vec![false; namings.len()];
    // The naming, by its index, each head read past the root's pops.
    let mut pushed: Vec<usize> = Vec::with_capacity(answered.heads.len());
    for head in &answered.heads[1..] {
        let bound = (0..head.named)
            .rev()
            .find(|&at| namings[at].node == head.dir && !descended[at] && pushes(&namings[at]));
        let Some(at) = bound else {
            return Err(format!(
                "a call of '{name}' listed {}, which it never named as a directory to descend into",
                head.dir
            ));
        };
        descended[at] = true;
        pushed.push(at);
    }
    let mut spelled: Vec<u64> = Vec::with_capacity(namings.len());
    for naming in namings {
        let below = match naming.head {
            Some(0) => None,
            Some(head) => Some(spelled[pushed[head - 1]]),
            None => {
                return Err(format!(
                    "a call of '{name}' named {} before reading its root's head",
                    naming.node
                ));
            }
        };
        spelled.push(spelling_below(below, naming.name.as_bytes()));
    }
    Ok((descended, spelled))
}

/// What is wrong with a path read's answer `path`, by its call's own records: its parent reads a
/// walk up from its target, each read's parent the next read's node, ending at its ancestor for a
/// path and at a node no tree holds for none, which `node_path` answers as the empty path; for a
/// path, its name reads those of the walk's nodes top-down, their names joined spelling it; for
/// none, no name read.
fn path_judged(call: &Call, answered: &Answered, path: Option<u64>) -> Option<String> {
    let name = call.kind.name;
    let (Some(node), Some(ancestor)) = (call.target, call.destination) else {
        return Some(format!(
            "a call of '{name}' read a path carrying no node or ancestor: {:?}, {:?}",
            call.target, call.destination
        ));
    };
    let mut at = node;
    for &(read, parent) in &answered.parents {
        // The walk stops at its ancestor, or at a node no tree holds, before reading its parent.
        if at == ancestor || !at.is_valid_node_id() {
            return Some(format!(
                "a call of '{name}' read {read}'s parent past where its walk up from {node} stops, \
                 at {at}"
            ));
        }
        if read != at {
            return Some(format!(
                "a call of '{name}' read {read}'s parent where its walk up from {node} stood at \
                 {at}"
            ));
        }
        at = parent;
    }
    let left = at != ancestor && !at.is_valid_node_id();
    // A walk that left the tree, which `node_path` spells as the empty path.
    let left_empty =
        left && std::ptr::eq(call.kind, &catalogue::NODE_PATH) && path == Some(spelling(b""));
    let ended = match path {
        Some(_) => at == ancestor || left_empty,
        None => left,
    };
    if !ended {
        return Some(format!(
            "a call of '{name}' answered {} below {ancestor}, its walk up from {node} ending at \
             {at}",
            if path.is_some() { "a path" } else { "no path" }
        ));
    }
    let Some(path) = path.filter(|_| !left_empty) else {
        return (!answered.names.is_empty()).then(|| {
            format!(
                "a call of '{name}' answered no path, reading names: {:?}",
                answered.names
            )
        });
    };
    let walked = answered.parents.iter().rev().map(|&(node, _)| node);
    let read = answered.names.iter().map(|(node, _)| *node);
    // An empty name adds nothing to a path, as a path's push leaves it out.
    let named = answered.names.iter().map(|(_, named)| named.as_bytes());
    let joined = path_spelling(named.filter(|named| !named.is_empty()));
    (!walked.eq(read) || joined != path).then(|| {
        format!(
            "a call of '{name}' answered a path whose names its name reads, top-down along its \
             walk, do not spell: {:?}",
            answered.names
        )
    })
}

/// What `call` is made on, for a text: its target, or none.
fn targeted(call: &Call) -> String {
    call.target
        .map_or_else(|| "no target".to_string(), |target| target.to_string())
}

/// Whether `node` carries the hash `name` in `dir`: a live child of it ([`live_in`]) by that hash.
fn carries(state: &StateModel, node: NodeID, (dir, name): (NodeID, u64)) -> bool {
    state
        .slot(node)
        .is_some_and(|slot| live_in(slot, dir) && slot.record.name_hash == name)
}

/// The reasons a section records refusing where its record alone decides a refusal for `reason`:
/// an in-place or owned change declined, a C API delete's level refused; none for the others.
fn recorded_as(reason: Refusal) -> &'static [Reason] {
    match reason {
        Refusal::Declined => &[
            Reason::InPlace,
            Reason::InPlaceAway,
            Reason::Owned,
            Reason::OwnedGone,
        ],
        Refusal::DeleteLevelStaged => &[Reason::DeleteLevelStaged],
        Refusal::DeleteLevelOther => &[Reason::DeleteLevelOther],
        _ => &[],
    }
}

/// What is wrong with `call`'s answer, as [`judged`] judges it.
fn answer_judged(
    (call, unlinks): (&Call, bool),
    answered: &Answered,
    (admits, revisions): (Admits, &BTreeMap<Hash, Snapshot>),
    counted: &mut BTreeMap<SelfReported, usize>,
) -> Option<String> {
    let name = call.kind.name;
    let changes = answered.published + answered.unlinked + answered.moves + answered.renames;
    // A move's or a rename's done answer is its one `Move` or `Rename`, as called, where the call
    // says; or none, its node in place already.
    let placing = matches!(call.outcome, Outcome::Done(Answer::Moved { .. }))
        || matches!(call.leaves, Some(Leaves::Placed | Leaves::Named));
    let how = if call.outcome.left_part_way() {
        "part way"
    } else {
        "done"
    };
    let misplaced = answered.moved.filter(|&(node, to, hash, spelled)| {
        placing
            && (call.target.is_some_and(|called| called != node)
                || call.destination.is_some_and(|called| called != to)
                || call.name.is_some_and(|called| called != hash)
                || call
                    .spelling
                    .zip(spelled)
                    .is_some_and(|(called, spelled)| called != spelled))
    });
    match &call.outcome {
        Outcome::Answer(Answer::Name(read)) => name_judged(call, answered, *read),
        Outcome::Answer(Answer::NameIs(is)) => name_is_judged(call, answered, *is),
        Outcome::Answer(Answer::Path(path)) => path_judged(call, answered, *path),
        Outcome::Answer(_) if resolves(call) => resolution_judged(call, answered),
        // A find answers what its last lookup found.
        Outcome::Answer(Answer::Found(answer))
            if !(call.lookups.last()).is_some_and(|last| finds(last, *answer)) =>
        {
            Some(format!(
                "a call of '{name}' answered {answer:?}, not what its last lookup found"
            ))
        }
        Outcome::Failed(error) if call.failed_reads == 0 => Some(format!(
            "a call of '{name}' failed with no read failing in its interval: {error}"
        )),
        Outcome::Failed(error) if !INJECTED_AS.iter().any(|text| error.contains(text)) => Some(
            format!("a call of '{name}' failed with an error no failed read gives: {error}"),
        ),
        Outcome::Dropped if !call.kind.cancellable && !admits.uncancellable_drops => Some(format!(
            "a call of '{name}' was dropped part way, which its production caller never drops"
        )),
        // Part way: no more than its done answer makes, a detach's one unlink, of its target.
        Outcome::Failed(_) | Outcome::Dropped
            if unlinks
                && (answered.unlinked > 1
                    || (answered.unlinked == 1 && !answered.unlinked_target)) =>
        {
            Some(format!(
                "a call of '{name}' answered {:?} with {} unlinks, its target {:?} {}among them",
                call.outcome,
                answered.unlinked,
                call.target,
                if answered.unlinked_target { "" } else { "not " }
            ))
        }
        Outcome::Refused(_) | Outcome::SelfReported(_) if changes != 0 => Some(format!(
            "a call of '{name}' answered {:?}, yet published {}, unlinked {}, moved {} and \
             renamed {}",
            call.outcome, answered.published, answered.unlinked, answered.moves, answered.renames
        )),
        Outcome::SelfReported(SelfReported::NeedLoad) if !answered.need_load => Some(format!(
            "a call of '{name}' self-reported NeedLoad, meeting no block out of memory"
        )),
        Outcome::SelfReported(reported) if !admits.self_reports.contains(reported) => Some(
            format!("a call of '{name}' reported {reported:?} in a run admitting no such outcome"),
        ),
        Outcome::SelfReported(reported) => {
            *counted.entry(*reported).or_default() += 1;
            None
        }
        Outcome::Done(Answer::Node(node)) if !answered.published_answer => Some(format!(
            "a call of '{name}' answered node {node}, which it did not publish"
        )),
        Outcome::Done(Answer::Held(node)) if answered.published != 0 => Some(format!(
            "a call of '{name}' answered the name held by {node}, publishing {}",
            answered.published
        )),
        Outcome::Done(Answer::Held(node)) if answered.seen & 1 == 0 => Some(format!(
            "a call of '{name}' answered the name held by {node}, a live child of {:?} named {:?} \
             at no seq of its interval",
            call.target, call.name
        )),
        Outcome::Done(Answer::Moved { .. }) if answered.moves != 1 => Some(format!(
            "a call of '{name}' answered moved, with {} moves",
            answered.moves
        )),
        Outcome::Done(Answer::Freed { files, directories })
            if answered.freed != (*files, *directories) =>
        {
            Some(format!(
                "a call of '{name}' answered freeing {files} files and {directories} directories, \
                 where its frees were {:?}",
                answered.freed
            ))
        }
        Outcome::Done(Answer::None) if unlinks && !answered.unlinked_target => Some(format!(
            "a call of '{name}' answered detached, its target {:?} not among its {} unlinks",
            call.target, answered.unlinked
        )),
        Outcome::Done(Answer::None)
            if std::ptr::eq(call.kind, &catalogue::SERVED_MARK) && answered.seen & 1 == 0 =>
        {
            Some(format!(
                "a call of '{name}' answered done, its target {:?} admitting no served change at \
                 any seq of its interval",
                call.target
            ))
        }
        // Done or part way: a move's one `Move` or `Rename`, as called.
        Outcome::Done(_) | Outcome::Failed(_) | Outcome::Dropped
            if placing && answered.moves + answered.renames > 1 =>
        {
            Some(format!(
                "a call of '{name}' answered {how} with {} moves and {} renames, more than its \
                 done answer makes",
                answered.moves, answered.renames
            ))
        }
        Outcome::Done(_) | Outcome::Failed(_) | Outcome::Dropped if misplaced.is_some() => {
            misplaced.map(|(node, to, hash, _)| {
                format!(
                    "a call of '{name}' answered {how}, moving {node} under {to} as {hash:#x}, \
                     not as called: {:?} under {:?} as {:?}",
                    call.target, call.destination, call.name
                )
            })
        }
        // What a done answer leaves, held once its own write applied, or, with none, at some `seq`.
        Outcome::Done(_) => Claim::of(call).and_then(|claim| match answered.after_write {
            Some(true) => None,
            Some(false) => Some(format!(
                "a call of '{name}' answered done, its claim {claim:?} not holding once its own \
                 last write on its target applied"
            )),
            None => (answered.seen & 1 == 0).then(|| {
                format!(
                    "a call of '{name}' answered done, its claim {claim:?} holding at no seq of \
                     its interval"
                )
            }),
        }),
        // A read by a listing's mark, whose claim held at no `seq`; a mark found not reattached
        // judged only under the gate, where the count it read lock-free is the model's as it began.
        Outcome::Answer(Answer::Since(Some(node))) if answered.seen & 1 == 0 => Some(format!(
            "a search of {} since its listing's mark in a call of '{name}' answered {node}, \
             carrying the name at no seq of its interval",
            targeted(call)
        )),
        Outcome::Answer(Answer::Since(None)) if answered.seen & 1 == 0 => Some(format!(
            "a search of {} since its listing's mark in a call of '{name}' answered none, a node \
             that joined it since the mark carrying the name throughout its interval",
            targeted(call)
        )),
        Outcome::Answer(Answer::Stale) if answered.seen & 1 == 0 => Some(format!(
            "a search of {} since its listing's mark in a call of '{name}' answered stale, its \
             count moving by less than a reattach past the mark's, and it a directory throughout",
            targeted(call)
        )),
        Outcome::Answer(Answer::Reattached(true)) if answered.seen & 1 == 0 => Some(format!(
            "a mark of {} in a call of '{name}' answered reattached, its count moving by less than \
             a reattach past the mark's",
            targeted(call)
        )),
        Outcome::Answer(Answer::Reattached(false)) if answered.seen & 1 == 0 && admits.gated => {
            Some(format!(
                "a mark of {} in a call of '{name}' answered not reattached, its count moved by a \
                 reattach past the mark's as it began",
                targeted(call)
            ))
        }
        // A collection's paths, by what it named; then its claim, read from its root's head read
        // where it read one.
        Outcome::Answer(Answer::Collected) => {
            collected_judged(call, answered, (admits.gated, revisions)).or_else(|| {
                (!answered.heads.is_empty() && answered.seen & 1 == 0).then(|| {
                    format!(
                        "a call of '{name}' left out a node in place, named and dirty below dirty \
                         directories throughout from its root's head read, below {}",
                        targeted(call)
                    )
                })
            })
        }
        // A has-children read whose claim held at no `seq` from its listing's head read; one with
        // no head read listed no directory, a file or a link, which answers no child, not judged
        // here.
        Outcome::Answer(Answer::Has(has)) if answered.seen & 1 == 0 => {
            let headed = (answered.heads.iter()).any(|head| Some(head.dir) == call.target);
            match (has, headed) {
                (true, _) => Some(format!(
                    "a call of '{name}' answered a child of {} carrying its flags, no node of the \
                     directory carrying them at any seq from its listing's head read",
                    targeted(call)
                )),
                (false, true) => Some(format!(
                    "a call of '{name}' answered no child of {} carrying its flags, one carrying \
                     them there throughout from its listing's head read",
                    targeted(call)
                )),
                (false, false) => None,
            }
        }
        // A refusal its section's record alone decides, among the call's records.
        Outcome::Refused(reason)
            if !recorded_as(*reason).is_empty()
                && !(answered.refused.iter())
                    .any(|refused| recorded_as(*reason).contains(refused)) =>
        {
            Some(format!(
                "a call of '{name}' refused {reason:?} on {:?}, no refusal of its sections \
                 recorded as {:?}: {:?}",
                call.target,
                recorded_as(*reason),
                answered.refused
            ))
        }
        // Bit 0 seen only where it claimed one.
        Outcome::Refused(reason) if answered.seen & 1 == 0 && Claim::claimed(*reason) => {
            Some(match Claim::of(call) {
                None => format!(
                    "a call of '{name}' refused {reason:?}, carrying no target, destination or \
                     name to read its condition by: {:?}, {:?}, {:?}",
                    call.target, call.destination, call.name
                ),
                Some(_) => format!(
                    "a call of '{name}' refused {reason:?} on {:?}, {:?}, its condition holding at \
                     no seq of its interval",
                    call.target, call.destination
                ),
            })
        }
        _ => None,
    }
}
