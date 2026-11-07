//! This module wraps the SWFS C API. Code that requires the SWFS library to be linked is
//! conditionally compiled for x86-64 Windows, the only target SWFS is available on.
//! The `SwfsInterface` type will always have the same interface, but on other targets it will
//! return a `SwfsInterfaceError`.
//! The `swfs_api` module will always contain the C API's types even if the library isn't linked.
pub mod swfs_api;

use std::ops::Deref;
use std::sync::Arc;

use dashmap::DashSet;
use lore_base::log::LoreLogLevel;
use lore_error_set::error_set;
use swfs_api::SWFSHandle;

use crate::event::LoreErrorDetail;
use crate::fs::swfs::handle_gate::HandleGate;
use crate::fs::swfs::handle_gate::HandleGateGuard;
use crate::fs::swfs::mount_resources::MountResources;
use crate::fs::swfs::mount_resources::SwfsExecutionToken;
use crate::fs::swfs::paths::RelativePathKey;
use crate::service_state::ServiceStateImpl;

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct WrappedHandle(#[allow(unused)] SWFSHandle);
unsafe impl Send for WrappedHandle {}
unsafe impl Sync for WrappedHandle {}

impl WrappedHandle {
    #[allow(unused)]
    pub fn handle(&self) -> SWFSHandle {
        self.0
    }

    pub fn new(handle: SWFSHandle) -> Self {
        Self(handle)
    }
}

#[error_set]
pub enum SwfsInterfaceError {}

#[derive(Debug)]
pub struct SwfsInterface {
    handle: WrappedHandle,
    /// Entered for every use of `handle` and of the interface from an SWFS callback, and closed
    /// by [`SwfsInterface::close`], which waits for the uses in progress before it closes the
    /// handle.
    gate: HandleGate,
    #[allow(unused)]
    resources: MountResources,
}

/// The SWFS handle, for one use of it. [`SwfsInterface::close`] waits for the guard to be
/// dropped before it closes the handle, so hold it across the call into SWFS.
pub struct SwfsHandleGuard<'a> {
    handle: &'a WrappedHandle,
    _use: HandleGateGuard<'a>,
}

impl Deref for SwfsHandleGuard<'_> {
    type Target = WrappedHandle;

    fn deref(&self) -> &WrappedHandle {
        self.handle
    }
}

impl SwfsInterface {
    /// The handle for one use of it, which keeps the mount from closing until the guard is
    /// dropped. Fails once the mount has begun to close.
    pub fn handle(&self) -> Result<SwfsHandleGuard<'_>, SwfsInterfaceError> {
        let Some(guard) = self.gate.enter() else {
            return Err(SwfsInterfaceError::internal(
                "Accessing SWFS handle after closing",
            ));
        };
        Ok(SwfsHandleGuard {
            handle: &self.handle,
            _use: guard,
        })
    }

    /// The handle as a key for finding this interface from an SWFS callback. It is never passed
    /// to SWFS, so it stays valid after the mount closes, to remove the entry it keys.
    pub fn lookup_key(&self) -> WrappedHandle {
        self.handle.clone()
    }

    /// Logs `error` and keeps it for the service's status, for code that can't propagate an error
    /// upwards by returning it, such as callbacks from SWFS.
    pub(crate) fn handle_lore_error(
        error: impl lore_error_set::FfiError + std::fmt::Display + lore_error_set::HasTrace,
    ) {
        let message = LoreErrorDetail::from_error(&error).message_with_trace();
        // Logged as well as kept for the service's status, as `handle_swfs_error` does: a
        // `read_file` that fails here is served as zeros.
        crate::lore_error!("SWFS callback failed: {message}");
        ServiceStateImpl::global().push_log(LoreLogLevel::Error, message);
    }
}

impl Drop for SwfsInterface {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Logic elsewhere should prevent an `SwfsInterface` from being constructed on a target SWFS
/// isn't available on, but have this in case of logic errors.
#[cfg(not(all(windows, target_arch = "x86_64")))]
impl SwfsInterface {
    fn no_swfs_error() -> SwfsInterfaceError {
        SwfsInterfaceError::internal("SWFS is only available on x86-64 Windows")
    }

