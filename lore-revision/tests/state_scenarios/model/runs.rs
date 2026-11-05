// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Runs on threads replayed: each mix of the harness's (`mixes`) in full, on four workers at once,
//! yielding often, its log replayed through the model started where its setup ended, which finds
//! no violation and matches the State it ran on. Each mix's seeds together leave the records its
//! kinds make, and show its kinds interleaved. Each mix runs again with operations dropped part
//! way, and with the store failing reads, in bursts.

use std::collections::BTreeSet;

use lore_revision::node::NodeBlock;
use lore_revision::node::NodeID;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::How;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;

use crate::calls::changes_chain_of;
use crate::calls::refused_through_absence;
use crate::calls::through_absence;
use crate::harness::catalogue::call::Answer;
use crate::harness::catalogue::call::Outcome;
use crate::harness::catalogue::call::Over;
use crate::harness::catalogue::call::path_spelled;
use crate::harness::catalogue::ops::Flagged;
use crate::harness::mixes::Mix;
use crate::harness::mixes::ROUNDS;
use crate::harness::mixes::Run;
use crate::harness::mixes::Settings;
use crate::harness::mixes::Variant;
use crate::harness::mixes::kinds;
use crate::harness::mixes::replayed_on_workers;

/// `mix` in full, on the tests' settings from `seed`: a failure in its run fails the test, and its
/// workers must have yielded.
fn replayed(mix: Mix, seed: u64) -> Run {
    replayed_as(mix, seed, Variant::Plain)
}

/// [`replayed`], as `variant` says.
fn replayed_as(mix: Mix, seed: u64, variant: Variant) -> Run {
    let ops = mix.ops(ROUNDS);
    let settings = Settings {
        variant,
        ..Settings::seeded(seed)
    };
    let run = replayed_on_workers(mix, &ops, settings).unwrap_or_else(|failure| {
        panic!(
            "{} {} on seed {seed}: {failure}",
            mix.name(),
            variant.name()
        )
    });
    assert!(run.yields > 0, "the workers yielded");
    run
}

/// How many calls of `runs` answered as `answered` picks.
fn calls_answering(runs: &[Run], answered: fn(&Outcome) -> bool) -> usize {
    (runs.iter().flat_map(|run| &run.calls))
        .filter(|call| answered(&call.outcome))
        .count()
}

/// Every mix with operations dropped part way where their callers may drop them, on two seeds:
/// each run replays to the State, and some calls were dropped, which the model held to what their
/// records tell.
#[test]
fn every_mix_with_drops_replays_to_the_state() {
    let runs: Vec<Run> = (Mix::EVERY.into_iter())
        .flat_map(|mix| [3, 8].map(|seed| replayed_as(mix, seed, Variant::Drop)))
        .collect();
    let dropped = calls_answering(&runs, |outcome| *outcome == Outcome::Dropped);
    assert!(dropped > 0, "no call was dropped");
}

/// Every mix the fault variant runs ([`Mix::runs`]), with the store failing reads after its first
/// burst, on two seeds: each run replays to the State, and some calls failed on a failed read,
/// which the model held to what their records tell.
#[test]
fn every_mix_with_faults_replays_to_the_state() {
    let runs: Vec<Run> = (Mix::EVERY.into_iter())
        .filter(|mix| mix.runs(Variant::Fault))
        .flat_map(|mix| [4, 9].map(|seed| replayed_as(mix, seed, Variant::Fault)))
        .collect();
    let failed = calls_answering(&runs, |outcome| matches!(outcome, Outcome::Failed(_)));
    assert!(failed > 0, "no call failed");
}

/// Asserts `records` hold a record of each kind `named`: the mix did what it is for.
fn made<'a>(records: impl IntoIterator<Item = &'a Record>, named: &[&str]) {
    let kinds = kinds(records);
    for name in named {
        assert!(
            kinds.contains_key(*name),
            "no record '{name}' in the mix's runs: {kinds:#?}"
        );
    }
}

/// The records of `runs` on the State each run changed.
fn on_states(runs: &[Run]) -> impl Iterator<Item = &Record> {
    runs.iter().flat_map(Run::on_state)
}

