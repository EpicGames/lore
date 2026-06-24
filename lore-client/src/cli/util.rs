// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::io::BufRead;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use lore::interface::LoreArray;
use lore::interface::LoreMaintenanceEventData;
use lore::interface::LorePathIgnoreEventData;
use lore::interface::LoreRevisionSyncProgressEventData;
use lore::interface::LoreString;

use crate::eprintln;
use crate::println;
use crate::styling::CommonStyles;

pub fn get_repository_path(path: Option<String>) -> LoreString {
    if let Some(path) = path {
        path.into()
    } else {
        let current_dir = std::env::current_dir().unwrap_or_default();
        let mut current_path = current_dir.as_path();
        loop {
            if current_path.join(".urc").is_dir() || current_path.join(".lore").is_dir() {
                break current_path.into();
            }
            if let Some(parent_path) = current_path.parent() {
                current_path = parent_path;
            } else {
                break current_dir.as_path().into();
            }
        }
    }
}

/// Compute `target` expressed relative to `base`, inserting `..` components as
/// needed. Returns `None` if a relative path can't be formed (e.g. mismatched
/// path prefixes/roots, such as different Windows drives).
fn diff_paths(target: &Path, base: &Path) -> Option<PathBuf> {
    let mut ta = target.components();
    let mut ba = base.components();

    let mut ta_rest;
    let mut ba_rest;
    loop {
        ta_rest = ta.clone();
        ba_rest = ba.clone();
        match (ta.next(), ba.next()) {
            (Some(Component::Normal(t)), Some(Component::Normal(b))) => {
                if !t.eq_ignore_ascii_case(b) {
                    // Components diverged; rewind both to this point.
                    break;
                }
            }
            (Some(t), Some(b)) => {
                // Root / prefix / cur-dir components must match exactly.
                if t != b {
                    return None;
                }
            }
            // One side exhausted (or both): rewind to the pre-`next()` state.
            _ => break,
        }
    }

    let mut result = PathBuf::new();
    for component in ba_rest {
        match component {
            Component::Normal(_) => result.push(".."),
            Component::CurDir => {}
            // A `..` or root/prefix in the remaining base means we can't form a
            // sane relative path.
            _ => return None,
        }
    }

    for component in ta_rest {
        result.push(component.as_os_str());
    }
    Some(result)
}

/// Build a repo-root-relative path for display, rebased on the current working
/// directory. Falls back to repo-root-relative string on any failure.
pub fn relativize_for_display(repo_root: &Path, cwd: &Path, repo_relative: &str) -> String {
    if repo_relative.is_empty() {
        return String::new();
    }
    let target = repo_root.join(repo_relative);
    match diff_paths(&target, cwd) {
        Some(rel) if rel.as_os_str().is_empty() => ".".to_string(),
        Some(rel) => rel.to_string_lossy().replace('\\', "/"),
        None => repo_relative.to_string(),
    }
}

pub fn read_targets_file(path: &String) -> Vec<String> {
    let targets_file = std::fs::File::open(path).unwrap();
    let reader = std::io::BufReader::new(targets_file);

    reader
        .lines()
        .map(|line| {
            if let Ok(line) = line {
                // TODO(mjansson): If this is a relative path it should be made absolute
                // using the target_file path as the base path
                line
            } else {
                "".to_owned()
            }
        })
        .filter(|path| !path.is_empty())
        .collect()
}

pub fn convert_to_lore_string_vec(paths: &[String]) -> Vec<LoreString> {
    paths.iter().map(LoreString::from).collect()
}

pub fn convert_paths_and_targets(
    paths: &Option<Vec<String>>,
    targets: &Option<String>,
) -> LoreArray<LoreString> {
    let mut converted = vec![];

    if let Some(paths) = paths {
        converted.append(&mut convert_to_lore_string_vec(paths));
    }

    if let Some(targets) = targets {
        converted.append(&mut convert_to_lore_string_vec(&read_targets_file(targets)));
    }

    LoreArray::from_vec(converted)
}

pub fn handle_maintenance_event(event: &LoreMaintenanceEventData) {
    eprintln!(
        "{}Server is in maintenance mode: {}{}",
        CommonStyles::MAINTENANCE,
        event.message,
        anstyle::Reset
    );
}

pub fn handle_path_ignore_event(event: &LorePathIgnoreEventData) {
    println!("Ignoring invalid path: {}", event.path);
}

