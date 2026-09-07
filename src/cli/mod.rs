mod args;
mod dispatch;
mod output;
mod terminal;

use args::{Cli, Command};
use clap::{CommandFactory, Parser};
use codex_vault::commands::BatchOptions;
use codex_vault::parallel::ProgressMode;
use std::io::{self, IsTerminal};
use std::process::ExitCode;

pub(super) fn run() -> ExitCode {
    let cli = Cli::parse();
    if let Some(Command::Mcp { cwd }) = &cli.command {
        return match codex_vault::mcp::serve(
            io::stdin().lock(),
            io::stdout().lock(),
            cwd.as_deref().map(std::path::Path::new),
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("{err}");
                ExitCode::from(err.exit_code())
            }
        };
    }

    let menu_cwd = match &cli.command {
        Some(Command::Menu { cwd }) => Some(cwd.clone()),
        None if io::stdin().is_terminal() && io::stdout().is_terminal() => Some(None),
        None => {
            let _ = Cli::command().print_help();
            return ExitCode::from(2);
        }
        _ => None,
    };
    if let Some(cwd) = menu_cwd {
        if cli.json {
            eprintln!("The menu is interactive. Use a direct command with --json.");
            return ExitCode::from(2);
        }
        return match terminal::menu(cwd) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("{err}");
                ExitCode::from(err.exit_code())
            }
        };
    }

    let compact = cli.json;
    let human = cli.human || (!cli.json && io::stdout().is_terminal());
    let scan_display = match &cli.command {
        Some(Command::Scan { all, paths, .. }) => Some((*all, *paths)),
        _ => None,
    };
    let batch = BatchOptions {
        jobs: cli.jobs,
        progress: ProgressMode::from_flags(cli.progress, cli.no_progress),
    };
    match dispatch::run_command(cli.command.expect("handled missing command"), batch) {
        Ok(value) => {
            if human {
                if let Some((all, paths)) = scan_display {
                    terminal::render_scan(&value, all, paths);
                } else {
                    terminal::render(&value);
                }
            } else {
                output::print_json(&value, &mut io::stdout().lock(), compact);
            }
            ExitCode::from(codex_vault::commands::output_exit_code(&value))
        }
        Err(err) => {
            if human {
                eprintln!("Error [{}]: {err}", err.code());
            } else {
                output::print_json(&err.to_json(), &mut io::stderr().lock(), compact);
            }
            ExitCode::from(err.exit_code())
        }
    }
}
