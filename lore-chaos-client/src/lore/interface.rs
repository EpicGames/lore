// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Error;
use lore::branch;
use lore::branch::LoreBranchInfoArgs;
use lore::branch::LoreBranchMergeResolveArgs;
use lore::branch::LoreBranchMergeResolveMineArgs;
use lore::branch::LoreBranchMergeResolveTheirsArgs;
use lore::branch::LoreBranchSwitchArgs;
use lore::file;
use lore::interface::LoreArray;
use lore::interface::LoreBranchCreateArgs;
use lore::interface::LoreBranchMergeStartArgs;
use lore::interface::LoreErrorEventData;
use lore::interface::LoreEvent;
use lore::interface::LoreFileStageArgs;
use lore::interface::LoreGlobalArgs;
use lore::interface::LoreLogConfig;
use lore::interface::LoreLogEventData;
use lore::interface::LoreLogLevel;
use lore::interface::LoreRepositoryStatusArgs;
use lore::interface::LoreString;
use lore::repository;
use lore::revision;
use lore::revision::LoreRevisionCommitArgs;
use parking_lot::Mutex;
use tracing::Span;
use tracing::error;
use tracing::info;

use crate::chaos::config::RunnerConfig;
use crate::chaos::files::add_or_modify;
use crate::operations::BranchInfoOperation;
use crate::operations::FileMergeOperation;
use crate::operations::StatusOperation;

#[derive(Debug, Clone)]
pub struct LoreInterface {
    global_args: LoreGlobalArgs,
}

impl LoreInterface {
    pub fn new(repo_path: LoreString, offline: bool) -> Self {
        Self {
            global_args: LoreGlobalArgs {
                repository_path: repo_path.clone(),
                offline: offline as u8,
                ..Default::default()
            },
        }
    }

    fn handle_lore_error(error: &LoreErrorEventData) {
        // Cannot use `self` because this needs to be called from task callbacks.
        error!(
            error.error_type,
            error_inner = error.error_inner.to_string(),
            "Lore Error"
        );
        panic!("Lore Error: {} {}", error.error_type, error.error_inner);
    }

    fn handle_lore_log(data: &LoreLogEventData) {
        if data.level >= LoreLogLevel::Info {
            println!("{}", data.message.as_str());
        }
    }

    fn handle_logic_error(error: &Error) {
        error!(?error, "Logic Error");
        panic!("Logic Error: {error}");
    }
}

macro_rules! error_and_log {
    ($span:ident, $event:ident) => {{
        let _span_guard = $span.clone().entered();
        match $event {
            LoreEvent::Error(error) => {
                Self::handle_lore_error(&error);
            }
            LoreEvent::Log(data) => {
                Self::handle_lore_log(&data);
            }
            _ => {}
        }
    }};
}

/// Operations that should change the state of the repo.
impl LoreInterface {
    pub fn setup() {
        lore::log::configure(&LoreLogConfig::default());
    }

    pub fn merge(&mut self, other_branch: &LoreString) -> anyhow::Result<Vec<LoreString>> {
        let has_conflicts = Arc::new(Mutex::new(None));
        let has_conflicts_clone = has_conflicts.clone();
        let conflicts = Arc::new(Mutex::new(Vec::new()));
        let conflicts_clone = conflicts.clone();
        lore::runtime().block_on(branch::merge_start(
            self.global_args.clone(),
            LoreBranchMergeStartArgs {
                branch: other_branch.clone(),
                message: Default::default(),
                no_commit: 0,
                link: Default::default(),
                ignore_links: 0,
            },
            Some(Box::new(move |event| match event {
                LoreEvent::BranchMergeStartEnd(event) => {
                    *has_conflicts_clone.lock() = Some(event.has_conflicts != 0);
                }
                LoreEvent::BranchMergeConflictFile(data) => {
                    conflicts_clone.lock().push(data.path.clone());
                }
                LoreEvent::Error(error) => {
                    Self::handle_lore_error(error);
                }
                _ => {}
            })),
        ));
        if let Some(has_conflicts) = *has_conflicts.lock() {
            let conflicts = conflicts.lock();
            if has_conflicts == conflicts.is_empty() {
                let error = anyhow::anyhow!(
                    "Has conflicts is {has_conflicts} but the set of conflicts is {conflicts:?}"
                );
                Self::handle_logic_error(&error);
            }
            Ok(conflicts.clone())
        } else {
            let error = anyhow::anyhow!("No conflicts set");
            Self::handle_logic_error(&error);
            Err(error)
        }
    }

