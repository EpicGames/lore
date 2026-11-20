#[cfg(feature = "test-util")]
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use crate::errors::FileNotFound;
use crate::fs::filesystem_provider::FsError;
use crate::fs::swfs::frozen_files::FrozenFile;
use crate::fs::swfs::frozen_files::FrozenSnapshot;
use crate::fs::swfs::modified_tree::Coverage;
use crate::fs::swfs::modified_tree::ModifiedTree;
use crate::fs::swfs::renamed_at_risk::RenamedAtRisk;
use crate::fs::swfs::served_tree::ServedTree;
use crate::state::StateError;
use crate::util::path::RelativePath;

mod content;
mod listing;

pub use content::ContentAnswer;
pub use content::HoldsAnswer;
pub use content::compare_content;
pub use listing::ListingAnswer;

/// The error for a read of `path`, which holds nothing.
pub fn file_not_found(path: &RelativePath) -> FsError {
    FileNotFound {
        resource: path.as_str().to_string(),
    }
    .into()
}

/// Why a read goes to the mount rather than to what answers for a path without reading it. Only a
/// test makes a read fall back, so a build without `test-util` has no fallback at all: every read
/// is answered from the revision, the frozen snapshot, or as deleted.
#[cfg(feature = "test-util")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    /// A test forced every read to fall back.
    Forced,
}

#[cfg(feature = "test-util")]
impl fmt::Display for FallbackReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// Where a read of one path is answered from.
#[derive(Debug)]
pub enum ReadSource<'a> {
    /// Nothing changed the path: the revision's tree and the immutable store hold its answer.
    Unmodified,
    /// The path was deleted, or lies below a directory that was: it is not there.
    Deleted,
    /// The path changed since the last thaw, and SWFS froze it with the mount.
    Covered(&'a FrozenFile),
    /// A test forced the read to the mount.
    #[cfg(feature = "test-util")]
    Fallback(FallbackReason),
}

/// What an SWFS operation reads, and from where: for each path, the revision, SWFS's frozen
/// snapshot, or the mount, chosen by what the modified-file record says of it.
pub struct SwfsView {
    record: Arc<ModifiedTree>,
    snapshot: FrozenSnapshot,
    /// What the mount served as the operation began, which unmodified paths are answered from:
    /// the revision's tree, and below a link or layer the repository mounted there (ADR-00036).
    served: Arc<ServedTree>,
    #[cfg(feature = "test-util")]
    fallbacks: AtomicU64,
    answered: AtomicU64,
    /// Whether the record was trusted as the operation began, when it is rebuilt where it is not.
    trusted: bool,
    /// The moves the mount kept, which tell where a renamed file's bytes come from where SWFS
    /// lists no origin for it.
    kept_moves: Option<Arc<RenamedAtRisk>>,
    #[cfg(feature = "test-util")]
    forced: bool,
}

impl SwfsView {
    /// A view trusting `record` as far as it is trusted now.
    pub fn new(
        record: Arc<ModifiedTree>,
        snapshot: FrozenSnapshot,
        served: Arc<ServedTree>,
    ) -> Self {
        let trusted = record.is_trusted();
        Self::with_trust(record, snapshot, served, trusted)
    }

    /// A view that answers from `record` where `trusted`, as one rebuilt as the operation began is:
    /// a callback that distrusts it since tells of a change after the freeze.
    pub fn with_trust(
        record: Arc<ModifiedTree>,
        snapshot: FrozenSnapshot,
        served: Arc<ServedTree>,
        trusted: bool,
    ) -> Self {
        Self {
            trusted,
            record,
            snapshot,
            served,
            #[cfg(feature = "test-util")]
            fallbacks: AtomicU64::new(0),
            answered: AtomicU64::new(0),
            kept_moves: None,
            #[cfg(feature = "test-util")]
            forced: false,
        }
    }

    /// The view, reading where a renamed file came from in `kept_moves` where SWFS lists no
    /// origin for it.
    pub fn with_kept_moves(mut self, kept_moves: Arc<RenamedAtRisk>) -> Self {
        self.kept_moves = Some(kept_moves);
        self
    }

    /// Makes every read fall back, which is how a test holds the operation to what the mount
    /// answers.
    #[cfg(feature = "test-util")]
    pub fn force_fallback(&mut self, forced: bool) {
        self.forced = forced;
    }

