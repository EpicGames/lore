//! The renamed files SWFS can serve wrong bytes for, and the rewrite that gives them bytes of their
//! own.
//!
//! A workaround, to be removed once SWFS reports a renamed file's source path
//! (TODO(SWFS): SWFS to name a renamed file's origin in `swfsGetModifiedFileList` while another
//! node holds the origin's path; ADR-00031). SWFS reads a file the user renamed, and has not
//! written since, from the source at the path it was renamed from. It lists that origin as a
//! deletion beside the renamed file (`frozen_files::moves_in`) until something else is made at the
//! origin's path, and from then on nothing tells Lore where the renamed file's bytes come from.
//! Once the source at the origin changes, as when the user commits the new file there, SWFS serves
//! the renamed file from it: silently wrong content.
//!
//! So the mount keeps the moves SWFS reports, and queues as at risk the destination of each move
//! whose origin something arrives at ([`RenamedAtRisk`]). The callbacks only queue, as they run on
//! SWFS's one callback thread, where a read of the mount waits for a callback that thread would
//! make. The next operation, before it freezes the mount, rewrites each queued file through the
//! mount with its own bytes ([`give_renamed_files_at_risk_bytes_of_their_own`]), after which SWFS
//! holds them itself.
//!
//! Remove this module, its calls in `api_interface.rs` and `SwfsFilesystem::begin_operation`, and
//! the kept origins `SwfsView::frozen_source` reads, once SWFS names the origin itself.

use std::collections::BTreeMap;
use std::path::Path;

use crate::fs::swfs::modified_tree::Coverage;
use crate::fs::swfs::mount_io::MountIo;
use crate::fs::swfs::mount_resources::MountResources;
use crate::fs::swfs::paths::rest_below;
use crate::lore_debug;
use crate::lore_warn;
use crate::util::path::RelativePath;

/// How many moves a mount keeps. A move past it is queued at risk at once, rather than kept, so
/// the next operation gives the file bytes of its own.
pub const KEPT_MOVES: usize = 4096;

/// How much of a file [`give_renamed_files_at_risk_bytes_of_their_own`] holds at once.
const REWRITE_WINDOW: usize = 1 << 20;

/// A move SWFS reported, from the path the moved node's bytes are read at in the source.
#[derive(Debug, Clone)]
struct KeptMove {
    origin: String,
    destination: String,
    directory: bool,
    /// The lowercase destination of the kept directory move the origin was found through, for a
    /// node moved out from below a moved directory: its origin lies below that move's.
    through: Option<String>,
    /// When the move was kept, as [`RenamedAtRisk::mark`] counts.
    seq: u64,
}

/// How a node at a path came there by kept moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Carried {
    /// A kept move left it there.
    Moved,
    /// It lies below a directory a kept move left there.
    Below,
}

#[derive(Debug, Default)]
struct Kept {
    /// The [`KeptMove::seq`] the next move kept takes.
    next_seq: u64,
    /// By lowercase destination.
    moves: BTreeMap<String, KeptMove>,
    /// The lowercase destinations moved from each lowercase origin.
    by_origin: BTreeMap<String, Vec<String>>,
    /// The node paths to give bytes of their own, by lowercase path.
    at_risk: BTreeMap<String, String>,
}

/// The moves SWFS reported on a mount, and the renamed files that need bytes of their own, as node
/// paths. Held in memory alone: a remounted instance keeps none.
///
/// The moves are bounded by [`KEPT_MOVES`]. What is queued grows with what the user moves and makes
/// at kept origins, and with the files below a directory queued, until an operation rewrites it:
/// what cannot be rewritten stays queued for the next.
#[derive(Debug, Default)]
pub struct RenamedAtRisk {
    kept: parking_lot::Mutex<Kept>,
}

/// The keys below `path`, a key, in `map`, and `path` itself where `inclusive`. Keys are paths in
/// ASCII lowercase, which spells each in as many bytes as it is spelled in, so a key and the path
/// it stands for split at the same places.
fn keys_below<V>(map: &BTreeMap<String, V>, path: &str, inclusive: bool) -> Vec<String> {
    let mut keys: Vec<String> = map
        .range(format!("{path}/")..format!("{path}0"))
        .map(|(key, _)| key.clone())
        .collect();
    if inclusive && map.contains_key(path) {
        keys.push(path.to_owned());
    }
    keys
}

