//! The `docker` CLI as a [`Runtime`], [`Container`] and [`Env`].
//!
//! Locally this runs `docker …`. For a remote target it runs the docker CLI *on
//! the remote host* over SSH (`ssh <host> docker …`), so no local docker CLI is
//! needed for `--remote`: only `ssh` must exist locally.
//!
//! The trade-off versus `DOCKER_HOST=ssh://…` is that the client no longer runs
//! locally, so file copies can't be passed by path: the source files live on
//! *this* machine, not the remote. `cp_in` therefore streams the bytes through
//! the SSH pipe instead.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::{Command, Stdio};

use super::{BuildSpec, Compose, Container, Containers, Listed, Runtime};
use crate::shell::shell_quote;
use crate::target::Target;

mod container;
mod copy;

pub use container::DockerContainer;

/// The docker CLI, pointed at a [`Target`].
#[derive(Clone)]
pub struct DockerCli {
    target: Target,
}

impl DockerCli {
    pub fn new(target: &Target) -> Self {
        Self {
            target: target.clone(),
        }
    }

    /// Builds a command that runs `docker <args>` against the target's daemon.
    /// Locally that's a plain `docker` invocation; for a remote target it becomes
    /// `ssh <host> docker <args>`. For remote commands stdin is closed
    /// (`/dev/null`) so ssh doesn't swallow the terminal; callers that need to
    /// stream data override it. Traced at `-v`. Every `docker` lodge runs is
    /// built here or in [`command_in`](Self::command_in).
    pub(super) fn command(&self, args: &[&str]) -> Command {
        let c = match &self.target {
            Target::Local => {
                let mut c = Command::new("docker");
                c.args(args);
                c
            }
            Target::Ssh(host) => {
                let mut c = crate::ssh::command(host, &docker_line(args));
                c.stdin(Stdio::null());
                c
            }
        };
        crate::output::trace(&c);
        c
    }

