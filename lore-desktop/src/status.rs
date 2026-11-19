// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The working tree status view's model: what a status call reported, folded from its events.

use lore::interface::LoreEvent;
use lore::interface::LoreFileAction;

/// Where the active branch stands against its remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteState {
    /// No remote is configured, or it can't be reached.
    Unavailable,
    /// The remote is reachable but did not answer for this identity.
    Unauthorized,
    /// The branch does not exist on the remote.
    NoBranch,
    /// Local and remote hold the same history.
    InSync,
    /// Local holds revisions the remote does not.
    Ahead,
    /// The remote holds revisions local does not.
    Behind,
    /// Each side holds revisions the other does not.
    Diverged,
}

impl RemoteState {
    /// Classifies the flags a status call reports for the active branch, as `lore status` reads
    /// them.
    pub fn classify(
        available: bool,
        authorized: bool,
        branch_exists: bool,
        local_ahead: bool,
        remote_ahead: bool,
    ) -> Self {
        match (local_ahead, remote_ahead) {
            (true, true) => return Self::Diverged,
            (true, false) => return Self::Ahead,
            (false, true) => return Self::Behind,
            (false, false) => {}
        }
        if !available {
            Self::Unavailable
        } else if !authorized {
            Self::Unauthorized
        } else if !branch_exists {
            Self::NoBranch
        } else {
            Self::InSync
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Unavailable => "Remote unavailable",
            Self::Unauthorized => "Remote reachable, not authorized",
            Self::NoBranch => "Branch not on remote",
            Self::InSync => "In sync with remote",
            Self::Ahead => "Ahead of remote",
            Self::Behind => "Behind remote",
            Self::Diverged => "Diverged from remote, sync to merge",
        }
    }
}

/// The section of the status view a file is listed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// Staged for the next commit.
    Staged,
    /// Staged by a merge, with its conflict not yet resolved.
    Unmerged,
    /// Changed in the working tree but not staged.
    Unstaged,
}

impl Section {
    pub const ALL: [Self; 3] = [Self::Unmerged, Self::Staged, Self::Unstaged];

    /// The section of a file `lore status` lists under, from its staged and unresolved flags.
    pub fn classify(staged: bool, conflict_unresolved: bool) -> Self {
        match (staged, conflict_unresolved) {
            (true, true) => Self::Unmerged,
            (true, false) => Self::Staged,
            (false, _) => Self::Unstaged,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Staged => "Staged",
            Self::Unmerged => "Unmerged",
            Self::Unstaged => "Not staged",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Unmerged => 0,
            Self::Staged => 1,
            Self::Unstaged => 2,
        }
    }
}

/// The active branch's position, as a status call reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionSummary {
    pub repository: String,
    pub branch: String,
    pub revision_number: u64,
    pub revision: String,
    pub remote: RemoteState,
    /// The incoming revision of a merge in progress.
    pub pending_merge: Option<String>,
}

/// One file a status call reported.
#[derive(Debug, Clone, PartialEq)]
pub struct FileRow {
    pub path: String,
    /// The previous path of a moved or copied file.
    pub from_path: Option<String>,
    pub action: LoreFileAction,
    pub size: u64,
    pub section: Section,
}

/// What a status call reports, copied out of its events, which live only for their callback.
#[derive(Debug, Clone, PartialEq)]
pub enum StatusMessage {
    Revision(RevisionSummary),
    File(FileRow),
    /// The call finished: `Err` carries the library's error message.
    Complete(Result<(), String>),
}

impl StatusMessage {
    /// The message for `event`, or `None` for an event the status view does not show.
    pub fn from_event(event: &LoreEvent) -> Option<Self> {
        match event {
            LoreEvent::RepositoryStatusRevision(data) => {
                let pending_merge = (!data.revision_staged.is_zero()
                    && data.revision_staged != data.revision
                    && !data.revision_merged.is_zero())
                .then(|| data.revision_merged.to_string());
                Some(Self::Revision(RevisionSummary {
                    repository: data.repository.to_string(),
                    branch: data.branch_name.as_str().to_owned(),
                    revision_number: data.revision_number,
                    revision: data.revision.to_string(),
                    remote: RemoteState::classify(
                        data.remote_available != 0,
                        data.remote_authorized != 0,
                        data.remote_branch_exist != 0,
                        data.is_local_ahead != 0,
                        data.is_remote_ahead != 0,
                    ),
                    pending_merge,
                }))
            }
            LoreEvent::RepositoryStatusFile(data) => Some(Self::File(FileRow {
                path: data.path.as_str().to_owned(),
                from_path: (!data.from_path.is_empty()).then(|| data.from_path.as_str().to_owned()),
                action: data.action,
                size: data.size,
                section: Section::classify(
                    data.flag_staged != 0,
                    data.flag_conflict_unresolved != 0,
                ),
            })),
            LoreEvent::Complete(data) => Some(Self::Complete(if data.status == 0 {
                Ok(())
            } else {
                Err(data.error.message.as_str().to_owned())
            })),
            _ => None,
        }
    }
}

/// Where the last status call stands.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Progress {
    /// No call has been made.
    #[default]
    Idle,
    Running,
    Done,
    Failed(String),
}

/// The status view's state: the last status call's results, by section.
#[derive(Debug, Default)]
pub struct StatusModel {
    pub revision: Option<RevisionSummary>,
    sections: [Vec<FileRow>; 3],
    pub progress: Progress,
}

impl StatusModel {
    /// Clears the results of the last call, for a new one.
    pub fn start(&mut self) {
        self.revision = None;
        for files in &mut self.sections {
            files.clear();
        }
        self.progress = Progress::Running;
    }

    /// Folds one message of the running call into the state.
    pub fn apply(&mut self, message: StatusMessage) {
        match message {
            StatusMessage::Revision(revision) => self.revision = Some(revision),
            StatusMessage::File(file) => self.sections[file.section.index()].push(file),
            StatusMessage::Complete(Ok(())) => {
                for files in &mut self.sections {
                    files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
                }
                self.progress = Progress::Done;
            }
            StatusMessage::Complete(Err(message)) => self.progress = Progress::Failed(message),
        }
    }

    pub fn files(&self, section: Section) -> &[FileRow] {
        &self.sections[section.index()]
    }

    pub fn is_running(&self) -> bool {
        self.progress == Progress::Running
    }
}

/// A short label for a file action.
pub fn action_label(action: LoreFileAction) -> &'static str {
    match action {
        LoreFileAction::Keep => "modified",
        LoreFileAction::Add => "added",
        LoreFileAction::Delete => "deleted",
        LoreFileAction::Move => "moved",
        LoreFileAction::Copy => "copied",
    }
}