/// Asserts some run holds a `Word` of the node its tree built at `path`, in `section`: the
/// operation on it wrote.
fn wrote(runs: &[Run], path: &str, section: Section) {
    assert!(
        runs.iter().any(|run| run.on_state().any(|record| {
            record.section == section
                && matches!(record.effect, Effect::Word { node, .. } if node == run.id(path))
        })),
        "no Word of '{path}' in {section:?}"
    );
}

/// Whether in some run a task's span on the run's State, from a record `opens` picks to its
/// task's next record `closes` picks, held a record of another task `inside` picks: the mix's
/// kinds ran interleaved, not one after another.
fn interleaved(
    runs: &[Run],
    opens: impl Fn(&Effect) -> bool,
    closes: impl Fn(&Effect) -> bool,
    inside: impl Fn(&Record) -> bool,
) -> bool {
    runs.iter().any(|run| {
        let log: Vec<&Record> = run.on_state().collect();
        log.iter().enumerate().any(|(at, open)| {
            if !opens(&open.effect) {
                return false;
            }
            let Some(close) = log[at + 1..]
                .iter()
                .position(|record| record.task == open.task && closes(&record.effect))
            else {
                return false;
            };
            log[at + 1..at + 1 + close]
                .iter()
                .any(|record| record.task != open.task && inside(record))
        })
    })
}

#[test]
fn the_exclusive_claims_changes_replay_to_the_state() {
    let runs: Vec<Run> = [3, 5, 9]
        .into_iter()
        .map(|seed| replayed(Mix::Exclusive, seed))
        .collect();
    made(on_states(&runs), Mix::Exclusive.makes());
    wrote(&runs, "q/s", Section::UnservedClear);
    wrote(&runs, "v", Section::UnservedClear);
    wrote(&runs, "q/w", Section::Rewrite);
    linked_through(&runs);
    // Another task's grab or free within a discard's span, from its add's grab to its free.
    assert!(
        interleaved(
            &runs,
            |effect| matches!(effect, Effect::Grab { .. }),
            |effect| matches!(effect, Effect::Free { .. }),
            |record| matches!(record.effect, Effect::Grab { .. } | Effect::Free { .. }),
        ),
        "the changes ran interleaved"
    );
}

#[test]
fn the_exclusive_claims_changes_across_blocks_replay_to_the_state() {
    let runs: Vec<Run> = [4, 6, 8]
        .into_iter()
        .map(|seed| replayed(Mix::ExclusiveAcrossBlocks, seed))
        .collect();
    made(on_states(&runs), Mix::ExclusiveAcrossBlocks.makes());
    let across = on_states(&runs).any(|record| {
        matches!(record.effect, Effect::Detach { node, parent, how: How::Discard, .. }
            if NodeBlock::index(node) != NodeBlock::index(parent))
    });
    assert!(
        across,
        "a discard unlinked a node from a parent in another block"
    );
    linked_through(&runs);
}

/// Asserts that in some run of `runs` the listings through the link, each walking a revision a load
/// opened afresh, which the model held to a pin the link held, met more than one pin: the listings
/// ran beside the repins. Each pin the mix makes exposes a node holding another count of children,
/// so the counts tell the pins apart. And that some resolution answered a node through the link,
/// held to a pin the link held from its hop to it. That the resolutions of one path meet more than
/// one pin is the explorations' to show: the chaos timing of the blocks-crossing variant had every
/// run's resolutions of each path meet one pin. And that some collection of `q`'s dirty paths
/// followed the dirty link into the revision holding markers it pins as the tree is built.
fn linked_through(runs: &[Run]) {
    let below = path_spelled("l/a");
    assert!(
        (runs.iter().flat_map(|run| run.calls.iter())).any(|call| call.paths.contains(&below)),
        "no collection of 'q' followed the dirty link"
    );
    let mut resolved = runs.iter().flat_map(|run| run.calls.iter());
    assert!(
        resolved.any(|call| matches!(call.outcome, Outcome::Answer(Answer::Linked { .. }))),
        "no resolution answered a node through the link"
    );
    let listed: Vec<BTreeSet<usize>> = (runs.iter())
        .map(|run| {
            let walks = run.calls.iter().flat_map(|call| call.walks.iter());
            let through = walks.filter(|walk| walk.over == Over::Link);
            through.map(|walk| walk.yielded.len()).collect()
        })
        .collect();
    assert!(
        listed.iter().any(|counts| counts.len() > 1),
        "no run's listings through the link met more than one pin: {listed:?}"
    );
}

