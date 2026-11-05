// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A stress harness for `State`'s node operations (`lore_revision::state::node`), run by hand
//! rather than in CI. See `scripts/state-stress/README.md` for how the driver runs it.
//!
//! - `chaos` runs mixes of operations over and over until its time is up: each run builds its
//!   mix's tree, runs a random number of rounds of the mix's operations in a random order, one
//!   task each, all at once on a runtime of a random number of workers that yield at random
//!   scheduling points, in a random variant, which may drop operations or fail the store's reads,
//!   then replays the run's log through the model and checks the State it ends with.
//! - `list` names the mixes and the operations of each.
//!
//! The deterministic scenarios, lore-revision's `tests/state_scenarios` and the C API's in the
//! `lore` package, share this harness, and the driver runs them.

mod args;
#[allow(
    dead_code,
    unused_imports,
    reason = "shared with the deterministic scenarios, which use what the harness's modes do not"
)]
mod harness;
mod rounds;

use std::process::ExitCode;

fn usage() -> ExitCode {
    eprintln!(
        "usage: state_stress chaos [--seconds N] [--runs N] [--mix <name|all>] \
         [--variant <name|all>] [--seed S] [--run-seed S] [--workers W] [--every E] [--rounds R] \
         [--hang-seconds H] [--stop-on-violation] | list [--ops]\n\
         see scripts/state-stress/README.md"
    );
    ExitCode::from(rounds::USAGE)
}

fn main() -> ExitCode {
    let args = args::Args::parse(std::env::args().skip(1));
    match args.positional.first().map(String::as_str) {
        Some("chaos") => ExitCode::from(rounds::main(&args)),
        Some("list") => {
            rounds::list(&args);
            ExitCode::SUCCESS
        }
        _ => usage(),
    }
}
