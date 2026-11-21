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
//! - [`targets`]: the list of targets, the one place that says which there are

mod attach;
mod bundle;
mod doctor;
mod init;
mod project;
mod prompt;
pub(crate) mod targets;

use anyhow::{Context, Result};
use clap::{CommandFactory, FromArgMatches};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use crate::bundle::store::{FileSink, FileStore, OciSink, OciStore, Sinks, Stores};
use crate::cli::{Cli, Command};
use crate::config::Config;
use crate::forward::{DetachedDaemon, ForwardDaemon};
use crate::host::{HostRef, LocalHost, SshHost};
use crate::local::{Local, SystemLocal};
use crate::provision::{Endpoints, Wiring};
use crate::tools::{
    DockerConfig, Ldd, NativeOci, OciFetcher, OciPusher, Patchelf, Toolchain, UreqHttp,
};

/// Whether a command works in a target at all. `init` and the generated
/// completions and man page do not, so a bad value in the config can't stop
/// `ldg init --force` from fixing the file.
fn uses_target(command: &Command) -> bool {
    !matches!(
        command,
        Command::Init { .. } | Command::Completions { .. } | Command::Man
    )
}

/// Which target this run works in: `--target`, else `target` in the config file,
/// else the default. A command that never touches one (`init`, the generated
/// completions and man page) does not read the config for it, so a bad value
/// can't stop `ldg init --force` from fixing the file.
fn choose_target(
    flag: Option<String>,
    command: &Command,
    config: &std::path::Path,
) -> Result<&'static str> {
    if let Some(target) = flag {
        return targets::check(&target);
    }
    if !uses_target(command) {
        return Ok(targets::default_name());
    }
    match crate::config::target_setting(config)? {
        Some(target) => targets::check(&target)
            .with_context(|| format!("invalid `target` in {}", config.display())),
        None => Ok(targets::default_name()),
    }
}

/// Builds the endpoints of `target`, from the settings the config gives it. A
/// command that never uses a target builds none that matters: the default, with no
/// settings, so a bad table can't stop `ldg init --force` from fixing the file.
fn build_endpoints(
    target: &str,
    config: &std::path::Path,
    with: Wiring,
    command: &Command,
) -> Result<Box<dyn Endpoints>> {
    let kind = targets::find(target).with_context(|| format!("no target `{target}`"))?;
    let settings = if uses_target(command) {
        crate::config::target_settings(config, target)?
    } else {
        toml::Table::new()
    };
    kind.build(with, &settings)
        .with_context(|| format!("setting up the {target} target (from {})", config.display()))
}

/// The command line, with the targets this build knows as the values `--target`
/// takes (so a typo is caught by the parser, and completions offer them).
pub fn command() -> clap::Command {
    use clap::builder::{PossibleValue, PossibleValuesParser};
    let values: Vec<PossibleValue> = targets::all()
        .iter()
        .map(|t| PossibleValue::new(t.name()).help(t.about()))
        .collect();
    Cli::command().mut_arg("target", |arg| {
        arg.value_parser(PossibleValuesParser::new(values))
    })
}

/// Parses the process's arguments; exits with the usage message when they are wrong.
pub fn parse() -> Cli {
    let matches = command().get_matches();
    Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
}

/// The arguments that make a child lodge (the forward daemon is one) reach the
/// same host and work in the same target as this one, in front of its subcommand.
/// The command-line surface is the command layer's to know, not the host's or the
/// target's.
fn respawn_args(remote: Option<&str>, target: &str) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(remote) = remote {
        args.extend(["--remote".to_string(), remote.to_string()]);
    }
    args.extend(["--target".to_string(), target.to_string()]);
    args
}

/// What every command works with: the host the project is on, the endpoints on it,
/// the helper tools and where the config is. Built once, in [`run`], which is the only place that
/// chooses an implementation of either; everything below takes the interfaces.
pub struct Ctx {
    pub host: HostRef,
    /// The machine lodge runs on: its environment, and a shell for secrets.
    pub local: Arc<dyn Local>,
    /// Where the project's work happens and how to get there: a devcontainer on
    /// an engine, or a plain host.
    pub endpoints: Box<dyn Endpoints>,
    /// The helper programs, for packing a bundle and fetching features.
    pub tools: Toolchain,
    /// Where a bundle can be read from, chosen by how the reference is written.
    pub bundles: Stores,
    /// Where a bundle can be written to, chosen the same way.
    pub sinks: Sinks,
    /// Starts port forwarding for a container once it is up.
    pub forwarder: Box<dyn ForwardDaemon>,
    pub config_path: PathBuf,
}

