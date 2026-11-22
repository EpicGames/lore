// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures plant roots, targets and stubs directly; what these test is how the checks judge them.
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use lore_base::test_util::TempDir;
#[cfg(target_family = "windows")]
use lore_io::clone::Locality;
use lore_io::clone::Space;
use lore_io::clone::Volume;
use lore_revision::fs::walk::Progress;
use lore_revision::global::external_dir::check::*;
use lore_revision::global::external_dir::record::MoveKind;
use lore_revision::global::external_dir::resolve::Availability;
use lore_revision::global::external_dir::resolve::RootState;
use lore_revision::global::external_dir::resolve::resolve_in;

/// The lower-case hex name of an external directory.
const INSTANCE_HEX: &str = "0123456789abcdef0123456789abcdef";

/// A global data directory, and a directory beside it for roots and targets.
struct Fixture {
    global: TempDir,
    volume: TempDir,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture {
            global: TempDir::new("lore-external-check-global-"),
            volume: TempDir::new("lore-external-check-volume-"),
        }
    }

    fn default_location(&self) -> PathBuf {
        self.global.path().join("external")
    }

    /// A path named `name` beside the global data directory, not created.
    fn path(&self, name: &str) -> PathBuf {
        self.volume.path().join(name)
    }

    fn state(&self) -> RootState {
        resolve_in(self.global.path().to_path_buf())
    }

    /// The default root, created with one external directory.
    fn default_root(&self) -> PathBuf {
        let root = self.default_location();
        std::fs::create_dir_all(root.join(INSTANCE_HEX)).expect("create the default root");
        root
    }

    fn check(&self, target: &Path) -> Result<Option<MoveKind>, CheckError> {
        check_target(&self.state(), target)
    }
}

/// Whether `result` refuses the target.
fn refuses_target(result: Result<Option<MoveKind>, CheckError>) -> bool {
    matches!(result, Err(err) if err.is_invalid_arguments())
}

/// The names in the directory `dir`.
#[cfg(target_family = "unix")]
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("list the directory")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// A root that reads as comparing names regardless of case: a lookup of its external directory's
/// name in upper case finds an entry, the directory itself on a volume that ignores case, and
/// elsewhere a second entry made with that name.
#[cfg(target_family = "unix")]
fn root_reading_as_case_insensitive(root: &Path) {
    std::fs::create_dir_all(root.join(INSTANCE_HEX)).expect("create the external directory");
    let upper = root.join(INSTANCE_HEX.to_ascii_uppercase());
    if !upper.exists() {
        std::fs::create_dir(upper).expect("create the upper-case entry");
    }
}

/// A target in a scratch directory that the file-system checks refuse against a root reading as
/// case-insensitive: on a volume that compares case, any path there, whose probe compares names
/// another way than the root; on one that ignores case, where every root reads as the probe does,
/// a path in a parent the probe can't write in, restored as the answer drops. `None` there when
/// running as root, whom no mode stops writing.
#[cfg(target_family = "unix")]
fn target_failing_the_checks() -> Option<(TempDir, PathBuf, ReadOnly)> {
    use std::os::unix::fs::PermissionsExt;

    let elsewhere = TempDir::new("lore-external-check-elsewhere-");
    if !probe_ignores_case(elsewhere.path()) {
        let target = elsewhere.path().join("moved");
        return Some((elsewhere, target, ReadOnly(None)));
    }
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return None;
    }
    let parent = elsewhere.path().join("parent");
    std::fs::create_dir(&parent).expect("the parent");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o500))
        .expect("make the parent read-only");
    let target = parent.join("moved");
    Some((elsewhere, target, ReadOnly(Some(parent))))
}

/// A directory made read-only for a test, made writable again as it drops.
#[cfg(target_family = "unix")]
struct ReadOnly(Option<PathBuf>);

#[cfg(target_family = "unix")]
impl Drop for ReadOnly {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;

        if let Some(dir) = &self.0 {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
}

/// A target equal to the current root has nothing to move, and a root that doesn't exist yet is
/// created rather than moved.
#[test]
fn the_current_root_moves_nowhere_and_a_missing_root_is_created() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.check(&fixture.default_location()).expect("check"),
        None
    );
    std::fs::create_dir(fixture.default_location()).expect("create the default root");
    assert_eq!(
        fixture.check(&fixture.default_location()).expect("check"),
        None
    );

    let fixture = Fixture::new();
    let state = available_at(&fixture, fixture.path("missing"));
    assert_eq!(
        check_target(&state, &fixture.default_location()).expect("check"),
        Some(MoveKind::Create)
    );
}

