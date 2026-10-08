---
lep: 2026-09-23-per-line-attribution-audit-api
title: Per-line attribution for files (lore file audit)
authors:
  - Raghav Narula
status: Draft
created: 2026-09-23
updated: 2026-10-06
discussion: <pending CR>
---

# Per-line attribution for files (`lore file audit`)

## Summary

Lore records history per file, not per line, so no command answers "which revision last changed this line". This proposal adds `lore file audit`, a read-only query that takes a path and returns, for each line, the revision that last changed it. The caller reads that revision's attribution metadata with `lore revision info`. The query walks the file's predecessor chain backwards and diffs consecutive versions, as `git blame` does. It lives in `lore-revision`, and the CLI and the C API share it.

## Motivation

A developer reading unfamiliar code needs to know who changed a line, to ask them about it or to find the change that introduced a defect. Lore cannot answer this.

The nearest commands stop at file granularity:

- `lore file history <path>` lists the revisions that changed a file (`lore-client/src/cli/commands/file.rs`), not the lines each one changed.
- `lore file diff --source <rev> --target <rev> <path>` needs the caller to know which two revisions to compare.

So the user diffs pairs of revisions by hand until the line appears, without knowing how many pairs that takes. `git blame` and `p4 annotate` answer directly.

## Goals / Non-Goals

### Goals

- Return, for each line of a file at a revision, the revision that last changed it.
- Serve the CLI, `lore-capi` editor integrations and CI from one implementation that a later server-side phase can reuse.
- Define a rule for every case the walk meets, and report why a range is unresolved where no rule decides.
- Bound the work per query.

### Non-Goals

- Auditing a whole repository or several paths at once.
- Detecting lines copied between files (`git blame -C`). The walk follows recorded moves only.
- Following a merge into the branch it merged. See *Merges*.
- Reproducing an answer after the comparison rules change.

## Proposed Design

### What you get

The caller names a file and, optionally, a revision. The query returns ranges of consecutive lines, each with the revision that last changed them. It returns ranges rather than one record per line, because code changes in blocks.

The C API has a blocking and an asynchronous entry point. Both send one `LORE_EVENT_FILE_AUDIT` event per range to the callback:

```c
int32_t lore_file_audit(const struct lore_global_args_t *globals,
                        const struct lore_file_audit_args_t *args,
                        struct lore_event_callback_config_t callback);

void lore_file_audit_async(const struct lore_global_args_t *globals,
                           const struct lore_file_audit_args_t *args,
                           struct lore_event_callback_config_t callback);

typedef struct lore_file_audit_args_t {
  struct lore_string_t path;
  struct lore_string_t revision;    // a signature, [branch]@<number>, [branch]@LATEST or <branch>@<hash>;
                                    // empty: the synced revision, accounting for the file on disk
  uint32_t max_revisions;           // 0: the default budget of 100
  uint8_t unlimited;                // walk the whole chain, ignoring max_revisions
  uint8_t no_ignore_space_at_eol;   // compare trailing whitespace literally
  uint8_t ignore_space_change;      // collapse runs of internal whitespace
} lore_file_audit_args_t;

typedef struct lore_file_audit_event_data_t {
  lore_repository_id_t repository;  // the repository the revision came from
  struct lore_hash_t revision;      // zero unless the outcome is ATTRIBUTED
  uint32_t line_first;              // 1-based
  uint32_t line_last;               // 1-based, inclusive
  enum lore_file_audit_outcome_t outcome;
} lore_file_audit_event_data_t;

typedef enum lore_file_audit_outcome_t {
  LORE_FILE_AUDIT_OUTCOME_ATTRIBUTED = 0,
  LORE_FILE_AUDIT_OUTCOME_UNCOMMITTED = 1,
  LORE_FILE_AUDIT_OUTCOME_BUDGET_EXHAUSTED = 2,
  LORE_FILE_AUDIT_OUTCOME_OBLITERATED = 3,
  LORE_FILE_AUDIT_OUTCOME_BINARY_IN_HISTORY = 4,
  LORE_FILE_AUDIT_OUTCOME_LINK_UNAVAILABLE = 5,
  LORE_FILE_AUDIT_OUTCOME_CONTENT_UNAVAILABLE = 6,
  LORE_FILE_AUDIT_OUTCOME_UNRESOLVED = 7,
} lore_file_audit_outcome_t;
```

