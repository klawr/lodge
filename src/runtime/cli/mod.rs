//! The docker-compatible command line (`docker`, `podman`) as a [`Runtime`],
//! [`Compose`], [`Container`] and [`Env`](super::Env). Podman's CLI is docker's
//! apart from a few places, which each engine's [`Flavor`] answers; everything else
//! is shared.
//!
//! It runs `docker …` (or `podman …`) *on a [`Host`](crate::host::Host)* it is given: this machine, or another one
//! the host reaches (so a remote engine needs no local docker CLI, only whatever
//! the host needs). It never starts a process itself and does not know which kind
//! of host it has.
//!
//! The trade-off versus `DOCKER_HOST=ssh://…` is that the client does not run
//! locally, so file copies can't be passed by path: the source files live on
//! *this* machine, not the host's. `cp_in` therefore streams the bytes through
//! the host's pipe instead.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::{Command, Stdio};

use super::{BuildSpec, Compose, Container, Containers, Listed, RunSpec, Runtime};
use crate::host::{HostRef, Repeat};

mod container;
mod copy;
mod docker;
mod podman;

pub use container::CliContainer;
pub use docker::DockerFlavor;
pub use podman::PodmanFlavor;

/// What one engine of the docker-compatible family does its own way: its
/// program, and the few places where its command line or its behaviour differs
/// from what [`CliEngine`] does for all of them. Docker and Podman are different
/// engines that share the plumbing; each is a `Flavor`, and a [`CliEngine`] of it
/// asks the flavor instead of checking which one it is.
pub trait Flavor: Clone + Send + Sync + 'static {
    /// The program, and what the user calls the engine (`--engine`).
    fn program(&self) -> &'static str;

    /// Flags for `run`, after the common ones and before the devcontainer's own
    /// `runArgs`, which can override them.
    fn extra_run_flags(&self, cli: &CliEngine<Self>, spec: &RunSpec) -> Vec<String>
    where
        Self: Sized;

    /// The arguments of the command that streams a tar into a container,
    /// keeping the archive's owners.
    fn cp_in_args<'a>(&self, target: &'a str) -> Vec<&'a str>;

    /// Whether `cp`'s error says the path is not in the container (as opposed to
    /// the container, the engine or the connection being the problem).
    fn says_path_is_missing(&self, stderr: &str) -> bool;

    /// The arguments that ask the compose provider for its version.
    fn compose_version_args(&self) -> &'static [&'static str];

    /// Whether a container's own address (from `inspect`) can be connected to
    /// from the host the engine runs on.
    fn addresses_are_reachable(&self, cli: &CliEngine<Self>) -> bool
    where
        Self: Sized;
}

/// A docker-compatible command line, run on a [`Host`](crate::host::Host).
#[derive(Clone)]
pub struct CliEngine<F: Flavor> {
    host: HostRef,
    flavor: F,
}

/// The `docker` CLI.
pub type Docker = CliEngine<DockerFlavor>;
/// The `podman` CLI.
pub type Podman = CliEngine<PodmanFlavor>;

impl<F: Flavor> CliEngine<F> {
    fn with(host: HostRef, flavor: F) -> Self {
        Self { host, flavor }
    }

