//! Command-line entry point. Presentation and dispatch live in the private cli module.

mod cli;

fn main() -> std::process::ExitCode {
    cli::run()
}
