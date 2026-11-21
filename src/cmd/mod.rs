//! The commands: each turns parsed arguments into calls on the modules that do
//! the work. This is the only layer that loads the config (once per command) and
//! decides what to print.
//!
//! - [`attach`]: `up`, which ends in an interactive session
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
use std::sync::Arc;

use crate::cli::{Cli, Command};
use crate::config::Config;
use crate::forward::{DetachedDaemon, ForwardDaemon};
use crate::host::{HostRef, LocalHost, SshHost};
use crate::runtime::docker::DockerCli;
use crate::runtime::{Engine, Runtime};
use crate::tools::{Ldd, Oras, Patchelf, Toolchain};

/// Which engine this run drives: `--engine`, else `engine` in the config file,
/// else docker. A command that never touches an engine (`init`, the generated
/// completions and man page) does not read the config for it, so a bad value
/// can't stop `ldg init --force` from fixing the file.
fn choose_engine(
    flag: Option<Engine>,
    command: &Command,
    config: &std::path::Path,
) -> Result<Engine> {
    if let Some(engine) = flag {
        return Ok(engine);
    }
    if matches!(
        command,
        Command::Init { .. } | Command::Completions { .. } | Command::Man
    ) {
        return Ok(Engine::default());
    }
    Ok(crate::config::engine_setting(config)?.unwrap_or_default())
}

/// Builds the engine `kind` on `host`. With the host, the only place that
/// chooses an implementation of the interface everything else takes.
fn build_engine(kind: Engine, host: HostRef) -> Box<dyn Runtime> {
    match kind {
        Engine::Docker => Box::new(DockerCli::new(host)),
    }
}

/// What every command works with: the host the project is on, the engine on it,
/// the helper tools and where the config is. Built once, in [`run`], which is the only place that
/// chooses an implementation of either; everything below takes the interfaces.
pub struct Ctx {
    pub host: HostRef,
    pub rt: Box<dyn Runtime>,
    /// The helper programs, for packing a bundle and fetching features.
    pub tools: Toolchain,
    /// Starts port forwarding for a container once it is up.
    pub forwarder: Box<dyn ForwardDaemon>,
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
            None if !self.host.shares_cwd() => {
                anyhow::bail!("with --remote, give the project's path on the remote host")
            }
            None => std::env::current_dir().context("finding the current directory")?,
        };
        self.host.canonical_dir(&path)
    }
}

pub fn run(cli: Cli) -> Result<ExitCode> {
    // The composition root: the one place that picks how the project's machine
    // is reached and which engine runs on it. Each is an interface everything
    // else takes; a new way of reaching a machine or a new engine is one more arm.
    // The helper tools (`ldd`, `patchelf`, `oras`) are chosen here too.
    let host: HostRef = match cli.remote {
        Some(remote) => Arc::new(SshHost::new(remote)),
        None => Arc::new(LocalHost),
    };
    // Open the connection while the terminal is ours, so a password or 2FA prompt
    // is answered once and everything after reuses it. The forward daemon has no
    // terminal: it joins the connection that is already there.
    if !matches!(cli.command, Command::Forward { .. }) {
        host.connect()?;
    }
    let config_path = crate::config::path_for(cli.config)?;
    let engine = choose_engine(cli.engine, &cli.command, &config_path)?;
    let rt = build_engine(engine, host.clone());
    let ctx = Ctx {
        forwarder: Box::new(DetachedDaemon::new(host.as_ref(), rt.as_ref())),
        rt,
        tools: Toolchain {
            deps: Box::new(Ldd),
            elf: Box::new(Patchelf),
            oci: Box::new(Oras),
        },
        host,
        config_path,
    };

    match cli.command {
        Command::Up {
            project,
            build,
            reinject,
            no_attach,
            bundle,
        } => attach::up(
            &ctx,
            project.path,
            build,
            reinject,
            no_attach,
            bundle.as_deref(),
        )?,
        Command::Down { project } => project::down(&ctx, project.path)?,
        Command::Status { project, json } => project::status(&ctx, project.path, json)?,
        Command::Rm { project } => project::rm(&ctx, project.path)?,
        Command::Ls { json } => project::ls(&ctx, json)?,
        Command::Ports { project, json } => project::ports(&ctx, project.path, json)?,
        Command::Logs { project, follow } => project::logs(&ctx, project.path, follow)?,
        Command::Clean { project } => project::clean(&ctx, project.path)?,
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
            let forwarders = crate::forward::HostForwarders::new(ctx.host.as_ref());
            crate::forward::run_daemon(ctx.rt.as_ref(), &forwarders, &container, &port);
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
    fn the_engine_flag_is_optional_and_checked() {
        use clap::Parser as _;
        let engine = |args: &[&str]| Cli::try_parse_from(args).map(|c| c.engine);
        assert_eq!(engine(&["ldg", "ls"]).unwrap(), None);
        assert_eq!(
            engine(&["ldg", "--engine", "docker", "ls"]).unwrap(),
            Some(Engine::Docker)
        );
        assert!(engine(&["ldg", "--engine", "nonesuch", "ls"]).is_err());
    }

    fn config_with(text: &str) -> (crate::tmpfile::TempDir, std::path::PathBuf) {
        let dir = crate::tmpfile::TempDir::new("engine-config").unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    #[test]
    fn the_engine_comes_from_the_flag_then_the_config_then_docker() {
        use clap::Parser as _;
        let ls = Cli::try_parse_from(["ldg", "ls"]).unwrap().command;
        let (_dir, with) = config_with("engine = \"docker\"\n");
        let (_dir2, without) = config_with("bundle = \"b.tar\"\n");
        let missing = with.with_file_name("nope.toml");
        for path in [&with, &without, &missing] {
            assert_eq!(choose_engine(None, &ls, path).unwrap(), Engine::Docker);
        }
        // A flag decides before the config is read, so a bad config value does
        // not matter then; without the flag it is an error naming the key.
        let (_dir3, bad) = config_with("engine = \"nonesuch\"\n");
        assert_eq!(
            choose_engine(Some(Engine::Docker), &ls, &bad).unwrap(),
            Engine::Docker
        );
        let err = choose_engine(None, &ls, &bad).unwrap_err();
        assert!(format!("{err:#}").contains("invalid `engine`"), "{err:#}");
        // Commands that never use an engine don't read it, so `init` can fix it.
        let init = Cli::try_parse_from(["ldg", "init", "--force"])
            .unwrap()
            .command;
        assert_eq!(choose_engine(None, &init, &bad).unwrap(), Engine::Docker);
    }

    #[test]
    fn a_child_lodge_is_pointed_at_the_same_engine_and_host() {
        use crate::host::SshHost;
        use clap::Parser as _;
        let host: HostRef = Arc::new(SshHost::new("me@h:22"));
        let engine = build_engine(Engine::Docker, host.clone());
        let mut args = vec!["ldg".to_string()];
        args.extend(host.respawn_args());
        args.extend(engine.respawn_args());
        args.push("ls".into());
        let cli = Cli::try_parse_from(args).unwrap();
        assert_eq!(cli.remote.as_deref(), Some("me@h:22"));
        assert_eq!(cli.engine, Some(Engine::Docker));
    }

    #[test]
    fn the_cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }
}