/// A target that exists is refused, and so is one inside the current root or containing it.
#[test]
fn an_existing_target_and_one_inside_or_containing_the_root_are_refused() {
    let fixture = Fixture::new();
    fixture.default_root();
    let existing = fixture.path("existing");
    std::fs::create_dir(&existing).expect("create the target");
    assert!(refuses_target(fixture.check(&existing)));
    let file = fixture.path("file");
    std::fs::write(&file, b"file").expect("write the file");
    assert!(refuses_target(fixture.check(&file)));

    let inside = fixture.default_location().join(INSTANCE_HEX).join("moved");
    assert!(refuses_target(fixture.check(&inside)));
    let inside = fixture.default_location().join("missing/moved");
    assert!(refuses_target(fixture.check(&inside)));
    assert!(refuses_target(fixture.check(fixture.global.path())));
}

/// A state whose root is `root`, available, in the global data directory of `fixture`.
fn available_at(fixture: &Fixture, root: PathBuf) -> RootState {
    RootState {
        availability: Availability::Available(root),
        ..fixture.state()
    }
}

/// A root beside the global data directory, with one external directory.
fn root_beside(fixture: &Fixture) -> PathBuf {
    let root = fixture.path("root");
    std::fs::create_dir_all(root.join(INSTANCE_HEX)).expect("create the root");
    root
}

/// A target that isn't UTF-8, or isn't absolute, is refused.
#[cfg(target_family = "unix")]
#[test]
fn a_target_that_is_not_utf8_or_not_absolute_is_refused() {
    use std::os::unix::ffi::OsStrExt;

    let fixture = Fixture::new();
    let name = std::ffi::OsStr::from_bytes(b"not-\xFF-utf8");
    assert!(refuses_target(
        fixture.check(&fixture.volume.path().join(name))
    ));
    assert!(refuses_target(fixture.check(Path::new("relative/root"))));
}

/// A target inside a repository instance's working tree is refused, and so is one inside a live
/// FUSE instance's mount, at the default location too.
#[test]
fn a_target_inside_an_instance_is_refused() {
    let fixture = Fixture::new();
    let instance = fixture.path("instance");
    std::fs::create_dir_all(instance.join(".lore")).expect("create the instance");
    assert!(refuses_target(
        fixture.check(&instance.join("assets/external"))
    ));

    let fixture = Fixture::new();
    let state = available_at(&fixture, root_beside(&fixture));
    check_target(&state, &fixture.default_location()).expect("outside any instance");
    std::fs::create_dir(fixture.global.path().join(".lore")).expect("create the instance");
    assert!(refuses_target(check_target(
        &state,
        &fixture.default_location()
    )));

    #[cfg(target_os = "linux")]
    {
        use lore_revision::fs::fuse::discover::test_util::fuse_lore_entry;
        use lore_revision::fs::fuse::discover::test_util::plant_table;

        let fixture = Fixture::new();
        let state = available_at(&fixture, root_beside(&fixture));
        let mount_point = fixture.global.path().canonicalize().expect("canonicalize");
        let _planted = plant_table(
            &mount_point,
            fuse_lore_entry(
                &mount_point,
                lore_revision::instance::InstanceId::generate(),
            ),
        );
        assert!(refuses_target(check_target(
            &state,
            &fixture.default_location()
        )));
    }
}

