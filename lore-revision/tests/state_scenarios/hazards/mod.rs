// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A scenario for each of these hazards from the node module's list (`lore_revision::state::node`).
//! Other scenarios and unit tests that pin a hazard cite it by number; H19 and H25, which a VFS
//! provider closes, have none.

mod h1;
mod h13;
mod h21;
mod h22;
mod h4;
mod h6;
mod h7;
mod h8;
mod h9;

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::Resource;

/// A flag one op sets and another reads, recorded for the scheduler as a resource of its own.
pub(crate) struct Flag(AtomicBool, &'static str);

impl Flag {
    pub(crate) const fn new(name: &'static str) -> Self {
        Self(AtomicBool::new(false), name)
    }

    pub(crate) fn set(&self) {
        hooks::touched(Resource::Named(self.1), true);
        self.0.store(true, Ordering::Relaxed);
    }

    pub(crate) fn get(&self) -> bool {
        hooks::touched(Resource::Named(self.1), false);
        self.0.load(Ordering::Relaxed)
    }
}
