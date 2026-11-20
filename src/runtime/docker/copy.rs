//! Copying files into and out of a container.
//!
//! Locally this is `docker cp`. With a remote target the docker CLI runs on the
//! remote host and cannot see local paths, so the bytes are streamed through the
//! SSH connection instead: a directory as a tar (packed and unpacked in-process),
//! a file through `docker exec -i … cat`.

use anyhow::{Context, Result, bail};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;

use super::container::DockerContainer;
use super::remote_script;
use crate::runtime::Env;
use crate::shell::shell_quote;
use crate::target::Target;

impl DockerContainer {
    /// `docker cp <src> <id>:<dest>`. With a remote target, `src` is a *local*
    /// path that the remote docker can't see, so the bytes are streamed over SSH.
    pub(super) fn copy_in(&self, src: &str, dest: &str) -> Result<()> {
        match &self.cli.target {
            Target::Local => {
                let to = format!("{}:{dest}", self.id);
                let status = self
                    .cli
                    .command(&["cp", src, &to])
                    .status()
                    .context("spawning docker cp")?;
                if !status.success() {
                    bail!("docker cp failed: {src} -> {to}");
                }
                Ok(())
            }
            Target::Ssh(host) => self.cp_in_remote(host, src, dest),
        }
    }

    /// `docker cp <id>:<src> <local_dst>` (works on stopped containers). With a
    /// remote target, the remote docker writes a tar to stdout, which is piped
    /// back over SSH and unpacked into `local_dst` here.
    pub(super) fn copy_out(&self, src: &str, local_dst: &str) -> Result<()> {
        match &self.cli.target {
            Target::Local => {
                let from = format!("{}:{src}", self.id);
                let status = self
                    .cli
                    .command(&["cp", &from, local_dst])
                    .status()
                    .context("spawning docker cp")?;
                if !status.success() {
                    bail!("docker cp failed: {from} -> {local_dst}");
                }
                Ok(())
            }
            Target::Ssh(host) => self.cp_out_remote(host, src, local_dst),
        }
    }

    // Streams `docker cp <id>:<src> -` from the remote and unpacks the resulting
    // tar locally, then moves the single top-level entry (named after `src`'s
    // basename) to `local_dst`.
    fn cp_out_remote(&self, host: &str, src: &str, local_dst: &str) -> Result<()> {
        let from = format!("{}:{src}", self.id);
        let mut ssh = crate::ssh::command(host, &remote_script(&["cp", &from, "-"]));
        ssh.stdin(Stdio::null()).stdout(Stdio::piped());
        crate::output::trace(&ssh);
        let mut child = ssh.spawn().context("spawning ssh docker cp")?;
        let stdout = child.stdout.take().context("ssh stdout was not piped")?;

        let staging = std::env::temp_dir().join(format!("lodge-cpout-{}", std::process::id()));
        crate::tmpfile::discard_dir(&staging);
        std::fs::create_dir_all(&staging).context("creating cp staging dir")?;

        let unpacked = tar::Archive::new(stdout).unpack(&staging);
        let ssh_status = child.wait().context("waiting on ssh docker cp")?;

        if unpacked.is_err() || !ssh_status.success() {
            crate::tmpfile::discard_dir(&staging);
            bail!("docker cp failed: {} -> {}", from, local_dst);
        }

        let name = Path::new(src).file_name().unwrap_or_default();
        let extracted = staging.join(name);
        if let Some(parent) = Path::new(local_dst).parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        crate::tmpfile::discard_file(Path::new(local_dst));
        let moved = std::fs::rename(&extracted, local_dst)
            .or_else(|_| std::fs::copy(&extracted, local_dst).map(|_| ()));
        crate::tmpfile::discard_dir(&staging);
        moved.with_context(|| format!("placing extracted file at {local_dst}"))?;
        Ok(())
    }

    // Copies a local `src` into the container without a local docker CLI:
    //   - a directory is tar'd locally and `docker cp -`'d into `dest` (the
    //     daemon unpacks the tar, so the container needs no tar);
    //   - a file is streamed straight into `dest` via `docker exec -i … cat`.
    fn cp_in_remote(&self, host: &str, src: &str, dest: &str) -> Result<()> {
        let meta = std::fs::metadata(src).with_context(|| format!("stat {src}"))?;
        if meta.is_dir() {
            // `docker cp -` extracts into an existing directory, so create it
            // first, then stream the *contents* of `src` (so `dest` mirrors
            // `src` regardless of their basenames).
            self.mkdir_p(&[dest])?;
            let target = format!("{}:{dest}", self.id);
            let mut ssh = crate::ssh::command(host, &remote_script(&["cp", "-", &target]));
            ssh.stdin(Stdio::piped());
            crate::output::trace(&ssh);
            let mut child = ssh.spawn().context("spawning ssh docker cp")?;

            // Pack straight into ssh's stdin. Symlinks are stored as symlinks,
            // not followed. Dropping the builder closes the pipe so the remote
            // docker sees end of input.
            let packed = {
                let stdin = child.stdin.take().context("ssh stdin was not piped")?;
                let mut builder = tar::Builder::new(stdin);
                builder.follow_symlinks(false);
                builder
                    .append_dir_all(".", src)
                    .and_then(|()| builder.finish())
            };
            let status = child.wait().context("waiting on ssh docker cp")?;
            if !status.success() {
                bail!("docker cp failed: {} -> {}", src, target);
            }
            packed.with_context(|| format!("packing {src}"))?;
        } else {
            let file = std::fs::File::open(src).with_context(|| format!("open {src}"))?;
            // `cat > dest` creates dest fresh, so its mode comes from the shell's
            // umask, not from src — silently dropping the execute bit on every
            // binary/shared-lib copy (invisible as root, which bypasses the
            // permission check, but breaks exec for the real `remoteUser`).
            // chmod afterward to restore src's actual mode.
            let mode = meta.permissions().mode() & 0o777;
            let inner = format!(
                "cat > {} && chmod {:o} {}",
                shell_quote(dest),
                mode,
                shell_quote(dest)
            );
            // `-u 0`: local `docker cp` writes as root via the daemon, so this
            // streaming fallback must too, or destinations under root-owned trees
            // (e.g. /opt/lodge) fail in images with an unprivileged default USER.
            let script = remote_script(&["exec", "-u", "0", "-i", &self.id, "sh", "-c", &inner]);
            let mut ssh = crate::ssh::command(host, &script);
            ssh.stdin(Stdio::from(file));
            crate::output::trace(&ssh);
            let status = ssh.status().context("spawning ssh docker exec for cp")?;
            if !status.success() {
                bail!("docker cp (stream) failed: {} -> {}:{}", src, self.id, dest);
            }
        }
        Ok(())
    }
}