/// `path` with `rest`, a path below it, joined on.
fn joined(path: &str, rest: &str) -> String {
    if rest.is_empty() {
        path.to_owned()
    } else {
        format!("{path}/{rest}")
    }
}

/// The ancestors of `path`, deepest first, each with what `path` holds below it.
fn ancestors(path: &str) -> impl Iterator<Item = (&str, &str)> {
    path.char_indices()
        .rev()
        .filter(|(_, character)| *character == '/')
        .map(move |(index, _)| (&path[..index], &path[index + 1..]))
}

impl Kept {
    fn insert(&mut self, mut kept: KeptMove) {
        let key = kept.destination.to_ascii_lowercase();
        self.remove(&key);
        kept.seq = self.next_seq;
        self.next_seq += 1;
        self.by_origin
            .entry(kept.origin.to_ascii_lowercase())
            .or_default()
            .push(key.clone());
        self.moves.insert(key, kept);
    }

    fn remove(&mut self, key: &str) -> Option<KeptMove> {
        let kept = self.moves.remove(key)?;
        let origin = kept.origin.to_ascii_lowercase();
        if let Some(destinations) = self.by_origin.get_mut(&origin) {
            destinations.retain(|destination| destination != key);
            if destinations.is_empty() {
                self.by_origin.remove(&origin);
            }
        }
        Some(kept)
    }

    fn queue(&mut self, path: String) {
        self.at_risk.insert(path.to_ascii_lowercase(), path);
    }

    /// Where the source holds the bytes of what is at `path`, and the kept directory move that
    /// says so where one does: the origin of the move that left it there, or the place below a
    /// moved directory's origin. `None` where no kept move leads there.
    fn source_of(&self, path: &str) -> Option<(String, Option<String>)> {
        let key = path.to_ascii_lowercase();
        if let Some(kept) = self.moves.get(&key) {
            return Some((kept.origin.clone(), kept.through.clone()));
        }
        ancestors(&key).find_map(|(ancestor, rest)| {
            self.moves
                .get(ancestor)
                .filter(|kept| kept.directory)
                .map(|kept| {
                    (
                        joined(&kept.origin, &path[path.len() - rest.len()..]),
                        Some(ancestor.to_owned()),
                    )
                })
        })
    }

    /// How the node at `path` came there by kept moves, if it did.
    fn carried(&self, path: &str) -> Option<Carried> {
        let key = path.to_ascii_lowercase();
        if self.moves.contains_key(&key) {
            return Some(Carried::Moved);
        }
        ancestors(&key)
            .any(|(ancestor, _)| self.moves.get(ancestor).is_some_and(|kept| kept.directory))
            .then_some(Carried::Below)
    }

    /// Queues the destination of each kept move whose origin something now arrives at, at `path`:
    /// SWFS no longer lists that origin, and reads the moved node from the source there.
    fn arrive(&mut self, path: &str, directory: bool) {
        if self.moves.is_empty() {
            return;
        }
        let key = path.to_ascii_lowercase();
        let mut queued: Vec<String> = Vec::new();
        for destination in self.by_origin.get(&key).into_iter().flatten() {
            if let Some(kept) = self.moves.get(destination) {
                queued.push(kept.destination.clone());
            }
        }
        // Below a moved directory's origin, the file at the same place below its destination. A
        // directory moved back to itself is its own origin, so what arrives below it is new.
        for (ancestor, rest) in ancestors(&key) {
            for destination in self.by_origin.get(ancestor).into_iter().flatten() {
                if let Some(kept) = self
                    .moves
                    .get(destination)
                    .filter(|kept| kept.directory && destination != ancestor)
                {
                    queued.push(joined(&kept.destination, &path[path.len() - rest.len()..]));
                }
            }
        }
        // A directory arriving over the origins below it.
        if directory {
            for origin in keys_below(&self.by_origin, &key, false) {
                for destination in &self.by_origin[&origin] {
                    if let Some(kept) = self
                        .moves
                        .get(destination)
                        .filter(|_| *destination != origin)
                    {
                        queued.push(kept.destination.clone());
                    }
                }
            }
        }
        for path in queued {
            self.queue(path);
        }
    }
}