    /// Like [`command`](Self::command) for operations that read *files* from
    /// disk (a compose project, a build context). Those are read client-side,
    /// i.e. on the host where the docker CLI runs, so the working directory has
    /// to be the project dir *there*: a `cd` over ssh when remote.
    fn command_in(&self, dir: Option<&Path>, args: &[&str]) -> Command {
        let c = match &self.target {
            Target::Local => {
                let mut c = Command::new("docker");
                c.args(args);
                if let Some(d) = dir {
                    c.current_dir(d);
                }
                c
            }
            Target::Ssh(host) => {
                let cd = dir.map_or_else(String::new, |d| {
                    format!("cd {} && ", shell_quote(&d.to_string_lossy()))
                });
                crate::ssh::command(host, &format!("{cd}{}", docker_line(args)))
            }
        };
        crate::output::trace(&c);
        c
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
    fn run(&self, args: &[&str]) -> Result<()> {
        status_ok(self.command(args), args)
    }

    /// Runs `docker <args>` quietly and returns stdout, or `None` if it could not
    /// run or exited non-zero. For probes whose failure is an answer, not an error.
    fn capture_ok(&self, args: &[&str]) -> Option<String> {
        let mut cmd = self.command(args);
        cmd.stdout(Stdio::piped()).stderr(Stdio::null());
        let out = cmd.output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Runs `cmd` with inherited stdio (so docker's own messages reach the user)
/// and errors if it exits non-zero, naming the docker subcommand.
fn status_ok(mut cmd: Command, args: &[&str]) -> Result<()> {
    let what = subcommand(args);
    let status = cmd
        .status()
        .with_context(|| format!("spawning docker {what}"))?;
    if !status.success() {
        bail!("docker {what} failed ({status})");
    }
    Ok(())
}

/// Runs `cmd` capturing its output and returns stdout; a non-zero exit is an
/// error carrying docker's stderr.
fn checked(mut cmd: Command, args: &[&str]) -> Result<String> {
    let what = subcommand(args);
    let out = cmd
        .output()
        .with_context(|| format!("spawning docker {what}"))?;
    if !out.status.success() {
        bail!(
            "docker {what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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

/// Joins `docker <args>` into a single shell-quoted command line: what ssh
/// hands the remote login shell, or what a local `sh -c` runs.
pub(super) fn docker_line(args: &[&str]) -> String {
    let mut s = String::from("docker");
    for a in args {
        s.push(' ');
        s.push_str(&shell_quote(a));
    }
    s
}

impl Runtime for DockerCli {
    fn container(&self, id: &str) -> Box<dyn Container> {
        Box::new(DockerContainer {
            cli: self.clone(),
            id: id.to_string(),
        })
    }

    fn version(&self) -> Result<String> {
        match self.capture_ok(&["version", "--format", "{{.Server.Version}}"]) {
            Some(v) if !v.trim().is_empty() => Ok(format!("docker {}", v.trim())),
            _ => bail!("cannot reach the docker daemon (is it running, and do you have access?)"),
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
        Ok(parse_listed(&checked(self.command(&args), &args)?))
    }

    fn find_by_name(&self, name: &str) -> Result<Option<String>> {
        let filter = format!("name=^{name}$");
        let args = ["ps", "-aq", "--filter", &filter];
        Ok(first_line(&checked(self.command(&args), &args)?))
    }

    fn build_image(&self, spec: &BuildSpec) -> Result<()> {
        let mut a: Vec<String> = vec!["build".into(), "-t".into(), spec.tag.clone()];
        if let Some(df) = &spec.dockerfile {
            a.push("-f".into());
            a.push(spec.context.join(df).to_string_lossy().into_owned());
        }
        if let Some(t) = &spec.target {
            a.push("--target".into());
            a.push(t.clone());
        }
        for (k, v) in &spec.args {
            a.push("--build-arg".into());
            a.push(format!("{k}={v}"));
        }
        a.push(spec.context.to_string_lossy().into_owned());
        // The build context (and -f dockerfile) are read client-side, so with a
        // remote target the build runs on the remote host via ssh.
        let args: Vec<&str> = a.iter().map(String::as_str).collect();
        status_ok(self.command_in(None, &args), &args)
    }

    fn run_detached(
        &self,
        name: &str,
        run_args: &[String],
        image: &str,
        command: &[&str],
    ) -> Result<()> {
        let mut args: Vec<&str> = vec!["run", "-d", "--name", name];
        args.extend(run_args.iter().map(String::as_str));
        args.push(image);
        args.extend_from_slice(command);
        self.run(&args)
    }

    fn start(&self, id: &str) -> Result<()> {
        self.run(&["start", id])
    }

    fn stop(&self, id: &str) -> Result<()> {
        self.run(&["stop", id])
    }

    fn remove(&self, id: &str) -> Result<()> {
        self.run(&["rm", "-f", id])
    }
}

/// `docker compose`, which every current docker CLI has (as a plugin).
impl Compose for DockerCli {
    fn version(&self) -> Result<String> {
        match self.capture_ok(&["compose", "version", "--short"]) {
            Some(v) if !v.trim().is_empty() => Ok(format!("docker compose {}", v.trim())),
            _ => bail!("`docker compose` is not available (install the compose plugin)"),
        }
    }

    fn up(&self, dir: &Path, project: &str, files: &[String], extra: &[&str]) -> Result<()> {
        let mut tail = vec!["up", "-d"];
        tail.extend_from_slice(extra);
        status_ok(self.compose(dir, project, files, &tail), &["compose", "up"])
    }

    fn down(&self, dir: &Path, project: &str, files: &[String]) -> Result<()> {
        status_ok(
            self.compose(dir, project, files, &["down"]),
            &["compose", "down"],
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
        let out = checked(self.compose(dir, project, files, &tail), &["compose", "ps"])?;
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
    use super::{docker_line, parse_listed, subcommand};

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
    fn docker_line_quotes_each_arg() {
        assert_eq!(
            docker_line(&["exec", "my container", "sh"]),
            "docker 'exec' 'my container' 'sh'"
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
