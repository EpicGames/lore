// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use lore_base::lore_spawn;
use smallvec::SmallVec;
use sysinfo::DiskRefreshKind;
use sysinfo::Disks;
use tokio::time::MissedTickBehavior;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::settings::LocalStoreMonitorSettings;

/// Returns true when `path` starts with every component of `root`.
///
/// Compares whole components, so `/variable` does not start with `/var`, and
/// ignores ASCII case, so a case-insensitive volume given a differently cased
/// path still matches. A path a case-sensitive volume holds apart from its root
/// matches all the same, which costs a warning naming the wrong location and
/// never a missed one. A root with no components matches nothing, so an empty
/// temporary root or mount point does not claim every path.
fn starts_with_components(path: &Path, root: &Path) -> bool {
    if root.as_os_str().is_empty() {
        return false;
    }

    let mut path_components = path.components();

    for root_component in root.components() {
        let Some(component) = path_components.next() else {
            return false;
        };

        if !component
            .as_os_str()
            .eq_ignore_ascii_case(root_component.as_os_str())
        {
            return false;
        }
    }

    true
}

/// `path` with a Windows verbatim prefix replaced by the plain spelling.
///
/// Canonicalization returns `\\?\C:\dir` and `\\?\UNC\server\share`, where the
/// mount table and the configuration carry `C:\dir` and `\\server\share`. A
/// verbatim volume name has no plain spelling and is left as it stands.
fn plain_prefix(path: PathBuf) -> PathBuf {
    let plain = path.to_str().and_then(|text| {
        let rest = text.strip_prefix(r"\\?\")?;

        if let Some(share) = rest.strip_prefix(r"UNC\") {
            return Some(format!(r"\\{share}"));
        }

        let spelling = rest.as_bytes();

        (spelling.first().is_some_and(u8::is_ascii_alphabetic) && spelling.get(1) == Some(&b':'))
            .then(|| rest.to_owned())
    });

    plain.map_or(path, PathBuf::from)
}

/// `path` resolved against the filesystem, spelled as the mount table is.
fn real_location(path: &Path) -> Option<PathBuf> {
    path.canonicalize().ok().map(plain_prefix)
}

/// `path` with its deepest existing ancestor resolved and the remainder kept.
///
/// Settles `..` and symlinks the way the filesystem does, which no lexical pass
/// can: `/srv/../tmp` is `/tmp` only while `/srv` is not a symlink. The ancestor
/// resolves before the path itself exists, which is when a store is classified.
fn resolved_location(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        let Some(real) = real_location(ancestor) else {
            continue;
        };

        return real.join(path.strip_prefix(ancestor).unwrap_or(Path::new("")));
    }

    path.to_path_buf()
}

/// Each path alongside its real location, where the two differ.
///
/// A path that does not exist cannot be resolved, so a symlinked directory is
/// matched in both forms.
fn with_real_locations(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut resolved = Vec::new();

    for path in paths {
        if let Some(real) = real_location(&path) {
            resolved.push(real);
        }
        resolved.push(path);
    }

    resolved.sort();
    resolved.dedup();
    resolved
}

/// The directories this platform hands out for temporary files.
///
/// On macOS the system temporary directory is a per-user path under
/// `/var/folders`, so the conventional Unix roots are carried too, each in both
/// forms because they are symlinks into `/private` there. Resolved once; they
/// do not move under a running server.
fn temporary_roots() -> &'static [PathBuf] {
    static ROOTS: OnceLock<Vec<PathBuf>> = OnceLock::new();

    ROOTS.get_or_init(|| {
        with_real_locations([
            std::env::temp_dir(),
            PathBuf::from("/tmp"),
            PathBuf::from("/var/tmp"),
        ])
    })
}

/// Returns true when `path` sits inside any of `roots`.
fn is_under_any(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| starts_with_components(path, root))
}

