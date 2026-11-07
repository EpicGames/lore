// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A read-write in-memory `Filesystem` and its mount, for the read-write mount tests.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_fuse::abi::Attr;
use lore_fuse::abi::Kstatfs;
use lore_fuse::abi::ROOT_ID;
use lore_fuse::abi::SetattrIn;
use lore_fuse::abi::entry::setattr_valid;
use lore_fuse::codec::dirent::DirentWriter;
use lore_fuse::filesystem::Filesystem;
use lore_fuse::filesystem::RequestInfo;
use lore_fuse::init::InitConfig;
use lore_fuse::mount::Mount;
use lore_fuse::mount::MountAccess;
use lore_fuse::mount::MountOptions;
use lore_fuse::mount::mount_with_access;
use lore_fuse::reply::AttrReply;
use lore_fuse::reply::CreateReply;
use lore_fuse::reply::EntryReply;
use lore_fuse::reply::Errno;
use lore_fuse::reply::OpenReply;
use lore_fuse::reply::ReadFault;
use lore_fuse::reply::ReplyOutcome;
use lore_fuse::reply::Timeouts;
use lore_fuse::session::Session;
use lore_fuse::session::SessionConfig;
use lore_fuse::transport::device::DeviceTransport;
use parking_lot::Mutex;

enum Kind {
    Dir(BTreeMap<Vec<u8>, u64>),
    File(Vec<u8>),
}

struct Node {
    mode: u32,
    kind: Kind,
}

struct Tree {
    nodes: BTreeMap<u64, Node>,
    next_ino: u64,
}

impl Tree {
    fn child(&self, parent: u64, name: &[u8]) -> Result<u64, Errno> {
        match self.nodes.get(&parent).map(|node| &node.kind) {
            Some(Kind::Dir(children)) => children.get(name).copied().ok_or(Errno::ENOENT),
            Some(Kind::File(_)) => Err(Errno::ENOTDIR),
            None => Err(Errno::ENOENT),
        }
    }

    fn children_mut(&mut self, parent: u64) -> Result<&mut BTreeMap<Vec<u8>, u64>, Errno> {
        match self.nodes.get_mut(&parent).map(|node| &mut node.kind) {
            Some(Kind::Dir(children)) => Ok(children),
            Some(Kind::File(_)) => Err(Errno::ENOTDIR),
            None => Err(Errno::ENOENT),
        }
    }
}

/// A read-write in-memory filesystem that serves every handle by inode, so a `WRITE` is served
/// the same with the `CREATE` handle or with handle 0. `OPEN` answers `ENOSYS`: zero-message
/// open. Listings are not exercised and come back empty.
pub struct MemoryFs {
    tree: Mutex<Tree>,
    uid: u32,
    gid: u32,
    pub last_create_fh: AtomicU64,
    pub last_write_fh: AtomicU64,
    pub largest_write: AtomicUsize,
}

impl MemoryFs {
    pub fn new() -> Arc<Self> {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            ROOT_ID,
            Node {
                mode: libc::S_IFDIR | 0o755,
                kind: Kind::Dir(BTreeMap::new()),
            },
        );
        Arc::new(Self {
            tree: Mutex::new(Tree {
                nodes,
                next_ino: ROOT_ID + 1,
            }),
            // SAFETY: getuid and getgid have no preconditions and cannot fail.
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            last_create_fh: AtomicU64::new(0),
            last_write_fh: AtomicU64::new(u64::MAX),
            largest_write: AtomicUsize::new(0),
        })
    }

    fn attr(&self, ino: u64, node: &Node) -> Attr {
        let (size, nlink) = match &node.kind {
            Kind::Dir(_) => (0, 2),
            Kind::File(content) => (content.len() as u64, 1),
        };
        Attr {
            ino,
            size,
            blocks: size.div_ceil(512),
            mode: node.mode,
            nlink,
            uid: self.uid,
            gid: self.gid,
            blksize: 4096,
            ..Attr::default()
        }
    }

    fn entry(&self, ino: u64, node: &Node) -> EntryReply {
        EntryReply {
            ino,
            generation: 0,
            attr: self.attr(ino, node),
            timeouts: Timeouts::ZERO,
        }
    }

    fn insert(&self, parent: u64, name: &[u8], node: Node) -> Result<EntryReply, Errno> {
        let mut tree = self.tree.lock();
        let ino = tree.next_ino;
        let children = tree.children_mut(parent)?;
        if children.contains_key(name) {
            return Err(Errno::EEXIST);
        }
        children.insert(name.to_vec(), ino);
        tree.next_ino += 1;
        let entry = self.entry(ino, &node);
        tree.nodes.insert(ino, node);
        Ok(entry)
    }

    /// The content of the file at `path`, one name per directory level below the root.
    pub fn content(&self, path: &[&[u8]]) -> Option<Vec<u8>> {
        let tree = self.tree.lock();
        let mut ino = ROOT_ID;
        for name in path {
            ino = tree.child(ino, name).ok()?;
        }
        match &tree.nodes.get(&ino)?.kind {
            Kind::File(content) => Some(content.clone()),
            Kind::Dir(_) => None,
        }
    }
}

