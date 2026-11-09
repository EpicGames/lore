// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Lowering the calling thread's disk priority for bulk work the user is not waiting on.
//!
//! - macOS: `setiopolicy_np` with `IOPOL_THROTTLE` for the thread. Measured: Lore commands and a
//!   build ran normally beside a delete of five million files at this priority.
//! - Linux: `ioprio_set` with the lowest best-effort level for the thread. Only the BFQ I/O
//!   scheduler orders requests by level. mq-deadline honours the class alone, and kyber and
//!   `none`, the usual default for NVM Express disks, ignore I/O priority. The kernel issues
//!   much of a removal's metadata writing from its own threads, which the setting does not
//!   cover. Unmeasured. The idle class would give the disk only when no one else wants it, which
//!   on a busy machine can be never.
//! - Windows: `THREAD_MODE_BACKGROUND_BEGIN`, which lowers the thread's disk and memory priority
//!   along with its scheduling priority. Unmeasured.
//!
//! The tests check the priority the system reports for the thread, not how the disk shares its
//! time. Where the system refuses, the work runs at its normal priority.

/// Keeps the calling thread at background disk priority until dropped, then restores what it
/// had. It belongs to the thread that made it.
///
/// Crate-private, so that the file operations it lowers are the driver's own.
#[lore_macro::test_pub]
pub(crate) struct BackgroundIo {
    previous: Option<i32>,
    _thread_bound: std::marker::PhantomData<*const ()>,
}

impl BackgroundIo {
    /// Lowers the calling thread's disk priority, and keeps the priority it had for the drop to
    /// restore. Where the system refuses, the thread keeps its priority and the drop changes
    /// nothing.
    #[lore_macro::test_pub]
    pub(crate) fn begin() -> Self {
        Self {
            previous: imp::begin(),
            _thread_bound: std::marker::PhantomData,
        }
    }
}

impl Drop for BackgroundIo {
    fn drop(&mut self) {
        if let Some(previous) = self.previous {
            imp::end(previous);
        }
    }
}

#[cfg(feature = "test-util")]
pub mod test_util {
    /// The calling thread's disk priority as the system reports it, `None` where the system
    /// reports none.
    pub fn thread_disk_priority() -> Option<i32> {
        super::imp::current()
    }

    /// The disk priority a [`BackgroundIo`](super::BackgroundIo) gives the thread, as
    /// [`thread_disk_priority`] reports it, `None` where the system reports none.
    pub fn background_disk_priority() -> Option<i32> {
        super::imp::BACKGROUND
    }

    /// The priority a [`BackgroundIo`](super::BackgroundIo) gives back to a thread that had
    /// `priority`, as [`thread_disk_priority`] reports them.
    pub fn restorable_disk_priority(priority: i32) -> i32 {
        super::imp::restorable(priority)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    unsafe extern "C" {
        fn getiopolicy_np(iotype: libc::c_int, scope: libc::c_int) -> libc::c_int;
        fn setiopolicy_np(
            iotype: libc::c_int,
            scope: libc::c_int,
            policy: libc::c_int,
        ) -> libc::c_int;
    }
    const IOPOL_TYPE_DISK: libc::c_int = 0;
    const IOPOL_SCOPE_THREAD: libc::c_int = 1;
    const IOPOL_THROTTLE: libc::c_int = 3;
    #[cfg(feature = "test-util")]
    pub(super) const BACKGROUND: Option<i32> = Some(IOPOL_THROTTLE);

    /// What [`current`] reports for a thread given `priority` back: the same.
    #[cfg(feature = "test-util")]
    pub(super) fn restorable(priority: i32) -> i32 {
        priority
    }

    pub(super) fn current() -> Option<i32> {
        // SAFETY: the call takes plain integers and reads the calling thread's policy.
        let policy = unsafe { getiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD) };
        (policy >= 0).then_some(policy)
    }

    pub(super) fn begin() -> Option<i32> {
        let previous = current()?;
        // SAFETY: the call takes plain integers and acts on the calling thread.
        (unsafe { setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD, IOPOL_THROTTLE) } == 0)
            .then_some(previous)
    }

    pub(super) fn end(previous: i32) {
        // SAFETY: as in `begin`.
        unsafe { setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD, previous) };
    }
}

