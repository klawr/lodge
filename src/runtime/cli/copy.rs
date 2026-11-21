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

use super::Flavor;
use super::container::CliContainer;
use crate::host::{Input, Repeat};
use crate::runtime::{ContainerUser, Env, ExecOpts, tarball};

impl<F: Flavor> CliContainer<F> {
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
        self.exec(
            &["mkdir", "-p", into],
            &ExecOpts::as_user(owner.exec_as()).repeatable(),
        )
        .with_context(|| format!("making {into} as {}", owner.name))?;

        let target = format!("{}:{into}", self.id);
        // Unpacking the same archive again leaves the same files, so a lost
        // connection reruns the whole send, packing the tar afresh.
        let mut packed = Ok(());
        let mut feed = |stdin: &mut dyn std::io::Write| {
            // Dropping the writer closes the pipe, so the engine sees the end of input.
            packed = tarball::pack(source, &name, owner, stdin);
            Ok(())
        };
        let ran = self
            .cli
            .host
            .exec(
                self.cli
                    .job(&self.cli.flavor.cp_in_args(&target))
                    .stdin(Input::Feed(&mut feed))
                    .repeat(Repeat::Safe),
            )
            .with_context(|| format!("running {} cp", self.cli.name()))?;
        if !ran.status.success() {
            bail!("{} cp failed: {src} -> {target}", self.cli.name());
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
            .host
            .exec(
                self.cli
                    .job(&["cp", &from, "-"])
                    .capture()
                    .repeat(Repeat::Safe),
            )
            .with_context(|| format!("spawning {} cp", self.cli.name()))?;
        if !out.status.success() {
            let err = out.stderr_text();
            if self.cli.flavor.says_path_is_missing(&err) {
                return Ok(None);
            }
            bail!("{} cp {from} failed: {}", self.cli.name(), err.trim());
        }
        let mut archive = tar::Archive::new(out.stdout.as_slice());
        let mut entries = archive
            .entries()
            .with_context(|| format!("reading {} cp's tar", self.cli.name()))?;
        let Some(entry) = entries.next() else {
            bail!("{} cp {from} returned an empty archive", self.cli.name());
        };
        let mut entry = entry.with_context(|| format!("reading {} cp's tar", self.cli.name()))?;
        if !entry.header().entry_type().is_file() {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .with_context(|| format!("reading {} cp's tar", self.cli.name()))?;
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }
}
