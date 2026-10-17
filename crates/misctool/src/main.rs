mod gh_pr_checks;

use std::process::ExitCode;

const HELP: &str = "Small, read-only tools for operator workflows.

Usage: cgwalters-forge-misctool <COMMAND>

Commands:
  gh-pr-checks OWNER/REPO#N   Show a pull request's checks using gh

Options:
  -h, --help     Print help
  -V, --version  Print version";

fn run() -> Result<u8, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--help" | "-h") if args.len() == 1 => {
            println!("{HELP}");
            Ok(0)
        }
        Some("--version" | "-V") if args.len() == 1 => {
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        Some("gh-pr-checks") => gh_pr_checks::run(&args[1..]),
        _ => Err(format!("expected a subcommand; use --help\n\n{HELP}")),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(2)
        }
    }
}
