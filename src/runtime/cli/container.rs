//! One container, driven through the docker CLI: running commands in it,
//! inspecting it, and resolving its peers.

use anyhow::{Context, Result, bail};

use super::{CliEngine, Flavor};
use crate::host::{Input, Repeat};
use crate::runtime::{Container, ContainerUser, Env, ExecOpts, Output, Reach, User};
use std::process::ExitStatus;

/// One container, driven through the docker CLI.
pub struct CliContainer<F: Flavor> {
    pub(super) cli: CliEngine<F>,
    pub(super) id: String,
}

impl<F: Flavor> CliContainer<F> {
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
impl<F: Flavor> Env for CliContainer<F> {
    fn exec(&self, argv: &[&str], opts: &ExecOpts) -> Result<()> {
        let args = self.exec_args(argv, opts);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let ran = self
            .cli
            .host
            .exec(self.cli.job(&refs).repeat(opts.repeat))
            .with_context(|| format!("spawning {} exec", self.cli.name()))?;
        if !ran.status.success() {
            bail!("{} exec failed: {argv:?}", self.cli.name());
        }
        Ok(())
    }

    fn output(&self, argv: &[&str], opts: &ExecOpts) -> Result<Output> {
        let args = self.exec_args(argv, opts);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self
            .cli
            .host
            .exec(self.cli.job(&refs).capture().repeat(opts.repeat))
            .with_context(|| format!("spawning {} exec", self.cli.name()))?;
        Ok(Output {
            success: out.status.success(),
            stdout: out.stdout_text(),
            stderr: out.stderr_text(),
        })
    }

    /// A tar stream through `docker cp -a -`; see [`copy`](super::copy).
    fn put(&self, src: &str, dest: &str, owner: &ContainerUser) -> Result<()> {
        self.copy_in(src, dest, owner)
    }

    fn write_secret(&self, dest: &str, contents: &[u8], mode: u32, user: User) -> Result<()> {
        let script = crate::runtime::WRITE_SECRET_SCRIPT.replace("%MODE%", &format!("{mode:o}"));
        // The value goes on stdin; argv carries only the script and the path.
        let argv = crate::shell::script_argv(&script, &[dest]);
        let mut args = self.exec_args(&argv, &ExecOpts::as_user(user));
        args.insert(1, "-i".into());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        // The script writes to a temporary file and renames it into place, so
        // sending the secret again after a lost connection is harmless.
        let mut feed = |stdin: &mut dyn std::io::Write| stdin.write_all(contents);
        let ran = self
            .cli
            .host
            .exec(
                self.cli
                    .job(&refs)
                    .stdin(Input::Feed(&mut feed))
                    .repeat(Repeat::Safe),
            )
            .with_context(|| format!("running {} exec", self.cli.name()))?;
        if !ran.status.success() {
            bail!("writing secret file {dest} failed ({})", ran.status);
        }
        ran.fed
            .with_context(|| format!("sending secret to {} exec", self.cli.name()))
    }

    fn interactive(&self, user: User, workdir: &str, inner: &str) -> std::io::Result<ExitStatus> {
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

impl<F: Flavor> Container for CliContainer<F> {
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
            .exec(self.cli.job(&args).capture().repeat(Repeat::Safe))
            .with_context(|| format!("spawning {} inspect", self.cli.name()))?;
        if out.status.success() {
            return Ok(out.stdout_text().trim() == "true");
        }
        let err = out.stderr_text();
        if is_no_such_container(&err) {
            return Ok(false);
        }
        bail!("{} inspect failed: {}", self.cli.name(), err.trim())
    }

    /// `docker cp <id>:<path> -`; see [`copy`](super::copy).
    fn read_stored_file(&self, path: &str) -> Result<Option<String>> {
        self.copy_out(path)
    }

    fn reach(&self, peer: Option<&str>) -> Option<Reach> {
        // A rootless engine's containers have an address, but not one the host
        // can connect to.
        if !self.cli.flavor.addresses_are_reachable(&self.cli) {
            return None;
        }
        match peer {
            None => self.ip_address(),
            Some(host) => self.resolve_peer(host),
        }
        .map(Reach::Address)
    }
}

impl<F: Flavor> CliContainer<F> {
    /// The container's address on its first network.
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
    use crate::host::fake::{FakeHost, Reply};
    use crate::runtime::cli::{Docker, DockerFlavor};
    use std::sync::Arc;

    fn container(host: Arc<FakeHost>) -> CliContainer<DockerFlavor> {
        CliContainer {
            cli: Docker::new(host),
            id: "abc".into(),
        }
    }

    fn words(line: &[&str]) -> Vec<String> {
        line.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn the_interactive_session_is_the_exec_command_with_a_tty_run_through_the_host() {
        let host = Arc::new(FakeHost::new());
        container(host.clone())
            .interactive(User::Named("dev"), "/w", "exec bash -l")
            .unwrap();
        assert_eq!(
            host.argvs(),
            [words(&[
                "interactive",
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
            ])]
        );
    }

    #[test]
    fn a_command_in_the_container_is_one_exec_through_the_host_as_the_user_asked() {
        let host = Arc::new(FakeHost::new().on(&["docker", "exec"], "out\n"));
        let c = container(host.clone());
        let opts = ExecOpts::as_user(User::Named("dev"));
        let out = c.output(&["echo", "x"], &opts).unwrap();
        assert!(out.success);
        assert_eq!(out.stdout, "out\n");
        c.exec(&["true"], &ExecOpts::root()).unwrap();
        assert_eq!(
            host.argvs(),
            [
                words(&["docker", "exec", "-u", "dev", "abc", "echo", "x"]),
                words(&["docker", "exec", "-u", "0", "abc", "true"]),
            ]
        );
    }

    #[test]
    fn a_failing_command_is_an_error_for_exec_and_a_value_for_output() {
        let host = Arc::new(FakeHost::new().reply(&["docker", "exec"], Reply::fail(3, "nope")));
        let c = container(host);
        assert!(c.exec(&["false"], &ExecOpts::default()).is_err());
        let out = c.output(&["false"], &ExecOpts::default()).unwrap();
        assert!(!out.success);
        assert_eq!(out.stderr, "nope");
    }

    #[test]
    fn a_secret_travels_on_stdin_and_never_in_the_arguments() {
        let host = Arc::new(FakeHost::new());
        container(host.clone())
            .write_secret("/run/s", b"hunter2", 0o600, User::Default)
            .unwrap();
        let call = &host.calls()[0];
        assert_eq!(call.fed, b"hunter2");
        assert!(call.argv.iter().all(|a| !a.contains("hunter2")), "{call:?}");
        assert!(call.argv.contains(&"-i".to_string()));
    }

    #[test]
    fn a_stopped_container_is_told_from_a_missing_one_and_an_unreachable_engine() {
        let running = |reply| {
            container(Arc::new(
                FakeHost::new().reply(&["docker", "inspect"], reply),
            ))
        };
        assert!(running(Reply::ok("true\n")).is_running().unwrap());
        assert!(!running(Reply::ok("false\n")).is_running().unwrap());
        assert!(
            !running(Reply::fail(1, "Error: No such object: abc"))
                .is_running()
                .unwrap()
        );
        assert!(
            running(Reply::fail(1, "Cannot connect to the Docker daemon"))
                .is_running()
                .is_err()
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