    /// How many reads were answered without the mount: from the revision, from the frozen
    /// snapshot, or by the path being deleted. Counted when a path is classified, so a read that
    /// then falls back for want of content counts as both.
    pub fn answered(&self) -> u64 {
        self.answered.load(Ordering::Relaxed)
    }

    /// How many files SWFS held changed when the operation began, as it froze them.
    pub fn frozen_file_count(&self) -> usize {
        self.snapshot.file_count()
    }

    /// Whether SWFS listed what it froze: a snapshot of nothing for want of a listing cannot say
    /// that a path it lacks holds no change.
    pub fn snapshot_captured(&self) -> bool {
        self.snapshot.is_captured()
    }

    /// Whether SWFS froze something directly in the directory at `path`.
    pub fn froze_in(&self, path: &RelativePath) -> bool {
        self.snapshot.children_of(path).next().is_some()
    }

    /// The files SWFS froze at and below `path`.
    pub fn frozen_files_below(&self, path: &RelativePath) -> Vec<RelativePath> {
        self.snapshot.files_below(path)
    }

    /// Whether SWFS froze a file at `path` whose bytes are its own or the revision's at `path`: not
    /// one the modified list names as renamed, itself or a directory above it, nor one a kept move
    /// leads to. A thaw naming a moved file also names its origin.
    pub fn froze_unmoved_file_at(&self, path: &RelativePath) -> bool {
        self.froze_file_at(path)
            && !self.snapshot.renamed_at_or_above(path)
            && self
                .kept_moves
                .as_ref()
                .is_none_or(|kept| kept.origin_of(path.as_str()).is_none())
    }

    /// Whether SWFS froze a file, rather than a directory, at `path`: a file the user holds.
    pub fn froze_file_at(&self, path: &RelativePath) -> bool {
        self.snapshot
            .get(path)
            .is_some_and(|file| !file.is_directory)
    }

    /// How many reads have fallen back to the mount.
    #[cfg(feature = "test-util")]
    pub fn fallbacks(&self) -> u64 {
        self.fallbacks.load(Ordering::Relaxed)
    }

    /// Where a read of `path` is answered from.
    pub async fn classify(&self, path: &RelativePath) -> Result<ReadSource<'_>, StateError> {
        #[cfg(feature = "test-util")]
        if self.forced {
            return Ok(ReadSource::Fallback(
                self.fall_back(path, FallbackReason::Forced),
            ));
        }
        // A record that may have missed a change cannot say a path has none. The operation
        // rebuilt it as it began, so one untrusted then is a failure to report: a callback that
        // distrusts it since tells of a change after the freeze, which the snapshot does not hold.
        if !self.trusted {
            return Err(StateError::internal(format!(
                "the modified-file record is untrusted, so it cannot answer for {path}"
            )));
        }
        // A path the ignore rules leave out of the record is one the revision does not track, so
        // it holds nothing there, and SWFS freezes every file it holds changed, ignored or not:
        // the path is answered as an unchanged one, from what SWFS froze there or as nothing.
        let source = match self.record.coverage(path.as_str()).await? {
            // SWFS froze a file the record holds nothing of where the record leaves it out, as it does
            // a file the ignore rules exclude: the frozen file is what the working tree holds.
            Coverage::Unchanged => match self.frozen_file(path) {
                Some(file) if !file.is_directory => ReadSource::Covered(file),
                _ => ReadSource::Unmodified,
            },
            Coverage::Deleted => ReadSource::Deleted,
            Coverage::Changed => match self.frozen_file(path) {
                Some(file) => ReadSource::Covered(file),
                None => ReadSource::Unmodified,
            },
        };
        self.answered.fetch_add(1, Ordering::Relaxed);
        Ok(source)
    }

    /// The frozen file of a path the record holds changed, or `None` for one SWFS holds no
    /// change to.
    ///
    /// SWFS freezes every file it holds changed, however many thaws ago the change was made, so
    /// a path with none is one a thaw has since replaced with what the revision holds, such as a
    /// file a reset restored. The record keeps a path until the next mount, so it can name one.
    fn frozen_file(&self, path: &RelativePath) -> Option<&FrozenFile> {
        self.snapshot.get(path)
    }

    /// Counts and logs a read going to the mount, and answers with why.
    #[cfg(feature = "test-util")]
    fn fall_back(&self, path: &RelativePath, reason: FallbackReason) -> FallbackReason {
        self.fallbacks.fetch_add(1, Ordering::Relaxed);
        crate::lore_debug!(
            "SWFS read of {} falls back to the mount: {reason}",
            path.as_str()
        );
        reason
    }
}