    pub fn switch_branch_to(&mut self, branch: &LoreString) {
        let span = Span::current();
        lore::runtime().block_on(branch::switch(
            self.global_args.clone(),
            LoreBranchSwitchArgs {
                branch: branch.clone(),
                revision: Default::default(),
                reset: 0,
                bare: 0,
            },
            Some(Box::new(move |event| error_and_log!(span, event))),
        ));
    }

    pub fn create_branch(&mut self, branch: &LoreString) {
        let span = Span::current();
        lore::runtime().block_on(branch::create(
            self.global_args.clone(),
            LoreBranchCreateArgs {
                branch: branch.clone(),
                category: LoreString::default(),
                id: LoreString::default(),
            },
            Some(Box::new(move |event| error_and_log!(span, event))),
        ));
    }

    pub fn stage_file(&mut self, files: LoreArray<LoreString>) {
        let span = Span::current();
        lore::runtime().block_on(file::stage(
            self.global_args.clone(),
            LoreFileStageArgs {
                paths: files,
                case_change: 0,
                scan: 1,
            },
            Some(Box::new(move |event| error_and_log!(span, event))),
        ));
    }

    pub fn commit(&mut self, description: LoreString, force: bool) {
        let span = Span::current();
        lore::runtime().block_on(revision::commit(
            LoreGlobalArgs {
                force: force as u8,
                ..self.global_args.clone()
            },
            LoreRevisionCommitArgs {
                message: description,
                link: Default::default(),
                ..Default::default()
            },
            Some(Box::new(move |event| error_and_log!(span, event))),
        ));
    }

    pub fn merge_file(
        &mut self,
        config: &RunnerConfig,
        file: &PathBuf,
        operation: &FileMergeOperation,
    ) -> std::io::Result<()> {
        info!("Merging file {file:?} with {operation:?}");
        let span = Span::current();
        let files = LoreArray::from_vec(vec![LoreString::from(file.to_str().unwrap())]);
        match operation {
            FileMergeOperation::Mine => {
                lore::runtime().block_on(branch::merge_resolve_mine(
                    self.global_args.clone(),
                    LoreBranchMergeResolveMineArgs { paths: files },
                    Some(Box::new(move |event| error_and_log!(span, event))),
                ));
            }
            FileMergeOperation::Theirs => {
                lore::runtime().block_on(branch::merge_resolve_theirs(
                    self.global_args.clone(),
                    LoreBranchMergeResolveTheirsArgs { paths: files },
                    Some(Box::new(move |event| error_and_log!(span, event))),
                ));
            }
            FileMergeOperation::New(contents) => {
                add_or_modify(file, config, contents)?;
                lore::runtime().block_on(branch::merge_resolve(
                    self.global_args.clone(),
                    LoreBranchMergeResolveArgs { paths: files },
                    Some(Box::new(move |event| error_and_log!(span, event))),
                ));
            }
        }
        Ok(())
    }

    pub fn finish_merge(&self) {
        let span = Span::current();
        lore::runtime().block_on(revision::commit(
            self.global_args.clone(),
            LoreRevisionCommitArgs {
                message: "Merge finish".into(),
                link: Default::default(),
                ..Default::default()
            },
            Some(Box::new(move |event| error_and_log!(span, event))),
        ));
    }

    pub fn status(&self, operation: &StatusOperation) {
        let span = Span::current();
        lore::runtime().block_on(repository::status(
            self.global_args.clone(),
            LoreRepositoryStatusArgs {
                staged: operation.staged as u8,
                scan: operation.unstaged as u8,
                check_dirty: 0,
                reset: 0,
                sync_point: operation.sync_point as u8,
                revision_only: 0,
                count: 0,
                paths: Default::default(),
            },
            Some(Box::new(move |event| error_and_log!(span, event))),
        ));
    }

    pub fn branch_info(&self, operation: &BranchInfoOperation) {
        let span = Span::current();
        lore::runtime().block_on(branch::info(
            self.global_args.clone(),
            LoreBranchInfoArgs {
                branch: operation
                    .name
                    .clone()
                    .map(LoreString::from)
                    .unwrap_or_default(),
            },
            Some(Box::new(move |event| error_and_log!(span, event))),
        ));
    }
}
