// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fs::File;
use std::io::BufRead;
use std::io::Read;
use std::io::Write;
#[cfg(target_os = "windows")]
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::slice;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use dashmap::DashMap;
use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::runtime::runtime;
use parking_lot::Mutex;
use tokio::io::AsyncReadExt;
use tokio::time::Instant;
use windows_sys::Win32;
use windows_sys::Win32::Storage::ProjectedFileSystem;

use crate::immutable;
use crate::interface::ExecutionContext;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_error;
use crate::lore_info;
use crate::node::Node;
use crate::node::NodeLink;
use crate::node::ROOT_NODE;
use crate::repository::RepositoryContext;
use crate::repository::clone::VirtualLayer;
use crate::state::State;
use crate::store::StoreMatch;
use crate::util::path::RelativePath;

const DOT_PROJFSID: &str = ".projfsid";

#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq)]
struct Win32Error(u32);

impl Win32Error {
    pub fn from(error_code: i32) -> Self {
        Self(error_code as u32)
    }

    pub fn get_last_error() -> Self {
        Self(unsafe { Win32::Foundation::GetLastError() })
    }
}

impl std::fmt::Display for Win32Error {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let error_code = self.0;
        let mut buffer = Vec::with_capacity(1000);

        let length = unsafe {
            Win32::System::Diagnostics::Debug::FormatMessageW(
                Win32::System::Diagnostics::Debug::FORMAT_MESSAGE_FROM_SYSTEM
                    | Win32::System::Diagnostics::Debug::FORMAT_MESSAGE_IGNORE_INSERTS,
                std::ptr::null(),
                error_code,
                0,
                buffer.as_mut_ptr(),
                buffer.capacity() as u32,
                std::ptr::null_mut(),
            )
        } as usize;

        let message = if length == 0 {
            None
        } else {
            unsafe {
                buffer.set_len(length);
            }
            String::from_utf16(&buffer).ok()
        };

        if let Some(message) = message {
            write!(fmt, "{message} ({error_code:#08x})")
        } else {
            write!(fmt, "{error_code:#08x}")
        }
    }
}

// GUID -> Context conversion is inlined in `context_from_guid` below.
// (Can't `impl From<GUID> for Context` here: orphan rule — both types are foreign
//  to this crate now that Context lives in lore_storage.)

/// ProjFS callbacks report failure to the OS as an HRESULT-shaped i32 (0 == S_OK).
const E_INVALIDARG: i32 = -2147024809; // 0x80070057

struct InstanceContext {
    execution: std::sync::Arc<ExecutionContext>,
    instance: ProjectedFileSystem::PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    instance_info: ProjectedFileSystem::PRJ_VIRTUALIZATION_INSTANCE_INFO,
    layers: Vec<VirtualLayer>,
    entry_map: DashMap<Context, EnumerationInstance>,
    file_log: Option<Mutex<File>>,
}

struct EnumerationEntry {
    node: Node,
    file_name: Vec<u16>,
}

impl PartialEq for EnumerationEntry {
    fn eq(&self, other: &Self) -> bool {
        self.file_name == other.file_name
    }

    fn ne(&self, other: &Self) -> bool {
        self.file_name != other.file_name
    }
}

struct EnumerationInstance {
    search: String,
    search_wide: Vec<u16>,
    base_path: String,
    file: Vec<EnumerationEntry>,
    last_index: Option<usize>,
    capture_search: bool,
    done: bool,
    timestamp: u64,
}

#[cfg(not(target_os = "windows"))]
fn is_supported() -> bool {
    false
}

#[cfg(target_os = "windows")]
fn is_supported() -> bool {
    true
}