A range carries the repository, the revision, the line span and an outcome. It carries no people. A revision records several (who created the work, who committed it, who reviewed it, who merged it) and the change request it came through (`lore-revision/src/metadata.rs:113-134`). The caller reads whichever it needs with `lore_revision_info`, once per distinct revision, so the audit API does not grow when a caller wants another key. The CLI summary shows the revision's `merged-by`, or its `committed-by` where it records no merger, resolved to a user name.

`UNCOMMITTED` marks lines that differ on disk from the synced revision. `ATTRIBUTED` names a revision. Every other outcome says why the walk could not attribute the lines.

The walk resolves the most recently changed blocks first, and the query sends each range as soon as the step that resolves it ends. Ranges therefore arrive most recently changed first, not in line order. Each range is complete: every line with one outcome is resolved in the same step, so no later range extends it.

### How it works

The query stores nothing ahead of time and computes each answer from scratch in one backward pass.

The walk starts with every line of the file at the requested revision unattributed. It compares that version with the previous one. The lines the comparison reports as changed belong to the current revision. The unchanged lines carry over to the previous version, and the walk repeats. It stops when no line is left unattributed, when it reaches the revision that added the file, or when the budget runs out.

Two properties of Lore keep the walk cheap:

- **Each file node records its predecessors** (`lore-revision/src/node.rs:1826-1844`), and `lore file history` already follows that chain (`lore-revision/src/file/history.rs:600-648`). The walk follows it too, so after its first step it reads only revisions that changed the file. On a sparse instance, where reading an old version can mean a network fetch, this matters.
- **The differ already computes changed regions with line numbers**, and discards them only when it renders patch text (`lore-revision/src/file/diff.rs:900-935`). The walk runs inside `lore-revision` and uses the regions directly. For the same reason the C API needs no new diff event: `lore_file_diff` returns rendered text (`lore-revision/src/file/diff.rs:117-118`).

The walk stops after 100 steps unless the caller changes the budget. The first step compares the requested revision, which need not have changed the file; every later step compares a revision that did. A file changed twice in a repository of ten thousand revisions costs at most three steps.

### Merges

A line a merge brought in is attributed to the merge revision. The walk follows first parents only and does not enter the merged branch. *Alternatives Considered* explains why.

### Where an answer is not available

The walk stops rather than guesses. When it cannot read a step, it reports every line still unattributed with the reason as its outcome. Lines it already attributed keep their outcome. It never attributes a line to a revision it did not verify.

| Outcome | Cause |
|---|---|
| `obliterated` | `lore file obliterate` removed an old payload. The address remains, the content does not. |
| `binary-in-history` | An older version holds binary content, which the walk cannot compare line by line. |
| `link-unavailable` | The file is in a linked repository that is unauthorized or, offline, unreachable. |
| `content-unavailable` | The content is not local, and `--offline` or an instance with no remote prevents fetching it. |
| `budget-exhausted` | The revision budget ran out. |

These outcomes describe the file or a configured state, and apply to the older versions the walk reads. An environment error, such as a lost connection or content this build cannot decompress, ends the query, and the completion event carries the error. If the error happens during the walk, the query first reports every line: attributed lines keep their outcome, and the lines the walk did not reach get `unresolved`. If it happens before the walk, for example while resolving the revision or reading the requested version, the query reports no ranges. The caller decides whether to show the ranges, the error, or both.

### Rules for the remaining cases

