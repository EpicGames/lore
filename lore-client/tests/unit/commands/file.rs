// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;

use lore::interface::Hash;
use lore::interface::LoreFileAuditEventData;
use lore::interface::LoreFileAuditOutcome;
use lore::interface::LoreMetadata;
use lore::interface::LoreMetadataEventData;
use lore::interface::LoreString;
use lore::interface::Partition;
use lore::interface::metadata;
use lore_client::commands::file::AuditAttribution;
use lore_client::commands::file::FileAuditArgs;
use lore_client::commands::file::audit_args;
use lore_client::commands::file::audit_attribution;
use lore_client::commands::file::audit_date;
use lore_client::commands::file::audit_rows;
use lore_client::commands::file::audit_span;
use lore_client::commands::file::parse_context_lines;

#[test]
fn rejects_non_numeric() {
    assert_eq!(
        parse_context_lines("abc").unwrap_err(),
        "expected a non-negative integer; got 'abc'"
    );
}

#[test]
fn rejects_negative() {
    assert_eq!(
        parse_context_lines("-1").unwrap_err(),
        "expected a non-negative integer; got '-1'"
    );
}

#[test]
fn accepts_valid() {
    assert_eq!(parse_context_lines("5").unwrap(), 5);
}

/// Verify `-1` reaches the value parser (rather than being rejected as an
/// unexpected argument), which is the behavior `allow_negative_numbers` enables.
#[test]
fn diff_negative_context_reaches_parser() {
    use clap::Parser;
    let result =
        lore_client::cli::LoreCli::try_parse_from(["lore", "diff", "-U", "-1", "original.txt"]);
    let err = result
        .err()
        .expect("expected -U -1 to be rejected")
        .to_string();
    assert!(err.contains("non-negative integer"), "got: {err}");
}

fn file_audit_args(max_revisions: u32) -> FileAuditArgs {
    FileAuditArgs {
        path: "file.txt".to_string(),
        revision: None,
        max_revisions,
        ignore_space_at_eol: false,
        no_ignore_space_at_eol: false,
        ignore_space_change: false,
    }
}

fn range(
    repository: u8,
    revision: u8,
    line_first: u32,
    line_last: u32,
    outcome: LoreFileAuditOutcome,
) -> LoreFileAuditEventData {
    LoreFileAuditEventData {
        repository: Partition::from([repository; 16]),
        revision: if outcome == LoreFileAuditOutcome::Attributed {
            Hash::from([revision; 32])
        } else {
            Hash::default()
        },
        line_first,
        line_last,
        outcome,
    }
}

/// A name resolver that knows no names, as the auth service answers when it
/// cannot be reached.
fn no_names(_ids: &[LoreString]) -> HashMap<String, String> {
    HashMap::new()
}

fn attribution(author: &str) -> AuditAttribution {
    AuditAttribution {
        author: author.to_string(),
        timestamp: 1_758_585_600_000,
    }
}

