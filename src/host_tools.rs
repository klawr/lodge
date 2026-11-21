//! The helper programs lodge may run on this machine, apart from the container
//! engine (`runtime::cli`) and the shell (`host`).
//!
//! One table says what each tool is for and how to get it, so `ldg doctor`, the
//! error messages and the docs say the same thing. Each is started only through
//! [`Tool::command`]; `conventions.rs` checks nothing else spawns them, and
//! `doc_facts.rs` checks the README names every one.

use std::path::PathBuf;
use std::process::Command;

/// What lodge needs a tool for, which decides when `ldg doctor` requires it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// `--remote`.
    Remote,
    /// Packing a bundle from the config.
    Pack,
}

pub struct Tool {
    pub name: &'static str,
    pub need: Need,
    /// What lodge needs it for, as a phrase ("needed to pack").
    pub needed_for: &'static str,
    /// How to get it.
    pub hint: &'static str,
}

impl Tool {
    /// A command for this tool, the one place it is started from.
    pub fn command(&self) -> Command {
        Command::new(self.name)
    }

    /// Where the tool is on `PATH`, if it is.
    pub fn find(&self) -> Option<PathBuf> {
        crate::shell::which(self.name)
    }
}

pub const SSH: Tool = Tool {
    name: "ssh",
    need: Need::Remote,
    needed_for: "needed for --remote",
    hint: "install an OpenSSH client",
};

pub const LDD: Tool = Tool {
    name: "ldd",
    need: Need::Pack,
    needed_for: "needed to pack (not when injecting from a bundle)",
    hint: "it ships with glibc (install your distribution's libc tools)",
};

pub const PATCHELF: Tool = Tool {
    name: "patchelf",
    need: Need::Pack,
    needed_for: "needed to pack (not when injecting from a bundle)",
    hint: "install patchelf (pacman -S patchelf / apt install patchelf)",
};

/// Every tool, in the order `ldg doctor` lists them.
pub const ALL: [&Tool; 3] = [&SSH, &LDD, &PATCHELF];
