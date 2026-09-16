use std::ffi::CString;
use std::path::Path;
use std::path::PathBuf;

use lore_base::error::InvalidPath;
use lore_error_set::ForwardStrict;
use lore_error_set::WrapInternal;
use lore_error_set::error_set;

use crate::fs::swfs::mount_resources::SwfsWorkError;
use crate::util::path::RelativePath;

/// A path provided by SWFS. Always starts with a leading `\` character that represents the root
/// mount directory
pub struct SwfsPath<'a>(pub &'a str);

impl SwfsPath<'_> {
    pub fn relative_path_string(&self) -> Result<&str, SwfsWorkError> {
        // SWFS may provide paths with or without leading backslash
        Ok(self.0.strip_prefix("\\").unwrap_or(self.0))
    }

    pub fn node_path(&self) -> Result<NodePath, SwfsWorkError> {
        Ok(NodePath(
            RelativePath::new_from_initial_path(self.relative_path_string()?)
                .forward::<SwfsWorkError>("Parsing relative path from SWFS")?,
        ))
    }
}

/// A Node's path, relative to the repository root with no prefix
pub struct NodePath(pub RelativePath);

impl NodePath {
    pub fn join(&self, child: &str) -> NodePath {
        if self.0.is_empty() {
            NodePath(RelativePath::new().join(child))
        } else {
            NodePath(self.0.join(child))
        }
    }
}

#[error_set]
pub enum MountPathError {
    InvalidPath,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct MountPath(PathBuf);

impl MountPath {
    pub fn new(path: &Path) -> Result<Self, MountPathError> {
        let file_name = path.file_name().ok_or_else(|| InvalidPath {
            path: format!("{}", path.display()),
        })?;
        let combined = if let Some(parent) = path.parent() {
            parent
                .canonicalize()
                .map_err(|_err| InvalidPath {
                    path: format!("{}", path.display()),
                })?
                .join(file_name)
        } else {
            PathBuf::from(file_name)
        };
        Ok(Self(combined))
    }

    pub fn nul_terminated_swfs_form(&self) -> Result<CString, MountPathError> {
        let prefix = "\\\\?\\";
        let path = if self.0.starts_with(prefix) {
            self.0
                .strip_prefix(prefix)
                .internal("Unable to strip prefix off mount path")?
        } else {
            &self.0
        };
        Ok(CString::new(path.as_os_str().as_encoded_bytes())
            .internal("Turning mount path into a CString")?)
    }
}

impl AsRef<Path> for MountPath {
    fn as_ref(&self) -> &Path {
        self.0.as_ref()
    }
}
