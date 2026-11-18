use std::collections::HashMap;
use std::collections::hash_map::Entry;

use lore_error_set::ForwardAny;
use lore_error_set::WrapInternal;

#[cfg(feature = "test-util")]
use super::FallbackReason;
use super::ReadSource;
use super::SwfsView;
use super::file_not_found;
use crate::fs::filesystem_provider::DirectoryEntry;
use crate::fs::filesystem_provider::DirectoryListing;
use crate::fs::filesystem_provider::FileInfo;
use crate::fs::filesystem_provider::FsError;
use crate::fs::swfs::modified_tree::Coverage;
use crate::util::path::RelativePath;

/// Where a directory's children are listed from.
pub enum ListingAnswer {
    Listing(Box<DirectoryListing>),
    /// A test forced the read to the mount.
    #[cfg(feature = "test-util")]
    Fallback(FallbackReason),
}

impl SwfsView {
    /// Whether the directory holding `path` holds a child named exactly as `path` spells it.
    /// A name in the directory under another case is not held in this spelling, and nothing is
    /// held under a directory that was deleted. `None` where the mount has to say.
    pub async fn holds_name_exactly(&self, path: &RelativePath) -> Result<Option<bool>, FsError> {
        let name = path.name();
        let answer = match self.read_directory(&path.parent_path()).await {
            Ok(answer) => answer,
            Err(FsError::FileNotFound(_)) => return Ok(Some(false)),
            Err(error) => return Err(error),
        };
        let mut listing = match answer {
            ListingAnswer::Listing(listing) => listing,
            // Counted and logged where the listing fell back.
            #[cfg(feature = "test-util")]
            ListingAnswer::Fallback(_) => return Ok(None),
        };
        while let Some(entry) = listing.next().await {
            if entry?.name == name {
                return Ok(Some(true));
            }
        }
        Ok(Some(false))
    }

    /// The children of the directory at `path`: what the revision holds there, changed by what the
    /// record holds directly in it. A child the record says changed is described as the frozen
    /// snapshot has it, under the spelling it has on disk, and one it says was deleted is not
    /// there.
    ///
    /// A link or layer is listed as the directory it mounts, as a plain working tree holds it. A
    /// directory that was deleted has no listing, and one neither the served tree nor the record
    /// holds, or a changed child the snapshot cannot describe, is left to the mount.
    pub async fn read_directory(&self, path: &RelativePath) -> Result<ListingAnswer, FsError> {
        let covered = match self
            .classify(path)
            .await
            .internal("Unable to ask the modified-file record")?
        {
            #[cfg(feature = "test-util")]
            ReadSource::Fallback(reason) => return Ok(ListingAnswer::Fallback(reason)),
            ReadSource::Deleted => return Err(file_not_found(path)),
            ReadSource::Covered(_) => true,
            ReadSource::Unmodified => false,
        };

        // A directory the record holds changed, because it was added, has nothing of the
        // revision's under it. Any other is the served tree's, or one SWFS froze files in that the
        // record does not hold, such as an ignored one, or it is not there.
        let base = self
            .served
            .children(path)
            .await
            .forward_any::<FsError>("Unable to list a served directory")?;
        if base.is_none()
            && !covered
            && !self
                .snapshot
                .get(path)
                .is_some_and(|file| file.is_directory)
            && self.snapshot.children_of(path).next().is_none()
        {
            return Err(file_not_found(path));
        }

        let mut entries: Vec<DirectoryEntry> = Vec::new();
        let mut at: HashMap<String, usize> = HashMap::new();
        // The names the record described, which the snapshot does not describe over.
        let mut recorded_names: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for child in base.unwrap_or_default() {
            let info = child.served.file_info(&path.join(&child.name)).await;
            at.insert(child.name.to_lowercase(), entries.len());
            entries.push(DirectoryEntry::new(child.name, info));
        }

        for recorded in self
            .record
            .children(path.as_str())
            .await
            .forward_any::<FsError>("Unable to ask the modified-file record")?
        {
            let key = recorded.name.to_lowercase();
            recorded_names.insert(key.clone());
            match recorded.coverage {
                Coverage::Deleted => {
                    if let Some(index) = at.remove(&key) {
                        entries[index].info = FileInfo::NotExist;
                    }
                }
                Coverage::Changed => {
                    // A change a thaw has since replaced: the revision's entry describes it.
                    let Some(frozen) = self.frozen_file(&path.join(&recorded.name)) else {
                        continue;
                    };
                    let described =
                        DirectoryEntry::new(frozen.name().to_owned(), frozen.file_info());
                    if let Some(index) = at.get(&key) {
                        entries[*index] = described;
                    } else {
                        at.insert(key, entries.len());
                        entries.push(described);
                    }
                }
                // Held as the way to what changed below it: the revision lists it, or something
                // new lives under it.
                Coverage::Unchanged => {
                    if let Entry::Vacant(vacant) = at.entry(key) {
                        vacant.insert(entries.len());
                        entries.push(DirectoryEntry::new(recorded.name, FileInfo::Directory));
                    }
                }
            }
        }

        // What SWFS froze that the record holds nothing of, such as an ignored file: listed where the
        // revision does not hold it, and described as frozen where it does, the working tree's edit
        // being the file there.
        for frozen in self.snapshot.children_of(path) {
            let key = frozen.name().to_lowercase();
            match at.entry(key.clone()) {
                Entry::Vacant(vacant) => {
                    vacant.insert(entries.len());
                    entries.push(DirectoryEntry::new(
                        frozen.name().to_owned(),
                        frozen.file_info(),
                    ));
                }
                Entry::Occupied(occupied) => {
                    if !frozen.is_directory && !recorded_names.contains(&key) {
                        entries[*occupied.get()] =
                            DirectoryEntry::new(frozen.name().to_owned(), frozen.file_info());
                    }
                }
            }
        }

        entries.retain(|entry| entry.info != FileInfo::NotExist);
        Ok(ListingAnswer::Listing(Box::new(
            DirectoryListing::from_entries(entries),
        )))
    }
}