#[test]
fn the_shared_claims_changes_replay_to_the_state() {
    let runs: Vec<Run> = [2, 7, 11]
        .into_iter()
        .map(|seed| replayed(Mix::Shared, seed))
        .collect();
    made(on_states(&runs), Mix::Shared.makes());
    // Another task's publish while one held the moves permit.
    assert!(
        interleaved(
            &runs,
            |effect| *effect == Effect::Permit,
            |effect| *effect == Effect::Unpermit,
            |record| matches!(record.effect, Effect::Publish { .. }),
        ),
        "the changes ran interleaved"
    );
    // A staged read that found a staged child, held to the directory. One that a C API change
    // lands within is the explorations' to witness: here the changes are few and slow beside a
    // read, so one landing within it is too rare to count on.
    assert!(
        (runs.iter().flat_map(|run| &run.calls)).any(|call| {
            std::ptr::eq(call.kind, Flagged::Staged.kind())
                && call.outcome == Outcome::Answer(Answer::Has(true))
        }),
        "no staged read found a staged child"
    );
    // A collection that collected a path, held to its names and to the directory.
    assert!(
        collected_some(&runs),
        "no collection of the dirty paths collected one"
    );
}

#[test]
fn served_changes_beside_a_settle_replay_to_the_state() {
    let runs: Vec<Run> = [2, 7, 13]
        .into_iter()
        .map(|seed| replayed(Mix::ServedSettle, seed))
        .collect();
    made(on_states(&runs), Mix::ServedSettle.makes());
    // A served change of `b` within the settle's window: the pending clear the settle set,
    // dropped by another task's change while it stood, before the settle's last phase.
    let landed = runs.iter().any(|run| {
        // The task whose `Word` of `b` set its pending clear, while the clear stands.
        let mut set_by = None;
        run.on_state().any(|record| match record.effect {
            Effect::Word {
                node,
                clear_pending,
                ..
            } if node == run.id("b") => {
                let dropped = !clear_pending && set_by.is_some_and(|task| task != record.task);
                set_by = if clear_pending {
                    set_by.or(Some(record.task))
                } else {
                    None
                };
                dropped
            }
            _ => false,
        })
    });
    assert!(
        landed,
        "a served change of 'b' landed in the settle's window"
    );
    // A created file renamed over a revision's file took its path over (H20): a modified file's
    // `Word` in the section of its move over a holder.
    let took_over = runs.iter().any(|run| {
        let replacing: BTreeSet<(u64, u64, NodeID)> = run
            .on_state()
            .filter_map(|record| match record.effect {
                Effect::Move {
                    node,
                    replaced: Some(_),
                    ..
                } => Some((record.state, record.seq, node)),
                _ => None,
            })
            .collect();
        run.on_state().any(|record| {
            matches!(&record.effect, Effect::Word { node, record: words, .. }
                if words.0.is_file()
                    && words.0.is_dirty_modify()
                    && replacing.contains(&(record.state, record.seq, *node)))
        })
    });
    assert!(took_over, "a created file took a revision file's path over");
    // A settle that cleared a directory, which the checkpoint held to its committed one (C11c).
    assert!(
        (runs.iter().flat_map(|run| &run.calls)).any(|call| {
            std::ptr::eq(call.kind, &crate::harness::catalogue::SETTLE)
                && call.outcome == Outcome::Done(Answer::None)
        }),
        "no settle cleared a directory"
    );
}

