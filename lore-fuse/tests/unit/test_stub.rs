// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A scripted `Filesystem` double shared by `dispatch`'s and `interrupt`'s unit tests (tasks
//! 12-13): every method increments `calls`; most pop a queued result the test pushed ahead of
//! time, defaulting to a plain error so a test that does not care still gets a deterministic
//! reply instead of a panic.

use std::collections::VecDeque;
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
use lore_fuse::abi::SetattrIn;
use lore_fuse::abi::file::open_flags;
use lore_fuse::codec::dirent::DirentWriter;
use lore_fuse::filesystem::EntryRequest;
use lore_fuse::filesystem::Fetch;
use lore_fuse::filesystem::Filesystem;
use lore_fuse::filesystem::RequestInfo;
use lore_fuse::reply::AttrReply;
use lore_fuse::reply::CreateReply;
use lore_fuse::reply::EntryReply;
use lore_fuse::reply::Errno;
use lore_fuse::reply::OpenReply;
use lore_fuse::reply::ReadFault;
use lore_fuse::reply::ReplyOutcome;
use lore_fuse::reply::Timeouts;
use parking_lot::Mutex;
use tokio::sync::Notify;

/// Inode the stub's entry-creating methods return.
pub const CREATED_INO: u64 = 100;
/// Handle the stub's `create` returns.
pub const CREATED_FH: u64 = 77;
/// Target the stub's `readlink` returns.
pub const LINK_TARGET: &[u8] = b"stub-target";

/// One write-family call the stub received, with the arguments dispatch decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    Setattr {
        ino: u64,
        valid: u32,
        fh: u64,
        size: u64,
        mode: u32,
    },
    Readlink {
        ino: u64,
        capacity: usize,
    },
    Symlink {
        parent: u64,
        name: Vec<u8>,
        target: Vec<u8>,
    },
    Mknod {
        parent: u64,
        name: Vec<u8>,
        mode: u32,
        rdev: u32,
        umask: u32,
    },
    Mkdir {
        parent: u64,
        name: Vec<u8>,
        mode: u32,
        umask: u32,
    },
    Unlink {
        parent: u64,
        name: Vec<u8>,
    },
    Rmdir {
        parent: u64,
        name: Vec<u8>,
    },
    Rename {
        parent: u64,
        name: Vec<u8>,
        newparent: u64,
        newname: Vec<u8>,
        flags: u32,
    },
    Link {
        ino: u64,
        newparent: u64,
        newname: Vec<u8>,
    },
    Create {
        parent: u64,
        name: Vec<u8>,
        mode: u32,
        umask: u32,
        flags: u32,
    },
    Write {
        ino: u64,
        fh: u64,
        offset: u64,
        data: Vec<u8>,
        write_flags: u32,
    },
    Flush {
        ino: u64,
        fh: u64,
        lock_owner: u64,
    },
    Fsync {
        ino: u64,
        fh: u64,
        datasync: bool,
    },
    Fsyncdir {
        ino: u64,
        fh: u64,
        datasync: bool,
    },
    Fallocate {
        ino: u64,
        fh: u64,
        offset: u64,
        length: u64,
        mode: u32,
    },
}

/// One [`Filesystem::fetch`] the stub received, owned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fetched {
    Entry {
        parent: u64,
        name: Vec<u8>,
        request: EntryRequest,
    },
    Rename {
        parent: u64,
        name: Vec<u8>,
        newparent: u64,
        newname: Vec<u8>,
        flags: u32,
    },
    Content {
        ino: u64,
        len: Option<u64>,
    },
    Flush {
        ino: u64,
        datasync: bool,
    },
}

impl From<Fetch<'_>> for Fetched {
    fn from(what: Fetch<'_>) -> Self {
        match what {
            Fetch::Entry {
                parent,
                name,
                request,
            } => Self::Entry {
                parent,
                name: name.to_vec(),
                request,
            },
            Fetch::Rename {
                parent,
                name,
                newparent,
                newname,
                flags,
            } => Self::Rename {
                parent,
                name: name.to_vec(),
                newparent,
                newname: newname.to_vec(),
                flags,
            },
            Fetch::Content { ino, len } => Self::Content { ino, len },
            Fetch::Flush { ino, datasync } => Self::Flush { ino, datasync },
        }
    }
}

