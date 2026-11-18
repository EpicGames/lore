use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use lore_storage::ContentSource;
use lore_storage::content::RangedRead;
use tokio::sync::Semaphore;

/// Bounds how many syscalls SWFS operations have in flight on the mounts this process serves
/// (ADR-00035).
///
/// A syscall on a mount runs on lore-io's syscall pool and can wait in the kernel for an SWFS
/// callback, which reads the store through the same pool before it returns. Held well below the
/// pool's size, the mount syscalls leave threads for the callbacks they wait on, unless the pool
/// has a single thread, which the bound of at least one then takes whole. The bound is one for the
/// process, not one per mount: every mount shares the one pool, so a bound per mount would let
/// enough mounts fill it.
pub struct MountIo {
    permits: Semaphore,
    in_flight: AtomicUsize,
    most_in_flight: AtomicUsize,
}

/// A permit to make one call on a mount, or one short sequence of them, held around those alone. A pointer wide, as a future
/// awaiting the call holds it: the semaphore's permit is given back when it is dropped.
pub struct MountPermit<'a> {
    io: &'a MountIo,
}

impl Drop for MountPermit<'_> {
    fn drop(&mut self) {
        self.io.in_flight.fetch_sub(1, Ordering::Relaxed);
        self.io.permits.add_permits(1);
    }
}

impl MountIo {
    /// A bound of `limit` syscalls at once, at least one.
    pub fn new(limit: usize) -> Self {
        Self {
            permits: Semaphore::new(limit.max(1)),
            in_flight: AtomicUsize::new(0),
            most_in_flight: AtomicUsize::new(0),
        }
    }

    /// The bound every SWFS operation in the process shares: a quarter of the syscall pool's
    /// threads, at least one, which leaves three quarters of the pool to the callbacks the mount
    /// syscalls wait on and to the process's other I/O.
    pub fn global() -> &'static MountIo {
        static GLOBAL: LazyLock<MountIo> =
            LazyLock::new(|| MountIo::new(lore_io::pool_stats().max_threads / 4));
        &GLOBAL
    }

    /// Waits for a permit to make one call on a mount.
    pub async fn permit(&self) -> MountPermit<'_> {
        // The semaphore is never closed, so the acquire does not fail. The permit is given back
        // by the `MountPermit` instead, which holds no pointer to the semaphore of its own.
        if let Ok(permit) = self.permits.acquire().await {
            permit.forget();
        }
        let in_flight = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        self.most_in_flight.fetch_max(in_flight, Ordering::Relaxed);
        MountPermit { io: self }
    }

    /// The most callers that held a permit at once.
    pub fn most_in_flight(&self) -> usize {
        self.most_in_flight.load(Ordering::Relaxed)
    }

    /// The content of the file at `path` on a mount, which is read through the mount a range at a
    /// time, each read with a permit from `self`, so no read of it holds more than one window and
    /// none waits on the pool outside the bound. Its size is the file's when named, so it is read
    /// before the operation writes the path again.
    pub async fn content(&'static self, path: PathBuf) -> std::io::Result<ContentSource<'static>> {
        let size = {
            let _permit = self.permit().await;
            lore_io::IoDriver::global().stat(path.clone()).await?.len
        };
        if size == 0 {
            return Ok(ContentSource::memory(Bytes::new()));
        }
        Ok(ContentSource::ranged(
            Arc::new(MountedRange { io: self, path }),
            size,
        ))
    }
}

/// The content of a file on a mount, read a range at a time within the bound.
struct MountedRange {
    io: &'static MountIo,
    path: PathBuf,
}

impl RangedRead for MountedRange {
    fn read_range<'a>(
        &'a self,
        offset: u64,
        into: &'a mut [u8],
    ) -> futures::future::BoxFuture<'a, std::io::Result<()>> {
        Box::pin(async move {
            let _permit = self.io.permit().await;
            // Opened for each range, so no handle stays open on the mount between reads.
            let file = lore_io::IoDriver::global()
                .open(&self.path, &lore_io::OpenOptions::new().read(true))
                .await?;
            let bytes = file.read_exact_at(into.len(), offset).await?;
            into.copy_from_slice(&bytes);
            Ok(())
        })
    }
}
