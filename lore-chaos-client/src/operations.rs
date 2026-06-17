// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod writer;

use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum RepoOperationKind {
    Commit,
    GoToBranch,
    CreateBranch,
    Merge,
    Status,
    BranchInfo,
}

impl RepoOperationKind {
    pub const ALL: &'static [Self] = &[
        Self::Commit,
        Self::GoToBranch,
        Self::CreateBranch,
        Self::Merge,
        Self::Status,
        Self::BranchInfo,
    ];

    pub const ALL_READ_ONLY: &'static [Self] = &[Self::Status, Self::BranchInfo];
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RepoOperation {
    Commit(CommitOperation),
    GoToBranch(GoToBranchOperation),
    CreateBranch(CreateBranchOperation),
    Merge(MergeOperation),
    Status(StatusOperation),
    BranchInfo(BranchInfoOperation),
    //MergeInto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FileChangeOperation {
    Add,
    Modify,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitOperation {
    pub changes: Vec<(PathBuf, FileChangeOperation)>,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoToBranchOperation {
    pub target_branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateBranchOperation {
    pub new_branch: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum FileMergeOperationKind {
    Mine,
    Theirs,
    New,
}

impl FileMergeOperationKind {
    pub const ALL: &'static [Self] = &[Self::Mine, Self::Theirs, Self::New];
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FileMergeOperation {
    Mine,
    Theirs,
    New(String),
}

impl FileMergeOperation {
    pub fn from_kind(kind: FileMergeOperationKind, contents: impl FnOnce() -> String) -> Self {
        match kind {
            FileMergeOperationKind::Mine => Self::Mine,
            FileMergeOperationKind::Theirs => Self::Theirs,
            FileMergeOperationKind::New => Self::New(contents()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeOperation {
    pub target_branch: String,
    pub resolutions: Vec<(PathBuf, FileMergeOperation)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusOperation {
    pub staged: bool,
    pub unstaged: bool,
    pub sync_point: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchInfoOperation {
    pub name: Option<String>,
}
