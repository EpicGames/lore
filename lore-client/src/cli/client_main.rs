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

/// Sets the variables the token store reads, `LORE_AUTH_STORE` and `LORE_AUTH_PATH`, from the
/// global configuration's `[auth]` settings, leaving any the environment already sets to a value
/// that isn't blank.
///
/// # Safety
///
/// It sets environment variables, so no other thread may exist while it runs.
unsafe fn apply_auth_settings(settings: &lore::AuthSettings) {
    for (name, value) in auth_variables_to_set(settings, |name| std::env::var_os(name)) {
        // SAFETY: the caller guarantees that no other thread exists.
        unsafe { std::env::set_var(name, value) };
    }
}

/// The variables of `settings` to set, given the value `current` reads for each: those it reads as
/// unset or blank.
#[lore_macro::test_pub]
fn auth_variables_to_set(
    settings: &lore::AuthSettings,
    current: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Vec<(&'static str, std::ffi::OsString)> {
    settings
        .variables()
        .into_iter()
        .filter(|(name, _)| current(name).is_none_or(|set| set.is_empty()))
        .collect()
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

    match lore::load_auth_settings() {
        // SAFETY: no thread has been started yet.
        Ok(settings) => unsafe { apply_auth_settings(&settings) },
        Err(err) => {
            crate::eprintln!("Error: {err}");
            return ExitCode::FAILURE;
        }
    }

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

    // After the thread limit, which this reads, and before the first command: the
    // handlers below build the runtime themselves and then call the async API, so
    // by the time the library could decide this the runtime it would size exists.
    if runs_the_service(cli_command) {
        lore::size_threads_for_service();
    } else {
        // A process that relays its calls builds its runtime here, sized for relaying, and
        // sizing for its connections then finds it built.
        lore::size_threads_for_relaying();
        lore::size_threads_for_connections(globals.max_connections);
    }

    // This executable runs the service with `service run`, so a command only the service may
    // carry out, such as one in a virtual clone, can start one from it.
    lore::serve_from_this_executable();

    let result = handle_lore_commands(cli_command, globals);

    lore::shutdown();

    if lore::memory_stats_requested() {
        crate::eprintln!("{}", lore::memory_summary());
    }

    if cli.time {
        crate::println!("Executed in {:.2}s", time_start.elapsed().as_secs_f32());
    }

    return ExitCode::from(result);
}
