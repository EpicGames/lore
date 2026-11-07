// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Latency summaries, met/missed verdicts and timed syscalls shared by the FUSE benchmarks.
#![allow(dead_code)]

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::time::Duration;
use std::time::Instant;

/// Nearest-rank percentile of `sorted`, which must be ascending; 0 when empty.
pub fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    pub count: usize,
    pub p50: u64,
    pub p99: u64,
    pub max: u64,
    pub mean: u64,
}

impl Summary {
    /// Sorts `samples` in place and summarises them.
    pub fn of(samples: &mut [u64]) -> Self {
        samples.sort_unstable();
        let sum: u128 = samples.iter().map(|&s| u128::from(s)).sum();
        Self {
            count: samples.len(),
            p50: percentile(samples, 50.0),
            p99: percentile(samples, 99.0),
            max: samples.last().copied().unwrap_or(0),
            mean: sum.checked_div(samples.len() as u128).unwrap_or(0) as u64,
        }
    }
}

/// Bytes per second over `elapsed`, in GB/s (10^9).
pub fn gb_per_s(bytes: u64, elapsed: Duration) -> f64 {
    bytes as f64 / elapsed.as_secs_f64() / 1e9
}

pub struct Row {
    pub measure: String,
    pub value: String,
    pub target: String,
    pub met: bool,
}

/// Renders rows as a fixed-width table ending in a newline.
pub fn render(rows: &[Row]) -> String {
    let width = |column: fn(&Row) -> &str, title: &str| {
        rows.iter()
            .map(|row| column(row).len())
            .chain([title.len()])
            .max()
            .unwrap_or(0)
    };
    let measure = width(|row| &row.measure, "Measure");
    let value = width(|row| &row.value, "Value");
    let target = width(|row| &row.target, "Target");
    let mut out = format!(
        "{:<measure$} | {:<value$} | {:<target$} | Verdict\n",
        "Measure", "Value", "Target"
    );
    out.push_str(&format!(
        "{}-|-{}-|-{}-|--------\n",
        "-".repeat(measure),
        "-".repeat(value),
        "-".repeat(target)
    ));
    for row in rows {
        out.push_str(&format!(
            "{:<measure$} | {:<value$} | {:<target$} | {}\n",
            row.measure,
            row.value,
            row.target,
            if row.met { "met" } else { "MISSED" }
        ));
    }
    out
}

/// Whether `larger` stays within `factor` of `smaller`, the test for a time that should not
/// grow with tree size.
pub fn within_factor(smaller: Duration, larger: Duration, factor: f64) -> bool {
    larger.as_secs_f64() <= smaller.as_secs_f64() * factor
}

pub fn filesystem_name(path: &Path) -> &'static str {
    let c_path = CString::new(path.as_os_str().as_bytes()).expect("path without NUL");
    // SAFETY: `statfs` is plain old data, valid when zeroed.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is NUL-terminated and `buf` is a valid out-pointer.
    let ret = unsafe { libc::statfs(c_path.as_ptr(), &mut buf) };
    assert_eq!(ret, 0, "statfs");
    match buf.f_type as u32 {
        0xef53 => "ext4",
        0x2fc1_2fc1 => "zfs",
        0x0102_1994 => "tmpfs",
        0x9123_683e => "btrfs",
        0x5846_5342 => "xfs",
        _ => "other",
    }
}

/// Nanoseconds one `statx` of `path` took.
pub fn statx_ns(path: &CString, flags: i32) -> u64 {
    // SAFETY: `statx` is plain old data, valid when zeroed.
    let mut buf: libc::statx = unsafe { std::mem::zeroed() };
    let started = Instant::now();
    // SAFETY: `path` is NUL-terminated and `buf` is a valid out-pointer.
    let ret = unsafe {
        libc::statx(
            libc::AT_FDCWD,
            path.as_ptr(),
            flags,
            libc::STATX_BASIC_STATS,
            &mut buf,
        )
    };
    let elapsed = started.elapsed().as_nanos() as u64;
    assert_eq!(ret, 0, "statx {path:?}");
    elapsed
}

pub fn c_path(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).expect("path without NUL")
}

/// Runs `find root -type f`; answers the files it listed and wall time.
pub fn find_files(root: &Path) -> (usize, Duration) {
    let started = Instant::now();
    let output = std::process::Command::new("find")
        .arg(root)
        .args(["-type", "f"])
        .output()
        .expect("find runs");
    let elapsed = started.elapsed();
    assert!(output.status.success(), "find {}", root.display());
    let files = output.stdout.iter().filter(|&&b| b == b'\n').count();
    (files, elapsed)
}

mod tests {
    use super::*;

    #[test]
    fn percentile_uses_nearest_rank() {
        let sorted: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&sorted, 50.0), 50);
        assert_eq!(percentile(&sorted, 99.0), 99);
        assert_eq!(percentile(&sorted, 100.0), 100);
        assert_eq!(percentile(&[7], 99.0), 7);
        assert_eq!(percentile(&[], 50.0), 0);
    }

    #[test]
    fn summary_sorts_and_reports_mean_and_max() {
        let mut samples = vec![30, 10, 20, 40];
        let summary = Summary::of(&mut samples);
        assert_eq!(samples, vec![10, 20, 30, 40]);
        assert_eq!(
            summary,
            Summary {
                count: 4,
                p50: 20,
                p99: 40,
                max: 40,
                mean: 25,
            }
        );
    }

    #[test]
    fn gb_per_s_is_decimal() {
        let rate = gb_per_s(2_000_000_000, Duration::from_millis(500));
        assert!((rate - 4.0).abs() < 1e-9, "{rate}");
        assert_eq!(gb_per_s(1, Duration::ZERO), f64::INFINITY);
    }

    #[test]
    fn render_marks_each_row_met_or_missed() {
        let table = render(&[
            Row {
                measure: "lookup p50".into(),
                value: "3 us".into(),
                target: "< 20 us".into(),
                met: true,
            },
            Row {
                measure: "read".into(),
                value: "1.0 GB/s".into(),
                target: ">= 2 GB/s".into(),
                met: false,
            },
        ]);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 4, "header, rule, two rows:\n{table}");
        assert!(lines[2].contains("lookup p50") && lines[2].ends_with("met"));
        assert!(lines[3].contains("read") && lines[3].ends_with("MISSED"));
        assert!(table.ends_with('\n'));
    }

    #[test]
    fn within_factor_compares_ratio() {
        let base = Duration::from_micros(100);
        assert!(within_factor(base, Duration::from_micros(200), 2.0));
        assert!(!within_factor(base, Duration::from_micros(201), 2.0));
        assert!(within_factor(Duration::ZERO, Duration::ZERO, 2.0));
    }
}
