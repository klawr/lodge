//! Lifecycle hooks (`onCreateCommand`, `postStartCommand`, …) and the
//! environment they run with. The env written for login shells is
//! [`profile`](super::profile)'s.
//!
//! A hook may be a shell string, an argv array, or a map of named commands.
//! `initializeCommand` runs on the host (or the remote host with `--remote`);
//! every other hook runs inside the container.

use crate::output::{info, warning};
use anyhow::{Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

use crate::runtime::{Env, ExecOpts};
use crate::shell::CmdLine;
use crate::target::Target;

// A devcontainer lifecycle command in any of its three spec shapes.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(super) enum Cmd {
    Shell(String),
    Exec(Vec<String>),
    Named(BTreeMap<String, OneCmd>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(super) enum OneCmd {
    Shell(String),
    Exec(Vec<String>),
}

impl OneCmd {
    /// The command to run; `None` for an empty argv, which runs nothing.
    fn step(&self) -> Option<CmdLine<'_>> {
        match self {
            OneCmd::Shell(s) => Some(CmdLine::Shell(s)),
            OneCmd::Exec(argv) if argv.is_empty() => None,
            OneCmd::Exec(argv) => Some(CmdLine::Argv(argv)),
        }
    }
}

impl Cmd {
    /// The commands this hook stands for, each with its name when the hook is
    /// a named map. An empty argv runs nothing and is left out.
    fn steps(&self) -> Vec<(Option<&str>, CmdLine<'_>)> {
        match self {
            Cmd::Shell(s) => vec![(None, CmdLine::Shell(s))],
            Cmd::Exec(argv) if argv.is_empty() => vec![],
            Cmd::Exec(argv) => vec![(None, CmdLine::Argv(argv))],
            Cmd::Named(map) => map
                .iter()
                .filter_map(|(name, c)| Some((Some(name.as_str()), c.step()?)))
                .collect(),
        }
    }
}

#[derive(Default)]
pub(super) struct Lifecycle {
    pub(super) initialize: Option<Cmd>,
    pub(super) on_create: Option<Cmd>,
    pub(super) update_content: Option<Cmd>,
    pub(super) post_create: Option<Cmd>,
    pub(super) post_start: Option<Cmd>,
    pub(super) post_attach: Option<Cmd>,
}

// Env to pass to hooks via `docker exec -e`. Values referencing other vars
// (${...}) are dropped here — they'd be set literally rather than expanded —
// and reach hooks via the sourced profile instead.
pub(super) fn exec_env(env: &BTreeMap<String, String>) -> Vec<(String, String)> {
    env.iter()
        .filter(|(_, v)| !v.contains("${"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Where and as whom container-side hooks run: the workspace folder, as
/// `remoteUser` (the spec's user for lifecycle commands; `None` is the
/// container's default user), with `containerEnv`/`remoteEnv` exported.
pub(super) struct HookCtx<'a> {
    pub workdir: &'a str,
    pub user: Option<&'a str>,
    pub vars: &'a [(String, String)],
}

// Runs a lifecycle command inside the container. A failing command is reported
// but never aborts lodge. Returns true only if every command succeeded (used to
// decide whether to mark the hook done).
pub(super) fn run_in_container(env: &dyn Env, ctx: &HookCtx, label: &str, cmd: &Cmd) -> bool {
    info!("{label}");
    let opts = ExecOpts {
        workdir: Some(ctx.workdir),
        env: ctx.vars,
        user: ctx.user.into(),
    };
    let mut ok = true;
    for (name, step) in cmd.steps() {
        if let Some(name) = name {
            info!("  {name}");
        }
        let argv: Vec<&str> = match step {
            CmdLine::Shell(s) => vec!["sh", "-c", s],
            CmdLine::Argv(argv) => argv.iter().map(String::as_str).collect(),
        };
        if let Err(e) = env.exec(&argv, &opts) {
            warning!("{} failed (continuing): {e}", name.unwrap_or(label));
            ok = false;
        }
    }
    ok
}

// Runs initializeCommand on the host, with the .devcontainer dir as cwd. The
// "host" is wherever the project files live: the local machine, or — with
// --remote — the remote host, over ssh (the dir doesn't exist locally there).
pub(super) fn run_on_host(dir: &Path, label: &str, cmd: &Cmd, target: &Target) -> Result<()> {
    info!("{label} (host)");
    for (name, step) in cmd.steps() {
        if let Some(name) = name {
            info!("  {name}");
        }
        if !target.run_in(dir, &step)?.success() {
            bail!("{} failed", name.unwrap_or(label));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeEnv;

    #[test]
    fn cmd_parses_all_three_forms() {
        assert!(matches!(
            serde_json::from_str::<Cmd>(r#""echo hi""#).unwrap(),
            Cmd::Shell(_)
        ));
        assert!(matches!(
            serde_json::from_str::<Cmd>(r#"["echo", "hi"]"#).unwrap(),
            Cmd::Exec(_)
        ));
        assert!(matches!(
            serde_json::from_str::<Cmd>(r#"{"a": "echo hi", "b": ["echo", "yo"]}"#).unwrap(),
            Cmd::Named(_)
        ));
    }

    #[test]
    fn every_form_becomes_steps_and_empty_argv_runs_nothing() {
        let cmd: Cmd =
            serde_json::from_str(r#"{"a": "echo hi", "b": ["ls", "-l"], "c": []}"#).unwrap();
        let steps = cmd.steps();
        assert_eq!(steps.len(), 2);
        assert!(matches!(steps[0], (Some("a"), CmdLine::Shell("echo hi"))));
        assert!(matches!(steps[1], (Some("b"), CmdLine::Argv(a)) if a == ["ls", "-l"]));
        assert!(Cmd::Exec(vec![]).steps().is_empty());

        // In the container, too: an empty argv is not handed to the engine.
        let env = FakeEnv::new();
        let ctx = HookCtx {
            workdir: "/w",
            user: None,
            vars: &[],
        };
        assert!(run_in_container(&env, &ctx, "x", &Cmd::Exec(vec![])));
    }
}
