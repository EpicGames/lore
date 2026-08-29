// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use clap::Args;
use clap::Subcommand;
use lore::interface::LoreEvent;
use lore::interface::LoreGlobalArgs;
use lore::interface::LoreLayerAddArgs;
use lore::interface::LoreLayerListArgs;
use lore::interface::LoreLayerRemoveArgs;
use lore::interface::LoreString;
use lore::layer;
use lore::runtime;

use crate::cli::EventCallbackExt;
use crate::cli::EventCallbackFn;
use crate::cli::output_formatter;
use crate::eprintln;
use crate::println;
use crate::progress_bar::ProgressBar;
use crate::styling::CommonStyles;
use crate::util;

#[derive(Args)]
pub struct LayerArgs {
    #[command(subcommand)]
    pub command: LayerCommands,
}

#[derive(Args)]
pub struct LayerAddArgs {
    /// Path in the current repository where the layer should be placed
    #[clap(value_name = "path")]
    target_path: String,

    /// Repository to add as a layer, either an ID or a name
    #[clap(value_name = "repository")]
    source_repository: String,

    /// Path in the layer repository where the layer should start
    #[clap(value_name = "path")]
    source_path: String,

    /// Metadata key to use for matching revisions
    #[clap(long, value_name = "metadata")]
    metadata: Option<String>,
}

#[derive(Args)]
pub struct LayerRemoveArgs {
    /// Path in the current repository where the layer is placed
    #[clap(value_name = "path")]
    target_path: String,

    /// Repository placed as a layer. Optional when the target path matches a
    /// single configured layer; required to disambiguate when multiple layers
    /// share the same target path.
    #[clap(value_name = "repository")]
    source_repository: Option<String>,

    /// Also delete untracked files and all directories inside the layer mount
    #[clap(long, action)]
    purge: bool,
}

#[derive(Subcommand)]
pub enum LayerCommands {
    /// Add a repository layer
    Add(LayerAddArgs),

    /// Remove a repository layer
    Remove(LayerRemoveArgs),

    /// List repository layers
    List,
}

pub fn handle_layer_add(globals: LoreGlobalArgs, args: &LayerAddArgs) -> u8 {
    let layer_args = LoreLayerAddArgs {
        target_path: LoreString::from(&args.target_path),
        source_repository: LoreString::from(&args.source_repository),
        source_path: LoreString::from(&args.source_path),
        metadata: LoreString::from(&args.metadata),
    };

    let start = std::time::Instant::now();
    let bar = ProgressBar::new_spinner("Cloning ...");

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::LayerAdd(data) => {
                println!(
                    "Clone layer in {}",
                    if data.target_path.is_empty() {
                        "/"
                    } else {
                        data.target_path.as_str()
                    }
                );
            }
            LoreEvent::RepositoryCloneBegin(data) => {
                println!(
                    "Cloning repository {} branch {} into {}",
                    data.repository, data.branch, data.path
                );
            }
            LoreEvent::RepositoryCloneProgress(data) => {
                crate::progress_bar::clone::apply_clone_progress(
                    data.count.file_count,
                    data.count.file_complete,
                    data.count.bytes_transferred,
                    data.count.bytes_total,
                    data.count.discovery_complete,
                    &bar,
                );
            }
            LoreEvent::RepositoryCloneEnd(data) => {
                println!(
                    "Cloned {}/{} files ({}/{})",
                    data.count.file_complete,
                    data.count.file_count,
                    crate::util::format_bytes_to_string(data.count.bytes_transferred),
                    crate::util::format_bytes_to_string(data.count.bytes_total),
                );
                println!(
                    "Layer clone complete in {:.2}s",
                    start.elapsed().as_secs_f32()
                );
            }
            LoreEvent::Complete(data) => {
                if data.status == 0 {
                    println!(
                        "{}Layer added successfully{}",
                        CommonStyles::SUCCESS,
                        anstyle::Reset
                    );
                } else {
                    eprintln!(
                        "{}Failed to add layer{}",
                        CommonStyles::FAILURE,
                        anstyle::Reset
                    );
                }
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(layer::layer_add(globals, layer_args, callback)) as u8;
}

pub fn handle_layer_remove(globals: LoreGlobalArgs, args: &LayerRemoveArgs) -> u8 {
    let layer_args = LoreLayerRemoveArgs {
        target_path: LoreString::from(&args.target_path),
        source_repository: LoreString::from(&args.source_repository),
        purge: args.purge.into(),
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::LayerRemove(data) => {
                let mut suffix = String::new();
                if data.forced != 0 && data.modified_count > 0 {
                    suffix.push_str(&format!(
                        ", {} locally modified discarded",
                        data.modified_count
                    ));
                }
                if data.purged != 0 {
                    suffix.push_str(", purged");
                }
                println!(
                    "Removed layer at {} ({} files, {} directories{})",
                    if data.target_path.is_empty() {
                        "/"
                    } else {
                        data.target_path.as_str()
                    },
                    data.file_count,
                    data.directory_count,
                    suffix
                );
            }
            LoreEvent::Complete(data) => {
                if data.status == 0 {
                    println!(
                        "{}Layer removed successfully{}",
                        CommonStyles::SUCCESS,
                        anstyle::Reset
                    );
                } else {
                    eprintln!(
                        "{}Failed to remove layer{}",
                        CommonStyles::FAILURE,
                        anstyle::Reset
                    );
                }
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    return runtime().block_on(layer::layer_remove(globals, layer_args, callback)) as u8;
}

pub fn handle_layer_list(globals: LoreGlobalArgs) -> u8 {
    let layer_args = LoreLayerListArgs {};

    let have_layers = Arc::new(AtomicBool::new(false));
    let have_layers_flag = have_layers.clone();
    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| {
            if let LoreEvent::LayerEntry(data) = event {
                if !have_layers_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    println!(
                        "{}Repository                       Revision                                                         Paths{}",
                        CommonStyles::HEADERS,
                        anstyle::Reset
                    );
                }
                println!(
                    "{} {} {} -> {}",
                    data.source_repository,
                    data.revision,
                    if data.source_path.is_empty() {
                        "/"
                    } else {
                        data.source_path.as_str()
                    },
                    if data.target_path.is_empty() {
                        "/"
                    } else {
                        data.target_path.as_str()
                    }
                );
                have_layers_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    let status = runtime().block_on(layer::layer_list(globals, layer_args, callback)) as u8;

    if status == 0 && !have_layers.load(std::sync::atomic::Ordering::Relaxed) {
        println!("No layers");
    }

    status
}

pub fn handle_layer_commands(cmd: &LayerCommands, globals: LoreGlobalArgs) -> u8 {
    // Layer action
    match cmd {
        LayerCommands::Add(args) => {
            return handle_layer_add(globals, args);
        }
        LayerCommands::Remove(args) => {
            return handle_layer_remove(globals, args);
        }
        LayerCommands::List => {
            return handle_layer_list(globals);
        }
    }
}