/// A move back to the default location, which holds the redirect to the current root, renames
/// the root, and a directory at the default location is refused as an existing target.
#[tokio::test]
async fn the_default_location_holding_the_redirect_is_the_target_of_a_rename() {
    use lore_revision::global::external_dir::marker::Marker;
    use lore_revision::global::external_dir::setting::RootId;
    use lore_revision::global::external_dir::setting::Setting;

    let fixture = Fixture::new();
    let root = fixture.path("root");
    std::fs::create_dir_all(root.join(INSTANCE_HEX)).expect("create the root");
    #[cfg(target_family = "unix")]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
            .expect("make the root private");
    }
    let (id, generation) = (RootId::generate(), std::num::NonZeroU64::MIN);
    Marker { id, generation }
        .save(&root)
        .await
        .expect("save the marker");
    Setting {
        path: root.to_str().expect("UTF-8").to_owned(),
        id,
        generation,
    }
    .save(fixture.global.path())
    .await
    .expect("save the setting");
    super::redirect(&fixture.default_location(), &root);

    assert_eq!(
        fixture.check(&fixture.default_location()).expect("check"),
        Some(MoveKind::Rename)
    );
    assert_eq!(fixture.check(&root).expect("check"), None);

    let other = fixture.path("other");
    std::fs::create_dir(&other).expect("create another directory");
    std::fs::remove_file(fixture.default_location())
        .or_else(|_| std::fs::remove_dir(fixture.default_location()))
        .expect("remove the redirect");
    super::redirect(&fixture.default_location(), &other);
    assert!(refuses_target(fixture.check(&fixture.default_location())));
}

/// The default location is exempt from the file-system checks: a target there passes against a
/// root reading as case-insensitive, while a target elsewhere that the checks refuse, as
/// [`target_failing_the_checks`] finds one, is refused and left with nothing made.
#[cfg(target_family = "unix")]
#[test]
fn the_default_location_is_exempt_from_the_file_system_checks() {
    let fixture = Fixture::new();
    let root = fixture.path("root");
    root_reading_as_case_insensitive(&root);
    let state = available_at(&fixture, root);

    assert_eq!(
        check_target(&state, &fixture.default_location()).expect("check"),
        Some(MoveKind::Rename)
    );
    if let Some((_elsewhere, target, _read_only)) = target_failing_the_checks() {
        assert!(refuses_target(check_target(&state, &target)));
        let parent = target.parent().expect("a parent");
        assert_eq!(names_in(parent), Vec::<String>::new());
    }
}

/// The capability probe passes on a scratch directory against a root of either case rule
/// that matches it and an empty one, refuses a root whose names compare in another way, and
/// leaves nothing behind. On a volume that ignores case, every root reads as case-insensitive,
/// so no root there compares names in another way, and only the matching half runs.
#[cfg(target_family = "unix")]
#[test]
fn the_capability_probe_compares_names_as_the_root_does_and_is_removed() {
    let fixture = Fixture::new();
    let ancestor = TempDir::new("lore-external-check-probe-");
    let ignores_case = probe_ignores_case(ancestor.path());

    let empty = fixture.path("empty");
    std::fs::create_dir(&empty).expect("create the empty root");
    probe_capabilities(ancestor.path(), Some(&empty)).expect("an empty root");
    probe_capabilities(ancestor.path(), None).expect("no root");

    let sensitive = fixture.path("sensitive");
    std::fs::create_dir_all(sensitive.join(INSTANCE_HEX)).expect("create the root");
    let insensitive = fixture.path("insensitive");
    root_reading_as_case_insensitive(&insensitive);
    if ignores_case {
        probe_capabilities(ancestor.path(), Some(&insensitive)).expect("a matching root");
    } else {
        probe_capabilities(ancestor.path(), Some(&sensitive)).expect("a matching root");
        assert!(matches!(
            probe_capabilities(ancestor.path(), Some(&insensitive)),
            Err(err) if err.is_invalid_arguments()
        ));
    }

    assert_eq!(names_in(ancestor.path()), Vec::<String>::new());
}

/// Whether names in the directory `dir` compare regardless of case.
#[cfg(target_family = "unix")]
fn probe_ignores_case(dir: &Path) -> bool {
    let probe = dir.join("case");
    std::fs::write(&probe, b"").expect("write the case probe");
    let ignores = dir.join("CASE").exists();
    std::fs::remove_file(probe).expect("remove the case probe");
    ignores
}