pub fn serve(
    path: impl AsRef<Path>,
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    layer: Option<VirtualLayer>,
    prefetch: Option<&str>,
) {
    if !is_supported() {
        lore_error!("ProjectedFS not supported");
        return;
    }

    let file_log = if let Ok(value) = std::env::var("LORE_VIRTUAL_FILE_LOG") {
        if value == "1" && prefetch.is_none() {
            std::fs::OpenOptions::new()
                .write(true)
                .read(true)
                .truncate(true)
                .create(true)
                .open("file_access.log")
                .map(|file| Mutex::new(file))
                .ok()
        } else {
            None
        }
    } else {
        None
    };

    let prefetch = if let Some(prefetch) = prefetch {
        if let Ok(prefetch) = std::fs::OpenOptions::new()
            .write(false)
            .read(true)
            .truncate(false)
            .create(false)
            .open(prefetch)
        {
            let lines: Vec<String> = std::io::BufReader::new(prefetch)
                .lines()
                .filter_map(|res| res.ok())
                .collect();
            Some(lines)
        } else {
            None
        }
    } else {
        None
    };

    let repo_path = match repository.require_path() {
        Ok(repo_path) => repo_path,
        Err(err) => {
            lore_error!("Repository has no working path: {err}");
            return;
        }
    };
    if let Err(err) = std::env::set_current_dir(repo_path) {
        lore_error!("Failed to set repository path as current working dir: {err}");
        return;
    }

    let id_path = path
        .as_ref()
        .join(crate::repository::RepositoryFormat::detect(path.as_ref()).dot_dir())
        .join(DOT_PROJFSID);
    let mut uuid = uuid::Uuid::nil();
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .truncate(false)
        .read(true)
        .write(false)
        .create(false)
        .open(id_path.as_path())
    {
        let mut data = [0; 16];
        if let Ok(numread) = file.read(&mut data) {
            if numread == data.len() {
                uuid = uuid::Uuid::from_bytes(data);
            }
        }
    }

    let pathref = path.as_ref();
    let mut path: Vec<u16> = {
        #[cfg(not(target_os = "windows"))]
        {
            pathref
                .as_os_str()
                .to_string_lossy()
                .encode_utf16()
                .collect()
        }
        #[cfg(target_os = "windows")]
        {
            pathref.as_os_str().encode_wide().collect()
        }
    };
    path.push(0);

    if uuid.is_nil() {
        uuid = uuid::Uuid::now_v7();
        // Safety: Win32 API call
        let res = unsafe {
            ProjectedFileSystem::PrjMarkDirectoryAsPlaceholder(
                path.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                (&raw const uuid).cast::<windows_sys::core::GUID>(),
            )
        };
        if res == 0 {
            let Ok(mut file) = std::fs::OpenOptions::new()
                .truncate(true)
                .read(true)
                .write(true)
                .create(true)
                .open(id_path.as_path())
            else {
                lore_error!("Failed to write ProjectedFS UUID to file");
                return;
            };

            let Ok(_numwrite) = file.write_all(uuid.as_bytes()) else {
                lore_error!("Failed to write ProjectedFS UUID to file");
                return;
            };
        } else {
            lore_error!(
                "Failed to mark directory as ProjectedFS placeholder: {}",
                Win32Error::from(res)
            );
            return;
        }
    }

    let callback_table = ProjectedFileSystem::PRJ_CALLBACKS {
        StartDirectoryEnumerationCallback: Some(start_directory_enumeration),
        EndDirectoryEnumerationCallback: Some(end_directory_enumeration),
        GetDirectoryEnumerationCallback: Some(get_directory_enumeration),
        GetPlaceholderInfoCallback: Some(get_placeholder_info),
        GetFileDataCallback: Some(get_file_data),
        QueryFileNameCallback: Some(query_file_name),
        NotificationCallback: None,
        CancelCommandCallback: None,
    };

    /*
    urc_pfs_instance_context_t instance_context = {0};
    instance_context.repository = repository;
    urc_pfs_instance_context = &instance_context;

    urc_anchor_t anchor = urc_anchor_staged_deserialize(repository->urc_path);
    instance_context.state = urc_state_deserialize(repository->store, repository->id, anchor.signature);

    instance_context.map_mutex = urc_mutex_create();

    PRJ_NOTIFICATION_MAPPING notification_mapping = {0};
    notification_mapping.NotificationBitMask = PRJ_NOTIFY_NEW_FILE_CREATED | PRJ_NOTIFY_FILE_RENAMED |
                                               PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_MODIFIED |
                                               PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_DELETED;
    notification_mapping.NotificationRoot = L"";
    */
    let capped_core_count = std::cmp::min(
        32,
        std::cmp::max(
            std::thread::available_parallelism()
                .map(|count| count.get())
                .unwrap_or(8),
            8,
        ),
    );
    let instance_options = ProjectedFileSystem::PRJ_STARTVIRTUALIZING_OPTIONS {
        Flags: 0,
        PoolThreadCount: capped_core_count as u32,
        ConcurrentThreadCount: capped_core_count as u32,
        NotificationMappings: std::ptr::null_mut(),
        NotificationMappingsCount: 0,
    };

    let mut layers = vec![VirtualLayer {
        module: repository.clone(),
        module_path: RelativePath::default(),
        layer_path: RelativePath::default(),
        state: state.clone(),
    }];
    if let Some(layer) = layer {
        layers.push(layer);
    }

    let mut instance_context = InstanceContext {
        execution: execution_context(),
        instance: std::ptr::null_mut(),
        instance_info: ProjectedFileSystem::PRJ_VIRTUALIZATION_INSTANCE_INFO {
            InstanceID: windows_sys::core::GUID::from_u128(0),
            WriteAlignment: 0,
        },
        layers,
        entry_map: DashMap::default(),
        file_log,
    };

    // Safety: Win32 API call, all passed in raw pointers are valid
    let res = unsafe {
        ProjectedFileSystem::PrjStartVirtualizing(
            path.as_ptr(),
            &callback_table,
            (&raw const instance_context).cast(),
            (&raw const instance_options).cast(),
            (&raw mut instance_context.instance).cast(),
        )
    };
    if res != 0 {
        lore_error!(
            "Failed to start ProjectedFS virtualization: {}",
            Win32Error::from(res)
        );
        return;
    }

    // Safety: Win32 API call, pointers are valid
    unsafe {
        ProjectedFileSystem::PrjGetVirtualizationInstanceInfo(
            instance_context.instance,
            &raw mut instance_context.instance_info,
        );
    }

    lore_info!("Started ProjectedFS service for {}", pathref.display());

    if let Some(prefetch) = prefetch {
        let prefetch_count = prefetch.len();
        lore_info!("Start prefetching {prefetch_count} files");
        lore_spawn!(prefetch_files(repository.clone(), state.clone(), prefetch));
    }

    // Loop
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

unsafe fn wcslen(str: *const u16) -> usize {
    if str.is_null() {
        return 0;
    }

    // Safety: The given pointer is guaranteed to be zero terminated
    unsafe {
        let mut len = 0;
        while *str.wrapping_add(len) != 0 {
            len += 1;
        }

        len
    }
}

/// Convert milliseconds since Unix Epoch to MS filetime, which is
/// the count of 100‑nanosecond intervals since 1601‑01‑01T00:00:00Z
fn ms_filetime(ms: u64) -> u64 {
    const MS_OFFSET_TIME: u64 = 116444736000000000;
    (ms * 10_000) + MS_OFFSET_TIME
}

fn instance_context(cbdata: &*const ProjectedFileSystem::PRJ_CALLBACK_DATA) -> &InstanceContext {
    // Safety: This is guaranteed to be the existing object we passed it an instance creation
    unsafe { &*(**cbdata).InstanceContext.cast::<InstanceContext>() }
}

fn context_from_guid(guid: *const windows_sys::core::GUID) -> Context {
    // Safety: Only valid pointers from ProjFS are passed to this function.
    // GUID and Context share the same 16-byte raw-data layout.
    unsafe { std::mem::transmute_copy::<windows_sys::core::GUID, Context>(&*guid) }
}

unsafe extern "system" fn start_directory_enumeration(
    cbdata: *const ProjectedFileSystem::PRJ_CALLBACK_DATA,
    enumeration_id: *const windows_sys::core::GUID,
) -> i32 {
    // Safety: Guaranteed by ProjectedFS API to be valid
    let path: &[u16] =
        unsafe { slice::from_raw_parts((*cbdata).FilePathName, wcslen((*cbdata).FilePathName)) };
    let path = String::from_utf16_lossy(path);

    let instance_context = instance_context(&cbdata);

    let timestamp = runtime().block_on(LORE_CONTEXT.scope(
        instance_context.execution.clone(),
        async move {
            lore_debug!("Start enumeration: {path}");

            if let Ok(metadata) = instance_context.layers[0]
                .state
                .revision_metadata(instance_context.layers[0].module.clone())
                .await
            {
                metadata.timestamp
            } else {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|t| t.as_millis())
                    .unwrap_or_default() as u64
            }
        },
    ));

    let enum_instance = EnumerationInstance {
        search: String::default(),
        search_wide: Vec::default(),
        base_path: String::default(),
        file: Vec::with_capacity(256),
        last_index: None,
        capture_search: true,
        done: false,
        timestamp,
    };

    let context = context_from_guid(enumeration_id);
    instance_context.entry_map.insert(context, enum_instance);

    0
}