    /// The program's name, for messages.
    pub(super) fn name(&self) -> &'static str {
        self.flavor.program()
    }

    /// The program and its arguments, as a vector.
    fn argv<'a>(&self, args: &[&'a str]) -> Vec<&'a str> {
        let mut v = vec![self.flavor.program()];
        v.extend_from_slice(args);
        v
    }

    /// Builds a command that runs `docker <args>` on the host. Stdin is closed
    /// (`/dev/null`) so a remote hop doesn't swallow the terminal; callers that
    /// need to stream data override it. Traced at `-v`. Every `docker` lodge runs
    /// is built here, in [`command_in`](Self::command_in) or in
    /// [`interactive`](Self::interactive).
    pub(super) fn command(&self, args: &[&str]) -> Command {
        let mut c = self.host.argv(&self.argv(args), None);
        c.stdin(Stdio::null());
        crate::output::trace(&c);
        c
    }

    /// Like [`command`](Self::command) for operations that read *files* from
    /// disk (a compose project, a build context). Those are read client-side,
    /// i.e. on the host where the docker CLI runs, so the working directory has
    /// to be the project dir *there*.
    fn command_in(&self, dir: Option<&Path>, args: &[&str]) -> Command {
        let c = self.host.argv(&self.argv(args), dir);
        crate::output::trace(&c);
        c
    }

    /// A command that opens an interactive `docker <args>` on the host, with a
    /// terminal.
    pub(super) fn interactive(&self, args: &[&str]) -> Command {
        self.host.interactive(&self.argv(args))
    }

    /// `docker compose -p <project> -f <file>… <tail…>`, run in `dir`.
    fn compose(&self, dir: &Path, project: &str, files: &[String], tail: &[&str]) -> Command {
        let mut args = vec!["compose", "-p", project];
        for f in files {
            args.extend(["-f", f.as_str()]);
        }
        args.extend_from_slice(tail);
        self.command_in(Some(dir), &args)
    }

    /// Runs `docker <args>` with inherited stdio; errors if it exits non-zero.
    /// Over ssh a dropped connection reruns it only if `repeat` says that is safe.
    fn run(&self, args: &[&str], repeat: Repeat) -> Result<()> {
        self.run_with(|| self.command(args), args, repeat)
    }

    /// Like [`run`](Self::run) for a command `make` builds (afresh for each try).
    fn run_with(&self, make: impl Fn() -> Command, args: &[&str], repeat: Repeat) -> Result<()> {
        let what = subcommand(args);
        let status = self
            .host
            .attempt(repeat, || make().status())
            .with_context(|| format!("spawning {} {what}", self.name()))?;
        if !status.success() {
            bail!("{} {what} failed ({status})", self.name());
        }
        Ok(())
    }

    /// Runs a command `make` builds, capturing its output, and returns stdout; a
    /// non-zero exit is an error carrying docker's stderr.
    fn checked(&self, make: impl Fn() -> Command, args: &[&str], repeat: Repeat) -> Result<String> {
        let what = subcommand(args);
        let out = self
            .host
            .attempt(repeat, || make().output())
            .with_context(|| format!("spawning {} {what}", self.name()))?;
        if !out.status.success() {
            bail!(
                "{} {what} failed: {}",
                self.name(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Runs `docker <args>` quietly and returns stdout, or `None` if it could not
    /// run or exited non-zero. For probes whose failure is an answer, not an error.
    /// A probe changes nothing, so a dropped connection reruns it.
    fn capture_ok(&self, args: &[&str]) -> Option<String> {
        let out = self
            .host
            .attempt(Repeat::Safe, || {
                let mut cmd = self.command(args);
                cmd.stdout(Stdio::piped()).stderr(Stdio::null());
                cmd.output()
            })
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// "compose up", "ps", …: the words before the first flag, for messages.
fn subcommand(args: &[&str]) -> String {
    let words: Vec<&str> = args
        .iter()
        .copied()
        .take_while(|a| !a.starts_with('-'))
        .collect();
    match words.as_slice() {
        ["compose", ..] => {
            // `compose -p x -f y up …`: name the compose verb after the flags.
            let verb = args
                .iter()
                .skip(1)
                .scan(false, |skip, a| {
                    let take = !*skip && !a.starts_with('-');
                    *skip = matches!(*a, "-p" | "-f");
                    Some(take.then_some(*a))
                })
                .flatten()
                .next()
                .unwrap_or("");
            format!("compose {verb}")
        }
        _ => words.first().copied().unwrap_or("").to_string(),
    }
}

/// The `run` flags for `spec`, ahead of the image: the common ones, then the
/// engine's own (`extra`), then the devcontainer's `engine_args`, last so they can
/// override the rest.
fn run_flags(spec: &RunSpec, extra: &[String]) -> Vec<String> {
    let mut args = Vec::new();
    for (k, v) in &spec.labels {
        args.extend(["--label".into(), format!("{k}={v}")]);
    }
    for (source, target) in &spec.binds {
        args.extend(["-v".into(), format!("{source}:{target}")]);
    }
    for mount in &spec.mounts {
        args.extend(["--mount".into(), mount.clone()]);
    }
    if let Some(dir) = &spec.workdir {
        args.extend(["-w".into(), dir.clone()]);
    }
    if spec.privileged {
        args.push("--privileged".into());
    }
    if spec.init {
        args.push("--init".into());
    }
    for cap in &spec.cap_add {
        args.extend(["--cap-add".into(), cap.clone()]);
    }
    for opt in &spec.security_opt {
        args.extend(["--security-opt".into(), opt.clone()]);
    }
    if let Some(user) = &spec.user {
        args.extend(["-u".into(), user.clone()]);
    }
    for port in &spec.publish {
        args.extend(["-p".into(), port.clone()]);
    }
    args.extend(extra.iter().cloned());
    args.extend(spec.engine_args.iter().cloned());
    args
}

impl<F: Flavor> Runtime for CliEngine<F> {
    fn container(&self, id: &str) -> Box<dyn Container> {
        Box::new(CliContainer {
            cli: self.clone(),
            id: id.to_string(),
        })
    }

    fn respawn_args(&self) -> Vec<String> {
        vec!["--engine".into(), self.name().into()]
    }

    fn version(&self) -> Result<String> {
        match self.capture_ok(&["version", "--format", "{{.Server.Version}}"]) {
            Some(v) if !v.trim().is_empty() => Ok(format!("{} {}", self.name(), v.trim())),
            _ => bail!(
                "cannot reach {} (is it running, and do you have access?)",
                self.name()
            ),
        }
    }

    fn compose(&self) -> Result<&dyn Compose> {
        Ok(self)
    }

    fn list_labeled(&self, label: &str) -> Result<Vec<Listed>> {
        let format =
            format!("{{{{.ID}}}}\t{{{{.Names}}}}\t{{{{.State}}}}\t{{{{.Label \"{label}\"}}}}");
        let filter = format!("label={label}");
        let args = ["ps", "-a", "--filter", &filter, "--format", &format];
        Ok(parse_listed(&self.checked(
            || self.command(&args),
            &args,
            Repeat::Safe,
        )?))
    }

    fn find_by_name(&self, name: &str) -> Result<Option<String>> {
        let filter = format!("name=^{name}$");
        let args = ["ps", "-aq", "--filter", &filter];
        Ok(first_line(&self.checked(
            || self.command(&args),
            &args,
            Repeat::Safe,
        )?))
    }

    fn build_image(&self, spec: &BuildSpec) -> Result<()> {
        let mut a: Vec<String> = vec!["build".into(), "-t".into(), spec.tag.clone()];
        if let Some(df) = &spec.dockerfile {
            a.push("-f".into());
            a.push(spec.context.join(df).to_string_lossy().into_owned());
        }
        if let Some(t) = &spec.stage {
            a.push("--target".into());
            a.push(t.clone());
        }
        for (k, v) in &spec.args {
            a.push("--build-arg".into());
            a.push(format!("{k}={v}"));
        }
        a.push(spec.context.to_string_lossy().into_owned());
        // The build context (and -f dockerfile) are read client-side, so the
        // build runs on the host that has them (through `command_in`).
        let args: Vec<&str> = a.iter().map(String::as_str).collect();
        // A build is cached layer by layer, so doing it again is harmless.
        self.run_with(|| self.command_in(None, &args), &args, Repeat::Safe)
    }

    fn run_detached(&self, spec: &RunSpec) -> Result<()> {
        let name = spec.name.as_str();
        let flags = run_flags(spec, &self.flavor.extra_run_flags(self, spec));
        let mut args: Vec<&str> = vec!["run", "-d", "--name", name];
        args.extend(flags.iter().map(String::as_str));
        args.push(&spec.image);
        args.extend(spec.command.iter().map(String::as_str));
        // `docker run` is not repeatable as such: a second one fails on the name
        // the first took, if it got that far. So a try after a lost connection
        // first looks for the container, and starts what it finds (it may have
        // been created but not started) instead of running again.
        let mut first = true;
        let status = self
            .host
            .attempt(Repeat::Safe, || {
                if !std::mem::take(&mut first)
                    && let Ok(Some(_)) = self.find_by_name(name)
                {
                    return self.command(&["start", name]).status();
                }
                self.command(&args).status()
            })
            .with_context(|| format!("spawning {} run", self.name()))?;
        if !status.success() {
            bail!("{} run failed ({status})", self.name());
        }
        Ok(())
    }

    fn start(&self, id: &str) -> Result<()> {
        self.run(&["start", id], Repeat::Safe)
    }

    fn stop(&self, id: &str) -> Result<()> {
        self.run(&["stop", id], Repeat::Safe)
    }

    fn remove(&self, id: &str) -> Result<()> {
        // `-f` makes removing what is already gone a success.
        self.run(&["rm", "-f", id], Repeat::Safe)
    }
}

/// `docker compose`, which every current docker CLI has (as a plugin).
impl<F: Flavor> Compose for CliEngine<F> {
    fn version(&self) -> Result<String> {
        match self.capture_ok(self.flavor.compose_version_args()) {
            Some(v) if !v.trim().is_empty() => Ok(format!(
                "{} compose {}",
                self.name(),
                v.lines().next().unwrap_or("").trim()
            )),
            _ => bail!(
                "`{} compose` is not available (install a compose provider)",
                self.name()
            ),
        }
    }

    fn up(&self, dir: &Path, project: &str, files: &[String], extra: &[&str]) -> Result<()> {
        let mut tail = vec!["up", "-d"];
        tail.extend_from_slice(extra);
        // `up -d` converges on the same state when run again (with
        // `--force-recreate` it recreates once more, which costs time only).
        self.run_with(
            || self.compose(dir, project, files, &tail),
            &["compose", "up"],
            Repeat::Safe,
        )
    }

    fn down(&self, dir: &Path, project: &str, files: &[String]) -> Result<()> {
        self.run_with(
            || self.compose(dir, project, files, &["down"]),
            &["compose", "down"],
            Repeat::Safe,
        )
    }

    fn ps(
        &self,
        dir: &Path,
        project: &str,
        files: &[String],
        service: &str,
        which: Containers,
    ) -> Result<Option<String>> {
        let mut tail = vec!["ps"];
        if which == Containers::All {
            tail.push("-a");
        }
        tail.extend(["-q", service]);
        let out = self.checked(
            || self.compose(dir, project, files, &tail),
            &["compose", "ps"],
            Repeat::Safe,
        )?;
        Ok(first_line(&out))
    }
}

/// The first non-empty line of `out`, trimmed.
fn first_line(out: &str) -> Option<String> {
    out.lines()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(String::from)
}

/// Parses `docker ps` lines of `id<TAB>name<TAB>state<TAB>label`.
fn parse_listed(out: &str) -> Vec<Listed> {
    out.lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            Some(Listed {
                id: f.next()?.trim().to_string(),
                name: f.next()?.trim().to_string(),
                running: f.next()?.trim() == "running",
                label: f.next().unwrap_or("").trim().to_string(),
            })
        })
        .filter(|l| !l.id.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Docker, Podman, RunSpec, parse_listed, run_flags, subcommand};
    use crate::host::LocalHost;
    use std::sync::Arc;

    #[test]
    fn a_run_spec_becomes_docker_flags_with_the_pass_through_ones_last() {
        let spec = RunSpec {
            labels: vec![("a".into(), "b".into())],
            binds: vec![("/src".into(), "/dst".into())],
            mounts: vec!["type=volume,source=v,target=/d".into()],
            workdir: Some("/w".into()),
            privileged: true,
            init: true,
            cap_add: vec!["SYS_PTRACE".into()],
            security_opt: vec!["seccomp=unconfined".into()],
            user: Some("dev".into()),
            publish: vec!["127.0.0.1:3000:3000".into()],
            engine_args: vec!["--cap-drop=ALL".into()],
            ..Default::default()
        };
        assert_eq!(
            run_flags(&spec, &[]),
            [
                "--label",
                "a=b",
                "-v",
                "/src:/dst",
                "--mount",
                "type=volume,source=v,target=/d",
                "-w",
                "/w",
                "--privileged",
                "--init",
                "--cap-add",
                "SYS_PTRACE",
                "--security-opt",
                "seccomp=unconfined",
                "-u",
                "dev",
                "-p",
                "127.0.0.1:3000:3000",
                "--cap-drop=ALL"
            ]
        );
        assert!(run_flags(&RunSpec::default(), &[]).is_empty());
    }

    #[test]
    fn parses_listed_containers() {
        let l = parse_listed("abc\tlodge-x-1\trunning\t/home/me/x\ndef\ty\texited\t/p q\n\n");
        assert_eq!(l.len(), 2);
        assert_eq!(
            (
                l[0].id.as_str(),
                l[0].name.as_str(),
                l[0].running,
                l[0].label.as_str()
            ),
            ("abc", "lodge-x-1", true, "/home/me/x")
        );
        assert!(!l[1].running);
        assert_eq!(l[1].label, "/p q");
    }

    #[test]
    fn the_program_is_the_first_word_of_the_argument_vector() {
        let args = ["exec", "my container", "sh"];
        assert_eq!(
            Docker::new(Arc::new(LocalHost)).argv(&args),
            ["docker", "exec", "my container", "sh"]
        );
        assert_eq!(
            Podman::new(Arc::new(LocalHost)).argv(&args),
            ["podman", "exec", "my container", "sh"]
        );
    }

    #[test]
    fn the_engines_own_flags_come_after_the_common_ones_and_before_runargs() {
        let spec = RunSpec {
            user: Some("dev".into()),
            engine_args: vec!["--cap-drop=ALL".into()],
            ..Default::default()
        };
        assert_eq!(
            run_flags(&spec, &["--mine".into()]),
            ["-u", "dev", "--mine", "--cap-drop=ALL"]
        );
    }

    #[test]
    fn messages_name_the_subcommand() {
        assert_eq!(subcommand(&["ps", "-a"]), "ps");
        assert_eq!(subcommand(&["compose", "up"]), "compose up");
        assert_eq!(
            subcommand(&["compose", "-p", "x", "-f", "a.yml", "down"]),
            "compose down"
        );
    }
}
