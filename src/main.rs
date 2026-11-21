// Output goes through `output` so every message is prefixed and classified the
// same way; clippy rejects a stray `println!` in non-test code.
#![cfg_attr(not(test), deny(clippy::print_stdout, clippy::print_stderr))]
// Non-test code reports failure with `Result`; a panic is a bug, not an error path.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]
// Ignoring a `Result` must be a decision: handle it, or say why in an `#[allow]`.
#![cfg_attr(not(test), deny(clippy::let_underscore_must_use))]

mod bundle;
mod cli;
mod cmd;
mod config;
#[cfg(test)]
mod conventions;
mod devcontainer;
#[cfg(test)]
mod doc_facts;
mod fingerprint;
mod forward;
mod host;
mod host_tools;
mod inject;
mod layout;
mod output;
mod port;
mod provision;
mod runtime;
mod secret;
mod session;
mod shell;
mod tmpfile;
mod tools;
mod tree;

use clap::Parser;
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    output::set_verbosity(cli.verbose);
    match cmd::run(cli) {
        Ok(code) => code,
        Err(e) => {
            output::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}
