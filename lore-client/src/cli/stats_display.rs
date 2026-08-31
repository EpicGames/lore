// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Rendering for the statistics a commit or a push reports: a block of totals
//! printed once the operation has finished.
//!
//! Every ratio is labelled with the quantity it is a fraction of. A line whose
//! count is zero is omitted, and a section in which nothing happened is omitted
//! entirely.

use lore::interface::LoreBranchPushStatsEventData;
use lore::interface::LoreCommitFileStatsData;
use lore::interface::LoreFragmentStatsData;

use crate::println;
use crate::util::format_bytes_to_string;

/// `value` as a percentage of `total`, or zero where there is no total.
fn percent(value: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        100.0 * (value as f64) / (total as f64)
    }
}

/// Payload bytes the operation prepared for storage, across data fragments and
/// fragment lists.
fn prepared_payload(fragments: &LoreFragmentStatsData) -> u64 {
    fragments.data_payload_bytes + fragments.fragmentlist_payload_bytes
}

/// Files by the action that staged each, and the content read and committed.
///
/// Prints nothing for a commit that touched no file.
pub fn print_commit_file_totals(files: &LoreCommitFileStatsData) {
    if files.files + files.deleted + files.directories_deleted == 0 {
        return;
    }

    println!("Files");
    print_count_line("Added", files.added, files.files);
    print_count_line("Modified", files.modified, files.files);
    print_count_line("Moved", files.moved, files.files);
    print_count_line("Copied", files.copied, files.files);
    if files.deleted > 0 {
        println!("  {:<28}: {}", "Deleted", files.deleted);
    }
    if files.directories_deleted > 0 {
        println!(
            "  {:<28}: {}",
            "Directories deleted", files.directories_deleted
        );
    }
    println!(
        "  {:<28}: {} ({})",
        "Read",
        files.files_read,
        format_bytes_to_string(files.bytes_transferred),
    );
    println!(
        "  {:<28}: {} ({})",
        "Committed",
        files.files,
        format_bytes_to_string(files.file_bytes),
    );
}

/// One `label: count (bytes)` line.
fn print_count_bytes(label: &str, count: u64, bytes: u64) {
    println!(
        "  {:<28}: {} ({})",
        label,
        count,
        format_bytes_to_string(bytes)
    );
}

/// One `label: count (share%)` line. Omitted where the count is zero; the share
/// is omitted where it would read `100.0%`.
fn print_count_line(label: &str, count: u64, total: u64) {
    if count == 0 {
        return;
    }
    if count == total {
        println!("  {label:<28}: {count}");
    } else {
        println!("  {:<28}: {} ({:.1}%)", label, count, percent(count, total));
    }
}

/// The fragment, local-store and remote-store totals of a commit.
///
/// `committed_bytes` is the content the commit wrote, which its file totals carry.
/// It is the denominator for the share that crossed the wire.
pub fn print_fragment_totals(fragments: &LoreFragmentStatsData, committed_bytes: u64) {
    if fragments.fragments_produced == 0 {
        return;
    }

    println!("Fragments");
    print_count_bytes(
        "Produced",
        fragments.fragments_produced,
        fragments.fragment_content_bytes,
    );
    print_count_bytes(
        "Already stored",
        fragments.fragments_deduplicated,
        fragments.deduplicated_content_bytes,
    );
    print_count_bytes(
        "Processed",
        fragments.fragments_processed,
        fragments.processed_content_bytes,
    );

    if fragments.no_payload_fragments > 0 {
        print_count_bytes(
            "Deduplicated no cache",
            fragments.no_payload_fragments,
            fragments.no_payload_content_bytes,
        );
    }

    if fragments.data_fragments > 0 {
        let saved = fragments
            .data_content_bytes
            .saturating_sub(fragments.data_payload_bytes);
        println!(
            "  {:<28}: {} ({} / {} - {:.1}% compression)",
            "Output payloads",
            fragments.data_fragments,
            format_bytes_to_string(fragments.data_payload_bytes),
            format_bytes_to_string(fragments.data_content_bytes),
            percent(saved, fragments.data_content_bytes),
        );
    }
    if fragments.fragmentlists > 0 {
        println!(
            "  {:<28}: {} ({})",
            "Output fragment lists",
            fragments.fragmentlists,
            format_bytes_to_string(fragments.fragmentlist_payload_bytes),
        );
    }

    let prepared = prepared_payload(fragments);
    if fragments.local_writes > 0 {
        println!("Local store");
        println!("  {:<28}: {}", "Metadata", fragments.local_writes);
        println!(
            "  {:<28}: {} ({})",
            "Payloads",
            fragments.local_payload_writes,
            format_bytes_to_string(fragments.local_payload_bytes),
        );
        println!(
            "  {:<28}: {} / {} prepared ({:.1}%)",
            "Total write",
            format_bytes_to_string(fragments.local_payload_bytes),
            format_bytes_to_string(prepared),
            percent(fragments.local_payload_bytes, prepared),
        );
    }

    if fragments.remote_writes > 0 {
        println!("Remote store");
        println!(
            "  {:<28}: {} ({:.1}%)",
            "Copy",
            fragments.remote_copy_writes,
            percent(fragments.remote_copy_writes, fragments.remote_writes),
        );
        println!(
            "  {:<28}: {} ({:.1}%)",
            "Put",
            fragments.remote_put_writes,
            percent(fragments.remote_put_writes, fragments.remote_writes),
        );
        if fragments.remote_already_durable > 0 {
            println!(
                "  {:<28}: {}",
                "Already durable", fragments.remote_already_durable
            );
        }
        if fragments.local_only_writes > 0 {
            println!("  {:<28}: {}", "Local only", fragments.local_only_writes);
        }

        if committed_bytes > 0 {
            println!(
                "  {:<28}: {} / {} committed ({:.1}%)",
                "Total transfer",
                format_bytes_to_string(fragments.remote_put_bytes),
                format_bytes_to_string(committed_bytes),
                percent(fragments.remote_put_bytes, committed_bytes),
            );
        }
        if fragments.remote_upload_failed > 0 {
            println!("  {:<28}: {}", "Failed", fragments.remote_upload_failed);
        }
    }
}

/// What a push did with the fragments the peer was asked about.
///
/// Prints nothing for a push that had nothing to register.
pub fn print_push_totals(fragments: &LoreBranchPushStatsEventData) {
    let total = fragments.deduplicated + fragments.copied + fragments.put;
    if total == 0 {
        return;
    }

    println!("Fragments");
    print_count_line("Already stored", fragments.deduplicated, total);
    print_count_line("Copy", fragments.copied, total);
    print_count_line("Put", fragments.put, total);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_total_yields_a_zero_share_rather_than_a_panic() {
        assert_eq!(percent(0, 0), 0.0);
        assert_eq!(percent(5, 0), 0.0);
    }

    #[test]
    fn a_share_is_a_percentage_of_its_total() {
        assert_eq!(percent(1, 4), 25.0);
        assert_eq!(percent(4, 4), 100.0);
    }

    #[test]
    fn prepared_payload_covers_both_kinds_of_output() {
        let fragments = LoreFragmentStatsData {
            data_payload_bytes: 900,
            fragmentlist_payload_bytes: 100,
            ..Default::default()
        };

        assert_eq!(prepared_payload(&fragments), 1000);
    }
}