impl RenamedAtRisk {
    /// Notes that SWFS moved the node at `from` to `to`: kept where `byte_less`, from where the
    /// source holds its bytes, and what is queued or kept below a moved directory moves with it.
    /// Something arriving at `to` puts at risk what was moved from there. A node that holds bytes
    /// of its own, as one the user made or wrote, is at no risk, and its move is not kept.
    pub fn moved(&self, from: &str, to: &str, directory: bool, byte_less: bool) {
        let mut kept = self.kept.lock();
        let from_key = from.to_ascii_lowercase();
        let (source, through) = kept
            .source_of(from)
            .unwrap_or_else(|| (from.to_owned(), None));
        kept.remove(&from_key);
        let mut rebased = Vec::new();
        for key in keys_below(&kept.moves, &from_key, false) {
            if let Some(mut moved) = kept.remove(&key) {
                moved.destination = joined(to, &moved.destination[from.len() + 1..]);
                rebased.push(moved);
            }
        }
        // What was found through a directory moved here is found through it where it now is.
        if directory {
            let to_key = to.to_ascii_lowercase();
            let rebase = |through: &mut Option<String>| {
                if let Some(rest) = through
                    .as_deref()
                    .and_then(|through| through.strip_prefix(from_key.as_str()))
                    .filter(|rest| rest.is_empty() || rest.starts_with('/'))
                {
                    *through = Some(format!("{to_key}{rest}"));
                }
            };
            for moved in kept.moves.values_mut() {
                rebase(&mut moved.through);
            }
            for moved in &mut rebased {
                rebase(&mut moved.through);
            }
        }
        for moved in rebased {
            kept.insert(moved);
        }
        for key in keys_below(&kept.at_risk, &from_key, true) {
            if let Some(queued) = kept.at_risk.remove(&key) {
                kept.queue(joined(to, queued.get(from.len() + 1..).unwrap_or_default()));
            }
        }
        kept.arrive(to, directory);
        if !byte_less {
            return;
        }
        // A node moved back to where its bytes are, which reads from there again, is kept as moved
        // to itself, so that a move of it later is known to move no bytes of its own though the
        // record holds its path changed since.
        if kept.moves.len() < KEPT_MOVES {
            kept.insert(KeptMove {
                origin: source,
                destination: to.to_owned(),
                directory,
                through,
                seq: 0,
            });
        } else {
            kept.queue(to.to_owned());
        }
    }

    /// How the node at `path` came there by kept moves, if it did.
    pub fn carried(&self, path: &str) -> Option<Carried> {
        let kept = self.kept.lock();
        if kept.moves.is_empty() {
            return None;
        }
        kept.carried(path)
    }

    /// A mark of the moves kept so far, for [`Self::prune_unlisted`] to leave those kept after it.
    pub fn mark(&self) -> u64 {
        self.kept.lock().next_seq
    }

    /// Forgets each move kept before `before` whose destination SWFS no longer lists as renamed,
    /// as `listed` tells: a thaw naming the destination has SWFS read it from the source at its own
    /// path. A node found through a directory move forgotten so, or through one found so in turn,
    /// is read from below that directory's destination since, and is queued at risk, as something
    /// may stand there now: a rewrite for each such node, whether or not anything does. A node kept
    /// as moved to itself stays, as SWFS lists it as renamed no longer.
    pub fn prune_unlisted(&self, before: u64, listed: impl Fn(&str) -> bool) {
        let mut kept = self.kept.lock();
        let stale: std::collections::HashSet<String> = kept
            .moves
            .iter()
            .filter(|(_, moved)| {
                moved.seq < before
                    && !moved.origin.eq_ignore_ascii_case(&moved.destination)
                    && !listed(&moved.destination)
            })
            .map(|(key, _)| key.clone())
            .collect();
        // Each directory whose origin changed: its key, the origin what was found through it lies
        // below, and where that now lies instead.
        let mut changed: Vec<(String, String, String)> = Vec::new();
        for key in &stale {
            if let Some(forgotten) = kept.remove(key)
                && forgotten.directory
            {
                changed.push((key.clone(), forgotten.origin, forgotten.destination));
            }
        }
        if changed.is_empty() {
            return;
        }
        // What was found through each directory, gathered once rather than once per directory.
        let mut found_through: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for (destination, moved) in &kept.moves {
            if let Some(through) = &moved.through {
                found_through
                    .entry(through.clone())
                    .or_default()
                    .push(destination.clone());
            }
        }
        while let Some((key, old_origin, new_origin)) = changed.pop() {
            for destination in found_through.remove(&key).unwrap_or_default() {
                let Some(mut moved) = kept.remove(&destination) else {
                    continue;
                };
                moved.through = None;
                if let Some(origin) =
                    rest_below(&moved.origin, &old_origin).map(|rest| joined(&new_origin, rest))
                {
                    let previous = std::mem::replace(&mut moved.origin, origin);
                    kept.queue(moved.destination.clone());
                    if moved.directory {
                        changed.push((destination, previous, moved.origin.clone()));
                    }
                }
                kept.insert(moved);
            }
        }
    }

