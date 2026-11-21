//! Copying files out of a container. (In is [`Inside::extract`](crate::runtime::Inside::extract): a tar stream built
//! in-process and handed to `cp -a -`, which unpacks it without a `tar` in the
//! container and keeps the archive's owners.)
//!
//! Out is `cp <id>:<path> -`, a tar on stdout, which works the same
//! locally and over SSH, and on a stopped container too.

use anyhow::{Context, Result, bail};
use std::io::Read;

use super::Flavor;
use super::container::CliContainer;
use crate::host::Repeat;

impl<F: Flavor> CliContainer<F> {
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