unsafe extern "system" fn end_directory_enumeration(
    cbdata: *const ProjectedFileSystem::PRJ_CALLBACK_DATA,
    enumeration_id: *const windows_sys::core::GUID,
) -> i32 {
    let instance_context = instance_context(&cbdata);
    let context = context_from_guid(enumeration_id);
    instance_context.entry_map.remove(&context);
    0
}

unsafe extern "system" fn get_directory_enumeration(
    cbdata: *const ProjectedFileSystem::PRJ_CALLBACK_DATA,
    enumeration_id: *const windows_sys::core::GUID,
    search_expression: *const u16,
    dir_entry_buffer_handle: *mut core::ffi::c_void,
) -> i32 {
    let instance_context = instance_context(&cbdata);
    let context = context_from_guid(enumeration_id);
    let Some(mut enum_instance) = instance_context.entry_map.get_mut(&context) else {
        return 0;
    };

    match runtime().block_on(
        LORE_CONTEXT.scope(instance_context.execution.clone(), unsafe {
            get_directory_enumeration_async(
                instance_context,
                enum_instance.value_mut(),
                &*cbdata,
                search_expression,
                dir_entry_buffer_handle,
            )
        }),
    ) {
        Ok(()) => 0,
        Err(code) => code,
    }
}

async unsafe fn get_directory_enumeration_async(
    instance_context: &InstanceContext,
    enum_instance: &mut EnumerationInstance,
    cbdata: &ProjectedFileSystem::PRJ_CALLBACK_DATA,
    search_expression: *const u16,
    dir_entry_buffer_handle: *mut core::ffi::c_void,
) -> Result<(), i32> {
    if cbdata.Flags & ProjectedFileSystem::PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN != 0 {
        enum_instance.done = false;
        enum_instance.capture_search = true;
        enum_instance.last_index = None;
        lore_debug!(
            "Get enumeration: RESTART {} ({:x})",
            String::from_utf16_lossy(unsafe {
                slice::from_raw_parts(cbdata.FilePathName, wcslen(cbdata.FilePathName))
            }),
            cbdata.Flags
        );
    } else {
        lore_debug!(
            "Get enumeration: CONTINUE {} ({:x})",
            String::from_utf16_lossy(unsafe {
                slice::from_raw_parts(cbdata.FilePathName, wcslen(cbdata.FilePathName))
            }),
            cbdata.Flags
        );
    }

    if enum_instance.capture_search {
        enum_instance.capture_search = false;
        if search_expression.is_null() || unsafe { search_expression.read() } == 0 {
            lore_debug!("Get enumeration: search <none>");
            enum_instance.search.clear();
        } else {
            enum_instance.search_wide =
                unsafe { slice::from_raw_parts(search_expression, wcslen(search_expression)) }
                    .to_vec();
            enum_instance.search = String::from_utf16_lossy(&enum_instance.search_wide);
            enum_instance.search_wide.push(0);
            lore_debug!("Get enumeration: search {}", enum_instance.search);
        }

        enum_instance.file.clear();

        let file_path = String::from_utf16_lossy(unsafe {
            slice::from_raw_parts(cbdata.FilePathName, wcslen(cbdata.FilePathName))
        });

        let mut base_nodes = vec![];
        base_nodes.resize(instance_context.layers.len(), None);

        // TODO(vri): UCS-19230 - Links: Handle link nodes in ProjFS directory enumeration and find
        let relative_path = RelativePath::new_from_user_path(
            instance_context.layers[0]
                .module
                .path
                .as_deref()
                .expect("virtual layer module path"),
            file_path.as_str(),
        )
        .unwrap_or_default();

        for (layer_index, layer) in instance_context.layers.iter().enumerate() {
            if !file_path.is_empty() {
                if layer.layer_path.is_empty()
                    || layer.layer_path.as_str().starts_with(file_path.as_str())
                {
                    let Ok(node_link) = layer
                        .state
                        .find_node_link(layer.module.clone(), relative_path.as_str())
                        .await
                    else {
                        lore_debug!(
                            "Layer {layer_index} found no node for base directory: {relative_path}"
                        );
                        continue;
                    };
                    if node_link.repository != layer.module.id {
                        lore_debug!("Layer {layer_index} modules not yet supported");
                        continue;
                    }
                    base_nodes[layer_index] = Some(node_link.node);
                    lore_debug!(
                        "Layer {layer_index} base directory: {relative_path} (node {})",
                        node_link.node
                    );
                }
            } else if layer.layer_path.is_empty() {
                base_nodes[layer_index] = Some(ROOT_NODE);
            }
        }

        enum_instance.base_path = String::default();

        if enum_instance.search == "*" || enum_instance.search == "/" {
            // Grab all files
            enum_instance.search_wide.clear();
            enum_instance.search.clear();
        }

        if let Some(sep) = enum_instance.search.rfind('/') {
            if sep > 0 {
                let (directory_path, search) = enum_instance.search.split_at(sep);

                let relative_path = RelativePath::new_from_user_path(
                    instance_context.layers[0]
                        .module
                        .path
                        .as_deref()
                        .expect("virtual layer module path"),
                    directory_path,
                )
                .unwrap_or_default();

                // TODO(vri): UCS-19230 - Links: Handle link nodes in ProjFS directory enumeration and find
                for (layer_index, layer) in instance_context.layers.iter().enumerate() {
                    if layer.layer_path.is_empty()
                        || layer.layer_path.as_str().starts_with(directory_path)
                    {
                        if let Ok(base_link) = layer
                            .state
                            .find_node_link(layer.module.clone(), relative_path.as_str())
                            .await
                        {
                            base_nodes[layer_index] = Some(base_link.node);
                            lore_debug!(
                                "Search directory: Layer {layer_index} search {} directory {} matched node {}",
                                enum_instance.search,
                                directory_path,
                                base_nodes[layer_index].unwrap_or_default()
                            );
                        } else {
                            lore_debug!(
                                "Search directory: Layer {layer_index} search {} directory {} found no link",
                                enum_instance.search,
                                directory_path
                            );
                            base_nodes[layer_index] = None;
                        };
                    } else {
                        base_nodes[layer_index] = None;
                    }
                }

                let search = search.to_string();
                let directory_path = directory_path.to_string();

                lore_debug!("Search: {search}");

                enum_instance.search_wide = search.encode_utf16().collect();
                enum_instance.search_wide.push(0);

                enum_instance.search = search;
                enum_instance.base_path = directory_path;
            }
        }

        let mut files = Vec::with_capacity(250);

        for (layer_index, layer) in instance_context.layers.iter().enumerate() {
            if let Some(base_node) = base_nodes[layer_index] {
                // TODO(vri): UCS-19230 - Links: Handle link nodes in ProjFS directory enumeration and find
                let Ok(child_nodes) = layer
                    .state
                    .collect_named_children_unsorted(layer.module.clone(), base_node, false, false)
                    .await
                else {
                    lore_debug!(
                        "Search directory: Layer {layer_index} failed to enumerate children for {base_node}"
                    );
                    continue;
                };
                let child_nodes = child_nodes.children;

                for child_node in child_nodes.iter() {
                    if let Ok(node) = layer
                        .state
                        .node(layer.module.clone(), child_node.node)
                        .await
                    {
                        lore_debug!(
                            "Search directory: Layer {layer_index} found node {}",
                            child_node.name_string
                        );
                        let mut entry = EnumerationEntry {
                            node,
                            file_name: child_node.name_string.encode_utf16().collect(),
                        };
                        entry.file_name.push(0);

                        files.push(entry);
                    }
                }
            }
        }

        enum_instance.file = files;

        enum_instance.file.sort_unstable_by(|lhs, rhs| unsafe {
            let order = ProjectedFileSystem::PrjFileNameCompare(
                lhs.file_name.as_ptr(),
                rhs.file_name.as_ptr(),
            );
            if order < 0 {
                std::cmp::Ordering::Less
            } else if order > 0 {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        });

        enum_instance.file.dedup();
    }

    let mut index = enum_instance.last_index.unwrap_or(0);
    if index >= enum_instance.file.len() {
        lore_debug!("Get enumeration: done");
        enum_instance.done = true;
        return Ok(());
    }

    let mut added = 0;
    while index < enum_instance.file.len() {
        let file = &enum_instance.file[index];

        if enum_instance.search.is_empty()
            || unsafe {
                ProjectedFileSystem::PrjFileNameMatch(
                    file.file_name.as_ptr(),
                    enum_instance.search_wide.as_ptr(),
                )
            }
        {
            let timestamp = ms_filetime(enum_instance.timestamp) as i64;
            let file_info = if file.node.is_directory() {
                ProjectedFileSystem::PRJ_FILE_BASIC_INFO {
                    IsDirectory: true,
                    FileSize: 0,
                    CreationTime: timestamp,
                    ChangeTime: timestamp,
                    LastAccessTime: timestamp,
                    LastWriteTime: timestamp,
                    FileAttributes: Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY,
                }
            } else {
                ProjectedFileSystem::PRJ_FILE_BASIC_INFO {
                    IsDirectory: false,
                    FileSize: file.node.size as i64,
                    CreationTime: timestamp,
                    ChangeTime: timestamp,
                    LastAccessTime: timestamp,
                    LastWriteTime: timestamp,
                    FileAttributes: Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
                }
            };

            let mut file_path: Vec<u16> = enum_instance.base_path.encode_utf16().collect();
            if file_path.last().is_some_and(|&c| c != ('/' as u16)) {
                file_path.push('/' as u16);
            }
            file_path.extend(&file.file_name);

            let file_path_string = String::from_utf16_lossy(&file_path);

            lore_debug!(
                "Get enumeration: index {} / {} - {} ({} {})",
                index + 1,
                enum_instance.file.len(),
                file_path_string,
                if file_info.IsDirectory { "dir" } else { "file" },
                file_info.FileSize,
            );

            file_path.push(0);

            let res = unsafe {
                ProjectedFileSystem::PrjFillDirEntryBuffer(
                    file_path.as_ptr(),
                    &file_info,
                    dir_entry_buffer_handle,
                )
            };
            if res == ERROR_INSUFFICIENT_BUFFER as i32 {
                enum_instance.last_index = Some(index);

                // According to docs, if it returns HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER) for the first entry added
                // during any invocation of a PRJ_GET_DIRECTORY_ENUMERATION_CB callback, the provider must return
                // HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER) from the callback.
                if added == 0 {
                    lore_debug!(
                        "Get enumeration: insufficient buffer on first element, return error"
                    );
                    return Err(res);
                }

                // According to docs, if this routine returns HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER) when adding
                // an entry to the enumeration, the provider returns S_OK from the callback and waits for the next
                // PRJ_GET_DIRECTORY_ENUMERATION_CB callback.
                lore_debug!("Get enumeration: insufficient buffer");
                return Ok(());
            }
        }

        index += 1;
        added += 1;
    }

    enum_instance.last_index = Some(index);

    lore_debug!("Get enumeration: exit");

    Ok(())
}

const ERROR_FILE_NOT_FOUND: i32 = 0x80070002u32 as i32;
const ERROR_OUTOFMEMORY: i32 = 0x8007000eu32 as i32;
const ERROR_READ_FAULT: i32 = 0x8007001eu32 as i32;
const ERROR_INSUFFICIENT_BUFFER: i32 = 0x8007007au32 as i32;

unsafe extern "system" fn get_placeholder_info(
    cbdata: *const ProjectedFileSystem::PRJ_CALLBACK_DATA,
) -> i32 {
    let instance_context = instance_context(&cbdata);

    // Safety: Guaranteed by ProjectedFS API to be valid
    let path: &[u16] =
        unsafe { slice::from_raw_parts((*cbdata).FilePathName, wcslen((*cbdata).FilePathName)) };
    let path = String::from_utf16_lossy(path);

    match runtime().block_on(
        LORE_CONTEXT.scope(instance_context.execution.clone(), unsafe {
            get_placeholder_info_async(instance_context, cbdata, path)
        }),
    ) {
        Ok(()) => 0,
        Err(code) => code,
    }
}

async unsafe fn get_placeholder_info_async(
    instance_context: &InstanceContext,
    cbdata: *const ProjectedFileSystem::PRJ_CALLBACK_DATA,
    path: String,
) -> Result<(), i32> {
    for (layer_index, layer) in instance_context.layers.iter().enumerate() {
        let repository = layer.module.clone();
        let state = layer.state.clone();

        let relative_path = RelativePath::new_from_user_path(
            repository.require_path().map_err(|_| E_INVALIDARG)?,
            path.as_str(),
        )
        .unwrap_or_default();

        let Ok(node_link) = state
            .find_node_link(repository.clone(), relative_path.as_str())
            .await
        else {
            continue;
        };
        if !node_link.is_valid() {
            continue;
        }
        let Ok((repository, state)) = node_link.resolve(repository.clone(), state.clone()).await
        else {
            continue;
        };
        let Ok(node) = state.node(repository.clone(), node_link.node).await else {
            continue;
        };
        if node.is_staged_delete() {
            continue;
        }

        let timestamp = if let Ok(metadata) = state.revision_metadata(repository.clone()).await {
            metadata.timestamp
        } else {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|t| t.as_millis())
                .unwrap_or_default() as u64
        };

        let timestamp = ms_filetime(timestamp) as i64;

        // Safety: This type is safe to zero initialize but not marked with default in windows_sys
        let mut placeholder_info = ProjectedFileSystem::PRJ_PLACEHOLDER_INFO::default();

        if node.is_directory() {
            placeholder_info.FileBasicInfo.IsDirectory = true;
            placeholder_info.FileBasicInfo.FileAttributes =
                Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
        } else {
            placeholder_info.FileBasicInfo.FileSize = node.size as i64;
            placeholder_info.FileBasicInfo.FileAttributes =
                Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
        }

        placeholder_info.FileBasicInfo.CreationTime = timestamp;
        placeholder_info.FileBasicInfo.ChangeTime = timestamp;
        placeholder_info.FileBasicInfo.LastAccessTime = timestamp;
        placeholder_info.FileBasicInfo.LastWriteTime = timestamp;

        placeholder_info.VersionInfo.ProviderID[..std::mem::size_of::<Context>()]
            .copy_from_slice(repository.id.data());
        placeholder_info.VersionInfo.ContentID[..std::mem::size_of::<Hash>()]
            .copy_from_slice(node.address.hash.data());

        lore_debug!(
            "Get placeholder info: Layer {layer_index} path {path} ({} {})",
            if placeholder_info.FileBasicInfo.IsDirectory {
                "dir"
            } else {
                "file"
            },
            placeholder_info.FileBasicInfo.FileSize
        );

        unsafe {
            ProjectedFileSystem::PrjWritePlaceholderInfo(
                instance_context.instance,
                (*cbdata).FilePathName,
                &placeholder_info,
                std::mem::size_of::<ProjectedFileSystem::PRJ_PLACEHOLDER_INFO>() as u32,
            );
        }

        return Ok(());
    }

    Err(ERROR_FILE_NOT_FOUND)
}

