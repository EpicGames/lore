---
status: accepted
date: 2026-11-19
---

# ADR-00042: egui as the desktop client's GUI toolkit

## Context and Problem Statement

`lore-desktop` ([LEP](../../proposals/2026-11-19-open-source-rust-desktop-client.md)) needs a GUI
toolkit. The choice shapes every view, so it is hard to reverse. The client must stay responsive on
status lists and histories with very many rows, build with the workspace's Rust toolchain alone, and
run on Windows, macOS and Linux.

## Decision Drivers

- Lists of any length draw at a steady cost.
- Pure Rust: no JavaScript runtime, webview or second language.
- Licences that pass `deny.toml`.
- Little code between a library event and a drawn row.

## Considered Options

- egui with eframe
- Iced
- Slint

## Decision Outcome

Chosen option: "egui with eframe", because its tables draw only the rows in view
(`egui_extras::TableBuilder::body().rows()`), its immediate mode lets a view read the model the
library's events fill with no separate binding layer, and it is MIT or Apache-2.0 licensed.

### Consequences

- Good, because a view is a function of the model, so a library event needs only a model update and
  a repaint request.
- Good, because eframe runs on all three platforms with its default renderer.
- Bad, because egui's look is its own rather than native, and a polished theme is work the client
  does itself.
- Bad, because egui's API changes between minor releases (0.37 replaced `App::update` with
  `App::ui`), so upgrades need code changes.

## Pros and Cons of the Options

### Iced

- Good, because its Elm-style update loop suits message-driven state.
- Bad, because large lists need a lazy widget the client would assemble itself.

### Slint

- Good, because its declarative markup separates layout from logic.
- Bad, because its licence is GPL-3.0, royalty-free or commercial, and needs checking against
  `deny.toml`; and markup adds a second language.