/// A parent belongs to the user or to root, and only its owner can write it: no write bit for
/// group or others, where on Linux the group bits are an access ACL's mask, and on macOS no
/// extended ACL that lets anyone else add or delete entries.
#[cfg(target_family = "unix")]
#[test]
fn the_parent_rule_admits_a_parent_of_the_user_or_root_that_others_cannot_write() {
    let user = 1000;
    assert!(is_safe_parent(user, 0o40700, false, user));
    assert!(is_safe_parent(user, 0o40755, false, user));
    assert!(is_safe_parent(0, 0o40755, false, user));
    assert!(is_safe_parent(0, 0o40700, false, user));
    assert!(!is_safe_parent(1001, 0o40755, false, user), "another user");
    assert!(!is_safe_parent(user, 0o40775, false, user), "group bits");
    assert!(
        !is_safe_parent(user, 0o40770, false, user),
        "an ACL mask granting write"
    );
    assert!(!is_safe_parent(user, 0o40757, false, user), "others");
    assert!(!is_safe_parent(0, 0o41777, false, user), "a shared /tmp");
    assert!(
        !is_safe_parent(user, 0o40700, true, user),
        "an extended ACL"
    );
    assert!(!is_safe_parent(0, 0o40755, true, user), "an extended ACL");
}

/// A target whose nearest existing ancestor others can write is refused.
#[cfg(target_family = "unix")]
#[test]
fn a_target_in_a_directory_others_can_write_is_refused() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let shared = fixture.path("shared");
    std::fs::create_dir(&shared).expect("create the parent");
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777))
        .expect("let others write the parent");
    assert!(refuses_target(fixture.check(&shared.join("a/moved"))));

    let state = available_at(&fixture, root_beside(&fixture));
    check_target(&state, &fixture.default_location()).expect("a private parent");
    std::fs::set_permissions(
        fixture.global.path(),
        std::fs::Permissions::from_mode(0o770),
    )
    .expect("let the group write the global data directory");
    assert!(refuses_target(check_target(
        &state,
        &fixture.default_location()
    )));
}

/// On macOS, a target whose parent's extended ACL lets another user add entries is refused.
#[cfg(target_os = "macos")]
#[test]
fn a_target_in_a_directory_an_extended_acl_opens_is_refused() {
    let fixture = Fixture::new();
    let shared = fixture.path("shared");
    std::fs::create_dir(&shared).expect("create the parent");
    let granted = std::process::Command::new("chmod")
        .args(["+a", "everyone allow add_file,add_subdirectory"])
        .arg(&shared)
        .status()
        .expect("run chmod");
    assert!(granted.success());
    assert!(refuses_target(fixture.check(&shared.join("moved"))));
}

/// D20: the root renames where both paths have one volume ID and, on Linux, one mount ID, or on
/// macOS and Windows one mount point, and copies otherwise.
#[test]
fn a_move_renames_on_one_volume_and_mount_and_copies_otherwise() {
    let volume = |id: u64, mount_point: &str, mount_id: Option<u64>| {
        (
            Volume {
                id,
                filesystem: "xfs".to_string(),
                mount_point: mount_point.into(),
            },
            mount_id,
        )
    };
    let renames = |current: &(Volume, Option<u64>), target: &(Volume, Option<u64>)| match move_kind(
        (&current.0, current.1),
        (&target.0, target.1),
    ) {
        MoveKind::Rename => true,
        MoveKind::Copy => false,
        other => panic!("{other:?}"),
    };

    assert!(renames(
        &volume(1, "/mnt/a", Some(7)),
        &volume(1, "/mnt/a", Some(7))
    ));
    assert!(!renames(
        &volume(1, "/mnt/a", Some(7)),
        &volume(2, "/mnt/a", Some(7))
    ));
    assert!(
        !renames(&volume(1, "/mnt/a", Some(7)), &volume(1, "/mnt/a", Some(8))),
        "a bind mount of one file system"
    );
    assert!(renames(
        &volume(1, "/Volumes/a", None),
        &volume(1, "/Volumes/a", None)
    ));
    assert!(
        !renames(&volume(1, "C:\\", None), &volume(1, "D:\\", None)),
        "two volumes sharing a serial number"
    );

    let root = TempDir::new("lore-external-check-one-volume-");
    let inner = root.path().join("inner");
    std::fs::create_dir(&inner).expect("create the directory");
    let read = |path: &Path| {
        (
            lore_io::clone::volume_of(path).expect("the volume"),
            lore_io::clone::locality_of(path)
                .expect("the locality")
                .mount_id,
        )
    };
    assert!(renames(&read(root.path()), &read(&inner)));
}