impl Ctx {
    /// Loads and validates the config. Commands call this once.
    pub fn config(&self) -> Result<Config> {
        let config = Config::load(&self.config_path)?;
        // A table for a target that does not exist is a typo, and nothing reads it.
        for name in config.targets.keys() {
            targets::check(name)
                .with_context(|| format!("`[targets.{name}]` in {}", self.config_path.display()))?;
        }
        Ok(config)
    }

    /// The project folder: `path` or the current directory, made absolute and
    /// canonical on the project's host, so `ldg up`, `ldg up .` and
    /// `ldg up $PWD` (or `/p` and `/p/` with `--remote`) all name the same
    /// container.
    pub fn project(&self, path: Option<PathBuf>) -> Result<PathBuf> {
        let path = match path {
            Some(p) => p,
            None if !self.host.shares_cwd() && self.endpoints.needs_project_path() => {
                anyhow::bail!("with --remote, give the project's path on the remote host")
            }
            // A plain host needs no project: work where the host starts (its login
            // directory over ssh).
            None if !self.host.shares_cwd() => PathBuf::from("."),
            None => std::env::current_dir().context("finding the current directory")?,
        };
        self.host.canonical_dir(&path)
    }
}

/// Where a bundle can come from. A new place is one more entry here.
fn bundle_stores(oci: Arc<dyn OciFetcher>) -> Stores {
    Stores::new(vec![Box::new(FileStore), Box::new(OciStore::new(oci))])
}

/// Where a bundle can be written. A new place is one more entry here.
fn bundle_sinks(oci: Arc<dyn OciPusher>) -> Sinks {
    Sinks::new(vec![Box::new(FileSink), Box::new(OciSink::new(oci))])
}

