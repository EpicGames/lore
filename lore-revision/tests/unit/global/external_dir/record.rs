// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures write the move record directly; what these test is how it is read.
#![allow(clippy::disallowed_methods)]

use lore_base::test_util::TempDir;
use lore_revision::global::external_dir::record::*;

/// A saved record reads back as it was written, and an absent one reads as none.
#[tokio::test]
async fn a_record_round_trips_and_an_absent_one_is_none() {
    let global = TempDir::new("lore-external-record-");
    assert_eq!(MoveRecord::load(global.path()).expect("read"), None);

    let saved = MoveRecord {
        from: String::new(),
        to: "/mnt/work/lore-external".to_owned(),
        kind: MoveKind::Copy,
        phase: MovePhase::SetAside,
    };
    saved.save(global.path()).await.expect("save");

    assert_eq!(MoveRecord::load(global.path()).expect("read"), Some(saved));
}

/// Each kind and phase is written in lower case, with words joined by a hyphen.
#[test]
fn every_kind_and_phase_parses_from_its_name() {
    let kinds = [
        ("rename", MoveKind::Rename),
        ("copy", MoveKind::Copy),
        ("create", MoveKind::Create),
        ("adopt", MoveKind::Adopt),
        ("abandon", MoveKind::Abandon),
    ];
    let phases = [
        ("mark", MovePhase::Mark),
        ("identify", MovePhase::Identify),
        ("copy", MovePhase::Copy),
        ("place", MovePhase::Place),
        ("switch", MovePhase::Switch),
        ("unmark", MovePhase::Unmark),
        ("set-aside", MovePhase::SetAside),
        ("redirect", MovePhase::Redirect),
    ];
    let global = TempDir::new("lore-external-record-names-");
    for (kind_name, kind) in kinds {
        for (phase_name, phase) in phases {
            std::fs::write(
                global.path().join(MOVE_RECORD),
                format!(
                    "from = \"\"\nto = \"/target\"\nkind = \"{kind_name}\"\nphase = \"{phase_name}\"\n"
                ),
            )
            .expect("write the record");

            let record = MoveRecord::load(global.path())
                .expect("read")
                .expect("present");

            assert_eq!((record.kind, record.phase), (kind, phase));
        }
    }
}