pub fn format_bytes_to_string(bytes: u64) -> String {
    let mut unit = "bytes";

    let converted = if bytes > 1024 * 1024 * 1024 {
        unit = "GiB";
        (bytes / (1024 * 1024)) as f64 / 1024.0
    } else if bytes > 1024 * 1024 {
        unit = "MiB";
        (bytes / 1024) as f64 / 1024.0
    } else if bytes > 1024 {
        unit = "KiB";
        bytes as f64 / 1024.0
    } else {
        bytes as f64
    };

    format!("{converted:.2} {unit}")
}

pub fn progress_info_display(progress: &LoreRevisionSyncProgressEventData) -> String {
    let bytes_info = if progress.bytes_update_total > 0 {
        format!(
            ", {}/{}",
            format_bytes_to_string(progress.bytes_update),
            format_bytes_to_string(progress.bytes_update_total)
        )
    } else {
        String::new()
    };

    if progress.file_conflict > 0 {
        format!(
            "Syncing {}/{} files{}, {}/{} deleted, {} merged, {} conflicted",
            progress.file_update,
            progress.file_update_total,
            bytes_info,
            progress.file_delete,
            progress.file_delete_total,
            progress.file_automerge,
            progress.file_conflict
        )
    } else if progress.file_automerge > 0 {
        format!(
            "Syncing {}/{} files{}, {}/{} deleted, {} merged",
            progress.file_update,
            progress.file_update_total,
            bytes_info,
            progress.file_delete,
            progress.file_delete_total,
            progress.file_automerge
        )
    } else {
        format!(
            "Syncing {}/{} files{}, {}/{} deleted",
            progress.file_update,
            progress.file_update_total,
            bytes_info,
            progress.file_delete,
            progress.file_delete_total
        )
    }
}

pub fn merge_result_display(progress: &LoreRevisionSyncProgressEventData) -> String {
    format!(
        "Merged files, {} updated, {} deleted, {} merged, {} conflicted",
        progress.file_update, progress.file_delete, progress.file_automerge, progress.file_conflict
    )
}

pub async fn listen_for_termination(timeout: Option<Duration>) -> tokio::io::Result<()> {
    let timeout = timeout.unwrap_or(Duration::from_secs(u64::MAX));

    #[cfg(unix)]
    let (mut ctrl_c, mut sigterm, timeout) = {
        use tokio::signal::unix::SignalKind;
        use tokio::signal::unix::signal;
        (
            signal(SignalKind::interrupt())?,
            signal(SignalKind::terminate())?,
            tokio::time::sleep(timeout),
        )
    };

    #[cfg(unix)]
    tokio::select! {
        _ = ctrl_c.recv() => { println!(); },
        _ = sigterm.recv() => { println!("SIGTERM received"); },
        _ = timeout => {}
    }

    #[cfg(windows)]
    let (mut ctrl_c, timeout) = {
        (
            tokio::signal::windows::ctrl_c()?,
            tokio::time::sleep(timeout),
        )
    };

    #[cfg(windows)]
    tokio::select! {
        _ = ctrl_c.recv() => { println!(); },
        _ = timeout => {}
    }

    Ok(())
}

/// Read a line from stdin with proper visual line editing (backspace, etc.).
///
/// Some terminals have `ECHOE` / `ECHOK` disabled, which causes backspace to
/// echo as `^?` instead of visually erasing the character. This function
/// temporarily enables those flags for the duration of the read, then restores
/// the original terminal settings.
pub fn read_line_with_editing(buf: &mut String) -> std::io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;

        let fd = std::io::stdin().as_raw_fd();
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        let got_attrs = unsafe { libc::tcgetattr(fd, &mut original) } == 0;

        if got_attrs {
            let needs_fix =
                (original.c_lflag & libc::ECHOE) == 0 || (original.c_lflag & libc::ECHOK) == 0;

            if needs_fix {
                let mut modified = original;
                modified.c_lflag |= libc::ECHOE | libc::ECHOK;
                unsafe {
                    libc::tcsetattr(fd, libc::TCSANOW, &modified);
                }
                let result = std::io::stdin().read_line(buf);
                unsafe {
                    libc::tcsetattr(fd, libc::TCSANOW, &original);
                }
                return result;
            }
        }

        std::io::stdin().read_line(buf)
    }

    #[cfg(not(unix))]
    {
        std::io::stdin().read_line(buf)
    }
}
