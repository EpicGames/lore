use std::process::ExitCode;
use std::time::Instant;

use clap::CommandFactory;
use clap::Parser;

use crate::cli::LoreCli;
use crate::cli::LoreCommands;
use crate::cli::handle_lore_commands;
use crate::cli::lore_globals_from_args;
use crate::commands::service::ServiceCommands;
use crate::config::setup_config;
use crate::logging;

/// Whether this command is the service rather than a client of one.
///
/// The service does the work its callers relay to it, so its pools are sized for
/// working even on a machine whose clients relay. Sizing it for relaying would
/// leave the process that does all the work with the pools of one that does none.
#[lore_macro::test_pub]
fn runs_the_service(command: &LoreCommands) -> bool {
    matches!(
        command,
        LoreCommands::Service(service) if matches!(service.command, ServiceCommands::Run(_))
    )
}

pub fn client_main() -> ExitCode {
    #[cfg(target_family = "windows")]
    // safety: safe Win32 call; no invariants to uphold
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleOutputCP(
            windows_sys::Win32::Globalization::CP_UTF8,
        )
    };

    let time_start = Instant::now();

    let cli = LoreCli::parse();
    if cli.markdown_help {
        clap_markdown::print_help_markdown::<LoreCli>();
        return ExitCode::SUCCESS;
    }

    let Some(cli_command) = &cli.command else {
        let mut cmd = LoreCli::command();
        let _ = cmd.print_help();
        return ExitCode::from(2);
    };

    let log_config = match logging::log_config_from_args(&cli) {
        Ok(config) => config,
        Err(err) => {
            crate::eprintln!("Error: {err}");
            return ExitCode::FAILURE;
        }
    };

    setup_config(
        cli.json,
        log_config.level,
        cli.no_pager,
        cli.debug,
        cli.non_interactive,
    );

    lore::log::initialize();
    lore::log::configure(&log_config);

    if let Some(max_threads) = cli.max_threads {
        lore::set_thread_limit(max_threads);
    }

    // After the thread limit, which this reads, and before the first command: the
    // handlers below build the runtime themselves and then call the async API, so
    // by the time the library could decide this the runtime it would size exists.
    if !runs_the_service(cli_command) {
        lore::size_threads_for_relaying();
    }

    let mut globals = match lore_globals_from_args(&cli) {
        Ok(globals) => globals,
        Err(err) => {
            crate::eprintln!("Error: {err}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(err) = globals.validate() {
        crate::eprintln!("Error: {err}");
        return ExitCode::FAILURE;
    }

    let result = handle_lore_commands(cli_command, globals);

    lore::shutdown();

    if cli.time {
        crate::println!("Executed in {:.2}s", time_start.elapsed().as_secs_f32());
    }

    return ExitCode::from(result);
}
