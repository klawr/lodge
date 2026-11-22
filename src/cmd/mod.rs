//! The commands: each turns parsed arguments into calls on the modules that do
//! the work. This is the only layer that loads the config (once per command) and
//! decides what to print.
//!
//! - [`attach`]: `up` and `attach`, which end in an interactive session
//! - [`resume`]: `ldg` on its own, which offers the places a session was opened in
//! - [`project`]: `down`, `status`, `rm`, `ls`, `ports`, `logs`, `clean`
//! - [`doctor`]: the `doctor` command
//! - [`init`]: the `init` command
//! - [`configure`]: the config screen of a bare `ldg`: browse, add and remove what is injected
//! - [`inject`]: `inject add|rm|ls`, which edit the `[[inject]]` entries of the config
//! - [`bundle`]: `pack`, `bundle`, `cache`
//! - [`prompt`]: the questions asked on the terminal, and the interface they are asked through
//! - [`select`]: the list to choose from with the keyboard, behind `prompt`
//! - [`table`]: rows of cells laid out for whatever width the terminal has
//! - [`targets`]: the list of targets, the one place that says which there are

mod attach;
mod bundle;
mod candidates;
mod configure;
mod doctor;
mod init;
mod inject;
mod project;
mod prompt;
mod resume;
mod select;
mod table;
pub(crate) mod targets;

