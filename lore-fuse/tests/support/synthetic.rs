// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A small, in-memory filesystem tree served through a real `Session`, for integration tests
//! that need actual kernel round-trips (mounts, interrupts, benchmarks) without a real backing
//! store. Every entry is world-readable (and, for executables, world-executable) at `0o555` so
//! `default_permissions` never depends on which uid the test process happens to run as; the
//! mount itself is always `ro`, so writes fail at the VFS before `Filesystem::open` ever runs.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_fuse::abi::Attr;
use lore_fuse::abi::DirentType;
use lore_fuse::abi::EntryOut;
use lore_fuse::abi::Kstatfs;
use lore_fuse::abi::ROOT_ID;
use lore_fuse::abi::file::open_flags;
use lore_fuse::codec::dirent::DirentWriter;
use lore_fuse::error::FuseError;
use lore_fuse::filesystem::Filesystem;
use lore_fuse::filesystem::RequestInfo;
use lore_fuse::mount::Mount;
use lore_fuse::mount::MountOptions;
use lore_fuse::reply::AttrReply;
use lore_fuse::reply::EntryReply;
use lore_fuse::reply::Errno;
use lore_fuse::reply::OpenReply;
use lore_fuse::reply::ReadFault;
use lore_fuse::reply::ReplyOutcome;
use lore_fuse::reply::Timeouts;
use lore_fuse::session::Session;
use lore_fuse::session::SessionConfig;
use lore_fuse::transport::device::DeviceTransport;
use tokio::sync::Notify;

/// One entry a test tree can describe. `BlockingRead`/`BlockingLookup` never complete on their
/// own: they exist to give an interrupt/reply-failure test (task 16) a request the dispatcher
/// is guaranteed still to be holding open when the test sends its signal. Each notifies
/// `release` exactly once, the moment its handler starts waiting, so a test can await that
/// notification instead of sleeping to know the request has actually reached the daemon.
pub enum SynthNode {
    File { content: Vec<u8>, executable: bool },
    Dir(Vec<(String, SynthNode)>),
    BlockingRead { release: Arc<Notify> },
    BlockingLookup { release: Arc<Notify> },
}

enum EntryKind {
    Dir { children: Vec<(String, u64)> },
    File { content: Vec<u8>, executable: bool },
    BlockingRead { release: Arc<Notify> },
    BlockingLookup { release: Arc<Notify> },
}

struct SynthEntry {
    kind: EntryKind,
}

/// A `Filesystem` backed by a fixed in-memory tree built once by [`SyntheticFs::from_tree`].
/// `nodes[i]` holds inode `i as u64 + 1` (inode 0 is never valid; the root is `ROOT_ID == 1`).
pub struct SyntheticFs {
    nodes: Vec<SynthEntry>,
    /// Keyed by parent inode, then by name, so `lookup` can query with a borrowed `&[u8]`
    /// (`Vec<u8>: Borrow<[u8]>`) instead of allocating a fresh key on every call.
    children: HashMap<u64, HashMap<Vec<u8>, u64>>,
    pub reply_failed_count: AtomicUsize,
    pub lookups: AtomicU64,
    zero_message_open: AtomicBool,
}

impl SyntheticFs {
    /// Builds a tree rooted at `ROOT_ID` with `root` as the root directory's children.
    pub fn from_tree(root: Vec<(String, SynthNode)>) -> Arc<Self> {
        let mut nodes = vec![SynthEntry {
            kind: EntryKind::Dir {
                children: Vec::new(),
            },
        }];
        let mut children = HashMap::new();
        let root_children = build(&mut nodes, &mut children, ROOT_ID, root);
        nodes[0].kind = EntryKind::Dir {
            children: root_children,
        };

        Arc::new(Self {
            nodes,
            children,
            reply_failed_count: AtomicUsize::new(0),
            lookups: AtomicU64::new(0),
            zero_message_open: AtomicBool::new(false),
        })
    }

    /// Answer OPEN with ENOSYS, which switches the kernel to zero-message opens for the rest of
    /// the connection.
    pub fn set_zero_message_open(&self, enabled: bool) {
        self.zero_message_open.store(enabled, Ordering::Relaxed);
    }

    /// A file that reads back as `len` zero bytes without holding `len` bytes in memory as a
    /// `Vec` field on every other node -- only this one call pays for the allocation, once, at
    /// tree-construction time (the benchmark that uses this needs a big file, not a big test
    /// binary).
    pub fn zero_file(len: u64) -> SynthNode {
        SynthNode::File {
            content: vec![0u8; len as usize],
            executable: false,
        }
    }

    fn entry(&self, ino: u64) -> Result<&SynthEntry, Errno> {
        let index = ino.checked_sub(1).ok_or(Errno::ENOENT)?;
        self.nodes.get(index as usize).ok_or(Errno::ENOENT)
    }

