// Output goes through `output` so every message is prefixed and classified the
// same way; clippy rejects a stray `println!` in non-test code.
#![cfg_attr(not(test), deny(clippy::print_stdout, clippy::print_stderr))]
// Non-test code reports failure with `Result`; a panic is a bug, not an error path.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]
// Ignoring a `Result` must be a decision: handle it, or say why in an `#[allow]`.
#![cfg_attr(not(test), deny(clippy::let_underscore_must_use))]

mod bundle;
mod cli;
mod config;
mod container;
#[cfg(test)]
mod conventions;
mod deps;
mod devcontainer;
mod doctor;
mod elf;
mod features;
mod fingerprint;
mod forward;
mod inject;
mod output;
mod runtime;
mod secret;
mod shell;
mod ssh;
mod target;
mod terminfo;
mod tmpfile;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Command};
use std::path::PathBuf;
use target::Target;

/// The project to act on: the given path, or the current directory.
fn project_dir(path: Option<PathBuf>) -> Result<PathBuf> {
    match path {
        Some(p) => Ok(p),
        None => std::env::current_dir().context("finding the current directory"),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    output::set_verbosity(cli.verbose);
    if let Some(path) = cli.config {
        config::use_config_file(path)?;
    }
    let target = Target::new(cli.remote);

    let rt = runtime::connect(&target);

    match cli.command {
        Command::Up {
            path,
            build,
            no_attach,
            bundle,
        } => {
            let project_path = project_dir(path)?;
            // Ready the bundle first, so a problem with it fails before anything
            // is started.
            let prepared = bundle::source::prepare(&config::Config::load()?, bundle.as_deref())?;
            let started = devcontainer::start(&project_path, build, rt.as_ref())?;
            let container = started.container.as_ref();
            let remote_user = started.remote_user.as_deref();
            forward::spawn_daemon(container.id(), &target, &started.forward_ports);
            inject::lodge(container, remote_user, &prepared)?;
            if !no_attach {
                started.run_post_attach()?;
                let secret_setup = secret::deliver::deliver(
                    container,
                    remote_user,
                    &config::Config::load()?.secret,
                )?;
                container::enter(
                    container,
                    &started.session_name,
                    &started.workdir,
                    remote_user,
                    &secret_setup,
                )?;
            }
        }
        Command::Down { path } => {
            let project_path = project_dir(path)?;
            devcontainer::stop(&project_path, rt.as_ref())?;
        }
        Command::Status { path } => {
            let project_path = project_dir(path)?;
            devcontainer::status(&project_path, rt.as_ref())?;
        }
        Command::Rm { path } => {
            let project_path = project_dir(path)?;
            devcontainer::remove(&project_path, rt.as_ref())?;
        }
        Command::Exec {
            container,
            no_attach,
            bundle,
        } => {
            let prepared = bundle::source::prepare(&config::Config::load()?, bundle.as_deref())?;
            let handle = rt.container(&container);
            inject::lodge(handle.as_ref(), None, &prepared)?;
            if !no_attach {
                let session_name = format!("lodge-{container}");
                let secret_setup = secret::deliver::deliver(
                    handle.as_ref(),
                    None,
                    &config::Config::load()?.secret,
                )?;
                container::enter(handle.as_ref(), &session_name, "/", None, &secret_setup)?;
            }
        }
        Command::Pack { output } => {
            let config = config::Config::load()?;
            let output = output.unwrap_or_else(|| PathBuf::from(bundle::default_file_name()));
            let manifest = bundle::pack::pack_to_file(&config, &output)?;
            output::info!(
                "packed {} tool(s) and {} librar{} for {} into {} (id {})",
                manifest.tools.len(),
                manifest.libs.len(),
                if manifest.libs.len() == 1 { "y" } else { "ies" },
                manifest.arch,
                output.display(),
                manifest.id()
            );
        }
        Command::Bundle { command } => match command {
            cli::BundleCommand::Inspect { file } => bundle::inspect(&file)?,
            cli::BundleCommand::Verify { file } => bundle::verify_file(&file)?,
        },
        Command::Doctor => {
            if !doctor::run(rt.as_ref(), &target)? {
                std::process::exit(1);
            }
        }
        Command::Forward { container, port } => {
            let ports: Vec<_> = port
                .iter()
                .filter_map(|s| forward::PortForward::decode(s))
                .collect();
            forward::run_daemon(&container, &target, &ports);
        }
    }

    Ok(())
}
