//! One container, driven through the docker CLI: running commands in it,
//! inspecting it, and resolving its peers.

use anyhow::{Context, Result, bail};

use super::DockerCli;
use crate::host::Repeat;
use crate::runtime::{Container, ContainerUser, Env, ExecOpts, Output, User};

/// One container, driven through the docker CLI.
pub struct DockerContainer {
    pub(super) cli: DockerCli,
    pub(super) id: String,
}

impl DockerContainer {
    /// `exec [-u U] [--workdir W] [-e K=V …] <id> <argv…>`.
    fn exec_args(&self, argv: &[&str], opts: &ExecOpts) -> Vec<String> {
        let mut a: Vec<String> = vec!["exec".into()];
        match opts.user {
            User::Default => {}
            User::Root => a.extend(["-u".into(), "0".into()]),
            User::Named(u) => a.extend(["-u".into(), u.into()]),
        }
        if let Some(wd) = opts.workdir {
            a.extend(["--workdir".into(), wd.into()]);
        }
        for (k, v) in opts.env {
            a.extend(["-e".into(), format!("{k}={v}")]);
        }
        a.push(self.id.clone());
        a.extend(argv.iter().map(|s| s.to_string()));
        a
    }

    // Resolves `host` via DNS inside the container: Docker's embedded DNS knows
    // compose service names, container names and network aliases on user-defined
    // networks, which is exactly what the devcontainer's own processes see.
    fn resolve_via_container_dns(&self, host: &str) -> Option<String> {
        let out = self
            .cli
            .capture_ok(&["exec", &self.id, "getent", "hosts", host])?;
        // getent output: "<addr>  <name> ..." per line; prefer an IPv4 address.
        let addrs: Vec<std::net::IpAddr> = out
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|a| a.parse().ok())
            .collect();
        addrs
            .iter()
            .find(|a| a.is_ipv4())
            .or(addrs.first())
            .map(|a| a.to_string())
    }

    // Looks for a container on this one's networks, matched by container name or
    // compose naming convention. Covers the default bridge network (no embedded
    // DNS) and containers without a shell or `getent`.
    fn resolve_via_networks(&self, host: &str) -> Option<String> {
        let nets = self.cli.capture_ok(&[
            "inspect",
            "--format",
            "{{range $k, $v := .NetworkSettings.Networks}}{{$k}}\n{{end}}",
            &self.id,
        ])?;
        for net in nets.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let containers = self.cli.capture_ok(&[
                "network",
                "inspect",
                "--format",
                "{{range .Containers}}{{.Name}} {{.IPv4Address}}\n{{end}}",
                net,
            ])?;
            for line in containers.lines() {
                let mut cols = line.split_whitespace();
                let (Some(name), Some(addr)) = (cols.next(), cols.next()) else {
                    continue;
                };
                if name == host || compose_service_matches(name, host) {
                    // IPv4Address is CIDR-form ("172.18.0.5/16").
                    return addr.split('/').next().map(String::from);
                }
            }
        }
        None
    }
}

/// Whether docker's error says the container doesn't exist (as opposed to the
/// daemon or the connection to it failing). The wording's case has changed
/// between docker versions ("No such object", "no such object").
fn is_no_such_container(stderr: &str) -> bool {
    let err = stderr.to_ascii_lowercase();
    err.contains("no such object") || err.contains("no such container")
}

// True if `name` follows the compose container naming convention
// <project>-<service>-<index> (or the underscore form of older compose) for `service`.
fn compose_service_matches(name: &str, service: &str) -> bool {
    ['-', '_'].iter().any(|&sep| {
        name.rsplit_once(sep).is_some_and(|(stem, index)| {
            !index.is_empty()
                && index.chars().all(|c| c.is_ascii_digit())
                && stem
                    .strip_suffix(service)
                    .is_some_and(|project| project.ends_with(sep))
        })
    })
}
impl Env for DockerContainer {
    fn exec(&self, argv: &[&str], opts: &ExecOpts) -> Result<()> {
        let args = self.exec_args(argv, opts);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let status = self
            .cli
            .host
            .attempt(opts.repeat, || self.cli.command(&refs).status())
            .context("spawning docker exec")?;
        if !status.success() {
            bail!("docker exec failed: {argv:?}");
        }
        Ok(())
    }