fn created_entry() -> EntryReply {
    EntryReply {
        ino: CREATED_INO,
        generation: 0,
        attr: Attr {
            ino: CREATED_INO,
            ..Attr::default()
        },
        timeouts: Timeouts::ZERO,
    }
}

#[derive(Default)]
pub struct StubFilesystem {
    pub calls: AtomicU64,
    pub lookup_results: Mutex<VecDeque<Result<EntryReply, Errno>>>,
    pub getattr_results: Mutex<VecDeque<Result<AttrReply, Errno>>>,
    pub read_results: Mutex<VecDeque<Result<usize, ReadFault>>>,
    pub forget_calls: Mutex<Vec<(u64, u64)>>,
    pub reply_failed_calls: Mutex<Vec<ReplyOutcome>>,
    pub readdir_handles: Mutex<Vec<u64>>,
    pub write_family_calls: Mutex<Vec<Call>>,
    /// When set, every write-family method fails with this errno.
    pub write_family_error: Mutex<Option<Errno>>,
    /// When set, the next `read(.., fetch: false)` call awaits `local_gate` before popping its
    /// queued result (task 13's in-flight-interrupt test).
    pub hang_local_read: AtomicBool,
    pub local_gate: Notify,
    /// When set, the next `read(.., fetch: true)` call awaits `fetch_gate` before popping its
    /// queued result (task 12/13's deferred-read tests).
    pub hang_fetch_read: AtomicBool,
    pub fetch_gate: Notify,
    /// When set, `lookup_local`, `readdir_local` and `readdirplus_local` answer `WouldFetch`.
    pub defer_listing: AtomicBool,
    /// When set, `lookup`, `readdir` and `readdirplus` await `listing_gate` first.
    pub hang_listing: AtomicBool,
    pub listing_gate: Notify,
    /// The node ids of the records `readdirplus` writes, 0 for a plain one; it reports those
    /// that are not 0 as emitted.
    pub readdirplus_nodeids: Mutex<Vec<u64>>,
    /// When set, `readdirplus` awaits `listing_gate` once it has written its records, as a page
    /// whose next row waits on the store does, and sets `readdirplus_parked` first.
    pub park_readdirplus: AtomicBool,
    pub readdirplus_parked: AtomicBool,
    /// When set, `readdirplus` answers this error once it has written its records.
    pub readdirplus_error: Mutex<Option<Errno>>,
    /// When set, the local forms of CREATE, MKNOD, MKDIR, SYMLINK, UNLINK, RMDIR, RENAME, WRITE
    /// and FSYNC answer `WouldFetch`.
    pub defer_writes: AtomicBool,
    /// Every `fetch` received, in order.
    pub fetches: Mutex<Vec<Fetched>>,
    /// When set, `fetch` awaits `fetch_phase_gate` after recording its call.
    pub hang_fetch_phase: AtomicBool,
    pub fetch_phase_gate: Notify,
    /// When set, the full forms of the deferrable write-family requests await
    /// `completion_gate` after counting themselves in `completions_entered`.
    pub hang_completion: AtomicBool,
    pub completion_gate: Notify,
    pub completions_entered: AtomicUsize,
}

/// The one entry the stub's `readdir` writes.
pub const STUB_DIRENT: &[u8] = b"stub-entry";

impl StubFilesystem {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    async fn await_listing_gate(&self) {
        if self.hang_listing.load(Ordering::SeqCst) {
            self.listing_gate.notified().await;
        }
    }

    fn listing_deferred(&self) -> bool {
        self.defer_listing.load(Ordering::SeqCst)
    }

    fn writes_deferred(&self) -> Result<(), ReadFault> {
        if self.defer_writes.load(Ordering::SeqCst) {
            return Err(ReadFault::WouldFetch);
        }
        Ok(())
    }

    /// [`Self::record`] for the full form of a deferrable request, which first waits on
    /// `completion_gate` while `hang_completion` is set.
    async fn complete(&self, call: Call) -> Result<(), Errno> {
        self.completions_entered.fetch_add(1, Ordering::SeqCst);
        if self.hang_completion.load(Ordering::SeqCst) {
            self.completion_gate.notified().await;
        }
        self.record(call)
    }

    /// Records `call`, then fails with the configured errno if one is set.
    fn record(&self, call: Call) -> Result<(), Errno> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.write_family_calls.lock().push(call);
        match *self.write_family_error.lock() {
            Some(errno) => Err(errno),
            None => Ok(()),
        }
    }
}

