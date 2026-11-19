# Plan: Rust desktop client (`lore-desktop`)

Design and rationale: `docs/proposals/2026-11-19-open-source-rust-desktop-client.md`.
Toolkit: egui/eframe, recorded in `docs/developing/decisions/00042-desktop-client-toolkit-egui.md`.
This fork is for personal use: no upstream review, sign-off or LEP acceptance gates.

## Phase 0 — Proposal
- [x] Draft the LEP.
- [ ] Fill its remaining `{TODO}`s.

## Phase 1 — Skeleton
- [x] `lore-desktop` crate in the workspace; depends on `lore` only (plus egui).
- [x] Toolkit ADR: egui.
- [x] Plumbing: each call on its own thread via `run_command`, events copied into messages on a
      channel, UI repaints on each; errors from the `Complete` event shown in the window.
- [x] Read-only status view: refresh / scan, branch and remote state, virtualised file tables.
- [ ] Measure frame time on a large instance.
- [ ] `cargo deny` passes with egui's dependency tree.

## Phase 2 — Daily loop
- [ ] Open/close instances (tabs), remembered in per-user config.
- [ ] Changes view: status (virtualised), stage/unstage, revert, commit with message.
- [ ] Sync and push, with progress and cancel.
- [ ] Accounts: login/logout via `lore::auth`.

## Phase 3 — History and branches
- [ ] History view on `lore::revision_graph` / `lore::log`; revision details.
- [ ] Diff view: text side-by-side, binary as metadata; size cap.
- [ ] Branches: list, create, switch; merge with conflict list and resolve mine/theirs.

## Phase 4 — Collaboration
- [ ] Locks view: holders, lock/unlock.
- [ ] Live updates through `lore::notification`; refresh-on-focus fallback.

## Phase 5 — Ship
- [ ] Packaging per platform; CI build of the client.
- [ ] Docs: how-to page and tutorial; release note in `docs/release-notes/`.

## Each change
Follow the repo's code standards: tests in `tests/unit/`, `cargo clippy --all-targets -- -D warnings --no-deps`.

Run: `cargo run --release -p lore-desktop -- <instance path>`
