//! This module wraps the SWFS C API. Code that requires the SWFS library to be linked is
//! conditionally compiled based on the "swfs" feature.
//! The `SwfsInterface` type will always have the same interface, but if the "swfs" feature is
//! unavailable it will return a `SwfsInterfaceError`.
//! The `swfs_api` module will always contain the C API's types even if the library isn't linked.
pub mod swfs_api;

use std::sync::Arc;

use lore_error_set::error_set;
use swfs_api::SWFSHandle;

use crate::fs::swfs::mount_resources::MountResources;
use crate::fs::swfs::mount_resources::SwfsExecutionToken;

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
    #[allow(unused)]
    handle: Option<WrappedHandle>,
    #[allow(unused)]
    resources: MountResources,
}

impl SwfsInterface {
    pub fn handle(&self) -> Result<WrappedHandle, SwfsInterfaceError> {
        match self.handle.clone() {
            Some(handle) => Ok(handle),
            None => Err(SwfsInterfaceError::internal(
                "Accessing SWFS handle after closing",
            )),
        }
    }
}

impl Drop for SwfsInterface {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Logic elsewhere should prevent an `SwfsInterface` from being constructed if the "swfs" feature
/// isn't available, but have this in case of logic errors.
#[cfg(not(feature = "swfs"))]
impl SwfsInterface {
    fn no_swfs_error() -> SwfsInterfaceError {
        SwfsInterfaceError::internal("SWFS feature not enabled")
    }

    pub fn resources(&self) -> &MountResources {
        &self.resources
    }

    pub fn freeze(&self) -> Result<(), SwfsInterfaceError> {
        Err(Self::no_swfs_error())
    }
    #[allow(clippy::unused_async)]
    pub async fn thaw(
        &self,
        _token: SwfsExecutionToken,
        _apply_changes: bool,
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

    fn cleanup(&mut self) {
        let _ = self;
    }
}

#[cfg(feature = "swfs")]
mod swfs_impl {
    use std::ffi::CStr;
    use std::ffi::CString;
    use std::ffi::c_char;
    use std::ffi::c_int;
    use std::ffi::c_void;
    use std::ptr::slice_from_raw_parts_mut;

    use lore_error_set::ForwardStrict;
    use lore_error_set::WrapInternal;
    use tokio::task::yield_now;

    use super::*;
    use crate::event::LoreErrorDetail;
    use crate::fs::filesystem_provider::FileInfo;
    use crate::fs::swfs::api_interface::swfs_api::SWFSCallbacks;
    use crate::fs::swfs::api_interface::swfs_api::SWFSFile;
    use crate::fs::swfs::api_interface::swfs_api::SWFSHandle;
    use crate::fs::swfs::api_interface::swfs_api::SWFSInit;
    use crate::fs::swfs::api_interface::swfs_api::SWFSResult_Enum;
    use crate::fs::swfs::api_interface::swfs_api::swfs_u64;
    use crate::fs::swfs::api_interface::swfs_api::swfsClose;
    use crate::fs::swfs::api_interface::swfs_api::swfsFillDirAddFiles;
    use crate::fs::swfs::api_interface::swfs_api::swfsFreeze;
    use crate::fs::swfs::api_interface::swfs_api::swfsInit;
    use crate::fs::swfs::api_interface::swfs_api::swfsIsBusy;
    use crate::fs::swfs::api_interface::swfs_api::swfsThaw;
    use crate::fs::swfs::file::SwfsFile;
    use crate::fs::swfs::mount_manager_state::MountManagerState;
    use crate::fs::swfs::mount_resources::MountResources;
    use crate::fs::swfs::mount_resources::SwfsWorkError;
    use crate::fs::swfs::paths::SwfsPath;
    use crate::lore_error;

    impl SwfsInterface {
        pub fn resources(&self) -> &MountResources {
            &self.resources
        }

        pub fn freeze(&self) -> Result<(), SwfsInterfaceError> {
            let handle = self.handle()?.handle();
            let root_path = CString::new("\\").internal("Making CString from root directory")?;
            if unsafe { swfsFreeze(handle, root_path.as_ptr(), 1) } {
                Ok(())
            } else {
                Err(SwfsInterfaceError::internal("Failed to freeze"))
            }
        }