/// The free space a copy needs is what the root takes without what each external directory's
/// `write-staging/` holds, and a volume with less is refused.
#[test]
fn free_space_must_cover_the_root_without_write_staging() {
    let root = TempDir::new("lore-external-check-space-");
    let instance = root.path().join(INSTANCE_HEX);
    std::fs::create_dir_all(instance.join("write")).expect("create the write directory");
    std::fs::write(instance.join("write/kept"), vec![1u8; 4096]).expect("write a file");
    std::fs::create_dir_all(instance.join("write-staging")).expect("create the staging");
    std::fs::write(instance.join("write-staging/staged"), vec![1u8; 1 << 20])
        .expect("write a staged file");

    let size = measure_root(root.path(), 4096, &Progress::default()).expect("measure");
    #[cfg(target_os = "linux")]
    assert_eq!(
        size.size,
        4 * 4096,
        "two directories, the staging and one file"
    );
    #[cfg(not(target_os = "linux"))]
    assert_eq!(size.file_count, 2);

    let at = root.path();
    let space = |available| Space {
        available,
        block: 4096,
    };
    require_space(size.size, &space(size.size), at).expect("just enough");
    assert!(matches!(
        require_space(size.size, &space(size.size - 1), at),
        Err(err) if err.is_invalid_arguments()
    ));
}

/// D13: the configured executable is probed for the command, unless it is the binary the
/// service restarts from, and with none configured nothing is probed.
#[test]
fn the_configured_executable_is_probed_for_the_command() {
    let stubs = TempDir::new("lore-external-check-stub-");
    let knows = stub(stubs.path(), "knows", StubAnswer::KnowsTheCommand);
    let older = stub(stubs.path(), "older", StubAnswer::Fails);
    let hangs = stub(stubs.path(), "hangs", StubAnswer::Hangs);
    let timeout = Duration::from_secs(10);

    probe_service_executable(Some(&knows), None, timeout).expect("a build that knows it");
    probe_service_executable(Some(&knows), Some(&older), timeout).expect("another restart");
    probe_service_executable(None, Some(&older), timeout).expect("none configured");
    probe_service_executable(Some(&older), Some(&older), timeout).expect("the restarted binary");
    for (refused, why) in [
        (older.clone(), "an older build"),
        (stubs.path().join("missing"), "a missing executable"),
    ] {
        assert!(
            matches!(
                probe_service_executable(Some(&refused), None, timeout),
                Err(err) if err.is_internal()
            ),
            "{why}"
        );
    }
    assert!(matches!(
        probe_service_executable(Some(&hangs), None, Duration::from_millis(200)),
        Err(err) if err.is_internal()
    ));
}

/// How a stub executable answers.
enum StubAnswer {
    /// Succeeds for `service set-external-directory --help`, and fails otherwise.
    KnowsTheCommand,
    /// Fails, as a build without the command does.
    Fails,
    /// Runs past any probe's bound.
    Hangs,
}

/// Writes a stub executable named `name` in `dir`.
fn stub(dir: &Path, name: &str, answer: StubAnswer) -> PathBuf {
    #[cfg(target_family = "unix")]
    {
        use std::os::unix::fs::PermissionsExt;

        let body = match answer {
            StubAnswer::KnowsTheCommand => {
                "[ \"$1 $2 $3\" = \"service set-external-directory --help\" ]"
            }
            StubAnswer::Fails => "exit 2",
            StubAnswer::Hangs => "exec sleep 60",
        };
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write the stub");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make the stub executable");
        path
    }
    #[cfg(target_family = "windows")]
    {
        let body = match answer {
            StubAnswer::KnowsTheCommand => {
                "@if \"%1 %2 %3\"==\"service set-external-directory --help\" (exit /b 0) else (exit /b 1)"
            }
            StubAnswer::Fails => "@exit /b 2",
            StubAnswer::Hangs => "@ping -n 60 127.0.0.1 >nul",
        };
        let path = dir.join(format!("{name}.cmd"));
        std::fs::write(&path, format!("{body}\r\n")).expect("write the stub");
        path
    }
}

