//! Lifecycle hooks (`onCreateCommand`, `postStartCommand`, …) and the
//! environment they run with. The env written for login shells is
//! [`profile`](super::profile)'s.
//!
//! A hook may be a shell string, an argv array, or a map of named commands.
//! `initializeCommand` runs on the host (or the remote host with `--remote`);
//! every other hook runs inside the container.

use super::markers;
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
pub(super) enum Hook {
    Shell(String),
    Exec(Vec<String>),
    Named(BTreeMap<String, HookStep>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(super) enum HookStep {
    Shell(String),
    Exec(Vec<String>),
}

impl HookStep {
    /// The command to run; `None` for an empty argv, which runs nothing.
    fn step(&self) -> Option<CmdLine<'_>> {
        match self {
            HookStep::Shell(s) => Some(CmdLine::Shell(s)),
            HookStep::Exec(argv) if argv.is_empty() => None,
            HookStep::Exec(argv) => Some(CmdLine::Argv(argv)),
        }
    }
}

impl Hook {
    /// The commands this hook stands for, each with its name when the hook is
    /// a named map. An empty argv runs nothing and is left out.
    fn steps(&self) -> Vec<(Option<&str>, CmdLine<'_>)> {
        match self {
            Hook::Shell(s) => vec![(None, CmdLine::Shell(s))],
            Hook::Exec(argv) if argv.is_empty() => vec![],
            Hook::Exec(argv) => vec![(None, CmdLine::Argv(argv))],
            Hook::Named(map) => map
                .iter()
                .filter_map(|(name, c)| Some((Some(name.as_str()), c.step()?)))
                .collect(),
        }
    }
}

#[derive(Default)]
pub(super) struct Lifecycle {
    pub(super) initialize: Option<Hook>,
    pub(super) on_create: Option<Hook>,
    pub(super) update_content: Option<Hook>,
    pub(super) post_create: Option<Hook>,
    pub(super) post_start: Option<Hook>,
    pub(super) post_attach: Option<Hook>,
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
pub(super) fn run_in_container(env: &dyn Env, ctx: &HookCtx, label: &str, cmd: &Hook) -> bool {
    info!("{label}");
    let opts = ExecOpts {
        workdir: Some(ctx.workdir),
        env: ctx.vars,
        user: ctx.user.into(),
        // A hook is the user's, so it is never run a second time on its own.
        ..ExecOpts::default()
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
            warning!("{} failed (continuing): {e:#}", name.unwrap_or(label));
            ok = false;
        }
    }
    ok
}

/// Runs the create-time hooks (`onCreateCommand`, `updateContentCommand`,
/// `postCreateCommand`), each once *successfully*: one is skipped if its success
/// marker exists, and the marker is written only when it succeeds, so a failed
/// hook retries on the next `ldg up` rather than only on a recreate. A failing
/// hook is reported but never aborts lodge; failing to record a success does.
pub(super) fn run_create_hooks(env: &dyn Env, lifecycle: &Lifecycle, ctx: &HookCtx) -> Result<()> {
    for (label, key, hook) in [
        ("onCreateCommand", "onCreate", &lifecycle.on_create),
        (
            "updateContentCommand",
            "updateContent",
            &lifecycle.update_content,
        ),
        ("postCreateCommand", "postCreate", &lifecycle.post_create),
    ] {
        let Some(hook) = hook else { continue };
        // Not knowing whether it ran is not "it didn't": a create hook need
        // not be safe to run twice, so it is skipped rather than repeated.
        match markers::done(env, key) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(e) => {
                warning!("skipping {label}: could not tell whether it already ran: {e:#}");
                continue;
            }
        }
        if run_in_container(env, ctx, label, hook) {
            markers::mark_done(env, key)?;
        }
    }
    Ok(())
}

// Runs initializeCommand on the host, with the .devcontainer dir as cwd. The
// "host" is wherever the project files live: the local machine, or — with
// --remote — the remote host, over ssh (the dir doesn't exist locally there).
pub(super) fn run_on_host(dir: &Path, label: &str, cmd: &Hook, target: &Target) -> Result<()> {
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
    fn a_failed_create_hook_is_retried_until_it_succeeds_and_then_never_again() {
        let step = ["sh", "-c", "make setup"];
        let env = FakeEnv::new().with_failure(&step);
        let lifecycle = Lifecycle {
            post_create: Some(Hook::Shell("make setup".into())),
            ..Lifecycle::default()
        };
        let ctx = HookCtx {
            workdir: "/w",
            user: Some("dev"),
            vars: &[],
        };
        let runs = |env: &FakeEnv| env.calls().iter().filter(|(c, _)| c == &step).count();

        run_create_hooks(&env, &lifecycle, &ctx).unwrap();
        assert_eq!(runs(&env), 1);
        assert!(
            env.file("/opt/lodge/state/postCreate").is_none(),
            "failed, so not marked"
        );

        env.recover(&step);
        run_create_hooks(&env, &lifecycle, &ctx).unwrap();
        assert_eq!(runs(&env), 2, "retried");
        assert!(env.file("/opt/lodge/state/postCreate").is_some());

        run_create_hooks(&env, &lifecycle, &ctx).unwrap();
        assert_eq!(runs(&env), 2, "a hook that succeeded is not run again");
    }

    #[test]
    fn hooks_parse_all_three_forms() {
        assert!(matches!(
            serde_json::from_str::<Hook>(r#""echo hi""#).unwrap(),
            Hook::Shell(_)
        ));
        assert!(matches!(
            serde_json::from_str::<Hook>(r#"["echo", "hi"]"#).unwrap(),
            Hook::Exec(_)
        ));
        assert!(matches!(
            serde_json::from_str::<Hook>(r#"{"a": "echo hi", "b": ["echo", "yo"]}"#).unwrap(),
            Hook::Named(_)
        ));
    }

    #[test]
    fn every_form_becomes_steps_and_empty_argv_runs_nothing() {
        let cmd: Hook =
            serde_json::from_str(r#"{"a": "echo hi", "b": ["ls", "-l"], "c": []}"#).unwrap();
        let steps = cmd.steps();
        assert_eq!(steps.len(), 2);
        assert!(matches!(steps[0], (Some("a"), CmdLine::Shell("echo hi"))));
        assert!(matches!(steps[1], (Some("b"), CmdLine::Argv(a)) if a == ["ls", "-l"]));
        assert!(Hook::Exec(vec![]).steps().is_empty());

        // In the container, too: an empty argv is not handed to the engine.
        let env = FakeEnv::new();
        let ctx = HookCtx {
            workdir: "/w",
            user: None,
            vars: &[],
        };
        assert!(run_in_container(&env, &ctx, "x", &Hook::Exec(vec![])));
    }
}