| Case | Rule |
|---|---|
| Path absent at the requested revision | `FileNotFound`; no walk runs. A deleted path counts as not found |
| Binary file | Error. Classification reuses `make_diff_content` (`lore-revision/src/file/diff.rs:1221-1227`) |
| Empty file | Zero ranges and a successful completion |
| Last line without a terminator | An ordinary line. Adding a terminator changes it |
| Uncommitted edits on disk | Reported as `uncommitted` with no revision, and excluded from the walk. Line numbers are those on disk, so an editor can align them |
| Revision named explicitly | The query ignores the working tree. The file on disk only describes the synced revision, so naming any revision, even the synced one, is a history query |
| No file on disk at the path | Not an edit. A view filter or a sparse instance may leave the path empty on disk, so the revision's lines attribute normally. An empty file on disk has no lines and yields no ranges |
| Recorded rename | Followed. The predecessor of a recorded move is the file node at its old path |
| Unrecorded move (delete plus add) | Not detected. The adding revision owns the added lines |
| Cherry-pick, revert, restore | Not followed. The cherry-pick revision owns the line; its metadata holds the origin hash |
| Path in a linked repository | Followed (`lore-revision/src/file/diff.rs:1255-1263`). Each range names its own repository |
| Content absent locally | Fetched through the existing read path (`lore-revision/src/file/diff.rs:1289-1296`). For an older version, `--offline` reports `content-unavailable` instead |
| Whitespace-only change | Trailing whitespace is ignored by default (`--no-ignore-space-at-eol` turns this off); reindentation counts as a change unless `--ignore-space-change` is given |
| Repeat query for the same revision | Recomputed. Version 1 caches nothing. A consumer can cache on the arguments it passed when it names an explicit revision |
| Comparison rules change in a later release | An answer can change, and the old rules are not kept. A consumer that caches across releases includes the Lore version in its key |

### Surfaces

- CLI: `lore file audit <path> [--revision <revision>]`, printing one row per range with the revision, author and date, or `--json` for scripts. The whitespace flags reuse the names `lore file diff` uses (`lore-client/src/cli/commands/file.rs:261-267`), because they control the same comparison (`lore-revision/src/file/diff.rs:124-132`); the audit adds `--no-ignore-space-at-eol` because it ignores trailing whitespace by default.
- C API: `lore_file_audit` and `lore_file_audit_async`, as above.

## Compatibility

- **Wire format** — The client-server protocol is unchanged. The local service protocol gains a `FileAudit` command and event. Both enums are serialized by variant index, so both new variants are appended at the end and existing variants keep their index. A later server-side phase will add an RPC and state its own compatibility.
- **Client/server protocols** — N/A. The walk uses existing reads.
- **On-disk format** — N/A. The query is read-only and stores nothing.
- **CLI and public API** — Additive at source level: a new subcommand, two functions, an argument struct, an event tag and its payload. No existing command, function or output changes. **The C ABI is not additive.** Event payloads sit by value in one tagged union in `lore-capi/lore.h`, so a new member can change its size, and the new tag extends `lore_event_id_t`. C consumers must recompile, and exhaustive `switch` statements need a new arm, as with every earlier event addition.

## Non-Functional Considerations

- **Concurrency** — The query only reads, takes no locks, and resolves against an immutable revision, so concurrent commits, syncs and merges cannot change an answer mid-walk. Working-tree mode reads the disk once.
- **Memory** — Proportional to the largest version of the file: the walk holds two versions at a time. The set of unattributed lines shrinks as the walk proceeds. Each range is sent as soon as its step ends.
- **State** — None. Version 1 caches and writes nothing.
- **Determinism** — Identical output for a fixed path, revision, whitespace settings, budget and Lore version. Two exceptions, both visible in the output: working-tree mode depends on the disk, and `--offline` reports lines that resolve once the content is available.
- **Latency** — Cost scales with the number of revisions in the file's predecessor chain that the walk visits, not with file or repository size. Each step reads two versions, each possibly a remote fetch, so on a client the network dominates. The budget bounds the cost.

## Migration Plan

N/A. Nothing breaks, so nothing migrates.

## Security Considerations

The trust model does not change. The query reads through the existing authorization path and exposes nothing that `lore file history`, `lore file diff` and `lore revision metadata get` do not already expose. Following a link uses the linked repository's authorization; an unauthorized link yields `link-unavailable` and no content or metadata from that repository.

The default budget bounds the cost of a long history. Unlimited walks are opt-in.

Obliterated content stays obliterated: the walk reports `obliterated` and does not infer the removed payload.

## Privacy Considerations