impl Filesystem for MemoryFs {
    async fn lookup(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
    ) -> Result<EntryReply, Errno> {
        let tree = self.tree.lock();
        let ino = tree.child(parent, name)?;
        let node = tree.nodes.get(&ino).ok_or(Errno::ENOENT)?;
        Ok(self.entry(ino, node))
    }

    fn forget(&self, _ino: u64, _nlookup: u64) {}

    async fn getattr(&self, _req: &RequestInfo, ino: u64) -> Result<AttrReply, Errno> {
        let tree = self.tree.lock();
        let node = tree.nodes.get(&ino).ok_or(Errno::ENOENT)?;
        Ok(AttrReply {
            attr: self.attr(ino, node),
            timeout: Duration::ZERO,
        })
    }

    fn statfs(&self, _req: &RequestInfo) -> Result<Kstatfs, Errno> {
        Ok(Kstatfs {
            bsize: 4096,
            frsize: 4096,
            namelen: 255,
            ..Kstatfs::default()
        })
    }

    fn opendir(&self, _req: &RequestInfo, _ino: u64) -> Result<OpenReply, Errno> {
        Ok(OpenReply { fh: 0, flags: 0 })
    }

    async fn readdir(
        &self,
        _req: &RequestInfo,
        _ino: u64,
        _fh: u64,
        _offset: u64,
        _out: &mut DirentWriter<'_>,
    ) -> Result<(), Errno> {
        Ok(())
    }

    async fn readdirplus(
        &self,
        _req: &RequestInfo,
        _ino: u64,
        _fh: u64,
        _offset: u64,
        _out: &mut DirentWriter<'_>,
    ) -> Result<usize, Errno> {
        Ok(0)
    }

    fn releasedir(&self, _ino: u64, _fh: u64) {}

    fn open(&self, _req: &RequestInfo, _ino: u64, _flags: u32) -> Result<OpenReply, Errno> {
        Err(Errno::ENOSYS)
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
        let tree = self.tree.lock();
        match tree.nodes.get(&ino).map(|node| &node.kind) {
            Some(Kind::File(content)) => {
                let start = (offset as usize).min(content.len());
                let n = (content.len() - start).min(out.len());
                out[..n].copy_from_slice(&content[start..start + n]);
                Ok(n)
            }
            Some(Kind::Dir(_)) => Err(Errno::EISDIR.into()),
            None => Err(Errno::ENOENT.into()),
        }
    }

    fn release(&self, _ino: u64, _fh: u64) {}

    fn reply_failed(&self, _req: &RequestInfo, _outcome: &ReplyOutcome) {}

    async fn setattr(
        &self,
        _req: &RequestInfo,
        ino: u64,
        attr: &SetattrIn,
    ) -> Result<AttrReply, Errno> {
        let mut tree = self.tree.lock();
        let node = tree.nodes.get_mut(&ino).ok_or(Errno::ENOENT)?;
        if attr.valid & setattr_valid::MODE != 0 {
            node.mode = (node.mode & libc::S_IFMT) | (attr.mode & 0o7777);
        }
        if attr.valid & setattr_valid::SIZE != 0 {
            match &mut node.kind {
                Kind::File(content) => content.resize(attr.size as usize, 0),
                Kind::Dir(_) => return Err(Errno::EISDIR),
            }
        }
        Ok(AttrReply {
            attr: self.attr(ino, node),
            timeout: Duration::ZERO,
        })
    }

