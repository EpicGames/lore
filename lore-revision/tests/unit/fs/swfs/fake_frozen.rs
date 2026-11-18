// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_revision::fs::swfs::api_interface::SwfsInterfaceError;
use lore_revision::fs::swfs::frozen_files::FrozenFile;
use lore_revision::fs::swfs::frozen_files::FrozenReader;

/// SWFS as a test describes it: the files it lists, their content, and what its reads cost.
#[derive(Default)]
pub struct FakeFrozen {
    pub files: Vec<FrozenFile>,
    /// What the modified list holds beyond the frozen files, such as a move's origin.
    pub modified: Vec<FrozenFile>,
    pub content: HashMap<String, Vec<u8>>,
    pub fail_list: bool,
    pub fail_read: bool,
    pub reads: AtomicUsize,
    pub read_paths: parking_lot::Mutex<Vec<String>>,
    /// The most bytes any one read asked for.
    pub largest_read: AtomicUsize,
}

impl FakeFrozen {
    pub fn with_file(mut self, path: &str, content: &[u8]) -> Self {
        self.files.push(FrozenFile {
            path: path.to_string(),
            mod_flags: 1 << 15,
            file_index: self.files.len() as u32 + 1,
            size: content.len() as u64,
            time_create: 5,
            time_write: 7,
            is_directory: false,
        });
        self.content.insert(path.to_string(), content.to_vec());
        self
    }

    pub fn with_directory(mut self, path: &str) -> Self {
        self.files.push(FrozenFile {
            path: path.to_string(),
            mod_flags: 1 << 15,
            file_index: self.files.len() as u32 + 1,
            size: 0,
            time_create: 5,
            time_write: 7,
            is_directory: true,
        });
        self
    }

    /// Listed, but SWFS has no content to give for it, as for a renamed file never written.
    pub fn with_unreadable_file(mut self, path: &str, size: u64) -> Self {
        self.files.push(FrozenFile {
            path: path.to_string(),
            mod_flags: 0x406,
            file_index: self.files.len() as u32 + 1,
            size,
            time_create: 5,
            time_write: 7,
            is_directory: false,
        });
        self
    }
}

impl FakeFrozen {
    /// Listed as the frozen list lists a file renamed without being written: changed while frozen,
    /// and with no bytes of its own for SWFS to give.
    pub fn with_unreadable_frozen_file(mut self, path: &str, size: u64) -> Self {
        self.files.push(FrozenFile {
            path: path.to_string(),
            mod_flags: 1 << 15,
            file_index: self.files.len() as u32 + 1,
            size,
            time_create: 5,
            time_write: 7,
            is_directory: false,
        });
        self
    }

    /// The modified list names `path` as renamed, with no origin, as SWFS lists a renamed file once
    /// another node holds its origin's path.
    pub fn with_renamed(mut self, path: &str) -> Self {
        self.modified.push(FrozenFile {
            path: path.to_string(),
            mod_flags: 0x406,
            file_index: 200 + self.modified.len() as u32,
            size: 0,
            time_create: 5,
            time_write: 7,
            is_directory: false,
        });
        self
    }

    /// The modified list names a move of `from` to `to`: the destination renamed and the origin
    /// deleted, both carrying the node's `file_index`.
    pub fn with_move(mut self, from: &str, to: &str) -> Self {
        let file_index = 100 + self.modified.len() as u32;
        for (path, mod_flags) in [(to, 0x406), (from, 0x40a)] {
            self.modified.push(FrozenFile {
                path: path.to_string(),
                mod_flags,
                file_index,
                size: 0,
                time_create: 5,
                time_write: 7,
                is_directory: false,
            });
        }
        self
    }
}

impl FrozenReader for FakeFrozen {
    fn modified(&self) -> Result<Vec<FrozenFile>, SwfsInterfaceError> {
        Ok(self.modified.clone())
    }

    fn files(&self) -> Result<Vec<FrozenFile>, SwfsInterfaceError> {
        if self.fail_list {
            return Err(SwfsInterfaceError::internal("listing failed"));
        }
        Ok(self.files.clone())
    }

    fn read_range(
        &self,
        path: &str,
        offset: u64,
        into: &mut [u8],
    ) -> Result<bool, SwfsInterfaceError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.read_paths.lock().push(path.to_string());
        self.largest_read.fetch_max(into.len(), Ordering::Relaxed);
        if self.fail_read {
            return Err(SwfsInterfaceError::internal("read failed"));
        }
        let Some(content) = self.content.get(path) else {
            return Ok(false);
        };
        let start = offset as usize;
        let Some(range) = content.get(start..start + into.len()) else {
            return Err(SwfsInterfaceError::internal("read past the end"));
        };
        into.copy_from_slice(range);
        Ok(true)
    }
}
