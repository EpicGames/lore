use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_error_set::ForwardAny;
use lore_error_set::WrapInternal;
use lore_storage::Address;
use lore_storage::ContentHashes;
use lore_storage::ContentSource;

#[cfg(feature = "test-util")]
use super::FallbackReason;
use super::ReadSource;
use super::SwfsView;
use super::file_not_found;
use crate::fs::filesystem_provider::FileInfo;
use crate::fs::filesystem_provider::FsError;
use crate::fs::swfs::frozen_files::FrozenFile;
use crate::fs::swfs::served_tree::Served;
use crate::repository::RepositoryContext;
use crate::state::NodeComparison;
use crate::state::node_comparison;
use crate::util::path::RelativePath;

/// Where the content at a path is read from.
pub enum ContentAnswer {
    Source(ContentSource<'static>),
    /// A test forced the read to the mount.
    #[cfg(feature = "test-util")]
    Fallback(FallbackReason),
}

/// Whether a file holds the content an address names.
pub enum HoldsAnswer {
    Compared(NodeComparison),
    /// A test forced the read to the mount.
    #[cfg(feature = "test-util")]
    Fallback(FallbackReason),
}

impl SwfsView {
    /// Where the content at `path` is read from: the store for a file nothing changed, SWFS's
    /// snapshot for one that changed since the last thaw, and the mount where neither can say.
    /// A path that was deleted has none.
    pub async fn content_source(&self, path: &RelativePath) -> Result<ContentAnswer, FsError> {
        match self
            .classify(path)
            .await
            .internal("Unable to ask the modified-file record")?
        {
            #[cfg(feature = "test-util")]
            ReadSource::Fallback(reason) => Ok(ContentAnswer::Fallback(reason)),
            ReadSource::Deleted => Err(file_not_found(path)),
            ReadSource::Covered(file) => self.frozen_source(path, file).await,
            ReadSource::Unmodified => self.stored_source(path).await,
        }
    }

    /// Whether the file at `path` holds the content `previous` addresses, `previous_size` bytes of
    /// it, in `repository`'s store (a layer or link has one of its own). An unmodified file holds
    /// what its own node addresses, which settles most asks without reading anything.
    pub async fn file_holds_content(
        &self,
        repository: Arc<RepositoryContext>,
        path: &RelativePath,
        previous: Address,
        previous_size: u64,
        established: &ContentHashes,
    ) -> Result<HoldsAnswer, FsError> {
        match self
            .classify(path)
            .await
            .internal("Unable to ask the modified-file record")?
        {
            #[cfg(feature = "test-util")]
            ReadSource::Fallback(reason) => Ok(HoldsAnswer::Fallback(reason)),
            ReadSource::Deleted => Ok(HoldsAnswer::Compared(NodeComparison::Unreadable)),
            ReadSource::Covered(file) => {
                self.frozen_holds(repository, path, file, previous, previous_size, established)
                    .await
            }
            ReadSource::Unmodified => {
                let served = self.node(path).await?;
                let Some(Served::Node {
                    repository: served_in,
                    node,
                    ..
                }) = served
                else {
                    // A directory leading to a link or layer holds no content.
                    if served.is_some() {
                        return Ok(HoldsAnswer::Compared(NodeComparison::Unreadable));
                    }
                    // The revision holds nothing here, so the file is one SWFS froze that the
                    // record does not hold, such as an ignored one, or it is not there.
                    return match self.snapshot.get(path) {
                        Some(file) => {
                            self.frozen_holds(
                                repository,
                                path,
                                file,
                                previous,
                                previous_size,
                                established,
                            )
                            .await
                        }
                        None => Ok(HoldsAnswer::Compared(NodeComparison::Unreadable)),
                    };
                };
                if node.address.hash == previous.hash && node.size == previous_size {
                    return Ok(HoldsAnswer::Compared(NodeComparison::Matches));
                }
                let source = crate::immutable::stored_content(&served_in, &node);
                self.compare(repository, previous, previous_size, &source, established)
                    .await
            }
        }
    }

    async fn frozen_holds(
        &self,
        repository: Arc<RepositoryContext>,
        path: &RelativePath,
        file: &FrozenFile,
        previous: Address,
        previous_size: u64,
        established: &ContentHashes,
    ) -> Result<HoldsAnswer, FsError> {
        match self.frozen_source(path, file).await? {
            ContentAnswer::Source(source) => {
                self.compare(repository, previous, previous_size, &source, established)
                    .await
            }
            #[cfg(feature = "test-util")]
            ContentAnswer::Fallback(reason) => Ok(HoldsAnswer::Fallback(reason)),
        }
    }

    async fn compare(
        &self,
        repository: Arc<RepositoryContext>,
        previous: Address,
        previous_size: u64,
        source: &ContentSource<'_>,
        established: &ContentHashes,
    ) -> Result<HoldsAnswer, FsError> {
        Ok(HoldsAnswer::Compared(
            compare_content(repository, previous, previous_size, source, established).await?,
        ))
    }

    /// Whether the operation's revision holds a node at `path`.
    pub async fn revision_holds(&self, path: &RelativePath) -> Result<bool, FsError> {
        Ok(self
            .served
            .resolve(path)
            .await
            .forward_any::<FsError>("Unable to find file node")?
            .is_some())
    }

