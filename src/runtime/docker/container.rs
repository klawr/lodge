//! One container, driven through the docker CLI: running commands in it,
//! inspecting it, and resolving its peers.

use anyhow::{Context, Result, bail};

use super::DockerCli;
use crate::runtime::{Container, Env, ExecOpts, Output, User};
use crate::shell::shell_quote;
use crate::target::Target;

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

// True if `name` follows the compose container naming convention
// <project>-<service>-<index> (or the legacy underscore form) for `service`.
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
    fn target(&self) -> &Target {
        &self.cli.target
    }

    fn exec(&self, argv: &[&str], opts: &ExecOpts) -> Result<()> {
        let args = self.exec_args(argv, opts);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let status = self
            .cli
            .command(&refs)
            .status()
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
            .command(&refs)
            .output()
            .context("spawning docker exec")?;
        Ok(Output {
            success: out.status.success(),
            stdout: String::from_utf8(out.stdout)?,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// `docker cp <src> <id>:<dest>`; see [`copy`](super::copy).
    fn cp_in(&self, src: &str, dest: &str) -> Result<()> {
        self.copy_in(src, dest)
    }

    fn write_secret(&self, dest: &str, contents: &[u8], mode: u32, user: User) -> Result<()> {
        use std::io::Write as _;
        let script = crate::runtime::WRITE_SECRET_SCRIPT.replace("%MODE%", &format!("{mode:o}"));
        // The value goes on stdin; argv carries only the script and the path.
        let opts = ExecOpts {
            user,
            ..ExecOpts::default()
        };
        let mut args = self.exec_args(&["sh", "-c", &script, dest], &opts);
        args.insert(1, "-i".into());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        // `command` closes stdin for ssh; this one streams, so override it.
        let mut cmd = self.cli.command(&refs);
        cmd.stdin(std::process::Stdio::piped());
        let mut child = cmd.spawn().context("spawning docker exec")?;
        let written = child
            .stdin
            .take()
            .context("docker exec stdin was not piped")?
            .write_all(contents);
        let status = child.wait().context("waiting for docker exec")?;
        if !status.success() {
            bail!("writing secret file {dest} failed ({status})");
        }
        written.context("sending secret to docker exec")
    }

    fn interactive_command(&self, user: User, workdir: &str, inner: &str) -> String {
        let mut exec = String::from("docker exec -it");
        match user {
            User::Default => {}
            User::Root => exec.push_str(" -u 0"),
            User::Named(u) => exec.push_str(&format!(" -u {}", shell_quote(u))),
        }
        exec.push_str(&format!(
            " --workdir {} {} sh -c {}",
            shell_quote(workdir),
            shell_quote(&self.id),
            shell_quote(inner)
        ));
        match &self.cli.target {
            Target::Local => exec,
            // Run the docker CLI *on the remote* over `ssh -tt`.
            Target::Ssh(host) => crate::ssh::interactive_line(host, &exec),
        }
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

    fn is_running(&self) -> bool {
        self.cli
            .capture_ok(&["inspect", "--format", "{{.State.Running}}", &self.id])
            .is_some_and(|out| out.trim() == "true")
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

    #[test]
    fn compose_naming_matches_service() {
        assert!(compose_service_matches("proj-db-1", "db"));
        assert!(compose_service_matches("proj_db_1", "db"));
        assert!(!compose_service_matches("proj-db", "db"));
        assert!(!compose_service_matches("proj-mydb-1", "db"));
    }
}
