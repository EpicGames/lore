use std::ffi::CString;

use crate::fs::filesystem_provider::FileInfo;
use crate::fs::swfs::api_interface::SwfsInterfaceError;
use crate::fs::swfs::api_interface::swfs_api::SWFSFInfo;
use crate::fs::swfs::api_interface::swfs_api::SWFSFile;
use crate::fs::swfs::api_interface::swfs_api::swfs_u32;
use crate::fs::swfs::api_interface::swfs_api::swfs_u64;

pub struct SwfsFileArray {
    files: Box<[SwfsFile]>,
}

impl SwfsFileArray {
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    pub fn new(files: Vec<SwfsFile>) -> Self {
        let mut this = SwfsFileArray {
            files: files.into_boxed_slice(),
        };
        for i in 1..this.files.len() {
            let next_ptr = std::ptr::from_mut(&mut this.files[i].file);
            this.files[i - 1].file.next = next_ptr;
        }
        this
    }

    pub fn array_ptr(&self) -> *const SWFSFile {
        if self.files.is_empty() {
            std::ptr::null()
        } else {
            self.files.as_ptr().cast()
        }
    }
}

/// Binary equivalent wrapper of an `SWFSFile` that owns the memory for the file name.
#[repr(C)]
pub struct SwfsFile {
    file: SWFSFile,
}

unsafe impl Send for SwfsFile {}

impl Drop for SwfsFile {
    fn drop(&mut self) {
        unsafe {
            drop(CString::from_raw(self.file.path));
        }
    }
}

impl SwfsFile {
    pub fn new(
        name: CString,
        file_info: &FileInfo,
        flags: Option<swfs_u32>,
    ) -> Result<SwfsFile, SwfsInterfaceError> {
        let name = name.into_raw();
        Ok(Self {
            file: SWFSFile {
                path: name,
                finfo: SWFSFInfo {
                    time_create: file_info.mtime(),
                    time_write: file_info.mtime(),
                    size: file_info.size(),
                    attrs: if file_info.is_dir() {
                        // FILE_ATTRIBUTE_DIRECTORY
                        0x00000010
                    } else {
                        0
                    },
                },
                file_index: 0,
                mod_flags: flags.unwrap_or(0),
                next: std::ptr::null_mut(),
            },
        })
    }

    pub fn file(&self) -> &SWFSFile {
        &self.file
    }

    /// Stamps the create and write times SWFS reports for this entry.
    ///
    /// For callers whose [`FileInfo`] carries no time of its own:
    /// [`FileInfo::Directory`] holds none, so a directory entry built from one
    /// reports zero for both, and a caller that knows the time has nowhere to
    /// put it. Takes the value already in SWFS's own units rather than a
    /// `FileInfo`, because a caller reading the clock gets the platform's file
    /// time, not the Unix milliseconds a `FileInfo` holds.
    pub fn set_times(&mut self, time: swfs_u64) {
        self.file.finfo.time_create = time;
        self.file.finfo.time_write = time;
    }
}
