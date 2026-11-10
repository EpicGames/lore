// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Finding the backend that shares blocks between two directories, by cloning a probe file from
//! one into the other for real.

use std::io;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use super::Backend;
use super::Volume;
use super::volume::volume_of;

/// The name every probe file [`detect`] writes starts with.
pub const PROBE_PREFIX: &str = ".clone-probe-";

/// What [`detect`] found for cloning from one directory into another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detection {
    /// The volume of the source.
    pub source: Volume,
    /// The volume of the destination.
    pub destination: Volume,
    /// The backend that shares blocks between the two, or [`Backend::Copy`] where none does.
    pub backend: Backend,
}

/// Finds how a tree on the volume that holds the directory `source` can be cloned into the
/// directory `destination`.
///
/// Each of [`Backend::BLOCK_CLONING`] clones a small probe file written in `source` into
/// `destination`, and both files are removed again. The first that succeeds is the answer, and
/// the copy backend is the answer where none does. Volumes of two filesystems never share
/// blocks, so they get the copy backend without a probe. Volume identifiers decide nothing: Btrfs
/// gives each subvolume one of its own although `FICLONE` works between them, and two copies of
/// one virtual disk can carry the same one.
pub fn detect(source: &Path, destination: &Path) -> io::Result<Detection> {
    let source_volume = volume_of(source)?;
    let destination_volume = volume_of(destination)?;
    let backend = choose(
        source_volume.filesystem == destination_volume.filesystem,
        |backend| probe(backend, source, destination),
    )?;
    Ok(Detection {
        source: source_volume,
        destination: destination_volume,
        backend,
    })
}

/// The first of [`Backend::BLOCK_CLONING`] whose `probe` succeeds, where the two directories are
/// on volumes of the `same_filesystem`, and [`Backend::Copy`] otherwise. A probe that fails other
/// than with [`io::ErrorKind::Unsupported`] fails the choice.
#[lore_macro::test_pub]
fn choose(
    same_filesystem: bool,
    mut probe: impl FnMut(Backend) -> io::Result<()>,
) -> io::Result<Backend> {
    if !same_filesystem {
        return Ok(Backend::Copy);
    }
    for &backend in Backend::BLOCK_CLONING {
        match probe(backend) {
            Ok(()) => return Ok(backend),
            Err(err) if err.kind() == io::ErrorKind::Unsupported => {}
            Err(err) => return Err(err),
        }
    }
    Ok(Backend::Copy)
}

/// Clones a probe file written in `source` into `destination` with `backend`, then removes both
/// files.
fn probe(backend: Backend, source: &Path, destination: &Path) -> io::Result<()> {
    static PROBES: AtomicU64 = AtomicU64::new(0);
    let name = format!(
        "{PROBE_PREFIX}{}-{}",
        std::process::id(),
        PROBES.fetch_add(1, Ordering::Relaxed)
    );
    let original = source.join(&name);
    let clone = destination.join(format!("{name}-clone"));
    let cloned = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&original)
        .and_then(|mut file| file.write_all(b"clone probe"))
        .and_then(|()| backend.clone_file(&original, &clone));
    let _ = std::fs::remove_file(&clone);
    let _ = std::fs::remove_file(&original);
    cloned
}