    /// The origin of each rename of a file SWFS lists whose destination is in `paths`. A
    /// directory's origin is left out.
    pub fn origins_of_renamed_files(&self, paths: &[RelativePath]) -> Vec<RelativePath> {
        paths
            .iter()
            .filter(|path| {
                self.snapshot
                    .get(path)
                    .is_some_and(|file| !file.is_directory)
            })
            .filter_map(|path| self.snapshot.origin_of(path))
            .filter_map(|origin| RelativePath::new_from_initial_path(origin).ok())
            .collect()
    }

    /// Where `path` was moved from, where SWFS lists a directory above it as moved.
    pub fn moved_from(&self, path: &RelativePath) -> Option<RelativePath> {
        self.snapshot.moved_from(path)
    }

    /// What SWFS froze at `path`, which can be a file the record holds nothing of, such as an
    /// ignored one.
    pub fn frozen_info(&self, path: &RelativePath) -> Option<FileInfo> {
        self.snapshot.get(path).map(FrozenFile::file_info)
    }

    /// What the operation's served tree holds at `path`, or `None` where it serves nothing.
    async fn node(&self, path: &RelativePath) -> Result<Option<Served>, FsError> {
        let served = self
            .served
            .resolve(path)
            .await
            .forward_any::<FsError>("Unable to find file node")?;
        if served.is_none()
            && let Some(origin) = self.snapshot.moved_from(path)
        {
            return self
                .served
                .resolve(&origin)
                .await
                .forward_any::<FsError>("Unable to find file node");
        }
        Ok(served)
    }

    async fn stored_source(&self, path: &RelativePath) -> Result<ContentAnswer, FsError> {
        match self.node(path).await? {
            // Read from the store of the repository that serves the path, which below a link or
            // layer is the one mounted there.
            Some(Served::Node {
                repository, node, ..
            }) if !node.is_directory() => Ok(ContentAnswer::Source(
                crate::immutable::stored_content(&repository, &node),
            )),
            Some(_) => Err(directory_has_no_content(path)),
            // The revision holds nothing here, so the file is one SWFS froze that the record does
            // not hold, such as an ignored one, or it is not there.
            None => match self.snapshot.get(path) {
                Some(file) => self.frozen_source(path, file).await,
                None => Err(file_not_found(path)),
            },
        }
    }

    /// Whether SWFS holds bytes of its own for the file it froze at `path`, which a read of its
    /// first byte tells.
    pub fn froze_bytes_at(&self, path: &RelativePath) -> Result<bool, FsError> {
        Ok(self
            .snapshot
            .content(path)
            .internal("Unable to read a frozen file")?
            .is_some())
    }

    /// Where the bytes of a file SWFS froze at `path` with none of its own come from: the path
    /// SWFS lists the move as leaving or, where it lists none, as while another node holds that
    /// path, the one a move the mount kept names.
    pub fn frozen_origin(&self, path: &RelativePath) -> Option<RelativePath> {
        match self.snapshot.origin_of(path) {
            Some(origin) => RelativePath::new_from_initial_path(origin).ok(),
            None => self
                .kept_moves
                .as_ref()
                .and_then(|kept| kept.origin_of(path.as_str()))
                .and_then(|origin| RelativePath::new_from_initial_path(origin).ok()),
        }
    }

    /// The content of a changed file, from the snapshot, which SWFS is read for a range at a time
    /// as the content is read.
    async fn frozen_source(
        &self,
        path: &RelativePath,
        file: &FrozenFile,
    ) -> Result<ContentAnswer, FsError> {
        if file.is_directory {
            return Err(directory_has_no_content(path));
        }
        if let Some(source) = self
            .snapshot
            .content(path)
            .internal("Unable to read a frozen file")?
        {
            return Ok(ContentAnswer::Source(source));
        }
        // SWFS freezes a file changed only in name or attributes without bytes of its own: they
        // are the source's, for a renamed file at the path SWFS lists the move as leaving, and
        // otherwise at the path, as for a file renamed to another spelling of it. SWFS lists no
        // origin while another node holds its path, and the moves the mount kept tell it then.
        for source in [self.frozen_origin(path), Some(path.clone())]
            .into_iter()
            .flatten()
        {
            if let Some(Served::Node {
                repository, node, ..
            }) = self.node(&source).await?
                && !node.is_directory()
            {
                return Ok(ContentAnswer::Source(crate::immutable::stored_content(
                    &repository,
                    &node,
                )));
            }
        }
        // A read through the mount would wait on a callback that reads the store, on the pool the
        // read itself holds a thread of (ADR-00035).
        Err(FsError::internal(format!(
            "SWFS holds no bytes of its own for {}, and nothing tells where they come from",
            path.as_str()
        )))
    }
}

fn directory_has_no_content(path: &RelativePath) -> FsError {
    InvalidArguments {
        reason: format!("{} is a directory, which holds no content", path.as_str()),
    }
    .into()
}

/// Whether `source` holds the content `previous` addresses, `previous_size` bytes of it.
pub async fn compare_content(
    repository: Arc<RepositoryContext>,
    previous: Address,
    previous_size: u64,
    source: &ContentSource<'_>,
    established: &ContentHashes,
) -> Result<NodeComparison, FsError> {
    let matched = crate::immutable::file_matches(
        repository,
        previous,
        Some(previous_size as usize),
        source,
        established,
    )
    .await
    .forward_any::<FsError>("Failed to compare content to stored content")?;
    Ok(node_comparison(matched))
}
