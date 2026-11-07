// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Writes `READDIR`/`READDIRPLUS` records directly into the kernel-owned reply buffer.

use std::mem::size_of;

use zerocopy::FromBytes;
use zerocopy::IntoBytes;

use crate::abi::Dirent;
use crate::abi::DirentPlus;
use crate::abi::DirentType;
use crate::abi::EntryOut;
use crate::abi::dir::dirent_size;
use crate::abi::dir::direntplus_size;

/// Packs `READDIR`/`READDIRPLUS` records into a caller-owned buffer, refusing (rather than
/// truncating) any record that would not fit.
pub struct DirentWriter<'a> {
    buffer: &'a mut [u8],
    used: usize,
}

impl<'a> DirentWriter<'a> {
    pub fn new(buffer: &'a mut [u8]) -> Self {
        Self { buffer, used: 0 }
    }

    /// Appends a `READDIR` record. Returns `false`, leaving `self` unchanged, if it would not
    /// fit in the remaining buffer.
    pub fn push(&mut self, ino: u64, off: u64, kind: DirentType, name: &[u8]) -> bool {
        let size = dirent_size(name.len());
        if self.used + size > self.buffer.len() {
            return false;
        }

        let dirent = Dirent {
            ino,
            off,
            namelen: name.len() as u32,
            type_: kind as u32,
        };
        let start = self.used;
        self.write_dirent_and_name(start, &dirent, name, size);
        self.used += size;
        true
    }

    /// Appends a `READDIRPLUS` record. Returns `false`, leaving `self` unchanged, if it would
    /// not fit in the remaining buffer.
    pub fn push_plus(&mut self, entry: &EntryOut, off: u64, kind: DirentType, name: &[u8]) -> bool {
        let size = direntplus_size(name.len());
        if self.used + size > self.buffer.len() {
            return false;
        }

        let start = self.used;
        let entry_end = start + size_of::<EntryOut>();
        self.buffer[start..entry_end].copy_from_slice(entry.as_bytes());

        let dirent = Dirent {
            ino: entry.attr.ino,
            off,
            namelen: name.len() as u32,
            type_: kind as u32,
        };
        self.write_dirent_and_name(entry_end, &dirent, name, size - size_of::<EntryOut>());
        self.used += size;
        true
    }

    /// Writes a [`Dirent`] header, its name, and zeroed alignment padding starting at `start`,
    /// for a record whose total padded size is `record_size`.
    fn write_dirent_and_name(
        &mut self,
        start: usize,
        dirent: &Dirent,
        name: &[u8],
        record_size: usize,
    ) {
        let header_end = start + size_of::<Dirent>();
        self.buffer[start..header_end].copy_from_slice(dirent.as_bytes());

        let name_end = header_end + name.len();
        self.buffer[header_end..name_end].copy_from_slice(name);

        for byte in &mut self.buffer[name_end..start + record_size] {
            *byte = 0;
        }
    }

    /// Bytes written so far.
    pub fn used(&self) -> usize {
        self.used
    }

    /// Consumes the writer, returning the total bytes written.
    pub fn finish(self) -> usize {
        self.used
    }
}

/// The node ids the kernel takes a lookup reference for, in order, reading the `READDIRPLUS`
/// records in `listed`: one for each record whose node id is not 0, but none for `.` or `..`,
/// whatever theirs (Linux `fs/fuse/readdir.c`, `fuse_direntplus_link`).
pub fn referenced_nodeids(listed: &[u8]) -> impl Iterator<Item = u64> + '_ {
    let mut rest = listed;
    std::iter::from_fn(move || {
        while let Ok((record, after)) = DirentPlus::read_from_prefix(rest) {
            let namelen = record.dirent.namelen as usize;
            let name = after.get(..namelen).unwrap_or_default();
            rest = rest.get(direntplus_size(namelen)..).unwrap_or_default();
            if record.entry_out.nodeid != 0 && name != b"." && name != b".." {
                return Some(record.entry_out.nodeid);
            }
        }
        None
    })
}
