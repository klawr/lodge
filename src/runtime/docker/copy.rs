//! Copying files into and out of a container.
//!
//! In, it is always a tar stream built in-process ([`tarball`]) and handed to
//! `docker cp -q -a - <id>:<dir>`: locally and over SSH alike (a remote docker can't
//! see local paths, and a stream needs no files at all), without a `tar` in the
//! container (the daemon unpacks it), and with every entry owned by the user the
//! copy is for, because `-a` makes the daemon honour the archive's owners instead
//! of making everything root's. The directory it unpacks into is made first, *as
//! that user*, so what it creates there is theirs too and root is never needed.
//!
//! Out is `docker cp <id>:<path> -`, a tar on stdout, which works the same
//! locally and over SSH, and on a stopped container too.

use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::Path;
use std::process::Stdio;

use super::container::DockerContainer;
use crate::runtime::{ContainerUser, Env, ExecOpts, tarball};

impl DockerContainer {
    /// Copies the local `src` into the container with the semantics
    /// [`Env::put`] promises.
    pub(super) fn copy_in(&self, src: &str, dest: &str, owner: &ContainerUser) -> Result<()> {
        let source = Path::new(src);
        let meta = std::fs::metadata(source).with_context(|| format!("stat {src}"))?;
        // A directory is unpacked *into* `dest`; a file into its parent, under
        // its own name.
        let (into, name) = if meta.is_dir() {
            (dest, String::new())
        } else {
            let dest_path = Path::new(dest);
            let parent = dest_path
                .parent()
                .and_then(Path::to_str)
                .filter(|p| !p.is_empty())
                .with_context(|| format!("{dest} has no parent directory"))?;
            let file = dest_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .with_context(|| format!("{dest} names no file"))?;
            (parent, file)
        };
        self.exec(&["mkdir", "-p", into], &ExecOpts::as_user(owner.exec_as()))
            .with_context(|| format!("making {into} as {}", owner.name))?;

        let target = format!("{}:{into}", self.id);
        // `-q`: on a terminal docker otherwise prints "Successfully copied …" for
        // every file, which would bury lodge's own output.
        let mut cmd = self.cli.command(&["cp", "-q", "-a", "-", &target]);
        // `command` closes stdin for ssh; this one streams, so override it.
        cmd.stdin(Stdio::piped());
        let mut child = cmd.spawn().context("spawning docker cp")?;
        // Dropping the writer closes the pipe, so docker sees the end of input.
        let packed = {
            let stdin = child
                .stdin
                .take()
                .context("docker cp stdin was not piped")?;
            tarball::pack(source, &name, owner, stdin)
        };
        let status = child.wait().context("waiting on docker cp")?;
        if !status.success() {
            bail!("docker cp failed: {src} -> {target}");
        }
        packed.with_context(|| format!("packing {src}"))
    }

    /// The text of the regular file `path`, `None` if there is no such file
    /// (or it is a directory). Read with `docker cp`, so the container need not
    /// run.
    pub(super) fn copy_out(&self, path: &str) -> Result<Option<String>> {
        let from = format!("{}:{path}", self.id);
        let out = self
            .cli
            .command(&["cp", &from, "-"])
            .output()
            .context("spawning docker cp")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if is_missing_path(&err) {
                return Ok(None);
            }
            bail!("docker cp {from} failed: {}", err.trim());
        }
        let mut archive = tar::Archive::new(out.stdout.as_slice());
        let mut entries = archive.entries().context("reading docker cp's tar")?;
        let Some(entry) = entries.next() else {
            bail!("docker cp {from} returned an empty archive");
        };
        let mut entry = entry.context("reading docker cp's tar")?;
        if !entry.header().entry_type().is_file() {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .context("reading docker cp's tar")?;
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }
}

/// Whether `docker cp`'s error says the path doesn't exist in the container (as
/// opposed to the container, the daemon or the connection being the problem).
fn is_missing_path(stderr: &str) -> bool {
    stderr.contains("Could not find the file")
}

#[cfg(test)]
mod tests {
    use super::is_missing_path;

    #[test]
    fn a_missing_file_is_told_from_a_missing_container() {
        assert!(is_missing_path(
            "Error response from daemon: Could not find the file /nope/x in container f4b2"
        ));
        assert!(!is_missing_path(
            "Error response from daemon: No such container: c"
        ));
        assert!(!is_missing_path(
            "ssh: connect to host h port 22: Connection refused"
        ));
    }
}