The query exposes no data Lore does not already store and serve. Identities live in revision metadata, readable today through `lore history` and `lore revision metadata get`; this proposal makes them convenient at line granularity.

Per-line attribution makes per-person statistics easy to produce, and these can be misused to evaluate individuals. The design neither adds nor restricts such use. Documentation must not present line counts as a measure of contribution, and the CLI shows no per-author totals.

## Risks and Assumptions

**Assumptions**

- **Assumption:** most files resolve within 100 file-changing revisions — *invalidated if:* measurement shows common files exceeding it.

**Risks**

- **Risk:** on a sparse instance the walk is network-bound and too slow for an editor — *mitigation:* streaming, the default budget, and caching by the consumer on an explicit revision.
- **Risk:** users read an unresolved range as a defect — *mitigation:* every unresolved range carries a specific outcome, and the CLI prints it.

## Drawbacks

- A caller that wants attribution makes a second call per distinct revision.
- Without a cache, an editor walks the chain again on every query.
- A move recorded as a delete and an add loses the attribution from before the move.
- Answers cannot be regenerated after the comparison rules change.
- Lines a merge brought in are attributed to the merge revision, not to the revisions on the merged branch.

## Alternatives Considered

### Compute on the server in version 1

The server holds content locally, so it avoids client fetches, and one cache could serve every user.

*Rejected because:* it does not work offline, needs a new server verb first, and delays every client behind a server release. The walk stays reusable, so a server phase can follow.

### Follow merges into the merged branch

Follow the merge's second parent for affected lines, and report the revision that wrote them.

*Rejected because:* the walk becomes a tree instead of a line. Each merge adds a second chain, and those chains contain further merges, so the work grows with the number of merges.

### One record per line

One record per line spares consumers from expanding ranges.

*Rejected because:* the record count would equal the line count, not the number of changes. `lore-revision/src/branch.rs` has 3,968 lines and 31 revisions that changed it, so per-line output sends 3,968 records where ranges send at most a few hundred. Expanding ranges is trivial for a consumer.

### Store a precomputed index

Store per-line provenance so a query becomes a lookup.

*Rejected because:* storage grows with lines times revisions, every commit invalidates part of it, and one stored answer cannot serve every whitespace setting. Git recomputes on every call for the same reasons.

### Repeat the revision's attribution on every range

Each range carries the people, the timestamp and any keys the caller names, so the consumer needs no second call.

*Rejected because:* the same metadata travels once per range instead of once per revision, and the audit API grows with every key a caller wants. `lore revision info` already reports it.

### A header event with the query's facts

One event before the ranges states the path, the revision, the whitespace settings and an algorithm version.

*Rejected because:* the caller already knows all of these; it passed them.

### Report a lost connection as an unresolved range with a reason

Mark the lines the walk could not fetch with a reason, as for content an offline instance lacks.

*Rejected because:* a lost connection describes the environment, not the file, and a reason would hide a failure the caller should see. The query ends with the error on the completion event, and the lines it did not reach get `unresolved`.

### Do nothing

*Rejected because:* users already perform this walk by hand with `lore file history` and repeated `lore file diff`.

## Prior Art

**Git.** `git blame` walks backwards and diffs, recomputing on every call with no stored index. It skips revisions whose blob is unchanged, which the predecessor chain gives Lore directly, and stops once every line has an owner. `git blame --first-parent` follows only the first parent at merges, "to determine when a line was introduced to a particular integration branch" (`git blame --help`); this design uses the same rule. Git leaves copy detection (`-C`) off by default because of its cost; this proposal excludes it.

**Perforce.** `p4 annotate` attributes lines to changelists and by default does not follow integrations into other branches (`-i` follows them), matching the first-parent rule here. `-u` adds user and date, which the CLI summary shows by default.

**Mercurial.** `hg annotate` follows copies and renames by default. Lore records moves explicitly instead of detecting them, so following them costs nothing and needs no flag.

## Unresolved Questions

- Must the server-side phase reproduce the client's output byte for byte, or only the same attributions? Byte for byte would pin both implementations to one differ version.
- Is 100 the right default budget? It equals the default `lore file history` uses (`lore-revision/src/file/history.rs:583-587`), but the two are set separately.
