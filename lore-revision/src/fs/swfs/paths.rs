use std::ffi::CString;
use std::hash::Hash;
use std::hash::Hasher;
use std::path::Path;
use std::path::PathBuf;

use lore_base::error::InvalidPath;
use lore_error_set::ForwardStrict;
use lore_error_set::WrapInternal;
use lore_error_set::error_set;

use crate::fs::swfs::mount_resources::SwfsWorkError;
use crate::util::path::RelativePath;

#[derive(Clone)]
pub struct RelativePathKey(pub RelativePath);

impl Hash for RelativePathKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.as_str().hash(state);
    }
}

impl PartialEq<Self> for RelativePathKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_str() == other.0.as_str()
    }
}

impl Eq for RelativePathKey {}

/// What `path` holds below `ancestor`, compared in any spelling: empty where the two are the same
/// path, and `None` where `path` lies neither at nor below `ancestor`. The root is above every
/// path.
pub fn rest_below<'a>(path: &'a str, ancestor: &str) -> Option<&'a str> {
    if ancestor.is_empty() {
        return Some(path);
    }
    // Compared as bytes, which match only where `path` has a character boundary there too.
    let length = ancestor.len();
    if path.len() < length || !path.as_bytes()[..length].eq_ignore_ascii_case(ancestor.as_bytes()) {
        return None;
    }
    match &path[length..] {
        "" => Some(""),
        rest => rest.strip_prefix('/'),
    }
}

/// Whether `path` is `ancestor` or lies below it, in any spelling.
pub fn at_or_below(path: &RelativePath, ancestor: &RelativePath) -> bool {
    rest_below(path.as_str(), ancestor.as_str()).is_some()
}

/// Whether `path` lies below `ancestor`, and is not it, in any spelling.
pub fn strictly_below(path: &RelativePath, ancestor: &RelativePath) -> bool {
    rest_below(path.as_str(), ancestor.as_str()).is_some_and(|rest| !rest.is_empty())
}

/// A path provided by SWFS. Always starts with a leading `\` character that represents the root
/// mount directory
pub struct SwfsPath<'a>(pub &'a str);

impl SwfsPath<'_> {
    pub fn is_root(&self) -> bool {
        self.0 == "/" || self.0 == "\\"
    }

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

    pub fn c_string_from_relative_path(path: &RelativePath) -> Result<CString, SwfsWorkError> {
        let result = format!("/{}", path.as_str());
        Ok(CString::new(result).internal("Path contained a null byte")?)
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