unsafe extern "system" fn query_file_name(
    cbdata: *const ProjectedFileSystem::PRJ_CALLBACK_DATA,
) -> i32 {
    let instance_context = instance_context(&cbdata);

    // Safety: Guaranteed by ProjectedFS API to be valid
    let path: &[u16] =
        unsafe { slice::from_raw_parts((*cbdata).FilePathName, wcslen((*cbdata).FilePathName)) };
    let path = String::from_utf16_lossy(path);
    let relative_path = RelativePath::new_from_user_path(
        instance_context.layers[0]
            .module
            .path
            .as_deref()
            .expect("virtual layer module path"),
        path.as_str(),
    )
    .unwrap_or_default();

    let node_link = runtime().block_on(LORE_CONTEXT.scope(
        instance_context.execution.clone(),
        query_file_name_async(instance_context, relative_path),
    ));

    if node_link.is_ok_and(|node_link| node_link.is_valid()) {
        return 0;
    }

    ERROR_FILE_NOT_FOUND
}

async fn query_file_name_async(
    instance_context: &InstanceContext,
    relative_path: RelativePath,
) -> Result<NodeLink, i32> {
    for layer in instance_context.layers.iter() {
        if let Ok(node_link) = layer
            .state
            .find_node_link(layer.module.clone(), relative_path.as_str())
            .await
        {
            return Ok(node_link);
        }
    }

    Err(ERROR_FILE_NOT_FOUND)
}