    fn attr_for(ino: u64, entry: &SynthEntry) -> Attr {
        let (mode, size, nlink) = match &entry.kind {
            EntryKind::Dir { .. } => (libc::S_IFDIR | 0o555, 0u64, 2u32),
            EntryKind::File {
                content,
                executable,
            } => {
                let perm = if *executable { 0o555 } else { 0o444 };
                (libc::S_IFREG | perm, content.len() as u64, 1u32)
            }
            // A real read past `i_size` never reaches `Filesystem::read` at all -- the kernel's
            // generic read path clamps the length to 0 at the VFS layer and returns immediately.
            // `BlockingRead` needs a real nonzero size so the kernel actually issues a READ.
            EntryKind::BlockingRead { .. } => (libc::S_IFREG | 0o444, 4096u64, 1u32),
            EntryKind::BlockingLookup { .. } => (libc::S_IFREG | 0o444, 0u64, 1u32),
        };
        Attr {
            ino,
            size,
            blocks: size.div_ceil(512),
            mode,
            nlink,
            ..Default::default()
        }
    }
}

/// Recursively allocates inodes depth-first, so nested directories are fully built (including
/// their own `children` lists) before the caller wires them into their parent.
fn build(
    nodes: &mut Vec<SynthEntry>,
    children: &mut HashMap<u64, HashMap<Vec<u8>, u64>>,
    parent: u64,
    entries: Vec<(String, SynthNode)>,
) -> Vec<(String, u64)> {
    let mut ordered = Vec::with_capacity(entries.len());
    for (name, node) in entries {
        let ino = nodes.len() as u64 + 1;
        let kind = match node {
            SynthNode::Dir(sub) => {
                nodes.push(SynthEntry {
                    kind: EntryKind::Dir {
                        children: Vec::new(),
                    },
                });
                let sub_ordered = build(nodes, children, ino, sub);
                nodes[(ino - 1) as usize].kind = EntryKind::Dir {
                    children: sub_ordered,
                };
                children
                    .entry(parent)
                    .or_default()
                    .insert(name.clone().into_bytes(), ino);
                ordered.push((name, ino));
                continue;
            }
            SynthNode::File {
                content,
                executable,
            } => EntryKind::File {
                content,
                executable,
            },
            SynthNode::BlockingRead { release } => EntryKind::BlockingRead { release },
            SynthNode::BlockingLookup { release } => EntryKind::BlockingLookup { release },
        };
        nodes.push(SynthEntry { kind });
        children
            .entry(parent)
            .or_default()
            .insert(name.clone().into_bytes(), ino);
        ordered.push((name, ino));
    }
    ordered
}

impl Filesystem for SyntheticFs {
    async fn lookup(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
    ) -> Result<EntryReply, Errno> {
        self.lookups.fetch_add(1, Ordering::Relaxed);
        let ino = *self
            .children
            .get(&parent)
            .and_then(|by_name| by_name.get(name))
            .ok_or(Errno::ENOENT)?;
        let entry = self.entry(ino)?;
        if let EntryKind::BlockingLookup { release } = &entry.kind {
            release.notify_one();
            let pending: Result<EntryReply, Errno> = std::future::pending().await;
            return pending;
        }
        Ok(EntryReply {
            ino,
            generation: 0,
            attr: Self::attr_for(ino, entry),
            timeouts: Timeouts::ZERO,
        })
    }

    fn forget(&self, _ino: u64, _nlookup: u64) {}

    async fn getattr(&self, _req: &RequestInfo, ino: u64) -> Result<AttrReply, Errno> {
        let entry = self.entry(ino)?;
        Ok(AttrReply {
            attr: Self::attr_for(ino, entry),
            timeout: Duration::ZERO,
        })
    }

    fn statfs(&self, _req: &RequestInfo) -> Result<Kstatfs, Errno> {
        Ok(Kstatfs::default())
    }

    fn opendir(&self, _req: &RequestInfo, ino: u64) -> Result<OpenReply, Errno> {
        match &self.entry(ino)?.kind {
            EntryKind::Dir { .. } => Ok(OpenReply {
                fh: 0,
                flags: open_flags::CACHE_DIR | open_flags::KEEP_CACHE,
            }),
            _ => Err(Errno::ENOTDIR),
        }
    }

    async fn readdir(
        &self,
        _req: &RequestInfo,
        ino: u64,
        _fh: u64,
        offset: u64,
        out: &mut DirentWriter<'_>,
    ) -> Result<(), Errno> {
        let entry = self.entry(ino)?;
        let EntryKind::Dir { children } = &entry.kind else {
            return Err(Errno::ENOTDIR);
        };
        for (index, (name, child_ino)) in children.iter().enumerate().skip(offset as usize) {
            let kind = match &self.entry(*child_ino)?.kind {
                EntryKind::Dir { .. } => DirentType::Dir,
                _ => DirentType::Reg,
            };
            if !out.push(*child_ino, (index + 1) as u64, kind, name.as_bytes()) {
                break;
            }
        }
        Ok(())
    }