impl Filesystem for StubFilesystem {
    async fn lookup(
        &self,
        _req: &RequestInfo,
        _parent: u64,
        _name: &[u8],
    ) -> Result<EntryReply, Errno> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.await_listing_gate().await;
        self.lookup_results
            .lock()
            .pop_front()
            .unwrap_or(Err(Errno::ENOENT))
    }

    async fn lookup_local(
        &self,
        req: &RequestInfo,
        parent: u64,
        name: &[u8],
    ) -> Result<EntryReply, ReadFault> {
        if self.listing_deferred() {
            return Err(ReadFault::WouldFetch);
        }
        Ok(self.lookup(req, parent, name).await?)
    }

    async fn readdir_local(
        &self,
        req: &RequestInfo,
        ino: u64,
        fh: u64,
        offset: u64,
        out: &mut DirentWriter<'_>,
    ) -> Result<(), ReadFault> {
        if self.listing_deferred() {
            return Err(ReadFault::WouldFetch);
        }
        Ok(self.readdir(req, ino, fh, offset, out).await?)
    }

    async fn readdirplus_local(
        &self,
        req: &RequestInfo,
        ino: u64,
        fh: u64,
        offset: u64,
        out: &mut DirentWriter<'_>,
    ) -> Result<usize, ReadFault> {
        if self.listing_deferred() {
            return Err(ReadFault::WouldFetch);
        }
        Ok(self.readdirplus(req, ino, fh, offset, out).await?)
    }

    fn forget(&self, ino: u64, nlookup: u64) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.forget_calls.lock().push((ino, nlookup));
    }

    async fn getattr(&self, _req: &RequestInfo, _ino: u64) -> Result<AttrReply, Errno> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.getattr_results
            .lock()
            .pop_front()
            .unwrap_or(Err(Errno::ENOENT))
    }

    fn statfs(&self, _req: &RequestInfo) -> Result<Kstatfs, Errno> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Kstatfs::default())
    }

    fn opendir(&self, _req: &RequestInfo, _ino: u64) -> Result<OpenReply, Errno> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(OpenReply { fh: 0, flags: 0 })
    }

    async fn readdir(
        &self,
        _req: &RequestInfo,
        _ino: u64,
        fh: u64,
        _offset: u64,
        out: &mut DirentWriter<'_>,
    ) -> Result<(), Errno> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.await_listing_gate().await;
        self.readdir_handles.lock().push(fh);
        out.push(9, 1, DirentType::Reg, STUB_DIRENT);
        Ok(())
    }

    async fn readdirplus(
        &self,
        _req: &RequestInfo,
        _ino: u64,
        fh: u64,
        _offset: u64,
        out: &mut DirentWriter<'_>,
    ) -> Result<usize, Errno> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.await_listing_gate().await;
        self.readdir_handles.lock().push(fh);
        let mut emitted = 0;
        for (position, &nodeid) in self.readdirplus_nodeids.lock().iter().enumerate() {
            let entry = EntryOut {
                nodeid,
                ..EntryOut::default()
            };
            if !out.push_plus(&entry, position as u64 + 1, DirentType::Reg, STUB_DIRENT) {
                break;
            }
            emitted += usize::from(nodeid != 0);
        }
        if self.park_readdirplus.load(Ordering::SeqCst) {
            self.readdirplus_parked.store(true, Ordering::SeqCst);
            self.listing_gate.notified().await;
        }
        if let Some(errno) = *self.readdirplus_error.lock() {
            return Err(errno);
        }
        Ok(emitted)
    }

    fn releasedir(&self, _ino: u64, _fh: u64) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }

    fn open(&self, _req: &RequestInfo, _ino: u64, _flags: u32) -> Result<OpenReply, Errno> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(OpenReply { fh: 0, flags: 0 })
    }

    async fn read(
        &self,
        _req: &RequestInfo,
        _ino: u64,
        _fh: u64,
        _offset: u64,
        out: &mut [u8],
        fetch: bool,
    ) -> Result<usize, ReadFault> {
        self.calls.fetch_add(1, Ordering::SeqCst);

        if fetch {
            if self.hang_fetch_read.load(Ordering::SeqCst) {
                self.fetch_gate.notified().await;
            }
        } else if self.hang_local_read.load(Ordering::SeqCst) {
            self.local_gate.notified().await;
        }

        match self.read_results.lock().pop_front().unwrap_or(Ok(0)) {
            Ok(n) => {
                let n = n.min(out.len());
                out[..n].fill(b'x');
                Ok(n)
            }
            Err(fault) => Err(fault),
        }
    }

    fn release(&self, _ino: u64, _fh: u64) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }

    fn reply_failed(&self, _req: &RequestInfo, outcome: &ReplyOutcome) {
        self.reply_failed_calls.lock().push(*outcome);
    }

    async fn setattr(
        &self,
        _req: &RequestInfo,
        ino: u64,
        attr: &SetattrIn,
    ) -> Result<AttrReply, Errno> {
        self.record(Call::Setattr {
            ino,
            valid: attr.valid,
            fh: attr.fh,
            size: attr.size,
            mode: attr.mode,
        })?;
        Ok(AttrReply {
            attr: Attr {
                ino,
                size: attr.size,
                mode: attr.mode,
                ..Attr::default()
            },
            timeout: Duration::from_secs(1),
        })
    }

    async fn readlink(&self, _req: &RequestInfo, ino: u64, out: &mut [u8]) -> Result<usize, Errno> {
        self.record(Call::Readlink {
            ino,
            capacity: out.len(),
        })?;
        out[..LINK_TARGET.len()].copy_from_slice(LINK_TARGET);
        Ok(LINK_TARGET.len())
    }

    async fn symlink(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
        target: &[u8],
    ) -> Result<EntryReply, Errno> {
        self.complete(Call::Symlink {
            parent,
            name: name.to_vec(),
            target: target.to_vec(),
        })
        .await?;
        Ok(created_entry())
    }

    async fn mknod(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
        mode: u32,
        rdev: u32,
        umask: u32,
    ) -> Result<EntryReply, Errno> {
        self.complete(Call::Mknod {
            parent,
            name: name.to_vec(),
            mode,
            rdev,
            umask,
        })
        .await?;
        Ok(created_entry())
    }

    async fn mkdir(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
        mode: u32,
        umask: u32,
    ) -> Result<EntryReply, Errno> {
        self.complete(Call::Mkdir {
            parent,
            name: name.to_vec(),
            mode,
            umask,
        })
        .await?;
        Ok(created_entry())
    }

    async fn unlink(&self, _req: &RequestInfo, parent: u64, name: &[u8]) -> Result<(), Errno> {
        self.complete(Call::Unlink {
            parent,
            name: name.to_vec(),
        })
        .await
    }

    async fn rmdir(&self, _req: &RequestInfo, parent: u64, name: &[u8]) -> Result<(), Errno> {
        self.complete(Call::Rmdir {
            parent,
            name: name.to_vec(),
        })
        .await
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
        self.complete(Call::Rename {
            parent,
            name: name.to_vec(),
            newparent,
            newname: newname.to_vec(),
            flags,
        })
        .await
    }

    async fn link(
        &self,
        _req: &RequestInfo,
        ino: u64,
        newparent: u64,
        newname: &[u8],
    ) -> Result<EntryReply, Errno> {
        self.record(Call::Link {
            ino,
            newparent,
            newname: newname.to_vec(),
        })?;
        Ok(created_entry())
    }

    async fn create(
        &self,
        _req: &RequestInfo,
        parent: u64,
        name: &[u8],
        mode: u32,
        umask: u32,
        flags: u32,
    ) -> Result<CreateReply, Errno> {
        self.complete(Call::Create {
            parent,
            name: name.to_vec(),
            mode,
            umask,
            flags,
        })
        .await?;
        Ok(CreateReply {
            entry: created_entry(),
            open: OpenReply {
                fh: CREATED_FH,
                flags: open_flags::KEEP_CACHE,
            },
        })
    }

    async fn write(
        &self,
        _req: &RequestInfo,
        ino: u64,
        fh: u64,
        offset: u64,
        data: &[u8],
        write_flags: u32,
    ) -> Result<usize, Errno> {
        self.complete(Call::Write {
            ino,
            fh,
            offset,
            data: data.to_vec(),
            write_flags,
        })
        .await?;
        Ok(data.len())
    }

    async fn flush(
        &self,
        _req: &RequestInfo,
        ino: u64,
        fh: u64,
        lock_owner: u64,
    ) -> Result<(), Errno> {
        self.record(Call::Flush {
            ino,
            fh,
            lock_owner,
        })
    }

    async fn fsync(
        &self,
        _req: &RequestInfo,
        ino: u64,
        fh: u64,
        datasync: bool,
    ) -> Result<(), Errno> {
        self.complete(Call::Fsync { ino, fh, datasync }).await
    }

    async fn fsyncdir(
        &self,
        _req: &RequestInfo,
        ino: u64,
        fh: u64,
        datasync: bool,
    ) -> Result<(), Errno> {
        self.record(Call::Fsyncdir { ino, fh, datasync })
    }

    async fn fallocate(
        &self,
        _req: &RequestInfo,
        ino: u64,
        fh: u64,
        offset: u64,
        length: u64,
        mode: u32,
    ) -> Result<(), Errno> {
        self.record(Call::Fallocate {
            ino,
            fh,
            offset,
            length,
            mode,
        })
    }

    async fn create_local(
        &self,
        req: &RequestInfo,
        parent: u64,
        name: &[u8],
        mode: u32,
        umask: u32,
        flags: u32,
    ) -> Result<CreateReply, ReadFault> {
        self.writes_deferred()?;
        Ok(self.create(req, parent, name, mode, umask, flags).await?)
    }

    async fn mknod_local(
        &self,
        req: &RequestInfo,
        parent: u64,
        name: &[u8],
        mode: u32,
        rdev: u32,
        umask: u32,
    ) -> Result<EntryReply, ReadFault> {
        self.writes_deferred()?;
        Ok(self.mknod(req, parent, name, mode, rdev, umask).await?)
    }

    async fn mkdir_local(
        &self,
        req: &RequestInfo,
        parent: u64,
        name: &[u8],
        mode: u32,
        umask: u32,
    ) -> Result<EntryReply, ReadFault> {
        self.writes_deferred()?;
        Ok(self.mkdir(req, parent, name, mode, umask).await?)
    }

    async fn symlink_local(
        &self,
        req: &RequestInfo,
        parent: u64,
        name: &[u8],
        target: &[u8],
    ) -> Result<EntryReply, ReadFault> {
        self.writes_deferred()?;
        Ok(self.symlink(req, parent, name, target).await?)
    }

    async fn unlink_local(
        &self,
        req: &RequestInfo,
        parent: u64,
        name: &[u8],
    ) -> Result<(), ReadFault> {
        self.writes_deferred()?;
        Ok(self.unlink(req, parent, name).await?)
    }

    async fn rmdir_local(
        &self,
        req: &RequestInfo,
        parent: u64,
        name: &[u8],
    ) -> Result<(), ReadFault> {
        self.writes_deferred()?;
        Ok(self.rmdir(req, parent, name).await?)
    }

    async fn rename_local(
        &self,
        req: &RequestInfo,
        parent: u64,
        name: &[u8],
        newparent: u64,
        newname: &[u8],
        flags: u32,
    ) -> Result<(), ReadFault> {
        self.writes_deferred()?;
        Ok(self
            .rename(req, parent, name, newparent, newname, flags)
            .await?)
    }

    async fn write_local(
        &self,
        req: &RequestInfo,
        ino: u64,
        fh: u64,
        offset: u64,
        data: &[u8],
        write_flags: u32,
    ) -> Result<usize, ReadFault> {
        self.writes_deferred()?;
        Ok(self.write(req, ino, fh, offset, data, write_flags).await?)
    }

    async fn setattr_local(
        &self,
        req: &RequestInfo,
        ino: u64,
        attr: &SetattrIn,
    ) -> Result<AttrReply, ReadFault> {
        self.writes_deferred()?;
        Ok(self.setattr(req, ino, attr).await?)
    }

    async fn fallocate_local(
        &self,
        req: &RequestInfo,
        ino: u64,
        fh: u64,
        offset: u64,
        length: u64,
        mode: u32,
    ) -> Result<(), ReadFault> {
        self.writes_deferred()?;
        Ok(self.fallocate(req, ino, fh, offset, length, mode).await?)
    }

    async fn fsync_local(
        &self,
        req: &RequestInfo,
        ino: u64,
        fh: u64,
        datasync: bool,
    ) -> Result<(), ReadFault> {
        self.writes_deferred()?;
        Ok(self.fsync(req, ino, fh, datasync).await?)
    }

    async fn fetch(&self, _req: &RequestInfo, what: Fetch<'_>) -> Result<(), Errno> {
        self.fetches.lock().push(what.into());
        if self.hang_fetch_phase.load(Ordering::SeqCst) {
            self.fetch_phase_gate.notified().await;
        }
        Ok(())
    }
}
