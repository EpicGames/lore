// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Helpers the FUSE tests and benchmarks share: a seeded xorshift, `O_DIRECT` reads, READDIR
//! reply decoding, a mock session that sends raw requests through the dispatcher, and
//! mount-table probes.
#![allow(dead_code)]

use std::os::fd::AsRawFd as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use lore_fuse::filesystem::RequestInfo;
use lore_fuse::mount::unmount_path;
use lore_revision::fs::fuse::filesystem::FuseFilesystem;
use lore_revision::fs::fuse::inode::ROOT_INO;
use lore_revision::fs::mount_manager_state::MountManagerState;

/// The page size `O_DIRECT` aligns to.
pub const PAGE: usize = 4096;

/// A seeded xorshift64. A zero state stays zero, so seeds are made odd where they may be zero.
pub struct XorShift(pub u64);

impl XorShift {
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// `len` incompressible bytes, distinct per `seed`: the low byte of each xorshift step.
pub fn incompressible(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = XorShift(seed | 1);
    (0..len).map(|_| rng.next_u64() as u8).collect()
}

/// A zeroed page-aligned buffer, as `O_DIRECT` requires.
pub struct Aligned {
    ptr: *mut u8,
    layout: std::alloc::Layout,
}

impl Aligned {
    pub fn new(len: usize) -> Self {
        let layout = std::alloc::Layout::from_size_align(len, PAGE).expect("layout");
        // SAFETY: `layout` has a non-zero size.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null());
        Self { ptr, layout }
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` holds `layout.size()` initialized bytes for as long as `self` lives.
        unsafe { std::slice::from_raw_parts(self.ptr, self.layout.size()) }
    }
}

impl Drop for Aligned {
    fn drop(&mut self) {
        // SAFETY: allocated with this layout.
        unsafe { std::alloc::dealloc(self.ptr, self.layout) };
    }
}

pub fn open_direct(path: &Path) -> std::fs::File {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
        .expect("O_DIRECT open")
}

/// One `pread` of `len` bytes at `offset` into `buf`; answers the bytes read.
pub fn pread(file: &std::fs::File, buf: &Aligned, len: usize, offset: u64) -> usize {
    assert!(len <= buf.layout.size());
    // SAFETY: the fd is open and `buf` holds at least `len` writable bytes.
    let n = unsafe { libc::pread(file.as_raw_fd(), buf.ptr.cast(), len, offset as libc::off_t) };
    assert!(n >= 0, "pread: {}", std::io::Error::last_os_error());
    n as usize
}

/// One directory entry of a READDIR or READDIRPLUS reply, its name borrowed from the reply.
#[derive(Debug)]
pub struct Dirent<'a> {
    /// The entry's node id; 0 in a READDIR reply.
    pub nodeid: u64,
    pub kind: u32,
    pub name: &'a [u8],
    /// The offset that resumes after this entry.
    pub offset: u64,
}

/// The entries of one READDIR (or, with `plus`, READDIRPLUS) reply, `.` and `..` included.
/// Decoding allocates nothing, so a measured loop may call it.
pub fn dirents(buf: &[u8], plus: bool) -> impl Iterator<Item = Dirent<'_>> {
    let entry_out = std::mem::size_of::<lore_fuse::abi::EntryOut>();
    let word = |at: usize| u64::from_le_bytes(buf[at..at + 8].try_into().expect("8 bytes"));
    let half = |at: usize| u32::from_le_bytes(buf[at..at + 4].try_into().expect("4 bytes"));
    let mut at = 0;
    std::iter::from_fn(move || {
        if at >= buf.len() {
            return None;
        }
        let nodeid = if plus {
            let nodeid = word(at);
            at += entry_out;
            nodeid
        } else {
            0
        };
        let offset = word(at + 8);
        let len = half(at + 16) as usize;
        let kind = half(at + 20);
        let name = &buf[at + 24..at + 24 + len];
        at += (24 + len).div_ceil(8) * 8;
        Some(Dirent {
            nodeid,
            kind,
            name,
            offset,
        })
    })
}

/// A request from root, as the tests issue them directly.
pub fn req() -> RequestInfo {
    RequestInfo {
        unique: 1,
        nodeid: ROOT_INO,
        uid: 0,
        gid: 0,
        pid: 0,
    }
}

/// One raw request: its header, for node `nodeid`, then `body`.
pub fn raw_request(
    opcode: lore_fuse::abi::Opcode,
    unique: u64,
    nodeid: u64,
    body: &[u8],
) -> Vec<u8> {
    use zerocopy::IntoBytes as _;

    let header = lore_fuse::abi::InHeader {
        len: (std::mem::size_of::<lore_fuse::abi::InHeader>() + body.len()) as u32,
        opcode: opcode as u32,
        unique,
        nodeid,
        uid: 0,
        gid: 0,
        pid: 0,
        total_extlen: 0,
        padding: 0,
    };
    let mut bytes = header.as_bytes().to_vec();
    bytes.extend_from_slice(body);
    bytes
}

/// A session serving `fs` over a [`MockTransport`](lore_fuse::transport::mock::MockTransport)
/// with `workers` workers, past INIT: requests go through the dispatcher's reply path, which the
/// test controls.
pub async fn mock_session(
    fs: &Arc<FuseFilesystem>,
    workers: usize,
) -> (
    Arc<
        lore_fuse::session::Session<FuseFilesystem, Arc<lore_fuse::transport::mock::MockTransport>>,
    >,
    Arc<lore_fuse::transport::mock::MockTransport>,
) {
    use lore_fuse::abi::init_flags;
    use zerocopy::IntoBytes as _;

    let transport = lore_fuse::transport::mock::MockTransport::new();
    let (flags, flags2) = init_flags::split(lore_fuse::init::REQUIRED_FLAGS);
    let init = lore_fuse::abi::InitIn {
        major: lore_fuse::abi::KERNEL_MAJOR,
        minor: lore_fuse::abi::REQUIRED_MINOR,
        max_readahead: 0,
        flags,
        flags2,
        unused: [0; 11],
    };
    transport.push_request(raw_request(
        lore_fuse::abi::Opcode::Init,
        1,
        0,
        init.as_bytes(),
    ));
    let session = lore_fuse::session::Session::start(
        fs.clone(),
        Arc::new(transport.clone()),
        lore_fuse::session::SessionConfig {
            workers,
            init: lore_fuse::init::InitConfig::read_write(),
        },
    )
    .await
    .expect("INIT negotiates");
    (session, transport)
}

/// Waits until `transport` recorded the reply to request `unique`.
pub async fn await_reply(transport: &lore_fuse::transport::mock::MockTransport, unique: u64) {
    use zerocopy::FromBytes as _;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let replied = transport.replies.lock().iter().any(|(_, bytes)| {
            lore_fuse::abi::OutHeader::read_from_prefix(bytes)
                .is_ok_and(|(header, _)| header.unique == unique)
        });
        if replied {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "no reply to {unique}");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// Unmounts on drop, so a failing test leaves no mount: one the service's manager still tracks
/// is unmounted lazily through the manager, which forgets it; an untracked one is unmounted
/// directly, lazily if a plain unmount is refused.
pub struct MountGuard(pub PathBuf);

impl Drop for MountGuard {
    fn drop(&mut self) {
        let path = self.0.clone();
        let tracked = MountManagerState::mount_manager()
            .filter(|manager| manager.fuse().mounted(&path).is_some())
            .zip(tokio::runtime::Handle::try_current().ok());
        if tracked.is_none() && !is_mounted(&path) {
            return;
        }
        // Off the runtime: the unmount waits on the kernel, which may wait on the session a
        // runtime worker serves.
        let _ = std::thread::spawn(move || {
            if let Some((manager, runtime)) = tracked
                && runtime
                    .block_on(manager.fuse().unmount(&path, true))
                    .is_ok()
            {
                return;
            }
            if is_mounted(&path) && unmount_path(&path, false).is_err() {
                let _ = unmount_path(&path, true);
            }
        })
        .join();
    }
}

pub struct MountInfo {
    pub source: String,
    pub fstype: String,
    pub mount_options: String,
    pub super_options: String,
}

/// The `/proc/self/mountinfo` entry mounted at `path`, which must already be canonical: this
/// never touches the mount itself.
pub fn mount_entry(path: &Path) -> Option<MountInfo> {
    let contents = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    contents.lines().find_map(|line| {
        let (fields, rest) = line.split_once(" - ")?;
        let fields: Vec<&str> = fields.split_whitespace().collect();
        if Path::new(fields.get(4)?) != path {
            return None;
        }
        let mut rest = rest.split_whitespace();
        Some(MountInfo {
            fstype: rest.next()?.to_string(),
            source: rest.next()?.to_string(),
            super_options: rest.next()?.to_string(),
            mount_options: fields.get(5)?.to_string(),
        })
    })
}

pub fn entry_source(path: &Path) -> Option<String> {
    mount_entry(path).map(|entry| entry.source)
}

pub fn is_mounted(path: &Path) -> bool {
    mount_entry(path).is_some()
}