#[test]
fn served_changes_beside_a_swap_replay_to_the_state() {
    let holders = [
        (3, Mix::ServedSwapCopy),
        (5, Mix::ServedSwapDetach),
        (8, Mix::ServedSwapRewrite),
        (12, Mix::ServedFlip),
    ];
    let runs: Vec<Run> = holders
        .into_iter()
        .map(|(seed, mix)| {
            let run = replayed(mix, seed);
            made(
                run.on_state().filter(|record| record.task == Some(0)),
                mix.makes(),
            );
            run
        })
        .collect();
    // Another task's record within a holder's span, from its first record to its last.
    let overlapped = runs.iter().any(|run| {
        let log: Vec<&Record> = run.on_state().collect();
        let first = log.iter().position(|record| record.task == Some(0));
        let last = log.iter().rposition(|record| record.task == Some(0));
        first.zip(last).is_some_and(|(first, last)| {
            log[first..last].iter().any(|record| record.task != Some(0))
        })
    });
    assert!(
        overlapped,
        "a holder's change ran interleaved with the served changes"
    );
}

#[test]
fn listings_beside_served_changes_of_their_directories_replay_to_the_state() {
    let runs: Vec<Run> = [4, 9, 17]
        .into_iter()
        .map(|seed| replayed(Mix::Listing, seed))
        .collect();
    made(on_states(&runs), Mix::Listing.makes());
    // A walk's head read that another task's counted change of its directory followed within the
    // walk's call, which the model held the walk to.
    let met = runs.iter().any(|run| {
        run.calls.iter().any(|call| {
            call.walks.iter().any(|walk| {
                call.head_read(walk, &run.log, &[]).is_some_and(|head| {
                    run.on_state().any(|record| {
                        record.task != call.task
                            && head < record.seq
                            && record.seq < call.end
                            && changes_chain_of(&record.effect, walk.dir)
                    })
                })
            })
        })
    });
    assert!(
        met,
        "no walk's head read was followed by another task's change of its directory"
    );
    // A listing reader's create or rename through its absence, and one the log since its lookup
    // refused.
    assert!(
        on_states(&runs).any(|record| through_absence(&record.effect)),
        "no listing reader's change went through its absence"
    );
    assert!(
        on_states(&runs).any(|record| refused_through_absence(&record.effect)),
        "no listing reader's change was refused through its absence"
    );
    // A search since a mark that found a node, the task's own create among them, one made stale
    // by a reattach since the mark, and a mark found reattached, which the model held to the count.
    let answered = |answered: fn(&Answer) -> bool| {
        (runs.iter().flat_map(|run| &run.calls))
            .any(|call| matches!(&call.outcome, Outcome::Answer(answer) if answered(answer)))
    };
    assert!(
        answered(|answer| matches!(answer, Answer::Since(Some(_)))),
        "no search since a mark found a node"
    );
    assert!(
        answered(|answer| *answer == Answer::Stale),
        "no search since a mark was stale"
    );
    assert!(
        answered(|answer| *answer == Answer::Reattached(true)),
        "no mark was found reattached"
    );
    // A name read, a comparison by name and a path read answered, each held to its records.
    assert!(
        answered(|answer| matches!(answer, Answer::Name(Some(_)))),
        "no name read answered a name"
    );
    assert!(
        answered(|answer| matches!(answer, Answer::NameIs(_))),
        "no comparison by name answered"
    );
    assert!(
        answered(|answer| matches!(answer, Answer::Path(Some(_)))),
        "no path read answered a path"
    );
    // A dirty read that found a dirty child and one that found none, each held to the directory.
    // One another task's change lands within is the explorations' to witness: a read runs
    // unbroken in microseconds, and under a heavy load none that found a child met one in a run.
    for has in [true, false] {
        assert!(
            (runs.iter().flat_map(|run| &run.calls)).any(|call| {
                std::ptr::eq(call.kind, Flagged::Dirty.kind())
                    && call.outcome == Outcome::Answer(Answer::Has(has))
            }),
            "no dirty read answered {has}"
        );
    }
    assert!(
        collected_some(&runs),
        "no collection of the dirty paths collected one"
    );
}

/// Whether a collection of the dirty paths in `runs` collected one.
fn collected_some(runs: &[Run]) -> bool {
    (runs.iter().flat_map(|run| &run.calls))
        .any(|call| call.outcome == Outcome::Answer(Answer::Collected) && !call.paths.is_empty())
}