        pub async fn thaw(
            &self,
            token: SwfsExecutionToken,
            apply_changes: bool,
        ) -> Result<(), SwfsWorkError> {
            self.resources.update_state(token).await?;

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

            let handle = self
                .handle()
                .forward::<SwfsWorkError>("Getting SWFS handle for thaw")?
                .handle();
            let root_file = SwfsFile::new(
                CString::new("\\").internal("Unable to make CString from root file")?,
                &FileInfo::Directory,
                None,
            )
            .forward::<SwfsWorkError>("Making file info")?;
            if unsafe {
                swfsThaw(
                    handle,
                    if apply_changes {
                        std::ptr::from_ref(&root_file.file())
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

            unsafe {
                swfsInit(&mut init, &mut handle);
            }

            let this = Arc::new(Self {
                handle: Some(WrappedHandle(handle)),
                resources,
            });
            Ok(this)
        }

        pub async fn wait_for_ready(&self) -> Result<(), SwfsInterfaceError> {
            let handle = self.handle()?;
            while unsafe { swfsIsBusy(handle.handle()) } {
                yield_now().await;
            }
            Ok(())
        }

        pub fn cleanup(&mut self) {
            if let Some(handle) = self.handle.clone() {
                unsafe {
                    swfsClose(handle.handle());
                }
            }
        }

        fn read_file(
            &self,
            file: *mut SWFSFile,
            out_buffer: *mut c_void,
            read_offset: swfs_u64,
            num_bytes_to_read: swfs_u64,
        ) -> swfs_u64 {
            let file = unsafe { &mut *file };
            let file_name = unsafe { CStr::from_ptr(file.path).to_string_lossy() };
            let out_buffer = unsafe {
                &mut *slice_from_raw_parts_mut(out_buffer as *mut u8, num_bytes_to_read as usize)
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

        fn notify_write(&self, _file: *mut SWFSFile) {}
        fn notify_create(&self, _file: *mut SWFSFile) {}
        fn notify_move(&self, _old_file: *mut SWFSFile, _new_file: *mut SWFSFile) {}
        fn notify_delete(&self, _file: *mut SWFSFile) {}

        fn handle_swfs_error(
            error_code: SWFSResult_Enum,
            source_filename: *const c_char,
            source_line_number: c_int,
            error_message: *const c_char,
        ) -> bool {
            let source_filename = unsafe { CStr::from_ptr(source_filename) };
            let error_message = unsafe { CStr::from_ptr(error_message) };
            lore_error!(
                "SWFS Error ({}): {}::{}: {}",
                error_code,
                source_filename.to_string_lossy(),
                source_line_number,
                error_message.to_string_lossy()
            );
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

        fn handle_lore_error(
            error: impl lore_error_set::FfiError + std::fmt::Display + lore_error_set::HasTrace,
        ) {
            let error_detail = LoreErrorDetail::from_error(&error);
            lore_error!("{}", error_detail.message_with_trace());
        }
    }

    fn lookup_handle(handle: SWFSHandle) -> Result<Arc<SwfsInterface>, SwfsInterfaceError> {
        let mount_manager = MountManagerState::mount_manager().ok_or(
            SwfsInterfaceError::internal("Looking up SWFS interface without SWFS being enabled"),
        )?;
        mount_manager
            .get_interface_from_swfs_handle(handle)
            .ok_or(SwfsInterfaceError::internal(
                "Missing SWFS interface for handle",
            ))
    }

    /// Creates a C function that takes an `SWFSHandle` as its first argument and calls the associated
    /// method on that handle's `SwfsInterface`.
    macro_rules! handle_redirect {
            ($fn_name:ident, $interface_fn_name:ident, ($($arg_name:ident: $arg_ty:ty),* $(,)?) -> $ret_ty:ty) => {
                #[unsafe(no_mangle)]
                pub unsafe extern "C" fn $fn_name(handle: SWFSHandle, $($arg_name: $arg_ty),*) -> $ret_ty {
                    if let Some(interface) = SwfsInterface::handle_lore_result(lookup_handle(handle)) {
                        interface.$interface_fn_name($($arg_name),*)
                    } else {
                        Default::default()
                    }
                }
            };
            ($fn_name:ident, $interface_fn_name:ident, ($($arg_name:ident: $arg_ty:ty),* $(,)?)) => {
                #[unsafe(no_mangle)]
                pub unsafe extern "C" fn $fn_name(handle: SWFSHandle, $($arg_name: $arg_ty),*) {
                    if let Some(interface) = SwfsInterface::handle_lore_result(lookup_handle(handle)) {
                        interface.$interface_fn_name($($arg_name),*)
                    }
                }
            };
        }

    handle_redirect!(swfs_c_read_file, read_file, (file: *mut SWFSFile, out_buffer: *mut c_void, read_offset: swfs_u64, num_bytes_to_read: swfs_u64) -> swfs_u64);

    handle_redirect!(swfs_c_fill_dir_begin, fill_dir_begin, (path: *const c_char));
    handle_redirect!(swfs_c_notify_write, notify_write, (file: *mut SWFSFile));
    handle_redirect!(swfs_c_notify_create, notify_create, (file: *mut SWFSFile));
    handle_redirect!(swfs_c_notify_move, notify_move, (old_file: *mut SWFSFile, new_file: *mut SWFSFile));
    handle_redirect!(swfs_c_notify_delete, notify_delete, (file: *mut SWFSFile));

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn swfs_c_error_callback(
        _handle: SWFSHandle,
        error_code: SWFSResult_Enum,
        source_filename: *const c_char,
        source_line_number: c_int,
        error_message: *const c_char,
    ) -> bool {
        SwfsInterface::handle_swfs_error(
            error_code,
            source_filename,
            source_line_number,
            error_message,
        )
    }
}