    /// Notes that SWFS created a node at `path`, which puts at risk what was moved from there.
    pub fn created(&self, path: &str, directory: bool) {
        self.kept.lock().arrive(path, directory);
    }

    /// Notes that SWFS deleted the node at `path`, and with a directory what was below it: no
    /// move to there is kept, and nothing there is at risk.
    pub fn deleted(&self, path: &str) {
        let mut kept = self.kept.lock();
        if kept.moves.is_empty() && kept.at_risk.is_empty() {
            return;
        }
        let key = path.to_ascii_lowercase();
        for destination in keys_below(&kept.moves, &key, true) {
            kept.remove(&destination);
        }
        for queued in keys_below(&kept.at_risk, &key, true) {
            kept.at_risk.remove(&queued);
        }
    }

    /// Forgets the moves to each of `named`, which a thaw named, and so took off SWFS's list: SWFS
    /// reads each from the source at its own path from then on.
    pub fn forget_named(&self, named: &[RelativePath]) {
        let mut kept = self.kept.lock();
        if kept.moves.is_empty() {
            return;
        }
        for path in named {
            kept.remove(&path.as_str().to_ascii_lowercase());
        }
    }

    /// Where the source holds the bytes of the node at `path`, as kept moves tell: `None` where
    /// no kept move leads to it from elsewhere, as for a node moved back to where it was.
    pub fn origin_of(&self, path: &str) -> Option<String> {
        let kept = self.kept.lock();
        if kept.moves.is_empty() {
            return None;
        }
        kept.source_of(path)
            .map(|(origin, _)| origin)
            .filter(|origin| !origin.eq_ignore_ascii_case(path))
    }

    /// The node paths queued to be given bytes of their own.
    pub fn at_risk(&self) -> Vec<String> {
        self.kept.lock().at_risk.values().cloned().collect()
    }

    /// Takes `path` off the queue, and, where its file now holds bytes of its own, forgets the
    /// move that led to it.
    fn settled(&self, path: &str, own_bytes: bool) {
        let mut kept = self.kept.lock();
        let key = path.to_ascii_lowercase();
        kept.at_risk.remove(&key);
        if own_bytes {
            kept.remove(&key);
        }
    }

    /// Queues `path`, found below a directory at risk.
    fn queue(&self, path: String) {
        self.kept.lock().queue(path);
    }
}

/// Notes the move SWFS reported of the node at `from` to `to`, node paths, on the mount `resources`
/// serves ([`RenamedAtRisk::moved`]), before the modified-file record takes it in: the move is kept
/// only where what moved has no bytes of its own ([`moves_no_bytes_of_its_own`]). Reads the record
/// and the served tree, never the mount, as a callback runs on SWFS's one callback thread.
pub async fn note_move(resources: &MountResources, from: &str, to: &str, directory: bool) {
    let byte_less = moves_no_bytes_of_its_own(resources, from, directory).await;
    resources.renamed().moved(from, to, directory, byte_less);
}

/// Whether the node moved from `from` holds no bytes of its own, so that SWFS reads it from the
/// source: one a kept move left at `from`; one below a directory a kept move left there that the
/// record holds no change of its own to, and the ignore rules let it take in; or one the served
/// tree serves at `from`, which the view shows, and SWFS holds no change to. What the user made or
/// wrote, as the temporary file an editor saves through, holds bytes of its own. Where it moves to
/// does not matter: a file moved to an ignored path still reads from the source.
async fn moves_no_bytes_of_its_own(
    resources: &MountResources,
    from: &str,
    directory: bool,
) -> bool {
    let record = resources.modified();
    match resources.renamed().carried(from) {
        Some(Carried::Moved) => true,
        Some(Carried::Below) => {
            let Ok(from_path) = RelativePath::new_from_initial_path(from) else {
                return false;
            };
            !record.ignores(&from_path).await
                && record.is_modified(from).await.is_ok_and(|changed| !changed)
        }
        None => {
            if !record
                .coverage(from)
                .await
                .is_ok_and(|coverage| coverage == Coverage::Unchanged)
            {
                return false;
            }
            let Ok(from_path) = RelativePath::new_from_initial_path(from) else {
                return false;
            };
            !resources.hides(&from_path, directory)
                && resources
                    .served_tree()
                    .resolve(&from_path)
                    .await
                    .is_ok_and(|served| {
                        served.is_some_and(|served| served.is_directory() == directory)
                    })
        }
    }
}