/// On Windows, a volume a copy can't keep access control lists, alternate data streams and
/// sparse files on is refused.
#[cfg(target_family = "windows")]
#[test]
fn a_copy_target_must_hold_acls_streams_and_sparse_files() {
    use windows_sys::Win32::System::SystemServices::FILE_NAMED_STREAMS;
    use windows_sys::Win32::System::SystemServices::FILE_PERSISTENT_ACLS;
    use windows_sys::Win32::System::SystemServices::FILE_SUPPORTS_SPARSE_FILES;

    let volume = Volume {
        id: 1,
        filesystem: "NTFS".to_string(),
        mount_point: "D:\\".into(),
    };
    let locality = |flags| Locality {
        local: true,
        mount_id: None,
        flags,
    };
    let all = FILE_PERSISTENT_ACLS | FILE_NAMED_STREAMS | FILE_SUPPORTS_SPARSE_FILES;
    require_copy_features(&volume, locality(all)).expect("NTFS");
    assert!(matches!(
        require_copy_features(&volume, locality(all & !FILE_NAMED_STREAMS)),
        Err(err) if err.is_invalid_arguments()
    ));
}

/// On Windows, a move is refused while the caller's working directory is inside the current
/// root.
#[cfg(target_family = "windows")]
#[tokio::test]
async fn a_working_directory_inside_the_root_is_refused_on_windows() {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreString;

    let fixture = Fixture::new();
    let root = fixture.default_root();
    let working_directory = root.join(INSTANCE_HEX);
    let globals = LoreGlobalArgs {
        working_directory: LoreString::from(working_directory.to_str().expect("UTF-8")),
        ..LoreGlobalArgs::default()
    };
    let context = crate::helper::execution_in_with(fixture.global.path(), globals);
    let checked = LORE_CONTEXT
        .scope(context, async { fixture.check(&fixture.path("moved")) })
        .await;
    assert!(
        matches!(checked, Err(ref err) if err.is_internal()),
        "{checked:?}"
    );
}

/// A new root for `--abandon` is refused in a parent the capability probe can't write in.
#[cfg(target_family = "unix")]
#[test]
fn a_new_root_is_probed() {
    use std::os::unix::fs::PermissionsExt;

    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let fixture = Fixture::new();
    let parent = fixture.path("parent");
    std::fs::create_dir(&parent).expect("the parent");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o500))
        .expect("make the parent read-only");

    let checked = check_new_root(fixture.global.path(), &parent.join("new"));

    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))
        .expect("make the parent writable again");
    assert!(checked.is_err(), "{checked:?}");
}

/// A new root at the default location is accepted while it holds the command's redirect to the
/// root the setting names, and refused while it holds a link the command didn't make.
#[cfg(target_family = "unix")]
#[tokio::test]
async fn a_new_root_takes_the_place_of_the_commands_redirect_only() {
    let fixture = Fixture::new();
    let old = fixture.path("old");
    lore_revision::global::external_dir::setting::Setting {
        path: old.display().to_string(),
        id: lore_revision::global::external_dir::setting::RootId::generate(),
        generation: std::num::NonZeroU64::MIN,
    }
    .save(fixture.global.path())
    .await
    .expect("the setting");
    std::os::unix::fs::symlink(&old, fixture.default_location()).expect("the redirect");

    check_new_root(fixture.global.path(), &fixture.default_location())
        .expect("the command's redirect gives way");

    std::fs::remove_file(fixture.default_location()).expect("remove the redirect");
    std::os::unix::fs::symlink(fixture.path("elsewhere"), fixture.default_location())
        .expect("a link the command didn't make");
    let refused = check_new_root(fixture.global.path(), &fixture.default_location());
    assert!(
        matches!(&refused, Err(err) if err.is_invalid_arguments()),
        "{refused:?}"
    );
}

/// A failure to read a path is refused where the drive holds no media, which only Windows
/// reports, and internal otherwise.
#[test]
fn a_drive_with_no_media_is_refused() {
    let path = Path::new("target");
    let other = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
    assert!(read_failure("read the volume of", path, &other).is_internal());
    #[cfg(target_family = "windows")]
    {
        let no_media = std::io::Error::from_raw_os_error(
            i32::try_from(windows_sys::Win32::Foundation::ERROR_NOT_READY).expect("a small code"),
        );
        assert!(read_failure("read the volume of", path, &no_media).is_invalid_arguments());
    }
}