pub fn run(cli: Cli) -> Result<ExitCode> {
    // The composition root: the one place that picks how the project's machine
    // is reached and which target is worked in on it. Each is an interface everything
    // else takes; a new way of reaching a machine or a new target is one more arm.
    // The helper tools (`ldd`, `patchelf`, the registry client) are chosen here too.
    let remote = cli.remote.clone();
    // The forward daemon has no terminal, so its ssh commands must never prompt.
    let daemon = matches!(cli.command, Command::Forward { .. });
    let host: HostRef = match cli.remote {
        Some(remote) if daemon => Arc::new(SshHost::unattended(remote)),
        Some(remote) => Arc::new(SshHost::new(remote)),
        None => Arc::new(LocalHost),
    };
    // Open the connection while the terminal is ours, so a password or 2FA prompt
    // is answered once and everything after reuses it. The forward daemon has no
    // terminal: it joins the connection that is already there.
    if !daemon {
        host.connect()?;
    }
    let config_path = crate::config::path_for(cli.config)?;
    let target = choose_target(cli.target, &cli.command, &config_path)?;
    let local: Arc<dyn Local> = Arc::new(SystemLocal);
    // One registry client, handed to what reads from a registry and to what
    // writes to one: they are separate interfaces and the same implementation.
    let registry = Arc::new(NativeOci::new(
        Box::new(UreqHttp::new()),
        Box::new(DockerConfig::new(local.clone())),
    ));
    let tools = Toolchain {
        deps: Box::new(Ldd),
        elf: Box::new(Patchelf),
        oci: registry.clone(),
    };
    let endpoints = build_endpoints(
        target,
        &config_path,
        Wiring {
            host: host.clone(),
            local: local.clone(),
            oci: tools.oci.clone(),
        },
        &cli.command,
    )?;
    let ctx = Ctx {
        forwarder: Box::new(DetachedDaemon::new(respawn_args(remote.as_deref(), target))),
        endpoints,
        bundles: bundle_stores(tools.oci.clone()),
        sinks: bundle_sinks(registry),
        tools,
        host,
        local,
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
            clap_complete::generate(shell, &mut command(), "ldg", &mut std::io::stdout());
        }
        Command::Man => {
            clap_mangen::Man::new(command())
                .render(&mut std::io::stdout())
                .context("writing the man page")?;
        }
        Command::Forward {
            container,
            port,
            unreachable_polls,
        } => {
            let forwarders = crate::forward::HostForwarders::new(ctx.host.as_ref());
            crate::forward::run_daemon(
                ctx.endpoints.as_ref(),
                &forwarders,
                &container,
                &port,
                unreachable_polls,
            );
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
    use clap::Parser as _;

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(human_size(812), "812 B");
        assert_eq!(human_size(4_200_000), "4.2 MB");
        assert_eq!(human_size(1_500_000_000), "1.5 GB");
    }

    #[test]
    fn the_target_flag_is_optional_and_takes_only_the_targets_there_are() {
        let target = |args: &[&str]| {
            command()
                .try_get_matches_from(args)
                .map(|m| Cli::from_arg_matches(&m).unwrap().target)
        };
        assert_eq!(target(&["ldg", "ls"]).unwrap(), None);
        assert_eq!(
            target(&["ldg", "--target", "podman", "ls"])
                .unwrap()
                .as_deref(),
            Some("podman")
        );
        let err = target(&["ldg", "--target", "nonesuch", "ls"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("docker") && err.contains("host"), "{err}");
    }

    fn config_with(text: &str) -> (crate::tmpfile::TempDir, std::path::PathBuf) {
        let dir = crate::tmpfile::TempDir::new("target-config").unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    #[test]
    fn the_target_comes_from_the_flag_then_the_config_then_the_default() {
        let ls = Cli::try_parse_from(["ldg", "ls"]).unwrap().command;
        let (_dir, with) = config_with("target = \"podman\"\n");
        let (_dir2, without) = config_with("");
        assert_eq!(choose_target(None, &ls, &with).unwrap(), "podman");
        assert_eq!(choose_target(None, &ls, &without).unwrap(), "docker");
        assert_eq!(
            choose_target(None, &ls, &without.with_file_name("none.toml")).unwrap(),
            "docker"
        );
        // The flag wins over the file, even a bad one.
        let (_dir3, bad) = config_with("target = \"nonesuch\"\n");
        assert_eq!(
            choose_target(Some("host".into()), &ls, &bad).unwrap(),
            "host"
        );
        let err = choose_target(None, &ls, &bad).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("invalid `target`") && text.contains("docker, podman, host, kubernetes"),
            "{text}"
        );
        // Commands that never use a target don't read it, so `init` can fix it.
        let init = Cli::try_parse_from(["ldg", "init", "--force"])
            .unwrap()
            .command;
        assert_eq!(choose_target(None, &init, &bad).unwrap(), "docker");
    }

    #[test]
    fn a_bundle_is_named_with_a_flag_long_or_short() {
        for flag in ["--bundle", "-b"] {
            let up = Cli::try_parse_from(["ldg", "up", flag, "oci://ghcr.io/me/lodge:1"])
                .unwrap()
                .command;
            let Command::Up { bundle, build, .. } = up else {
                panic!("not up");
            };
            assert_eq!(
                bundle.as_deref(),
                Some("oci://ghcr.io/me/lodge:1"),
                "{flag}"
            );
            assert!(!build, "{flag} must not mean --build");
        }
    }

    #[test]
    fn a_target_is_built_from_its_own_table_and_an_unused_one_is_not_read() {
        use crate::host::fake::FakeHost;
        use crate::local::fake::FakeLocal;
        use crate::tools::NoRegistry;
        let wiring = || Wiring {
            host: Arc::new(FakeHost::new()),
            local: Arc::new(FakeLocal::new()),
            oci: Arc::new(NoRegistry),
        };
        let (_dir, path) =
            config_with("[targets.docker]\ncontext = \"colima\"\n[targets.podman]\nbogus = 1\n");
        let ls = Cli::try_parse_from(["ldg", "ls"]).unwrap().command;
        // Docker's own table is fine; podman's mistake is not docker's problem.
        build_endpoints("docker", &path, wiring(), &ls).unwrap();
        let err = build_endpoints("podman", &path, wiring(), &ls)
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("[targets.podman]"), "{err:#}");
        // `init` builds from no settings at all.
        let init = Cli::try_parse_from(["ldg", "init"]).unwrap().command;
        build_endpoints("podman", &path, wiring(), &init).unwrap();
    }

    #[test]
    fn a_child_lodge_is_pointed_at_the_same_target_and_host() {
        for target in targets::names() {
            let mut args = vec!["ldg".to_string()];
            args.extend(respawn_args(Some("me@h:22"), target));
            args.push("ls".into());
            let matches = command().try_get_matches_from(args).unwrap();
            let cli = Cli::from_arg_matches(&matches).unwrap();
            assert_eq!(cli.remote.as_deref(), Some("me@h:22"));
            assert_eq!(cli.target.as_deref(), Some(target));
        }
    }

    #[test]
    fn the_cli_definition_is_consistent() {
        command().debug_assert();
    }
}