    async fn mkdir(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
        mode: u32,
        umask: u32,
    ) -> Result<EntryReply, Errno> {
        let node = Node {
            mode: libc::S_IFDIR | (mode & !umask & 0o7777),
            kind: Kind::Dir(BTreeMap::new()),
        };
        self.insert(parent, name, node)
    }

    async fn unlink(&self, _req: &RequestInfo, parent: u64, name: &[u8]) -> Result<(), Errno> {
        let mut tree = self.tree.lock();
        let ino = tree.child(parent, name)?;
        if let Some(Kind::Dir(_)) = tree.nodes.get(&ino).map(|node| &node.kind) {
            return Err(Errno::EISDIR);
        }
        tree.children_mut(parent)?.remove(name);
        tree.nodes.remove(&ino);
        Ok(())
    }

    async fn rename(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
        newparent: u64,
        newname: &[u8],
        flags: u32,
    ) -> Result<(), Errno> {
        if flags & !libc::RENAME_NOREPLACE != 0 {
            return Err(Errno::EINVAL);
        }
        let mut tree = self.tree.lock();
        let ino = tree.child(parent, name)?;
        tree.children_mut(newparent)?;
        if let Ok(replaced) = tree.child(newparent, newname) {
            if flags & libc::RENAME_NOREPLACE != 0 {
                return Err(Errno::EEXIST);
            }
            tree.nodes.remove(&replaced);
        }
        tree.children_mut(parent)?.remove(name);
        tree.children_mut(newparent)?.insert(newname.to_vec(), ino);
        Ok(())
    }

    async fn create(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
        mode: u32,
        umask: u32,
        _flags: u32,
    ) -> Result<CreateReply, Errno> {
        let node = Node {
            mode: mode & !umask,
            kind: Kind::File(Vec::new()),
        };
        let entry = self.insert(parent, name, node)?;
        let fh = 1000 + entry.ino;
        self.last_create_fh.store(fh, Ordering::SeqCst);
        Ok(CreateReply {
            entry,
            open: OpenReply { fh, flags: 0 },
        })
    }

    async fn write(
        &self,
        _req: &RequestInfo,
        ino: u64,
        fh: u64,
        offset: u64,
        data: &[u8],
        _write_flags: u32,
    ) -> Result<usize, Errno> {
        self.last_write_fh.store(fh, Ordering::SeqCst);
        self.largest_write.fetch_max(data.len(), Ordering::SeqCst);
        let mut tree = self.tree.lock();
        match tree.nodes.get_mut(&ino).map(|node| &mut node.kind) {
            Some(Kind::File(content)) => {
                let start = offset as usize;
                let end = start + data.len();
                if content.len() < end {
                    content.resize(end, 0);
                }
                content[start..end].copy_from_slice(data);
                Ok(data.len())
            }
            Some(Kind::Dir(_)) => Err(Errno::EISDIR),
            None => Err(Errno::ENOENT),
        }
    }
}

/// Mounts `fs` read-write at `dir` with the read-write INIT defaults and zero-message open.
pub async fn mount_read_write(
    dir: &Path,
    fs: Arc<MemoryFs>,
    workers: usize,
) -> (Arc<Mount>, Arc<Session<MemoryFs, DeviceTransport>>) {
    let options = MountOptions {
        fsname: "lore:read-write".to_string(),
        max_read: 1 << 20,
        fusermount: None,
    };
    let mount = Arc::new(
        mount_with_access(dir, &options, MountAccess::ReadWrite).expect("read-write mount"),
    );
    let transport = Arc::new(DeviceTransport::new(Arc::clone(&mount)));
    let config = SessionConfig {
        workers,
        init: InitConfig {
            zero_message_open: true,
            ..InitConfig::read_write()
        },
    };
    let session = Session::start(fs, transport, config)
        .await
        .expect("session must negotiate against a real kernel");
    (mount, session)
}
