//! The commands: each turns parsed arguments into calls on the modules that do
//! the work. This is the only layer that loads the config (once per command) and
//! decides what to print.
//!
//! - [`attach`]: `up` and `exec`, which end in an interactive session
//! - [`project`]: `down`, `status`, `rm`, `ls`, `ports`, `logs`, `clean`
//! - [`doctor`]: the `doctor` command
//! - [`init`]: the `init` command
//! - [`bundle`]: `pack`, `bundle`, `cache`
//! - [`prompt`]: the questions `up` asks on the terminal

mod attach;
mod bundle;
mod doctor;
mod init;
mod project;
mod prompt;

use anyhow::{Context, Result};
use clap::CommandFactory;
use std::path::PathBuf;
use std::process::ExitCode;

use crate::cli::{Cli, Command};
use crate::config::Config;
use crate::devcontainer::{Recreate, StartOpts};
use crate::runtime::Runtime;
use crate::target::Target;

/// What every command works with: where the engine is and where the config is.
pub struct Ctx {
    pub target: Target,
    pub rt: Box<dyn Runtime>,
    pub config_path: PathBuf,
}

impl Ctx {
    /// Loads and validates the config. Commands call this once.
    pub fn config(&self) -> Result<Config> {
        Config::load(&self.config_path)
    }

    /// The project folder: `path` or the current directory, made absolute and
    /// canonical on the project's host, so `ldg up`, `ldg up .` and
    /// `ldg up $PWD` (or `/p` and `/p/` with `--remote`) all name the same
    /// container.
    pub fn project(&self, path: Option<PathBuf>) -> Result<PathBuf> {
        let path = match path {
            Some(p) => p,
            None if self.target.ssh_host().is_some() => {
                anyhow::bail!("with --remote, give the project's path on the remote host")
            }
            None => std::env::current_dir().context("finding the current directory")?,
        };
        self.target.canonical_dir(&path)
    }
}

pub fn run(cli: Cli) -> Result<ExitCode> {
    let target = Target::new(cli.remote);
    let ctx = Ctx {
        rt: crate::runtime::connect(&target),
        target,
        config_path: crate::config::path_for(cli.config)?,
    };

    match cli.command {
        Command::Up {
            project,
            build,
            yes,
            no_recreate,
            no_attach,
            bundle,
        } => {
            let recreate = if yes {
                Recreate::Yes
            } else if no_recreate {
                Recreate::No
            } else {
                Recreate::Ask
            };
            attach::up(
                &ctx,
                project.path,
                StartOpts { build, recreate },
                no_attach,
                bundle.as_deref(),
            )?;
        }
        Command::Exec {
            container,
            user,
            workdir,
            no_attach,
            bundle,
        } => attach::exec(
            &ctx,
            &container,
            user.as_deref(),
            &workdir,
            no_attach,
            bundle.as_deref(),
        )?,
        Command::Down { project } => project::down(&ctx, project.path)?,
        Command::Status { project, json } => project::status(&ctx, project.path, json)?,
        Command::Rm { project } => project::rm(&ctx, project.path)?,
        Command::Ls { json } => project::ls(&ctx, json)?,
        Command::Ports { project, json } => project::ports(&ctx, project.path, json)?,
        Command::Logs { project, follow } => project::logs(&ctx, project.path, follow)?,
        Command::Clean { project, container } => {
            project::clean(&ctx, project.path, container.as_deref())?
        }
        Command::Pack { output } => bundle::pack(&ctx, output)?,
        Command::Bundle { command } => bundle::bundle(command)?,
        Command::Cache { command } => bundle::cache(command)?,
        Command::Init { force, print } => init::run(&ctx, force, print)?,
        Command::Doctor { project, json } => {
            if !doctor::run(&ctx, project.path, json)? {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "ldg", &mut std::io::stdout());
        }
        Command::Man => {
            clap_mangen::Man::new(Cli::command())
                .render(&mut std::io::stdout())
                .context("writing the man page")?;
        }
        Command::Forward { container, port } => {
            crate::forward::run_daemon(&container, &ctx.target, &port);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Prints `value` as pretty JSON on stdout, for `--json`.
fn print_json(value: &serde_json::Value) -> Result<()> {
    crate::output::line!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// "3 s", "5 min", "2 h", "4 d": how long ago, roughly.
fn ago(time: std::time::SystemTime) -> String {
    let secs = time.elapsed().map_or(0, |d| d.as_secs());
    match secs {
        0..60 => format!("{secs} s"),
        60..3600 => format!("{} min", secs / 60),
        3600..86400 => format!("{} h", secs / 3600),
        _ => format!("{} d", secs / 86400),
    }
}

/// "812 B", "4.2 MB": a size for people.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["kB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64 / 1000.0;
    let mut unit = 0;
    while size >= 1000.0 && unit < UNITS.len() - 1 {
        size /= 1000.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(human_size(812), "812 B");
        assert_eq!(human_size(4_200_000), "4.2 MB");
        assert_eq!(human_size(1_500_000_000), "1.5 GB");
    }

    #[test]
    fn the_cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }
}