    async fn readdirplus(
        &self,
        _req: &RequestInfo,
        ino: u64,
        _fh: u64,
        offset: u64,
        out: &mut DirentWriter<'_>,
    ) -> Result<usize, Errno> {
        let entry = self.entry(ino)?;
        let EntryKind::Dir { children } = &entry.kind else {
            return Err(Errno::ENOTDIR);
        };
        let mut emitted = 0usize;
        for (index, (name, child_ino)) in children.iter().enumerate().skip(offset as usize) {
            let child_entry = self.entry(*child_ino)?;
            let kind = match &child_entry.kind {
                EntryKind::Dir { .. } => DirentType::Dir,
                _ => DirentType::Reg,
            };
            let entry_out = EntryOut {
                nodeid: *child_ino,
                generation: 0,
                entry_valid: 0,
                attr_valid: 0,
                entry_valid_nsec: 0,
                attr_valid_nsec: 0,
                attr: Self::attr_for(*child_ino, child_entry),
            };
            if !out.push_plus(&entry_out, (index + 1) as u64, kind, name.as_bytes()) {
                break;
            }
            emitted += 1;
        }
        Ok(emitted)
    }

    fn releasedir(&self, _ino: u64, _fh: u64) {}

    fn open(&self, _req: &RequestInfo, ino: u64, _flags: u32) -> Result<OpenReply, Errno> {
        if self.zero_message_open.load(Ordering::Relaxed) {
            return Err(Errno::ENOSYS);
        }
        match &self.entry(ino)?.kind {
            // Mirrors the provider: content never changes under an inode, so the page cache
            // survives reopening and close needs no FLUSH round trip.
            EntryKind::File { .. } => Ok(OpenReply {
                fh: 0,
                flags: open_flags::KEEP_CACHE | open_flags::NOFLUSH,
            }),
            // A page-cache-backed read never reaches `Filesystem::read` at all when it can be
            // satisfied (or short-circuited) at the VFS layer, and a background readahead fill
            // is not tied to the calling task's own interruptible wait -- the kernel never
            // queues `FUSE_INTERRUPT` for it. `DIRECT_IO` forces the kernel's synchronous,
            // per-task `fuse_direct_io` path instead, which is the one real `SIGINT` actually
            // interrupts (observed: without this, a real kernel never delivered `INTERRUPT` for
            // a blocked read, even after several seconds).
            EntryKind::BlockingRead { .. } => Ok(OpenReply {
                fh: 0,
                flags: open_flags::DIRECT_IO,
            }),
            EntryKind::Dir { .. } => Err(Errno::EISDIR),
            EntryKind::BlockingLookup { .. } => Err(Errno::ENOENT),
        }
    }

    async fn read(
        &self,
        _req: &RequestInfo,
        ino: u64,
        _fh: u64,
        offset: u64,
        out: &mut [u8],
        _fetch: bool,
    ) -> Result<usize, ReadFault> {
        let entry = self.entry(ino).map_err(ReadFault::from)?;
        match &entry.kind {
            EntryKind::File { content, .. } => {
                let start = (offset as usize).min(content.len());
                let n = (content.len() - start).min(out.len());
                out[..n].copy_from_slice(&content[start..start + n]);
                Ok(n)
            }
            EntryKind::BlockingRead { release } => {
                release.notify_one();
                std::future::pending::<Result<usize, ReadFault>>().await
            }
            EntryKind::Dir { .. } => Err(ReadFault::from(Errno::EISDIR)),
            EntryKind::BlockingLookup { .. } => Err(ReadFault::from(Errno::ENOENT)),
        }
    }

    fn release(&self, _ino: u64, _fh: u64) {}

    fn reply_failed(&self, _req: &RequestInfo, _outcome: &ReplyOutcome) {
        self.reply_failed_count.fetch_add(1, Ordering::SeqCst);
    }
}

/// Mounts `fs` at `dir` and starts a `Session` over the real device transport -- the common
/// setup every synthetic-mount test and benchmark shares.
pub async fn mount_synthetic(
    dir: &Path,
    fs: Arc<SyntheticFs>,
    config: SessionConfig,
) -> Result<(Arc<Mount>, Arc<Session<SyntheticFs, DeviceTransport>>), FuseError> {
    let options = MountOptions {
        fsname: "lore:synthetic".to_string(),
        max_read: 1 << 20,
        fusermount: None,
    };
    let mount_handle = Arc::new(lore_fuse::mount::mount(dir, &options)?);
    let transport = Arc::new(DeviceTransport::new(Arc::clone(&mount_handle)));
    let session = Session::start(fs, transport, config).await?;
    Ok((mount_handle, session))
}