/// Returns true when `path` is a temporary location that does not survive a reboot.
pub fn is_temporary_path(path: &Path) -> bool {
    is_under_any(&resolved_location(path), temporary_roots())
}

/// A mounted filesystem and the local stores kept on it.
struct Volume<'a> {
    mount_point: &'a Path,
    available_bytes: u64,
    stores: SmallVec<[&'a Path; STORES_PER_CHECK]>,
}

/// Local store paths a server configures, past which the check spills onto the
/// heap.
const STORES_PER_CHECK: usize = 4;

/// The volumes one check found, and the stores on each.
type Volumes<'a> = SmallVec<[Volume<'a>; STORES_PER_CHECK]>;

/// Comma-separated paths, formatted only where the message is emitted.
struct PathList<'a>(&'a [&'a Path]);

impl std::fmt::Display for PathList<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, path) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_str(", ")?;
            }

            write!(formatter, "{}", path.display())?;
        }

        Ok(())
    }
}

/// The mount backing `path`, from `mounts` as pairs of mount point and
/// available bytes.
///
/// The longest matching mount point wins, being the filesystem the path lives
/// on. `path` is matched as written, so the caller resolves it once rather than
/// once per mount.
fn mount_holding<'a>(
    path: &Path,
    mounts: impl Iterator<Item = (&'a Path, u64)>,
) -> Option<(&'a Path, u64)> {
    mounts
        .filter(|(mount_point, _)| starts_with_components(path, mount_point))
        .max_by_key(|(mount_point, _)| mount_point.components().count())
}

/// Records `store` on the volume mounted at `mount_point`, adding the volume
/// when it is the first store found there.
///
/// Free space is a property of the filesystem, not of the directory, so stores
/// sharing one volume are read and reported together rather than once each. One
/// filesystem mounted at two points is two volumes here: the mount point is the
/// only key sysinfo reports uniquely, device names repeating across
/// pseudo-filesystems.
fn add_store_to_volume<'a>(
    volumes: &mut Volumes<'a>,
    mount_point: &'a Path,
    available_bytes: u64,
    store: &'a Path,
) {
    match volumes
        .iter_mut()
        .find(|volume| volume.mount_point == mount_point)
    {
        Some(volume) => volume.stores.push(store),
        None => volumes.push(Volume {
            mount_point,
            available_bytes,
            stores: SmallVec::from_slice(&[store]),
        }),
    }
}

/// Warns when a volume holds less free space than the threshold.
fn report_volume(volume: &Volume<'_>, threshold_bytes: u64) {
    if volume.available_bytes >= threshold_bytes {
        debug!(
            mount_point = %volume.mount_point.display(),
            available_bytes = volume.available_bytes,
            "Local store disk space checked",
        );

        return;
    }

    warn!(
        mount_point = %volume.mount_point.display(),
        stores = %PathList(&volume.stores),
        available_bytes = volume.available_bytes,
        threshold_bytes,
        "The volume mounted at {} is running out of disk space. Free space has fallen to {} \
         bytes, below the configured threshold of {} bytes. Local stores held there: {}. Free \
         space or move the stores to a larger volume.",
        volume.mount_point.display(),
        volume.available_bytes,
        threshold_bytes,
        PathList(&volume.stores),
    );
}

