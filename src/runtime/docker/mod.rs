//! The `docker` CLI as a [`Runtime`], [`Container`] and [`Env`].
//!
//! Locally this runs `docker …`. For a remote target it runs the docker CLI *on
//! the remote host* over SSH (`ssh <host> docker …`), so no local docker CLI is
//! needed for `--remote`: only `ssh` (and `tar`, for copies) must exist locally.
//!
//! The trade-off versus `DOCKER_HOST=ssh://…` is that the client no longer runs
//! locally, so file copies can't be passed by path: the source files live on
//! *this* machine, not the remote. `cp_in`/`cp_out` therefore stream the bytes
//! through the SSH pipe instead.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::{Command, Stdio};

use super::{BuildSpec, Container, Runtime};
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
    /// stream data build their own ssh command.
    fn command(&self, args: &[&str]) -> Command {
        let c = match &self.target {
            Target::Local => {
                let mut c = Command::new("docker");
                c.args(args);
                c
            }
            Target::Ssh(host) => {
                let mut c = crate::ssh::command(host, &remote_script(args));
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
    /// to be the project dir *there*: a `cd` over ssh when remote. The caller
    /// traces the result.
    fn command_in(&self, dir: Option<&Path>, args: &[String]) -> Command {
        match &self.target {
            Target::Local => {
                let mut cmd = Command::new("docker");
                cmd.args(args);
                if let Some(d) = dir {
                    cmd.current_dir(d);
                }
                cmd
            }
            Target::Ssh(host) => {
                let mut script = String::new();
                if let Some(d) = dir {
                    script.push_str(&format!("cd {} && ", shell_quote(&d.to_string_lossy())));
                }
                script.push_str("docker");
                for a in args {
                    script.push(' ');
                    script.push_str(&shell_quote(a));
                }
                crate::ssh::command(host, &script)
            }
        }
    }

    /// Runs `docker <args>` with inherited stdio; errors if it exits non-zero.
    fn run(&self, args: &[&str]) -> Result<()> {
        let status = self.command(args).status().context("spawning docker")?;
        if !status.success() {
            bail!("docker {} failed", args.first().copied().unwrap_or(""));
        }
        Ok(())
    }

    /// Runs `docker <args>` and returns stdout, regardless of exit status.
    fn capture(&self, args: &[&str]) -> Result<String> {
        let out = self.command(args).output().context("spawning docker")?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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

/// Joins `docker <args>` into a single shell-quoted command line for the remote
/// login shell that ssh hands the command to.
pub(super) fn remote_script(args: &[&str]) -> String {
    let mut s = String::from("docker");
    for a in args {
        s.push(' ');
        s.push_str(&shell_quote(a));
    }
    s
}

impl Runtime for DockerCli {
    fn target(&self) -> &Target {
        &self.target
    }

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

    fn find_by_name(&self, name: &str) -> Result<Option<String>> {
        let out = self.capture(&["ps", "-aq", "--filter", &format!("name=^{name}$")])?;
        Ok(out
            .lines()
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(String::from))
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
        let mut cmd = self.command_in(None, &a);
        crate::output::trace(&cmd);
        let status = cmd.status().context("docker build")?;
        if !status.success() {
            bail!("docker build failed");
        }
        Ok(())
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

    fn compose_up(
        &self,
        dir: &Path,
        project: &str,
        files: &[String],
        extra: &[&str],
    ) -> Result<()> {
        let mut args = vec!["compose".to_string(), "-p".to_string(), project.to_string()];
        args.extend(compose_file_args(files));
        args.extend(["up".to_string(), "-d".to_string()]);
        args.extend(extra.iter().map(|s| s.to_string()));

        let mut cmd = self.command_in(Some(dir), &args);
        crate::output::trace(&cmd);
        let status = cmd.status().context("running docker compose up")?;
        if !status.success() {
            bail!("docker compose up failed");
        }
        Ok(())
    }

    fn compose_down(&self, dir: &Path, project: &str, files: &[String]) -> Result<()> {
        let mut args = vec!["compose".to_string(), "-p".to_string(), project.to_string()];
        args.extend(compose_file_args(files));
        args.push("down".to_string());

        let mut cmd = self.command_in(Some(dir), &args);
        crate::output::trace(&cmd);
        let status = cmd.status().context("running docker compose down")?;
        if !status.success() {
            bail!("docker compose down failed");
        }
        Ok(())
    }

    fn compose_ps(
        &self,
        dir: &Path,
        project: &str,
        files: &[String],
        service: &str,
        all: bool,
    ) -> Result<String> {
        let mut args = vec!["compose".to_string(), "-p".to_string(), project.to_string()];
        args.extend(compose_file_args(files));
        args.push("ps".to_string());
        if all {
            args.push("-a".to_string());
        }
        args.extend(["-q".to_string(), service.to_string()]);

        let mut cmd = self.command_in(Some(dir), &args);
        crate::output::trace(&cmd);
        let output = cmd.output().context("docker compose ps")?;
        if !output.status.success() {
            bail!(
                "docker compose ps failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        Ok(String::from_utf8(output.stdout)?
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string())
    }
}

fn compose_file_args(files: &[String]) -> Vec<String> {
    files
        .iter()
        .flat_map(|f| ["-f".to_string(), f.clone()])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::remote_script;

    #[test]
    fn remote_script_quotes_each_arg() {
        assert_eq!(
            remote_script(&["exec", "my container", "sh"]),
            "docker 'exec' 'my container' 'sh'"
        );
    }
}