unsafe extern "system" fn get_file_data(
    cbdata: *const ProjectedFileSystem::PRJ_CALLBACK_DATA,
    byte_offset: u64,
    length: u32,
) -> i32 {
    let instance_context = instance_context(&cbdata);

    // Safety: Guaranteed by ProjectedFS API to be valid
    let path: &[u16] =
        unsafe { slice::from_raw_parts((*cbdata).FilePathName, wcslen((*cbdata).FilePathName)) };
    let path = String::from_utf16_lossy(path);
    let relative_path = RelativePath::new_from_user_path(
        instance_context.layers[0]
            .module
            .path
            .as_deref()
            .expect("virtual layer module path"),
        path.as_str(),
    )
    .unwrap_or_default();

    match runtime().block_on(LORE_CONTEXT.scope(
        instance_context.execution.clone(),
        get_file_data_async(
            instance_context,
            relative_path,
            unsafe { (*cbdata).DataStreamId },
            byte_offset as usize,
            length as usize,
        ),
    )) {
        Ok(_) => 0,
        Err(err) => err,
    }
}

async fn get_file_data_async(
    instance_context: &InstanceContext,
    path: RelativePath,
    data_stream_id: windows_sys::core::GUID,
    byte_offset: usize,
    length: usize,
) -> Result<(), i32> {
    lore_debug!("Get file data: {path}");

    for layer in instance_context.layers.iter() {
        let Ok(node_link) = layer
            .state
            .find_node_link(layer.module.clone(), path.as_str())
            .await
        else {
            continue;
        };

        if !node_link.is_valid() {
            continue;
        }

        let Ok((repository, state)) = node_link
            .resolve(layer.module.clone(), layer.state.clone())
            .await
        else {
            continue;
        };

        let Ok(node) = state.node(repository.clone(), node_link.node).await else {
            continue;
        };

        if let Some(log) = instance_context.file_log.as_ref() {
            let mut log = log.lock();
            let _ = writeln!(log, "{}", path.as_str());
        }

        const SINGLE_READ_THRESHOLD: usize = 128 * 1024 * 1024;
        let capacity = std::cmp::min(length, SINGLE_READ_THRESHOLD);
        let mut write_offset = 0;
        let mut offset = byte_offset;
        let mut remain = length;

        // Safety: Call Win32 API - buffer is freed before returning
        let write_buffer = unsafe {
            ProjectedFileSystem::PrjAllocateAlignedBuffer(instance_context.instance, capacity)
        };
        if write_buffer.is_null() {
            lore_error!(
                "Failed to allocate aligned buffer: {}",
                Win32Error::get_last_error()
            );
            return Err(ERROR_OUTOFMEMORY);
        }

        while remain > 0 {
            let to_read = std::cmp::min(remain, capacity);

            if let Err(err) = immutable::read_into(
                repository.clone(),
                node.address,
                if offset == 0 && to_read == node.size as usize {
                    None
                } else {
                    Some(offset..(offset + to_read))
                },
                // Safety: Ok as buffer is verified non-null and range is clamped above
                unsafe { slice::from_raw_parts_mut(write_buffer.cast::<u8>(), to_read) },
                lore_storage::options::ReadOptions::default()
                    .with_decompress()
                    .with_remote()
                    .with_verify(),
            )
            .await
            {
                lore_error!("Failed to read from immutable data: {err}");
                break;
            }

            // Safety: Win32 API call above guarantees buffer validity and boundaries
            let res = unsafe {
                ProjectedFileSystem::PrjWriteFileData(
                    instance_context.instance,
                    &data_stream_id,
                    write_buffer,
                    write_offset as u64,
                    to_read as u32,
                )
            };
            if res != 0 {
                lore_error!(
                    "Failed to write file data to ProjFS: {}",
                    Win32Error::from(res)
                );
                break;
            }

            remain -= to_read;
            offset += to_read;
            write_offset += to_read;
        }

        unsafe { ProjectedFileSystem::PrjFreeAlignedBuffer(write_buffer) };

        if remain > 0 {
            return Err(ERROR_READ_FAULT);
        } else {
            return Ok(());
        }
    }

    Err(ERROR_FILE_NOT_FOUND)
}

