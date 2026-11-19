// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore::interface::LoreFileAction;
use lore_desktop::status::FileRow;
use lore_desktop::status::Progress;
use lore_desktop::status::RemoteState;
use lore_desktop::status::Section;
use lore_desktop::status::StatusMessage;
use lore_desktop::status::StatusModel;

fn file(path: &str, section: Section) -> StatusMessage {
    StatusMessage::File(FileRow {
        path: path.to_owned(),
        from_path: None,
        action: LoreFileAction::Keep,
        size: 0,
        section,
    })
}

fn paths(model: &StatusModel, section: Section) -> Vec<&str> {
    model
        .files(section)
        .iter()
        .map(|f| f.path.as_str())
        .collect()
}

#[test]
fn section_follows_staged_and_unresolved_flags() {
    assert_eq!(Section::classify(true, false), Section::Staged);
    assert_eq!(Section::classify(true, true), Section::Unmerged);
    assert_eq!(Section::classify(false, false), Section::Unstaged);
    // An unstaged file is listed as not staged whatever its conflict flag says.
    assert_eq!(Section::classify(false, true), Section::Unstaged);
}

#[test]
fn ahead_and_behind_take_precedence_over_reachability() {
    assert_eq!(
        RemoteState::classify(true, true, true, true, true),
        RemoteState::Diverged
    );
    assert_eq!(
        RemoteState::classify(true, true, true, true, false),
        RemoteState::Ahead
    );
    assert_eq!(
        RemoteState::classify(true, true, true, false, true),
        RemoteState::Behind
    );
    // Offline, a branch with local revisions still reads as ahead, as `lore status` says.
    assert_eq!(
        RemoteState::classify(false, false, false, true, false),
        RemoteState::Ahead
    );
}

#[test]
fn remote_state_without_divergence() {
    assert_eq!(
        RemoteState::classify(false, false, false, false, false),
        RemoteState::Unavailable
    );
    assert_eq!(
        RemoteState::classify(true, false, false, false, false),
        RemoteState::Unauthorized
    );
    assert_eq!(
        RemoteState::classify(true, true, false, false, false),
        RemoteState::NoBranch
    );
    assert_eq!(
        RemoteState::classify(true, true, true, false, false),
        RemoteState::InSync
    );
}

#[test]
fn files_go_to_their_section_and_sort_on_completion() {
    let mut model = StatusModel::default();
    model.start();
    for message in [
        file("b", Section::Staged),
        file("c", Section::Unstaged),
        file("a", Section::Staged),
        file("m", Section::Unmerged),
    ] {
        model.apply(message);
    }
    assert!(model.is_running());
    model.apply(StatusMessage::Complete(Ok(())));

    assert_eq!(model.progress, Progress::Done);
    assert_eq!(paths(&model, Section::Staged), ["a", "b"]);
    assert_eq!(paths(&model, Section::Unmerged), ["m"]);
    assert_eq!(paths(&model, Section::Unstaged), ["c"]);
}

#[test]
fn failure_keeps_the_error_and_start_clears_the_last_call() {
    let mut model = StatusModel::default();
    model.start();
    model.apply(file("a", Section::Unstaged));
    model.apply(StatusMessage::Complete(Err("no instance".to_owned())));
    assert!(matches!(model.progress, Progress::Failed(_)));

    model.start();
    assert_eq!(model.progress, Progress::Running);
    assert!(model.files(Section::Unstaged).is_empty());
    assert!(model.revision.is_none());
}
