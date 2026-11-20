//! Copying files into a container.
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
use crate::runtime::Env;
use crate::shell::shell_quote;
use crate::target::Target;

impl DockerContainer {
    /// `docker cp <src> <id>:<dest>`, with the semantics [`Env::cp_in`]
    /// promises. With a remote target, `src` is a *local* path that the remote
    /// docker can't see, so the bytes are streamed over SSH.
    pub(super) fn copy_in(&self, src: &str, dest: &str) -> Result<()> {
        match &self.cli.target {
            Target::Local => {
                let to = format!("{}:{dest}", self.id);
                // `dir/.` copies the directory's contents into `dest` whether or
                // not it exists; plain `dir` onto an existing `dest` would nest
                // it as `dest/dir`. The remote path mirrors contents the same way.
                let from = if Path::new(src).is_dir() {
                    format!("{}/.", src.trim_end_matches('/'))
                } else {
                    src.to_string()
                };
                let status = self
                    .cli
                    .command(&["cp", &from, &to])
                    .status()
                    .context("spawning docker cp")?;
                if !status.success() {
                    bail!("docker cp failed: {src} -> {to}");
                }
                Ok(())
            }
            Target::Ssh(_) => self.cp_in_remote(src, dest),
        }
    }

    // Copies a local `src` into the container without a local docker CLI:
    //   - a directory is tar'd locally and `docker cp -`'d into `dest` (the
    //     daemon unpacks the tar, so the container needs no tar);
    //   - a file is streamed straight into `dest` via `docker exec -i … cat`.
    fn cp_in_remote(&self, src: &str, dest: &str) -> Result<()> {
        let meta = std::fs::metadata(src).with_context(|| format!("stat {src}"))?;
        if meta.is_dir() {
            // `docker cp -` extracts into an existing directory, so create it
            // first, then stream the *contents* of `src` (so `dest` mirrors
            // `src` regardless of their basenames).
            self.mkdir_p(&[dest])?;
            let target = format!("{}:{dest}", self.id);
            let mut ssh = self.cli.command(&["cp", "-", &target]);
            ssh.stdin(Stdio::piped());
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
            let mut ssh = self
                .cli
                .command(&["exec", "-u", "0", "-i", &self.id, "sh", "-c", &inner]);
            ssh.stdin(Stdio::from(file));
            let status = ssh.status().context("spawning ssh docker exec for cp")?;
            if !status.success() {
                bail!("docker cp (stream) failed: {} -> {}:{}", src, self.id, dest);
            }
        }
        Ok(())
    }
}