fn format_bytes_to_string(bytes: usize) -> String {
    let mut unit = "bytes";

    let converted = if bytes > 1024 * 1024 * 1024 {
        unit = "GiB";
        (bytes / (1024 * 1024)) as f64 / 1024.0
    } else if bytes > 1024 * 1024 {
        unit = "MiB";
        (bytes / 1024) as f64 / 1024.0
    } else if bytes > 1024 {
        unit = "KiB";
        bytes as f64 / 1024.0
    } else {
        bytes as f64
    };

    format!("{converted:.2} {unit}")
}

async fn prefetch_files(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    prefetch: Vec<String>,
) {
    const MAX_CONCURRENT_PREFETCH: usize = 10000;
    let mut tasks = tokio::task::JoinSet::new();

    let mut last_file_count = 0;
    let mut last_print = Instant::now();
    let file_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let file_size = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    for path in prefetch {
        lore_spawn!(tasks, {
            let file_count = file_count.clone();
            let file_size = file_size.clone();
            let repository = repository.clone();
            let state = state.clone();
            async move {
                if let Ok(node_link) = state
                    .find_node_link(repository.clone(), path.as_str())
                    .await
                {
                    if node_link.is_valid() {
                        if let Ok((repository, state)) = node_link.resolve(repository, state).await
                        {
                            if let Ok(node) = state.node(repository.clone(), node_link.node).await {
                                if !node.address.hash.is_zero() {
                                    let _ = immutable::cache(
                                        repository.clone(),
                                        vec![node.address],
                                        true,
                                    )
                                    .await;

                                    if let Ok(result) = repository
                                        .immutable_store()
                                        .query(repository.id, node.address, StoreMatch::MatchHash)
                                        .await
                                    {
                                        /*
                                        file_count
                                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                        */
                                        file_size.fetch_add(
                                            result.fragment.size_content as usize,
                                            std::sync::atomic::Ordering::Relaxed,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }

                if let Ok(mut file) = tokio::fs::OpenOptions::new()
                    .read(true)
                    .write(false)
                    .truncate(false)
                    .create(false)
                    .open(path)
                    .await
                {
                    let mut buffer = [0u8; 32];
                    let _ = file.read(&mut buffer).await;
                    file_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    /*
                    if let Ok(metadata) = file.metadata().await {
                        file_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        file_size.fetch_add(
                            metadata.len() as usize,
                            std::sync::atomic::Ordering::Relaxed,
                        );
                    }
                    */
                }
            }
        });

        if last_print.elapsed().as_secs_f32() > 1.0 {
            let current_file_count = file_count.load(std::sync::atomic::Ordering::Relaxed);
            if current_file_count != last_file_count {
                println!(
                    "Prefetched {} files, {}",
                    current_file_count,
                    format_bytes_to_string(file_size.load(std::sync::atomic::Ordering::Relaxed))
                );
                last_file_count = current_file_count;
                last_print = Instant::now();
            }
        }

        if tasks.len() >= MAX_CONCURRENT_PREFETCH {
            let _ = tasks.join_next().await;
        }

        while tasks.try_join_next().is_some() {}
    }

    while !tasks.is_empty() {
        let _ = tasks.join_next().await;

        if last_print.elapsed().as_secs_f32() > 1.0 {
            let current_file_count = file_count.load(std::sync::atomic::Ordering::Relaxed);
            if current_file_count != last_file_count {
                println!(
                    "Prefetched {} files, {}",
                    current_file_count,
                    format_bytes_to_string(file_size.load(std::sync::atomic::Ordering::Relaxed))
                );
                last_file_count = current_file_count;
                last_print = Instant::now();
            }
        }
    }

    println!(
        "Prefetched done: {} files, {}",
        file_count.load(std::sync::atomic::Ordering::Relaxed),
        format_bytes_to_string(file_size.load(std::sync::atomic::Ordering::Relaxed))
    );
}
