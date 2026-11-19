// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore-desktop [INSTANCE_PATH]`: opens the desktop client, on the instance at the path if given.

use std::process::ExitCode;

use lore_desktop::app::LoreDesktop;

fn main() -> ExitCode {
    match lore::load_auth_settings() {
        Ok(settings) => {
            for (name, value) in settings.variables() {
                if std::env::var_os(name).is_none_or(|set| set.is_empty()) {
                    // SAFETY: no other thread exists yet.
                    unsafe { std::env::set_var(name, value) };
                }
            }
        }
        Err(err) => {
            eprintln!("Error: {err}");
            return ExitCode::FAILURE;
        }
    }

    // This process relays its calls to the service when one is in use, as the CLI does.
    lore::size_threads_for_relaying();

    let instance_path = std::env::args().nth(1);
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Lore")
            .with_inner_size([960.0, 640.0]),
        ..Default::default()
    };
    let result = eframe::run_native(
        "Lore",
        options,
        Box::new(|_| Ok(Box::new(LoreDesktop::new(instance_path)))),
    );

    lore::shutdown();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err}");
            ExitCode::FAILURE
        }
    }
}
