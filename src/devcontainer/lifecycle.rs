//! Lifecycle hooks (`onCreateCommand`, `postStartCommand`, …) and the
//! environment they run with.
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
use crate::target::{HostCmd, Target};

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
    fn step(&self) -> Option<HostCmd<'_>> {
        match self {
            OneCmd::Shell(s) => Some(HostCmd::Shell(s)),
            OneCmd::Exec(argv) if argv.is_empty() => None,
            OneCmd::Exec(argv) => Some(HostCmd::Argv(argv)),
        }
    }
}

impl Cmd {
    /// The commands this hook stands for, each with its name when the hook is
    /// a named map. An empty argv runs nothing and is left out.
    fn steps(&self) -> Vec<(Option<&str>, HostCmd<'_>)> {
        match self {
            Cmd::Shell(s) => vec![(None, HostCmd::Shell(s))],
            Cmd::Exec(argv) if argv.is_empty() => vec![],
            Cmd::Exec(argv) => vec![(None, HostCmd::Argv(argv))],
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

// Writes containerEnv/remoteEnv to /etc/profile.d so login shells inherit it.
pub(super) fn write_env_profile(env: &dyn Env, vars: &BTreeMap<String, String>) -> Result<()> {
    if vars.is_empty() {
        return Ok(());
    }
    write_profile(env, crate::layout::PROFILE_ENV, vars)
}

/// Writes `vars` as `export K=<value>` lines to the profile script at `path`.
/// Values are quoted with [`quote_env_value`], so literals are safe and
/// variable references still expand when the profile is sourced.
pub(super) fn write_profile(
    env: &dyn Env,
    path: &str,
    vars: &BTreeMap<String, String>,
) -> Result<()> {
    let mut script = String::new();
    for (k, v) in vars {
        script.push_str(&format!("export {k}={}\n", quote_env_value(v)));
    }
    env.write_file(path, script.as_bytes())
}

// Renders an env value for a POSIX `export K=<value>` line. Literal runs are
// shell-quoted so spaces and metacharacters (`code --wait`, `a;b`, `a'b`) are
// safe, while `${VAR}` and `$VAR` references are left bare so the login shell
// still expands them — `normalize_env_value` deliberately rewrites
// `${containerEnv:X}` to `${X}` for exactly that, and feature metadata writes
// `PATH=/x/bin:$PATH`, so quoting the whole value would break both.
fn quote_env_value(v: &str) -> String {
    let mut out = String::new();
    let mut literal = String::new();
    let mut rest = v;
    while let Some(start) = rest.find('$') {
        let after = &rest[start + 1..];
        let len = if after.starts_with('{') {
            after.find('}').map(|end| end + 1)
        } else {
            let n = after
                .char_indices()
                .take_while(|&(i, c)| {
                    c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit())
                })
                .count();
            (n > 0).then_some(n)
        };
        match len {
            Some(len) => {
                literal.push_str(&rest[..start]);
                if !literal.is_empty() {
                    out.push_str(&crate::shell::shell_quote(&literal));
                    literal.clear();
                }
                out.push_str(&rest[start..=start + len]); // the reference, bare
                rest = &rest[start + 1 + len..];
            }
            // A `$` that starts no reference is a literal.
            None => {
                literal.push_str(&rest[..=start]);
                rest = after;
            }
        }
    }
    literal.push_str(rest);
    if !literal.is_empty() || out.is_empty() {
        out.push_str(&crate::shell::shell_quote(&literal));
    }
    out
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
            HostCmd::Shell(s) => vec!["sh", "-c", s],
            HostCmd::Argv(argv) => argv.iter().map(String::as_str).collect(),
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
        assert!(matches!(steps[0], (Some("a"), HostCmd::Shell("echo hi"))));
        assert!(matches!(steps[1], (Some("b"), HostCmd::Argv(a)) if a == ["ls", "-l"]));
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

    #[test]
    fn quote_env_value_quotes_literals_but_expands_vars() {
        // Plain literals with spaces/metacharacters get single-quoted whole.
        assert_eq!(quote_env_value("code --wait"), "'code --wait'");
        assert_eq!(quote_env_value("a;b&&c"), "'a;b&&c'");
        assert_eq!(quote_env_value("it's"), "'it'\\''s'");
        // ${VAR} references stay bare so the login shell still expands them
        // (normalize_env_value rewrites ${containerEnv:X} to ${X}).
        assert_eq!(quote_env_value("${PATH}"), "${PATH}");
        assert_eq!(quote_env_value("${PATH}:/foo"), "${PATH}':/foo'");
        assert_eq!(quote_env_value("${A} ${B}"), "${A}' '${B}");
        assert_eq!(quote_env_value("pre ${X} post"), "'pre '${X}' post'");
        // A dangling ${ with no closing brace is treated as a literal.
        assert_eq!(quote_env_value("a${b"), "'a${b'");
        // $VAR without braces, as feature metadata writes it.
        assert_eq!(quote_env_value("/x/bin:$PATH"), "'/x/bin:'$PATH");
        assert_eq!(quote_env_value("$A$B c"), "$A$B' c'");
        // A `$` that starts no name is literal.
        assert_eq!(quote_env_value("cost $5 or $"), "'cost $5 or $'");
        assert_eq!(quote_env_value(""), "''");
    }

    #[test]
    fn env_profile_quotes_literals_and_keeps_variable_references() {
        let env = FakeEnv::new();
        let vars = BTreeMap::from([
            ("EDITOR".to_string(), "code --wait".to_string()),
            ("PATH".to_string(), "${PATH}:/opt/bin".to_string()),
        ]);
        write_env_profile(&env, &vars).unwrap();
        assert_eq!(
            env.file("/etc/profile.d/lodge-env.sh").unwrap(),
            "export EDITOR='code --wait'\nexport PATH=${PATH}':/opt/bin'\n"
        );
    }

    #[test]
    fn empty_env_writes_no_profile() {
        let env = FakeEnv::new();
        write_env_profile(&env, &BTreeMap::new()).unwrap();
        assert!(env.file("/etc/profile.d/lodge-env.sh").is_none());
    }
}