#[cfg(target_os = "linux")]
mod imp {
    const IOPRIO_WHO_PROCESS: libc::c_long = 1;
    const IOPRIO_CLASS_NONE: libc::c_long = 0;
    const IOPRIO_CLASS_BE: libc::c_long = 2;
    const IOPRIO_BE_LOWEST: libc::c_long = 7;
    const IOPRIO_CLASS_SHIFT: libc::c_long = 13;
    const LOWEST: libc::c_long = (IOPRIO_CLASS_BE << IOPRIO_CLASS_SHIFT) | IOPRIO_BE_LOWEST;
    #[cfg(feature = "test-util")]
    pub(super) const BACKGROUND: Option<i32> = Some(LOWEST as i32);

    // A `who` of 0 names the calling thread.
    pub(super) fn current() -> Option<i32> {
        // SAFETY: the ioprio system calls take plain integers.
        let priority = unsafe { libc::syscall(libc::SYS_ioprio_get, IOPRIO_WHO_PROCESS, 0) };
        (priority >= 0).then_some(priority as i32)
    }

    /// The value `ioprio_set` takes to give a thread `priority` back. A thread that set none
    /// reads, on some kernels, as the no-priority class with a level, which `ioprio_set` refuses,
    /// so it is given the class alone, which `ioprio_set` reads as none set. Kernels that read
    /// such a thread as the normal best-effort level give that level back, whatever its nice value
    /// gave it.
    pub(super) fn restorable(priority: i32) -> i32 {
        if priority as libc::c_long >> IOPRIO_CLASS_SHIFT == IOPRIO_CLASS_NONE {
            0
        } else {
            priority
        }
    }

    pub(super) fn begin() -> Option<i32> {
        let previous = restorable(current()?);
        // SAFETY: as in `current`.
        (unsafe { libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, LOWEST) } == 0)
            .then_some(previous)
    }

    pub(super) fn end(previous: i32) {
        // SAFETY: as in `current`.
        unsafe {
            libc::syscall(
                libc::SYS_ioprio_set,
                IOPRIO_WHO_PROCESS,
                0,
                previous as libc::c_long,
            )
        };
    }
}

#[cfg(target_family = "windows")]
mod imp {
    #[cfg(feature = "test-util")]
    use windows_sys::Win32::Foundation::ERROR_THREAD_MODE_ALREADY_BACKGROUND;
    #[cfg(feature = "test-util")]
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::Threading::GetCurrentThread;
    use windows_sys::Win32::System::Threading::SetThreadPriority;
    use windows_sys::Win32::System::Threading::THREAD_MODE_BACKGROUND_BEGIN;
    use windows_sys::Win32::System::Threading::THREAD_MODE_BACKGROUND_END;

    /// What [`current`] reports for a thread in background mode, and for one outside it.
    #[cfg(feature = "test-util")]
    const IN_BACKGROUND: i32 = 1;
    #[cfg(feature = "test-util")]
    const NORMAL: i32 = 0;
    #[cfg(feature = "test-util")]
    pub(super) const BACKGROUND: Option<i32> = Some(IN_BACKGROUND);

    /// What [`current`] reports for a thread given `priority` back: the same.
    #[cfg(feature = "test-util")]
    pub(super) fn restorable(priority: i32) -> i32 {
        priority
    }

    /// Windows reports no disk priority for a thread, but refuses to put a thread in background
    /// mode twice, so a try tells whether the thread is in it. A try that succeeds is undone.
    #[cfg(feature = "test-util")]
    pub(super) fn current() -> Option<i32> {
        if begin().is_some() {
            end(NORMAL);
            return Some(NORMAL);
        }
        // SAFETY: the call reads the calling thread's last error code.
        (unsafe { GetLastError() } == ERROR_THREAD_MODE_ALREADY_BACKGROUND).then_some(IN_BACKGROUND)
    }

    pub(super) fn begin() -> Option<i32> {
        // SAFETY: the pseudo handle names the calling thread and needs no closing.
        (unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) } != 0)
            .then_some(0)
    }

    pub(super) fn end(_previous: i32) {
        // SAFETY: as in `begin`.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END) };
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_family = "windows")))]
mod imp {
    #[cfg(feature = "test-util")]
    pub(super) const BACKGROUND: Option<i32> = None;

    #[cfg(feature = "test-util")]
    pub(super) fn restorable(priority: i32) -> i32 {
        priority
    }

    #[cfg(feature = "test-util")]
    pub(super) fn current() -> Option<i32> {
        None
    }

    pub(super) fn begin() -> Option<i32> {
        None
    }

    pub(super) fn end(_previous: i32) {}
}