/// The author column of each row: the third whitespace-separated field.
fn authors(rows: &[String]) -> Vec<String> {
    rows.iter()
        .map(|row| {
            row.split_whitespace()
                .nth(2)
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

#[test]
fn audit_span_of_one_line_prints_the_line() {
    assert_eq!(audit_span(7, 7), "7");
    assert_eq!(audit_span(7, 9), "7-9");
}

#[test]
fn audit_date_without_a_timestamp_prints_a_placeholder() {
    assert_eq!(audit_date(0), "-");
    assert_eq!(audit_date(1_758_585_600_000), "2025-09-23");
}

/// The timestamp is read as signed: the largest one is a millisecond before the epoch, and one
/// outside the years -9999 to 9999 prints the placeholder.
#[test]
fn audit_date_reads_the_timestamp_as_signed() {
    assert_eq!(audit_date(u64::MAX), "1969-12-31");
    assert_eq!(audit_date(1 << 63), "-");
}

#[test]
fn audit_date_past_the_year_9999_prints_a_placeholder() {
    assert_eq!(audit_date(253_402_300_799_999), "9999-12-31");
    assert_eq!(audit_date(253_402_300_800_000), "-");
}

/// `--max-revisions 0` removes the bound, which the library asks for as the
/// largest bound. Without the translation a zero would read as "use the default".
#[test]
fn max_revisions_of_zero_asks_for_an_unlimited_walk() {
    let args = audit_args(&file_audit_args(0));
    assert_eq!(args.max_revisions, u32::MAX);
}

#[test]
fn a_named_max_revisions_is_passed_through() {
    let args = audit_args(&file_audit_args(7));
    assert_eq!(args.max_revisions, 7);
}

/// An audit ignores trailing whitespace unless told otherwise, so a command
/// naming no whitespace flag asks for no refusal.
#[test]
fn whitespace_at_the_end_of_a_line_is_ignored_without_a_flag() {
    let args = audit_args(&file_audit_args(0));
    assert_eq!(args.no_ignore_space_at_eol, 0);
    assert_eq!(args.ignore_space_change, 0);
}

#[test]
fn naming_the_default_changes_nothing() {
    let mut named = file_audit_args(0);
    named.ignore_space_at_eol = true;

    assert_eq!(audit_args(&named).no_ignore_space_at_eol, 0);
}

#[test]
fn refusing_the_default_asks_for_a_literal_comparison() {
    let mut literal = file_audit_args(0);
    literal.no_ignore_space_at_eol = true;

    assert_eq!(audit_args(&literal).no_ignore_space_at_eol, 1);
}

#[test]
fn internal_whitespace_is_ignored_only_when_asked_for() {
    let mut collapsed = file_audit_args(0);
    collapsed.ignore_space_change = true;

    assert_eq!(audit_args(&collapsed).ignore_space_change, 1);
}

/// A revision that answers several ranges is read once, and a range no
/// revision holds reads none.
#[test]
fn audit_rows_read_each_distinct_revision_once() {
    let attributed = LoreFileAuditOutcome::Attributed;
    let ranges = [
        range(1, 7, 1, 2, attributed),
        range(1, 8, 3, 3, attributed),
        range(1, 7, 4, 9, attributed),
        range(1, 0, 10, 10, LoreFileAuditOutcome::Uncommitted),
        range(1, 0, 11, 12, LoreFileAuditOutcome::BudgetExhausted),
    ];

    let read = RefCell::new(Vec::new());
    let rows = audit_rows(
        &ranges,
        |revision| {
            read.borrow_mut().push(revision);
            Some(AuditAttribution::default())
        },
        no_names,
    );

    assert_eq!(rows.len(), ranges.len());
    assert_eq!(
        read.into_inner(),
        vec![Hash::from([7; 32]), Hash::from([8; 32])]
    );
}

/// One revision hash in two repositories is two revisions, so each is read.
#[test]
fn audit_rows_read_one_hash_in_two_repositories_twice() {
    let attributed = LoreFileAuditOutcome::Attributed;
    let ranges = [range(1, 7, 1, 1, attributed), range(2, 7, 2, 2, attributed)];

    let mut reads = 0;
    audit_rows(
        &ranges,
        |_revision| {
            reads += 1;
            Some(AuditAttribution::default())
        },
        no_names,
    );

    assert_eq!(reads, 2);
}

#[test]
fn an_attributed_row_shows_the_revision_author_and_date() {
    let ranges = [range(1, 7, 1, 38, LoreFileAuditOutcome::Attributed)];

    let rows = audit_rows(
        &ranges,
        |_revision| {
            Some(AuditAttribution {
                author: "ben".to_string(),
                timestamp: 1_758_585_600_000,
            })
        },
        no_names,
    );

    assert_eq!(
        rows,
        vec![format!(
            "{:>12}  {}  {:<16}  {}",
            "1-38",
            Hash::from([7; 32]),
            "ben",
            "2025-09-23"
        )]
    );
    assert_eq!(rows[0].len(), 108);
}

/// A revision that records no author or date prints placeholders rather
/// than dropping the row.
#[test]
fn an_attributed_row_without_an_author_or_date_prints_placeholders() {
    let ranges = [range(1, 7, 3, 3, LoreFileAuditOutcome::Attributed)];

    let rows = audit_rows(
        &ranges,
        |_revision| Some(AuditAttribution::default()),
        no_names,
    );

    assert_eq!(
        rows[0].split_whitespace().collect::<Vec<_>>(),
        vec!["3", Hash::from([7; 32]).to_string().as_str(), "-", "-"]
    );
}

/// A revision whose metadata cannot be read is still the revision that
/// changed the range, so the row keeps it and marks the author and date as
/// unresolved, in the same columns.
#[test]
fn an_attributed_row_whose_revision_cannot_be_read_marks_author_and_date_unresolved() {
    let ranges = [range(1, 7, 1, 38, LoreFileAuditOutcome::Attributed)];

    let rows = audit_rows(&ranges, |_revision| None, no_names);

    assert_eq!(
        rows,
        vec![format!(
            "{:>12}  {}  {:<16}  {}",
            "1-38",
            Hash::from([7; 32]),
            "unresolved",
            "unresolved"
        )]
    );
    assert_eq!(rows[0].len(), 108);
}

/// The author prints as the user name the stored ID resolves to.
#[test]
fn an_author_the_auth_service_knows_prints_as_its_name() {
    let ranges = [range(1, 7, 1, 38, LoreFileAuditOutcome::Attributed)];

    let rows = audit_rows(
        &ranges,
        |_revision| Some(attribution("019294c9dfaa7cd98f87283a30d1b1f6")),
        |_ids| {
            HashMap::from([(
                "019294c9dfaa7cd98f87283a30d1b1f6".to_string(),
                "ada".to_string(),
            )])
        },
    );

    assert_eq!(
        rows,
        vec![format!(
            "{:>12}  {}  {:<16}  {}",
            "1-38",
            Hash::from([7; 32]),
            "ada",
            "2025-09-23"
        )]
    );
}

/// An ID the auth service has no name for prints as stored.
#[test]
fn an_author_with_no_name_prints_as_the_stored_id() {
    let attributed = LoreFileAuditOutcome::Attributed;
    let ranges = [range(1, 7, 1, 1, attributed), range(1, 8, 2, 2, attributed)];

    let rows = audit_rows(
        &ranges,
        |revision| {
            Some(attribution(if revision == Hash::from([7; 32]) {
                "known-id"
            } else {
                "unknown-id"
            }))
        },
        |_ids| HashMap::from([("known-id".to_string(), "ada".to_string())]),
    );

    assert_eq!(authors(&rows), vec!["ada", "unknown-id"]);
}

/// Every author is resolved in one call, each distinct ID once, and a row
/// with no author asks for nothing.
#[test]
fn authors_are_resolved_in_one_call_with_the_distinct_ids() {
    let attributed = LoreFileAuditOutcome::Attributed;
    let ranges = [
        range(1, 7, 1, 1, attributed),
        range(1, 8, 2, 2, attributed),
        range(1, 9, 3, 3, attributed),
        range(1, 10, 4, 4, attributed),
        range(1, 0, 5, 5, LoreFileAuditOutcome::Uncommitted),
    ];

    let calls = RefCell::new(Vec::new());
    audit_rows(
        &ranges,
        |revision| {
            Some(if revision == Hash::from([8; 32]) {
                attribution("second-id")
            } else if revision == Hash::from([10; 32]) {
                AuditAttribution::default()
            } else {
                attribution("first-id")
            })
        },
        |ids| {
            let mut ids = ids.iter().map(LoreString::to_string).collect::<Vec<_>>();
            ids.sort();
            calls.borrow_mut().push(ids);
            HashMap::new()
        },
    );

    assert_eq!(
        calls.into_inner(),
        vec![vec!["first-id".to_string(), "second-id".to_string()]]
    );
}

/// With no author to name, the auth service is not asked: asked for no
/// identities, it answers with the local user instead.
#[test]
fn rows_without_an_author_do_not_ask_for_names() {
    let ranges = [
        range(1, 7, 1, 1, LoreFileAuditOutcome::Attributed),
        range(1, 8, 2, 2, LoreFileAuditOutcome::Attributed),
        range(1, 0, 3, 3, LoreFileAuditOutcome::Uncommitted),
    ];

    let asked = Cell::new(false);
    audit_rows(
        &ranges,
        |revision| (revision == Hash::from([7; 32])).then(AuditAttribution::default),
        |_ids| {
            asked.set(true);
            HashMap::new()
        },
    );

    assert!(!asked.get());
}

/// A row whose revision could not be read keeps its placeholders while the
/// rows around it resolve their authors.
#[test]
fn a_row_whose_revision_cannot_be_read_is_unaffected_by_name_resolution() {
    let attributed = LoreFileAuditOutcome::Attributed;
    let ranges = [range(1, 7, 1, 1, attributed), range(1, 8, 2, 2, attributed)];

    let rows = audit_rows(
        &ranges,
        |revision| (revision == Hash::from([7; 32])).then(|| attribution("known-id")),
        |_ids| HashMap::from([("known-id".to_string(), "ada".to_string())]),
    );

    assert_eq!(authors(&rows), vec!["ada", "unresolved"]);
    assert_eq!(
        rows[1],
        format!(
            "{:>12}  {}  {:<16}  {}",
            "2",
            Hash::from([8; 32]),
            "unresolved",
            "unresolved"
        )
    );
}

fn metadata_event(key: &str, value: LoreMetadata) -> LoreMetadataEventData {
    LoreMetadataEventData {
        key: key.into(),
        value,
    }
}

#[test]
fn an_attribution_takes_the_author_and_date_from_the_revision_s_metadata() {
    let reported = [
        metadata_event(metadata::MESSAGE, LoreMetadata::String("a message".into())),
        metadata_event(metadata::COMMITTED_BY, LoreMetadata::String("ada".into())),
        metadata_event(
            metadata::TIMESTAMP,
            LoreMetadata::Numeric(1_758_585_600_000),
        ),
    ];

    assert_eq!(
        audit_attribution(&reported),
        Some(AuditAttribution {
            author: "ada".to_string(),
            timestamp: 1_758_585_600_000,
        })
    );
}

/// A merge revision names who merged it, and the author is that merger,
/// whoever created or committed the work.
#[test]
fn the_merger_is_the_author_where_the_revision_records_one() {
    let reported = [
        metadata_event(metadata::CREATED_BY, LoreMetadata::String("creator".into())),
        metadata_event(
            metadata::COMMITTED_BY,
            LoreMetadata::String("committer".into()),
        ),
        metadata_event(metadata::MERGED_BY, LoreMetadata::String("merger".into())),
        metadata_event(metadata::TIMESTAMP, LoreMetadata::Numeric(1)),
    ];

    assert_eq!(
        audit_attribution(&reported).map(|a| a.author),
        Some("merger".to_string())
    );
}

/// A revision no merge created names no merger, and the author is the
/// committer, not the creator.
#[test]
fn the_committer_is_the_author_where_no_merger_is_recorded() {
    let reported = [
        metadata_event(metadata::CREATED_BY, LoreMetadata::String("creator".into())),
        metadata_event(
            metadata::COMMITTED_BY,
            LoreMetadata::String("committer".into()),
        ),
        metadata_event(metadata::TIMESTAMP, LoreMetadata::Numeric(1)),
    ];

    assert_eq!(
        audit_attribution(&reported).map(|a| a.author),
        Some("committer".to_string())
    );
}

/// An empty merger is no merger.
#[test]
fn an_empty_merger_falls_back_to_the_committer() {
    let reported = [
        metadata_event(
            metadata::COMMITTED_BY,
            LoreMetadata::String("committer".into()),
        ),
        metadata_event(
            metadata::MERGED_BY,
            LoreMetadata::String(String::new().into()),
        ),
        metadata_event(metadata::TIMESTAMP, LoreMetadata::Numeric(1)),
    ];

    assert_eq!(
        audit_attribution(&reported).map(|a| a.author),
        Some("committer".to_string())
    );
}

/// A revision that records metadata but no author is read, and prints `-`.
#[test]
fn metadata_without_an_author_is_a_read_attribution() {
    let reported = [metadata_event(
        metadata::TIMESTAMP,
        LoreMetadata::Numeric(1_758_585_600_000),
    )];

    assert_eq!(
        audit_attribution(&reported),
        Some(AuditAttribution {
            author: String::new(),
            timestamp: 1_758_585_600_000,
        })
    );
}

/// Every committed revision records a timestamp, so a reply with no
/// metadata is a revision whose metadata was not read.
#[test]
fn no_metadata_is_an_unread_attribution() {
    assert_eq!(audit_attribution(&[]), None);
}

#[test]
fn a_row_no_revision_holds_shows_its_outcome() {
    let ranges = [
        range(1, 0, 2, 2, LoreFileAuditOutcome::Uncommitted),
        range(1, 0, 3, 5, LoreFileAuditOutcome::BinaryInHistory),
    ];

    let rows = audit_rows(&ranges, |_revision| None, no_names);

    assert_eq!(
        rows,
        vec![
            format!("{:>12}  uncommitted", "2"),
            format!("{:>12}  binary-in-history", "3-5"),
        ]
    );
}

/// The two spellings of the same setting contradict each other, so clap
/// refuses them together rather than letting one silently win.
#[test]
fn the_two_spellings_of_the_setting_cannot_both_be_given() {
    use clap::Parser;
    let result = lore_client::cli::LoreCli::try_parse_from([
        "lore",
        "file",
        "audit",
        "file.txt",
        "--ignore-space-at-eol",
        "--no-ignore-space-at-eol",
    ]);

    assert!(result.is_err(), "expected the two flags to conflict");
}