/// The volumes holding `paths`, warning once per path no mount matches.
///
/// `mounts` yields the mount table afresh for each path, the caller holding it.
/// Each path is placed on the volume backing it, so several stores on one
/// filesystem draw one reading.
///
/// An unreadable path warns rather than passing quietly, so a monitor taking no
/// reading is not mistaken for one reporting all clear. It warns once per spell,
/// `warned_unreadable` holding one flag per entry in `paths`, so a mount table
/// that keeps a path unmatched is reported once and a path that comes back and
/// goes again is reported anew. A lost reading never stops the remaining paths
/// from being checked.
fn volumes_holding<'a, M, I>(
    paths: &'a [PathBuf],
    mounts: M,
    warned_unreadable: &mut [bool],
) -> Volumes<'a>
where
    M: Fn() -> I,
    I: Iterator<Item = (&'a Path, u64)>,
{
    let mut volumes = Volumes::new();

    for (path, warned) in paths.iter().zip(warned_unreadable.iter_mut()) {
        let resolved = resolved_location(path);

        match mount_holding(&resolved, mounts()) {
            Some((mount_point, available_bytes)) => {
                *warned = false;
                add_store_to_volume(&mut volumes, mount_point, available_bytes, path);
            }
            None if !*warned => {
                *warned = true;
                warn!(
                    path = %path.display(),
                    "Cannot check the disk space available to the local store at {}: no mounted \
                     filesystem matches that path. Free space is going unmonitored.",
                    path.display(),
                );
            }
            None => {}
        }
    }

    volumes
}

/// Warns for every volume below the threshold.
///
/// Only the space figures are refreshed. Device kind and per-disk I/O counters
/// cost syscalls the check has no use for.
fn check_available_space(
    disks: &mut Disks,
    paths: &[PathBuf],
    threshold_bytes: u64,
    warned_unreadable: &mut [bool],
) {
    disks.refresh_specifics(true, DiskRefreshKind::nothing().with_storage());

    let volumes = volumes_holding(
        paths,
        || {
            disks
                .list()
                .iter()
                .map(|disk| (disk.mount_point(), disk.available_space()))
        },
        warned_unreadable,
    );

    for volume in &volumes {
        report_volume(volume, threshold_bytes);
    }
}

/// Starts periodic monitoring of the disk space available to the local stores.
///
/// The first tick fires immediately, so a server starting on a full volume says
/// so at once. Duplicate paths are collapsed, stores sharing a volume are
/// checked once, and a `check_interval_seconds` of zero turns monitoring off.
/// One mount table is kept for the server's life and refreshed in place.
pub fn start_local_store_monitor(mut paths: Vec<PathBuf>, settings: &LocalStoreMonitorSettings) {
    paths.sort();
    paths.dedup();

    if paths.is_empty() {
        return;
    }

    if settings.check_interval_seconds == 0 {
        info!("Local store disk space monitoring is disabled by configuration");
        return;
    }

    let threshold_bytes = settings.low_space_threshold_bytes;
    let interval = Duration::from_secs(settings.check_interval_seconds);

    lore_spawn!(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut warned_unreadable = vec![false; paths.len()];
        let mut disks = Disks::new();

        loop {
            ticker.tick().await;
            check_available_space(&mut disks, &paths, threshold_bytes, &mut warned_unreadable);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount_for<'a>(path: &str, mounts: &'a [(&'a str, u64)]) -> Option<(&'a Path, u64)> {
        mount_holding(
            Path::new(path),
            mounts
                .iter()
                .map(|(mount_point, available_bytes)| (Path::new(*mount_point), *available_bytes)),
        )
    }

    fn space_for(path: &str, mounts: &[(&str, u64)]) -> Option<u64> {
        mount_for(path, mounts).map(|(_, available_bytes)| available_bytes)
    }

    /// The volumes `paths` land on, as mount point and the stores held there.
    fn volumes_for<'a>(
        paths: &'a [&'a str],
        mounts: &'a [(&'a str, u64)],
    ) -> Vec<(&'a Path, Vec<&'a Path>)> {
        let mut volumes = Volumes::new();

        for path in paths {
            if let Some((mount_point, available_bytes)) = mount_for(path, mounts) {
                add_store_to_volume(&mut volumes, mount_point, available_bytes, Path::new(*path));
            }
        }

        volumes
            .into_iter()
            .map(|volume| (volume.mount_point, volume.stores.into_vec()))
            .collect()
    }

    #[test]
    fn a_path_under_a_root_is_under_it() {
        let roots = vec![PathBuf::from("/tmp"), PathBuf::from("/var/tmp")];

        assert!(is_under_any(Path::new("/tmp/lore-server"), &roots));
        assert!(is_under_any(Path::new("/var/tmp/lore-server"), &roots));
    }

    #[test]
    fn a_root_is_under_itself() {
        let roots = vec![PathBuf::from("/tmp")];

        assert!(is_under_any(Path::new("/tmp"), &roots));
    }

    #[test]
    fn a_path_outside_every_root_is_not_under_any() {
        let roots = vec![PathBuf::from("/tmp"), PathBuf::from("/var/tmp")];

        assert!(!is_under_any(Path::new("/srv/lore/store"), &roots));
    }

    #[test]
    fn a_root_matches_only_on_whole_components() {
        let roots = vec![PathBuf::from("/var")];

        assert!(!is_under_any(Path::new("/variable/lore-server"), &roots));
    }

    /// Forward slashes so the components split alike on every platform.
    #[test]
    fn a_differently_cased_root_matches() {
        assert!(starts_with_components(
            Path::new("/Users/u/appdata/local/temp/lore-server"),
            Path::new("/Users/u/AppData/Local/Temp"),
        ));
    }

    #[test]
    fn ignoring_case_still_matches_only_whole_components() {
        assert!(!starts_with_components(
            Path::new("/VARIABLE/lore-server"),
            Path::new("/var"),
        ));
    }

    #[test]
    fn a_root_longer_than_the_path_does_not_match() {
        assert!(!starts_with_components(
            Path::new("/tmp"),
            Path::new("/tmp/lore-server"),
        ));
    }

    /// Mirrors macOS, where a store at `/tmp` is caught only by the
    /// conventional roots.
    #[test]
    fn a_per_user_temp_dir_does_not_hide_the_conventional_roots() {
        let roots = vec![
            PathBuf::from("/var/folders/ab/cd1234/T"),
            PathBuf::from("/tmp"),
            PathBuf::from("/var/tmp"),
        ];

        assert!(is_under_any(Path::new("/tmp/lore-server"), &roots));
        assert!(is_under_any(
            Path::new("/var/folders/ab/cd1234/T/lore-server"),
            &roots
        ));
    }

    #[test]
    fn temporary_roots_carry_the_system_temp_dir() {
        assert!(temporary_roots().contains(&std::env::temp_dir()));
    }

    /// Mirrors macOS, where `/tmp` and `/var` are symlinks into `/private`.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_root_is_carried_in_both_forms() {
        let dir = lore_base::test_util::TempDir::new("local-store-monitor-symlink-");
        let target = dir.child("target");
        let link = dir.child("link");
        std::fs::create_dir(&target).expect("create target directory");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");

        let roots = with_real_locations([link.clone()]);

        assert!(roots.contains(&link));
        assert!(roots.contains(&target.canonicalize().expect("resolve target")));
    }

    #[cfg(unix)]
    #[test]
    fn a_path_under_a_symlinked_root_is_matched_before_it_exists() {
        let dir = lore_base::test_util::TempDir::new("local-store-monitor-unborn-");
        let target = dir.child("target");
        let link = dir.child("link");
        std::fs::create_dir(&target).expect("create target directory");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");

        let roots = with_real_locations([link]);
        let store = target
            .canonicalize()
            .expect("resolve target")
            .join("lore-server");

        assert!(!store.exists());
        assert!(is_under_any(&store, &roots));
    }

    #[test]
    fn a_directory_in_the_system_temp_dir_is_temporary() {
        let dir = lore_base::test_util::TempDir::new("local-store-monitor-temporary-");

        assert!(is_temporary_path(dir.path()));
    }

    #[test]
    fn a_persistent_path_is_not_temporary() {
        assert!(!is_temporary_path(Path::new("/srv/lore/store")));
    }

    /// A lexical prefix says nothing once `..` is involved.
    #[test]
    fn a_parent_component_leading_out_of_the_temp_dir_is_not_temporary() {
        assert!(!is_temporary_path(&std::env::temp_dir().join("..")));
    }

    #[test]
    fn a_parent_component_is_settled_against_the_filesystem() {
        let dir = lore_base::test_util::TempDir::new("local-store-monitor-parent-");
        let inside = dir.child("inside");
        std::fs::create_dir(&inside).expect("create directory");

        let through_parent = inside.join("..").join("inside").join("store");
        let settled = real_location(&inside)
            .expect("resolve directory")
            .join("store");

        assert!(!through_parent.exists());
        assert_eq!(resolved_location(&through_parent), settled);
    }

    #[test]
    fn space_comes_from_the_only_matching_mount() {
        assert_eq!(space_for("/srv/lore", &[("/", 100)]), Some(100));
    }

    #[test]
    fn the_longest_matching_mount_point_wins() {
        assert_eq!(
            space_for("/var/tmp/lore", &[("/", 100), ("/var", 50)]),
            Some(50)
        );
    }

    #[test]
    fn the_longest_matching_mount_point_wins_whatever_the_order() {
        assert_eq!(
            space_for("/var/tmp/lore", &[("/var", 50), ("/", 100)]),
            Some(50)
        );
    }

    #[test]
    fn a_mount_point_equal_to_the_path_matches() {
        assert_eq!(space_for("/data", &[("/data", 50)]), Some(50));
    }

    #[test]
    fn a_textual_prefix_is_not_a_matching_mount() {
        assert_eq!(space_for("/variable/lore", &[("/var", 50)]), None);
    }

    #[test]
    fn a_path_on_no_known_mount_has_no_reading() {
        assert_eq!(space_for("/srv/lore", &[("/data", 50)]), None);
    }

    #[test]
    fn the_volume_is_the_longest_matching_mount_point() {
        assert_eq!(
            mount_for("/var/tmp/lore", &[("/", 100), ("/var", 50)]),
            Some((Path::new("/var"), 50))
        );
    }

    /// One reading covers the filesystem, so two stores on it are one entry.
    #[test]
    fn two_stores_on_one_volume_are_checked_once() {
        assert_eq!(
            volumes_for(
                &["/data/immutable", "/data/mutable"],
                &[("/", 100), ("/data", 50)]
            ),
            vec![(
                Path::new("/data"),
                vec![Path::new("/data/immutable"), Path::new("/data/mutable")]
            )]
        );
    }

    #[test]
    fn stores_on_separate_volumes_are_checked_apart() {
        assert_eq!(
            volumes_for(
                &["/data/immutable", "/srv/mutable"],
                &[("/", 100), ("/data", 50)]
            ),
            vec![
                (Path::new("/data"), vec![Path::new("/data/immutable")]),
                (Path::new("/"), vec![Path::new("/srv/mutable")]),
            ]
        );
    }

    /// A repeated path joins the volume rather than opening a second one.
    /// `start_local_store_monitor` drops duplicates before the check runs.
    #[test]
    fn a_repeated_path_joins_the_volume_it_already_sits_on() {
        assert_eq!(
            volumes_for(&["/data/store", "/data/store"], &[("/data", 50)]),
            vec![(
                Path::new("/data"),
                vec![Path::new("/data/store"), Path::new("/data/store")]
            )]
        );
    }

    #[test]
    fn a_store_on_no_known_mount_lands_on_no_volume() {
        assert!(volumes_for(&["/srv/lore"], &[("/data", 50)]).is_empty());
    }

    #[test]
    fn a_path_list_separates_paths_with_commas() {
        let paths = [Path::new("/data/immutable"), Path::new("/data/mutable")];

        assert_eq!(
            PathList(&paths).to_string(),
            "/data/immutable, /data/mutable"
        );
    }

    /// `TMPDIR=""` makes `std::env::temp_dir()` empty, and a root that matched
    /// every path would report every store as ephemeral.
    #[test]
    fn an_empty_root_matches_nothing() {
        assert!(!starts_with_components(
            Path::new("/srv/lore/store"),
            Path::new(""),
        ));
        assert!(!is_under_any(
            Path::new("/srv/lore/store"),
            &[PathBuf::new()]
        ));
    }

    #[test]
    fn an_empty_mount_point_holds_nothing() {
        assert_eq!(space_for("/srv/lore", &[("", 50)]), None);
    }

    fn plainly(path: &str) -> PathBuf {
        plain_prefix(PathBuf::from(path))
    }

    /// Windows canonicalization spells a drive verbatim, the mount table does
    /// not, and a store on a drive matches no mount until the two agree.
    #[test]
    fn a_verbatim_disk_is_spelled_as_the_mount_table_spells_it() {
        assert_eq!(
            plainly(r"\\?\C:\lore\store"),
            PathBuf::from(r"C:\lore\store")
        );
    }

    #[test]
    fn a_verbatim_share_is_spelled_as_the_mount_table_spells_it() {
        assert_eq!(
            plainly(r"\\?\UNC\server\share\store"),
            PathBuf::from(r"\\server\share\store")
        );
    }

    /// A volume name has no plain spelling to fall back on.
    #[test]
    fn a_verbatim_volume_name_stands() {
        let volume = r"\\?\Volume{b75e2c83-0000-0000-0000-602f00000000}\store";

        assert_eq!(plainly(volume), PathBuf::from(volume));
    }

    #[test]
    fn a_path_carrying_no_verbatim_prefix_stands() {
        assert_eq!(plainly("/srv/lore/store"), PathBuf::from("/srv/lore/store"));
    }

    /// A directory that exists, so `resolved_location` settles the store paths
    /// under a mount point the test names, whatever the platform spells one.
    ///
    /// Read through [`real_location`], which is how the check spells a path it
    /// resolves: `canonicalize` alone answers a verbatim path on Windows, and a
    /// mount point spelled that way matches none of the stores under it.
    fn mount_root(prefix: &str) -> (lore_base::test_util::TempDir, PathBuf) {
        let dir = lore_base::test_util::TempDir::new(prefix);
        let root = real_location(dir.path()).expect("resolve temporary directory");

        (dir, root)
    }

    fn check(
        root: &Path,
        paths: &[PathBuf],
        mounted: bool,
        warned: &mut [bool],
    ) -> Vec<Vec<PathBuf>> {
        let mount = [(root, 50u64)];
        let mounts = if mounted { &mount[..] } else { &[][..] };

        volumes_holding(paths, || mounts.iter().copied(), warned)
            .into_iter()
            .map(|volume| volume.stores.iter().map(PathBuf::from).collect())
            .collect()
    }

    #[test]
    fn stores_under_one_mount_share_a_volume() {
        let (_dir, root) = mount_root("local-store-monitor-one-volume-");
        let paths = [root.join("one"), root.join("two")];

        assert_eq!(
            check(&root, &paths, true, &mut [false, false]),
            vec![paths.to_vec()]
        );
    }

    #[test]
    fn a_path_no_mount_matches_yields_no_volume() {
        let (_dir, root) = mount_root("local-store-monitor-no-mount-");
        let paths = [root.join("one")];

        assert!(check(&root, &paths, false, &mut [false]).is_empty());
    }

    /// The flag holds the warning back for as long as the condition lasts, and
    /// a reading releases it so the next spell is reported afresh.
    #[test]
    fn an_unreadable_path_warns_once_per_spell() {
        let (_dir, root) = mount_root("local-store-monitor-spell-");
        let paths = [root.join("one")];
        let mut warned = [false];

        check(&root, &paths, false, &mut warned);
        assert_eq!(warned, [true]);

        check(&root, &paths, false, &mut warned);
        assert_eq!(warned, [true]);

        check(&root, &paths, true, &mut warned);
        assert_eq!(warned, [false]);

        check(&root, &paths, false, &mut warned);
        assert_eq!(warned, [true]);
    }
}