    pub fn resources(&self) -> &MountResources {
        &self.resources
    }

    #[allow(clippy::unused_async)]
    pub async fn freeze(&self, _token: SwfsExecutionToken) -> Result<(), SwfsInterfaceError> {
        Err(Self::no_swfs_error())
    }
    #[allow(clippy::unused_async)]
    pub async fn thaw(
        &self,
        _token: SwfsExecutionToken,
        _apply_changes: bool,
        _written_paths: &DashSet<RelativePathKey>,
    ) -> Result<(), SwfsInterfaceError> {
        Err(Self::no_swfs_error())
    }
    pub fn mount(_resources: MountResources) -> Result<Arc<Self>, SwfsInterfaceError> {
        Err(Self::no_swfs_error())
    }
    #[allow(clippy::unused_async)]
    pub async fn wait_for_ready(&self) -> Result<(), SwfsInterfaceError> {
        let _ = self;
        Err(Self::no_swfs_error())
    }

    /// Close the mount. Does nothing if it is already closed.
    pub fn close(&self) {
        self.gate.close(|| {});
    }

    fn cleanup(&mut self) {
        self.close();
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
mod swfs_impl {
    use std::ffi::CStr;
    use std::ffi::c_char;
    use std::ffi::c_int;
    use std::ffi::c_void;
    use std::ptr::slice_from_raw_parts_mut;

    use lore_base::log::LoreLogLevel;
    use lore_error_set::ForwardStrict;
    use tokio::task::yield_now;

    use super::*;
    use crate::fs::mount_manager_state::MountManagerState;
    use crate::fs::swfs::api_interface::swfs_api::SWFSCallbacks;
    use crate::fs::swfs::api_interface::swfs_api::SWFSFile;
    use crate::fs::swfs::api_interface::swfs_api::SWFSHandle;
    use crate::fs::swfs::api_interface::swfs_api::SWFSInit;
    use crate::fs::swfs::api_interface::swfs_api::SWFSResult_Enum;
    use crate::fs::swfs::api_interface::swfs_api::swfs_u64;
    use crate::fs::swfs::api_interface::swfs_api::swfs_ubyte;
    use crate::fs::swfs::api_interface::swfs_api::swfsClose;
    use crate::fs::swfs::api_interface::swfs_api::swfsFillDirAddFiles;
    use crate::fs::swfs::api_interface::swfs_api::swfsFreeze;
    use crate::fs::swfs::api_interface::swfs_api::swfsInit;
    use crate::fs::swfs::api_interface::swfs_api::swfsIsBusy;
    use crate::fs::swfs::api_interface::swfs_api::swfsThaw;
    use crate::fs::swfs::file::SwfsFileArray;
    use crate::fs::swfs::mount_resources::MountResources;
    use crate::fs::swfs::mount_resources::SwfsWorkError;
    use crate::fs::swfs::panic_guard::panic_guard_swfs_callback;
    use crate::fs::swfs::paths::SwfsPath;
    use crate::fs::swfs::state::written_files;
    use crate::lore_debug;
    use crate::service_state::ServiceStateImpl;

    impl SwfsInterface {
        pub fn resources(&self) -> &MountResources {
            &self.resources
        }

        #[allow(clippy::unused_async)]
        pub async fn freeze(&self, _token: SwfsExecutionToken) -> Result<(), SwfsWorkError> {
            let handle = self.handle().forward::<SwfsWorkError>("Unable to freeze")?;
            if unsafe {
                swfsFreeze(
                    handle.handle(),
                    Some(swfs_freeze_callback),
                    std::ptr::null_mut(),
                )
            } {
                Ok(())
            } else {
                Err(SwfsWorkError::internal("Failed to freeze"))
            }
        }

        pub async fn thaw(
            &self,
            token: SwfsExecutionToken,
            apply_changes: bool,
            written_paths: &DashSet<RelativePathKey>,
        ) -> Result<(), SwfsWorkError> {
            let file_time: u64;
            #[cfg(target_os = "windows")]
            {
                let mut windows_file_time = windows_sys::Win32::Foundation::FILETIME::default();
                unsafe {
                    windows_sys::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime(
                        std::ptr::from_mut(&mut windows_file_time),
                    );
                }
                file_time = ((windows_file_time.dwHighDateTime as u64) << 32)
                    + windows_file_time.dwLowDateTime as u64;
            }
            #[cfg(not(target_os = "windows"))]
            {
                file_time = 0;
            }

            // The written nodes are described as the state after this operation holds them.
            self.resources.update_state(token).await?;
            let state = self.resources.current_read_state().ok_or_else(|| {
                SwfsWorkError::internal("Describing written nodes with no repository state")
            })?;
            let mut files = Vec::new();
            written_files(
                &mut files,
                self.resources.repository(),
                state,
                written_paths,
                file_time,
            )
            .await?;
            let files = SwfsFileArray::new(files);
            let handle = self
                .handle()
                .forward::<SwfsWorkError>("Getting SWFS handle for thaw")?;
            if unsafe {
                swfsThaw(
                    handle.handle(),
                    if apply_changes {
                        files.array_ptr()
                    } else {
                        std::ptr::null_mut()
                    },
                )
            } {
                Ok(())
            } else {
                Err(SwfsWorkError::internal("Failed to thaw"))
            }
        }

        pub fn mount(resources: MountResources) -> Result<Arc<Self>, SwfsInterfaceError> {
            let mut init = SWFSInit {
                name: resources.name_nul_terminated.as_ptr(),
                write_dir: resources.write_path_nul_terminated.as_ptr(),
                mount_path: resources.mount_path_nul_terminated.as_ptr(),

                callbacks: SWFSCallbacks {
                    read_file: Some(swfs_c_read_file),
                    fill_dir_begin: Some(swfs_c_fill_dir_begin),
                    error_callback: Some(swfs_c_error_callback),
                    notify_write: Some(swfs_c_notify_write),
                    notify_create: Some(swfs_c_notify_create),
                    notify_move: Some(swfs_c_notify_move),
                    notify_delete: Some(swfs_c_notify_delete),
                },
                ..unsafe { std::mem::zeroed::<SWFSInit>() }
            };

            unsafe {
                init.mount_path = init.mount_path.offset(4);
            }

            let mut handle = SWFSHandle::default();

            // Paired with the lines around `swfsClose` in `close`, which name the same handle and
            // path, so that a mount's opening and closing can be matched in a debug log.
            let mount_path = resources.mount_path_nul_terminated.to_string_lossy();
            lore_debug!("swfsInit: mounting {mount_path}");
            unsafe {
                swfsInit(&mut init, &mut handle);
            }
            lore_debug!("swfsInit: mounted {mount_path} as SWFS handle {handle:?}");

            let this = Arc::new(Self {
                handle: WrappedHandle(handle),
                gate: HandleGate::default(),
                resources,
            });
            Ok(this)
        }

        pub async fn wait_for_ready(&self) -> Result<(), SwfsInterfaceError> {
            // A guard for each check rather than across the wait, so a close need not wait for
            // the mount to become ready.
            while unsafe { swfsIsBusy(self.handle()?.handle()) } {
                yield_now().await;
            }
            Ok(())
        }

        /// Unmount from SWFS and release the mount path. Does nothing if it is already closed.
        ///
        /// [`SwfsInterface::handle`] fails from the moment this is called, and SWFS callbacks no
        /// longer reach the interface. The handle is closed once the uses already in progress
        /// end, however long they take, and every caller returns only after it is closed. Must
        /// not be called while holding a [`SwfsHandleGuard`].
        pub fn close(&self) {
            self.gate.close(|| {
                // Paired with the lines around `swfsInit` in `mount`.
                let handle = self.handle.handle();
                let mount_path = self.resources.mount_path_nul_terminated.to_string_lossy();
                lore_debug!("swfsClose: closing SWFS handle {handle:?} at {mount_path}");
                unsafe {
                    swfsClose(handle);
                }
                lore_debug!("swfsClose: closed SWFS handle {handle:?} at {mount_path}");
            });
        }

        pub fn cleanup(&mut self) {
            self.close();
        }

        fn read_file(
            &self,
            file: *mut SWFSFile,
            out_buffer: *mut c_void,
            read_offset: swfs_u64,
            num_bytes_to_read: swfs_u64,
        ) -> swfs_u64 {
            self.resources.callbacks().record_read_file();
            let file = unsafe { &mut *file };
            let file_name = unsafe { CStr::from_ptr(file.path).to_string_lossy() };
            let out_buffer = unsafe {
                &mut *slice_from_raw_parts_mut(out_buffer.cast::<u8>(), num_bytes_to_read as usize)
            };
            let read_result = self
                .resources
                .run_in_runtime_with_context(async move |token| {
                    self.resources
                        .read_file(
                            token,
                            SwfsPath(&file_name),
                            read_offset as usize..(read_offset + num_bytes_to_read) as usize,
                            out_buffer,
                        )
                        .await
                });
            let Some((read_count, file_info)) = Self::handle_lore_result(read_result) else {
                return 0;
            };
            file.finfo.time_create = file_info.mtime();
            file.finfo.time_write = file_info.mtime();
            file.finfo.size = file_info.size();
            file.finfo.attrs = 0;
            read_count as swfs_u64
        }

        fn fill_dir_begin(&self, path: *const c_char) {
            self.resources.callbacks().record_directory_evaluation();
            let Some(handle) = Self::handle_lore_result(self.handle()) else {
                return;
            };
            let result = self
                .resources
                .run_in_runtime_with_context(async move |token| {
                    let path = unsafe { CStr::from_ptr(path).to_string_lossy() };
                    self.resources
                        .enumerate_directory(token, SwfsPath(&path))
                        .await
                });
            if let Some(files) = Self::handle_lore_result(result) {
                unsafe {
                    swfsFillDirAddFiles(handle.handle(), files.array_ptr());
                }
            }
        }

        #[allow(clippy::unused_self)]
        fn notify_write(&self, _file: *mut SWFSFile) {}
        #[allow(clippy::unused_self)]
        fn notify_create(&self, _file: *mut SWFSFile) {}
        #[allow(clippy::unused_self)]
        fn notify_move(&self, _old_file: *mut SWFSFile, _new_file: *mut SWFSFile) {}
        #[allow(clippy::unused_self)]
        fn notify_delete(&self, _file: *mut SWFSFile) {}

        #[allow(clippy::unused_self)]
        fn freeze_callback(
            &self,
            _user_data: *mut ::std::os::raw::c_void,
            _start_buff: *mut swfs_ubyte,
            _num_bytes: usize,
            _out_num_buff_bytes: *mut usize,
        ) -> *mut swfs_ubyte {
            // `start_buff` is an array of `SWFSFile` objects. By default, all files are frozen.
            // Setting the `mod_flag` will cause that file to not be frozen.
            // It would be optimal to only freeze the necessary files, but simply letting all freeze
            // is a correct implementation for the moment.
            std::ptr::null_mut()
        }

        fn handle_swfs_error(
            error_code: SWFSResult_Enum,
            source_filename: *const c_char,
            source_line_number: c_int,
            error_message: *const c_char,
        ) -> bool {
            let source_filename = unsafe { CStr::from_ptr(source_filename) };
            let error_message = unsafe { CStr::from_ptr(error_message) };
            let message = format!(
                "SWFS Error ({}): {}::{}: {}",
                error_code,
                source_filename.to_string_lossy(),
                source_line_number,
                error_message.to_string_lossy()
            );
            // Logged as well as kept for the service's status: the status is only read when asked
            // for, and SWFS serves a read it could not complete as zeros, so this may be the only
            // trace of a file served wrong.
            crate::lore_error!("{message}");
            // SWFS calls this from C with nothing of ours to carry the state,
            // so this is one of the paths that reaches for the global.
            ServiceStateImpl::global().push_log(LoreLogLevel::Error, message);
            false
        }

        /// Handle Lore errors for code that can't propagate an error upwards by returning it, such as
        /// callbacks from SWFS.
        pub fn handle_lore_result<
            T,
            E: lore_error_set::FfiError + std::fmt::Display + lore_error_set::HasTrace,
        >(
            r: Result<T, E>,
        ) -> Option<T> {
            match r {
                Ok(value) => Some(value),
                Err(err) => {
                    Self::handle_lore_error(err);
                    None
                }
            }
        }
    }

    fn lookup_handle(handle: SWFSHandle) -> Result<Arc<SwfsInterface>, SwfsInterfaceError> {
        let mount_manager = MountManagerState::mount_manager().ok_or(
            SwfsInterfaceError::internal("Looking up SWFS interface without SWFS being enabled"),
        )?;
        mount_manager
            .swfs()
            .get_interface_from_swfs_handle(handle)
            .ok_or(SwfsInterfaceError::internal(
                "Missing SWFS interface for handle",
            ))
    }

    /// Creates a C function that takes an `SWFSHandle` as its first argument and calls the associated
    /// method on that handle's `SwfsInterface`, under [`panic_guard_swfs_callback`].
    macro_rules! handle_redirect {
            ($fn_name:ident, $interface_fn_name:ident, ($($arg_name:ident: $arg_ty:ty),* $(,)?) -> $ret_ty:ty) => {
                #[unsafe(no_mangle)]
                pub unsafe extern "C" fn $fn_name(handle: SWFSHandle, $($arg_name: $arg_ty),*) -> $ret_ty {
                    panic_guard_swfs_callback(
                        stringify!($interface_fn_name),
                        || {
                            let Some(interface) = SwfsInterface::handle_lore_result(lookup_handle(handle)) else {
                                return Default::default();
                            };
                            // Held for the whole callback, so a close waits for it to finish.
                            let Ok(_use) = interface.handle() else {
                                return Default::default();
                            };
                            interface.$interface_fn_name($($arg_name),*)
                        },
                    )
                }
            };
            ($fn_name:ident, $interface_fn_name:ident, ($($arg_name:ident: $arg_ty:ty),* $(,)?)) => {
                #[unsafe(no_mangle)]
                pub unsafe extern "C" fn $fn_name(handle: SWFSHandle, $($arg_name: $arg_ty),*) {
                    panic_guard_swfs_callback(
                        stringify!($interface_fn_name),
                        || {
                            let Some(interface) = SwfsInterface::handle_lore_result(lookup_handle(handle)) else {
                                return;
                            };
                            // Held for the whole callback, so a close waits for it to finish.
                            let Ok(_use) = interface.handle() else {
                                return;
                            };
                            interface.$interface_fn_name($($arg_name),*)
                        },
                    )
                }
            };
        }

    handle_redirect!(swfs_c_read_file, read_file, (file: *mut SWFSFile, out_buffer: *mut c_void, read_offset: swfs_u64, num_bytes_to_read: swfs_u64) -> swfs_u64);

    handle_redirect!(swfs_c_fill_dir_begin, fill_dir_begin, (path: *const c_char));
    handle_redirect!(swfs_c_notify_write, notify_write, (file: *mut SWFSFile));
    handle_redirect!(swfs_c_notify_create, notify_create, (file: *mut SWFSFile));
    handle_redirect!(swfs_c_notify_move, notify_move, (old_file: *mut SWFSFile, new_file: *mut SWFSFile));
    handle_redirect!(swfs_c_notify_delete, notify_delete, (file: *mut SWFSFile));
    handle_redirect!(swfs_freeze_callback, freeze_callback, (user_data: *mut ::std::os::raw::c_void, start_buff: *mut swfs_ubyte, num_bytes: usize, out_num_buff_bytes: *mut usize) -> *mut swfs_ubyte);

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn swfs_c_error_callback(
        _handle: SWFSHandle,
        error_code: SWFSResult_Enum,
        source_filename: *const c_char,
        source_line_number: c_int,
        error_message: *const c_char,
    ) -> bool {
        panic_guard_swfs_callback("error_callback", || {
            SwfsInterface::handle_swfs_error(
                error_code,
                source_filename,
                source_line_number,
                error_message,
            )
        })
    }
}