/// What [`rewrite_in_place`] found at a path.
enum Rewritten {
    /// A file, now holding bytes of its own.
    File,
    /// A directory, holding these names.
    Directory(Vec<String>),
    /// Nothing.
    Nothing,
}

/// Rewrites each renamed file queued at risk through the mount at `mount_root`, one at a time, so
/// that SWFS holds bytes of its own for it rather than reading the source at a path that no longer
/// holds them. A directory queued is rewritten file by file. A file another handle writes to, or
/// one that cannot be rewritten for another reason, stays queued for the next operation.
///
/// The workaround [this module](self) describes: called by an operation that holds the mount
/// before it freezes it, so that what it rewrites reaches SWFS's list ahead of the freeze, and
/// outside any SWFS callback, as the reads need SWFS to call back. Each rewrite runs with leave
/// from `mount_io`, the bound on syscalls on a mount (ADR-00035). Remove it with the module.
pub async fn give_renamed_files_at_risk_bytes_of_their_own(
    at_risk: &RenamedAtRisk,
    mount_root: &Path,
    mount_io: &MountIo,
) {
    let mut pending = at_risk.at_risk();
    while let Some(path) = pending.pop() {
        let absolute = mount_root.join(path.replace('/', std::path::MAIN_SEPARATOR_STR));
        let rewritten = {
            let _permit = mount_io.permit().await;
            rewrite_in_place(&absolute).await
        };
        match rewritten {
            Ok(Rewritten::File) => {
                lore_debug!("SWFS: gave the renamed {path} bytes of its own");
                at_risk.settled(&path, true);
            }
            Ok(Rewritten::Nothing) => at_risk.settled(&path, true),
            Ok(Rewritten::Directory(names)) => {
                for name in names {
                    let child = joined(&path, &name);
                    at_risk.queue(child.clone());
                    pending.push(child);
                }
                // The move to the directory is kept, for the files below it that are not rewritten
                // yet.
                at_risk.settled(&path, false);
            }
            Err(error) => {
                lore_warn!(
                    "SWFS: unable to give the renamed {path} bytes of its own, left for the next \
                     operation: {error}"
                );
            }
        }
    }
}

/// Reads the file at `path` whole and writes the same bytes back through one handle that lets no
/// other write, a window at a time, and makes the write durable, so it reaches SWFS before the
/// freeze. Lists a directory instead.
async fn rewrite_in_place(path: &Path) -> std::io::Result<Rewritten> {
    let driver = lore_io::IoDriver::global();
    let metadata = match driver.metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Rewritten::Nothing);
        }
        Err(error) => return Err(error),
    };
    if metadata.is_dir() {
        let mut names = Vec::new();
        let mut entries = driver.read_dir(path).await?;
        while let Some(entry) = entries.next().await {
            if let Some(name) = entry?.file_name.to_str() {
                names.push(name.to_owned());
            }
        }
        return Ok(Rewritten::Directory(names));
    }
    let options = lore_io::OpenOptions::new().read(true).write(true);
    // Shares reading alone, so the open fails where another handle writes, and no other handle
    // writes while this one rewrites.
    #[cfg(target_family = "windows")]
    let options = options.share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ);
    let file = driver.open(path, &options).await?;
    let size = file.metadata().await?.len();
    let mut offset = 0;
    while offset < size {
        let want = (size - offset).min(REWRITE_WINDOW as u64) as usize;
        let bytes = file.read_exact_at(want, offset).await?;
        file.write_all_at(bytes, offset).await?;
        offset += want as u64;
    }
    file.sync_data().await?;
    Ok(Rewritten::File)
}
