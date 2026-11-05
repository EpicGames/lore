// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The orphans: nodes standing below a detached root that the root's detach should have kept out,
//! classified from the model by how the root was detached. A node joins a chain by its publish or
//! by a move.
//!
//! - Below a root detached as it stood (`IfEmpty`, `IfUnchanged`, a replaced holder), a node that
//!   joined after the detach: the nodes already below stay out, as the emptiness checks skip some
//!   children.
//! - Below an `IfClean` root, which closure makes clean with its whole subtree, a dirty node (H7).
//!   The action bits alone are no dirt: a staged node carries them, and the detach reads the dirty
//!   bit alone.
//! - On a served State, a node that joined below any detached root after the detach, whatever its
//!   words; below a root detached before the model started, any node that joined since.
//! - Below an unserved `Always` or `At` root, none: a pending root is detached `At`, and a staged
//!   add may still publish into a pending subtree. Below an unserved root detached before the
//!   start, none, as how it was detached is not known.
//! - Below a `Discard` root left partly freed, what stands.
//!
//! A served State's are violations, but for those of a rename's window, where a VFS rename over a
//! directory lets a create still land in the directory it replaces: below a replaced holder that
//! held nodes at the move, a child made inside it in the window explains a node that joined after.
//! An unserved State's are counted.

use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::state::node::hooks::log::How;

use super::StateModel;
use super::Status;
use super::Unchained;

/// A State's orphans, by how they count.
#[derive(Clone, Debug, Default)]
pub struct Orphans {
    /// A served State's, each named with its root and its detach.
    pub violations: Vec<String>,
    /// An unserved State's.
    pub counted: Vec<NodeID>,
    /// A served State's in a rename's window.
    pub window: Vec<NodeID>,
}

/// How a root came out of every chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Root {
    Detached(How),
    Replaced,
    /// Before the model started.
    Unknown,
}

impl StateModel {
    /// Every orphan below the State's detached roots, as the module doc classifies them.
    pub fn orphans(&self) -> Orphans {
        let mut orphans = Orphans::default();
        for root in self.nodes() {
            let Some(slot) = self.slot(root) else {
                continue;
            };
            let Some(detachment) = slot.detached else {
                continue;
            };
            let how = match slot.status {
                Status::Unchained(Unchained::Detached(how)) => Root::Detached(how),
                Status::Unchained(Unchained::Replaced) => Root::Replaced,
                Status::Unchained(Unchained::AtStart) => Root::Unknown,
                _ => continue,
            };
            for member in self.subtree(root).into_iter().skip(1) {
                let Some(below) = self.slot(member) else {
                    continue;
                };
                if below.status == Status::Free {
                    continue;
                }
                let after = below
                    .joined_at
                    .is_some_and(|joined| how == Root::Unknown || joined > detachment.seq);
                let dirty = below.record.flags & NodeFlags::Dirty.bits() != 0;
                let dirty_below_clean = how == Root::Detached(How::IfClean) && dirty;
                if detachment.served {
                    if how == Root::Replaced && detachment.held && after {
                        orphans.window.push(member);
                    } else if after || dirty_below_clean {
                        let why = if after {
                            "joined after the detach"
                        } else {
                            "dirty below a root detached clean"
                        };
                        orphans.violations.push(format!(
                            "State {}: {member} stands below {root}, {how:?} at seq {}: {why}",
                            self.id, detachment.seq
                        ));
                    }
                    continue;
                }
                let orphan = match how {
                    Root::Replaced | Root::Detached(How::IfEmpty | How::IfUnchanged) => after,
                    Root::Detached(How::IfClean) => dirty,
                    Root::Detached(How::Discard) => true,
                    Root::Detached(How::Always | How::At) | Root::Unknown => false,
                };
                if orphan {
                    orphans.counted.push(member);
                }
            }
        }
        orphans
    }
}