    fn output(&self, argv: &[&str], opts: &ExecOpts) -> Result<Output> {
        let args = self.exec_args(argv, opts);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self
            .cli
            .host
            .attempt(opts.repeat, || self.cli.command(&refs).output())
            .context("spawning docker exec")?;
        Ok(Output {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// A tar stream through `docker cp -a -`; see [`copy`](super::copy).
    fn put(&self, src: &str, dest: &str, owner: &ContainerUser) -> Result<()> {
        self.copy_in(src, dest, owner)
    }

    fn write_secret(&self, dest: &str, contents: &[u8], mode: u32, user: User) -> Result<()> {
        use std::io::Write as _;
        let script = crate::runtime::WRITE_SECRET_SCRIPT.replace("%MODE%", &format!("{mode:o}"));
        // The value goes on stdin; argv carries only the script and the path.
        let argv = crate::shell::script_argv(&script, &[dest]);
        let mut args = self.exec_args(&argv, &ExecOpts::as_user(user));
        args.insert(1, "-i".into());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        // The script writes to a temporary file and renames it into place, so
        // sending the secret again after a lost connection is harmless.
        let (status, written) = self
            .cli
            .host
            .attempt(Repeat::Safe, || {
                // `command` closes stdin; this one streams, so override it.
                let mut cmd = self.cli.command(&refs);
                cmd.stdin(std::process::Stdio::piped());
                let mut child = cmd.spawn()?;
                let written = child
                    .stdin
                    .take()
                    .ok_or_else(|| std::io::Error::other("docker exec stdin was not piped"))?
                    .write_all(contents);
                Ok((child.wait()?, written))
            })
            .context("running docker exec")?;
        if !status.success() {
            bail!("writing secret file {dest} failed ({status})");
        }
        written.context("sending secret to docker exec")
    }

    fn interactive_command(&self, user: User, workdir: &str, inner: &str) -> std::process::Command {
        let opts = ExecOpts {
            workdir: Some(workdir),
            ..ExecOpts::as_user(user)
        };
        let mut args = self.exec_args(&["sh", "-c", inner], &opts);
        args.insert(1, "-it".into());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        // The host decides how a terminal reaches the docker CLI.
        self.cli.interactive(&refs)
    }
}

impl Container for DockerContainer {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> Option<String> {
        let out = self
            .cli
            .capture_ok(&["inspect", "--format", "{{.Name}}", &self.id])?;
        let name = out.trim().trim_start_matches('/');
        (!name.is_empty()).then(|| name.to_string())
    }

    fn configured_user(&self) -> Option<String> {
        let out = self
            .cli
            .capture_ok(&["inspect", "--format", "{{.Config.User}}", &self.id])?;
        let user = out.trim();
        (!user.is_empty()).then(|| user.to_string())
    }

    fn mounts(&self) -> Vec<String> {
        let Some(out) = self.cli.capture_ok(&[
            "inspect",
            "--format",
            "{{range .Mounts}}{{.Destination}}\n{{end}}",
            &self.id,
        ]) else {
            return vec![];
        };
        out.lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
    }

    fn is_running(&self) -> Result<bool> {
        let args = ["inspect", "--format", "{{.State.Running}}", &self.id];
        let out = self
            .cli
            .host
            .attempt(Repeat::Safe, || self.cli.command(&args).output())
            .context("spawning docker inspect")?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).trim() == "true");
        }
        let err = String::from_utf8_lossy(&out.stderr);
        if is_no_such_container(&err) {
            return Ok(false);
        }
        bail!("docker inspect failed: {}", err.trim())
    }

    /// `docker cp <id>:<path> -`; see [`copy`](super::copy).
    fn read_stored_file(&self, path: &str) -> Result<Option<String>> {
        self.copy_out(path)
    }

    fn ip_address(&self) -> Option<String> {
        let out = self.cli.capture_ok(&[
            "inspect",
            "--format",
            "{{range .NetworkSettings.Networks}}{{.IPAddress}}\n{{end}}",
            &self.id,
        ])?;
        out.lines()
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(String::from)
    }

    fn resolve_peer(&self, host: &str) -> Option<String> {
        self.resolve_via_container_dns(host)
            .or_else(|| self.resolve_via_networks(host))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{LocalHost, SshHost};
    use std::sync::Arc;

    /// The program and arguments of a command, as one list.
    fn words(c: &std::process::Command) -> Vec<String> {
        std::iter::once(c.get_program())
            .chain(c.get_args())
            .map(|w| w.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_interactive_command_is_the_exec_command_with_a_tty_run_on_the_host() {
        let local = DockerContainer {
            cli: DockerCli::new(Arc::new(LocalHost)),
            id: "abc".into(),
        };
        assert_eq!(
            words(&local.interactive_command(User::Named("dev"), "/w", "exec bash -l")),
            [
                "docker",
                "exec",
                "-it",
                "-u",
                "dev",
                "--workdir",
                "/w",
                "abc",
                "sh",
                "-c",
                "exec bash -l"
            ]
        );
        // On another host it is the host that decides how the terminal gets there.
        let remote = DockerContainer {
            cli: DockerCli::new(Arc::new(SshHost::new("h"))),
            id: "abc".into(),
        };
        let line = words(&remote.interactive_command(User::Root, "/", "x"));
        assert_eq!(line[0], "ssh");
        assert!(line.contains(&"-tt".to_string()), "{line:?}");
        assert_eq!(
            line.last().unwrap(),
            "'docker' 'exec' '-it' '-u' '0' '--workdir' '/' 'abc' 'sh' '-c' 'x'"
        );
    }

    #[test]
    fn a_missing_container_is_told_from_an_unreachable_engine() {
        assert!(is_no_such_container("Error: No such object: abc\n"));
        // docker 29
        assert!(is_no_such_container(
            "Error: no such object: a94e3e83b066\n"
        ));
        assert!(is_no_such_container(
            "Error response from daemon: No such container: abc"
        ));
        assert!(!is_no_such_container(
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock"
        ));
        assert!(!is_no_such_container(
            "ssh: connect to host h port 22: Connection refused"
        ));
    }

    #[test]
    fn compose_naming_matches_service() {
        assert!(compose_service_matches("proj-db-1", "db"));
        assert!(compose_service_matches("proj_db_1", "db"));
        assert!(!compose_service_matches("proj-db", "db"));
        assert!(!compose_service_matches("proj-mydb-1", "db"));
    }
}