use anyhow::{Context, Result};
use clap::{CommandFactory, FromArgMatches};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use crate::bundle::store::{FileSink, FileStore, OciSink, OciStore, Replace, Sinks, Stores};
use crate::cli::{Cli, Command};
use crate::config::Config;
use crate::forward::{DetachedDaemon, ForwardDaemon};
use crate::history::{FileHistory, History, Place, Route};
use crate::host::{HostRef, LocalHost, SshHost};
use crate::local::{Local, SystemLocal};
use crate::output::warning;
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
        Command::Init { .. } | Command::Inject { .. } | Command::Completions { .. } | Command::Man
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
    project: Option<&Path>,
    with: Wiring,
    command: &Command,
) -> Result<Box<dyn Endpoints>> {
    let kind = targets::find(target).with_context(|| format!("no target `{target}`"))?;
    let settings = if uses_target(command) {
        let user = crate::config::target_settings(config, target)?;
        match project.filter(|_| kind.per_project()) {
            Some(dir) => {
                let theirs = crate::config::project_target_settings(dir, target)?;
                kind.merge_project(&user, &theirs)
            }
            None => user,
        }
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
fn respawn_args(route: &Route, project: Option<&Path>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(remote) = &route.remote {
        args.extend(["--remote".to_string(), remote.clone()]);
    }
    args.extend(["--target".to_string(), route.target.clone()]);
    if let Some(project) = project {
        args.extend(["--project-dir".to_string(), project.display().to_string()]);
    }
    args
}

/// The project folder a command is about, for the settings a project gives its
/// target: the path it was handed, else the folder it is run in. A command that
/// is about no project has none.
fn project_dir(command: &Command, handed: Option<PathBuf>) -> Option<PathBuf> {
    match command {
        Command::Up { project, .. }
        | Command::Down { project }
        | Command::Status { project, .. }
        | Command::Rm { project }
        | Command::Ports { project, .. }
        | Command::Logs { project, .. }
        | Command::Clean { project, .. }
        | Command::Doctor { project, .. } => project
            .path
            .clone()
            .or_else(|| std::env::current_dir().ok()),
        // The daemon is told which folder; the rest have no project.
        _ => handed,
    }
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
    /// The places sessions were opened in, for a bare `ldg` to offer again.
    pub history: Box<dyn History>,
    /// How this run got here: the target and the remote it works through.
    pub route: Route,
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

    /// Notes that a session is being opened in `place`, under the name the endpoint
    /// gave itself, else the place's own. The history is a convenience: failing to
    /// keep it is said, and never stops the session.
    pub fn remember(&self, place: Place, name: Option<&str>) {
        let name = name.map_or_else(|| place.fallback_name(), str::to_string);
        if let Err(e) = self.history.remember(place, self.route.clone(), name) {
            warning!("could not remember this session in the history: {e:#}");
        }
    }

    /// The container `name` (a name or an id) on the target's engine, one lodge
    /// did not make.
    pub fn existing_container(&self, name: &str) -> Result<Box<dyn crate::runtime::Container>> {
        let engine = self.endpoints.engine().with_context(
            || "this target has no container engine, so it has no containers to attach to",
        )?;
        Ok(engine.existing(name))
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

pub fn run(mut cli: Cli) -> Result<ExitCode> {
    // The composition root: the one place that picks how the project's machine
    // is reached and which target is worked in on it. Each is an interface everything
    // else takes; a new way of reaching a machine or a new target is one more arm.
    // The helper tools (`ldd`, `patchelf`, the registry client) are chosen here too.
    let history = FileHistory::in_data_dir();
    // Without a subcommand, the person picks one of the places they were in. That
    // comes first because the pick says which host and target to build.
    let subcommand = match cli.command.take() {
        Some(subcommand) => subcommand,
        None => match resume::pick(
            &history,
            &prompt::Terminal,
            cli.remote.as_deref(),
            cli.target.as_deref(),
        )? {
            Some(resume::Home::Place(resumed)) => {
                cli.remote = resumed.route.remote;
                cli.target = Some(resumed.route.target);
                resumed.command
            }
            Some(resume::Home::Configure) => {
                // Only the config is worked on: no host, no target, no engine.
                let path = crate::config::path_for(cli.config)?;
                let home = SystemLocal
                    .var("HOME")
                    .map_or_else(|| PathBuf::from("/"), PathBuf::from);
                let path_var = SystemLocal.var("PATH").unwrap_or_default();
                let local: Arc<dyn Local> = Arc::new(SystemLocal);
                let registry = Arc::new(NativeOci::new(
                    Box::new(UreqHttp::new()),
                    Box::new(DockerConfig::new(local)),
                ));
                let packer = bundle::LivePacker {
                    tools: Toolchain {
                        deps: Box::new(Ldd),
                        elf: Box::new(Patchelf),
                        oci: registry.clone(),
                    },
                    sinks: bundle_sinks(registry),
                };
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let machine = configure::Machine {
                    home: &home,
                    path_var: &path_var,
                    cwd: &cwd,
                    bundles: &packer,
                };
                configure::run(&path, &machine, &prompt::Terminal, &prompt::Terminal)?;
                return Ok(ExitCode::SUCCESS);
            }
            None => return Ok(ExitCode::SUCCESS),
        },
    };
    let remote = cli.remote.clone();
    // The forward daemon has no terminal, so its ssh commands must never prompt.
    let daemon = matches!(subcommand, Command::Forward { .. });
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
    let target = choose_target(cli.target, &subcommand, &config_path)?;
    let route = Route {
        target: target.to_string(),
        remote,
    };
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
    // Read on this machine, even with `--remote`: the folder you stand in is here.
    let project = project_dir(&subcommand, cli.project_dir);
    let endpoints = build_endpoints(
        target,
        &config_path,
        project.as_deref(),
        Wiring {
            host: host.clone(),
            local: local.clone(),
            oci: tools.oci.clone(),
        },
        &subcommand,
    )?;
    let ctx = Ctx {
        forwarder: Box::new(DetachedDaemon::new(respawn_args(
            &route,
            project.as_deref(),
        ))),
        history: Box::new(history),
        route,
        endpoints,
        bundles: bundle_stores(tools.oci.clone()),
        sinks: bundle_sinks(registry),
        tools,
        host,
        local,
        config_path,
    };

    match subcommand {
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
        Command::Attach {
            container,
            workdir,
            reinject,
            no_attach,
            bundle,
        } => attach::attach_to(
            &ctx,
            &container,
            workdir,
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
        Command::Clean { project, container } => {
            project::clean(&ctx, project.path, container.as_deref())?
        }
        Command::Pack { output, no_clobber } => {
            let replace = if no_clobber {
                Replace::No
            } else {
                Replace::Yes
            };
            bundle::pack(&ctx, output, replace)?
        }
        Command::Bundle { command } => bundle::bundle(&ctx, command)?,
        Command::Cache { command } => bundle::cache(command)?,
        Command::Inject { command } => inject::run(&ctx, command)?,
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
        let ls = Cli::try_parse_from(["ldg", "ls"]).unwrap().command.unwrap();
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
            .command
            .unwrap();
        assert_eq!(choose_target(None, &init, &bad).unwrap(), "docker");
    }

    #[test]
    fn without_a_subcommand_ldg_parses_and_keeps_the_flags_that_narrow_the_history() {
        let cli = Cli::try_parse_from(["ldg"]).unwrap();
        assert!(cli.command.is_none());
        let cli = Cli::try_parse_from(["ldg", "--remote", "me@box", "--target", "podman"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.remote.as_deref(), Some("me@box"));
        assert_eq!(cli.target.as_deref(), Some("podman"));
    }

    #[test]
    fn a_bundle_is_named_with_a_flag_long_or_short() {
        for flag in ["--bundle", "-b"] {
            let up = Cli::try_parse_from(["ldg", "up", flag, "oci://ghcr.io/me/lodge:1"])
                .unwrap()
                .command;
            let Some(Command::Up { bundle, build, .. }) = up else {
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
        let ls = Cli::try_parse_from(["ldg", "ls"]).unwrap().command.unwrap();
        // Docker's own table is fine; podman's mistake is not docker's problem.
        build_endpoints("docker", &path, None, wiring(), &ls).unwrap();
        let err = build_endpoints("podman", &path, None, wiring(), &ls)
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("[targets.podman]"), "{err:#}");
        // `init` builds from no settings at all.
        let init = Cli::try_parse_from(["ldg", "init"])
            .unwrap()
            .command
            .unwrap();
        build_endpoints("podman", &path, None, wiring(), &init).unwrap();
    }

    #[test]
    fn a_child_lodge_is_pointed_at_the_same_target_and_host() {
        for target in targets::names() {
            let mut args = vec!["ldg".to_string()];
            let route = Route {
                target: target.to_string(),
                remote: Some("me@h:22".into()),
            };
            args.extend(respawn_args(&route, None));
            args.push("ls".into());
            let matches = command().try_get_matches_from(args).unwrap();
            let cli = Cli::from_arg_matches(&matches).unwrap();
            assert_eq!(cli.remote.as_deref(), Some("me@h:22"));
            assert_eq!(cli.target.as_deref(), Some(target));
        }
    }

    #[test]
    fn a_pod_comes_from_the_project_over_the_users_config_and_the_daemon_is_told_where() {
        use crate::host::fake::FakeHost;
        use crate::local::fake::FakeLocal;
        use crate::tools::NoRegistry;
        let host = Arc::new(FakeHost::new());
        let wiring = || Wiring {
            host: host.clone(),
            local: Arc::new(FakeLocal::new()),
            oci: Arc::new(NoRegistry),
        };
        let (_dir, path) = config_with("[targets.kubernetes]\nselector = \"app=api\"\n");
        let project = crate::tmpfile::TempDir::new("project").unwrap();
        std::fs::write(
            project.path().join(crate::config::PROJECT_FILE),
            "[targets.kubernetes]\npod = \"dev-0\"\nnamespace = \"mine\"\n",
        )
        .unwrap();
        let down = Cli::try_parse_from(["ldg", "down"])
            .unwrap()
            .command
            .unwrap();
        let endpoints =
            build_endpoints("kubernetes", &path, Some(project.path()), wiring(), &down).unwrap();
        // The pod and the namespace are the project's: `kubectl` is asked about them.
        endpoints.checks();
        let asked = host.argvs().concat().join(" ");
        assert!(asked.contains("--namespace mine"), "{asked}");
        assert!(asked.contains("dev-0"), "{asked}");
        // A target that takes nothing per project ignores the file.
        build_endpoints("docker", &path, Some(project.path()), wiring(), &down).unwrap();
        // The folder reaches a child lodge, which has no project argument of its own.
        let route = Route {
            target: "kubernetes".into(),
            remote: None,
        };
        let args = respawn_args(&route, Some(project.path()));
        let mut full = vec!["ldg".to_string()];
        full.extend(args);
        full.extend(["_forward".into(), "pod".into()]);
        let cli = Cli::from_arg_matches(&command().try_get_matches_from(full).unwrap()).unwrap();
        assert_eq!(cli.project_dir.as_deref(), Some(project.path()));
        assert_eq!(
            project_dir(cli.command.as_ref().unwrap(), cli.project_dir.clone()).as_deref(),
            Some(project.path())
        );
    }

    #[test]
    fn the_cli_definition_is_consistent() {
        command().debug_assert();
    }
}
