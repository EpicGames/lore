// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(clippy::needless_return)]
mod cli;
mod client_main;
mod commands;
mod config;
mod logging;
mod pager;
mod print_macros;
mod progress_bar;
mod stats_display;
mod styling;
mod terminal_size;
mod util;

pub use client_main::client_main;
