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
// Lines are written verbatim (each shell-quoted for printf), so ${VAR}
// references inside values expand when the profile is sourced.
pub(super) fn write_env_profile(env: &dyn Env, vars: &BTreeMap<String, String>) -> Result<()> {
    if vars.is_empty() {
        return Ok(());
    }
    let mut script = String::new();
    for (k, v) in vars {
        script.push_str(&format!("export {k}={}\n", quote_env_value(v)));
    }
    env.write_file("/etc/profile.d/lodge-env.sh", script.as_bytes())
}

// Renders an env value for a POSIX `export K=<value>` line. Literal runs are
// shell-quoted so spaces and metacharacters (`code --wait`, `a;b`, `a'b`) are
// safe, while `${VAR}` references are left bare so the login shell still expands
// them — `normalize_env_value` deliberately rewrites `${containerEnv:X}` to
// `${X}` for exactly that, so quoting the whole value would break it.
fn quote_env_value(v: &str) -> String {
    let mut out = String::new();
    let mut rest = v;
    while let Some(start) = rest.find("${") {
        match rest[start..].find('}') {
            Some(end_rel) => {
                let end = start + end_rel + 1; // include the closing '}'
                if start > 0 {
                    out.push_str(&crate::shell::shell_quote(&rest[..start]));
                }
                out.push_str(&rest[start..end]); // ${VAR} kept bare
                rest = &rest[end..];
            }
            // No closing brace — treat the remainder as a literal.
            None => break,
        }
    }
    if !rest.is_empty() {
        out.push_str(&crate::shell::shell_quote(rest));
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

// Runs a lifecycle command inside the container, in the workspace folder, with
// `env` exported. A failing command is reported but never aborts lodge. Returns
// true only if every command succeeded (used to decide whether to mark the hook
// done).
pub(super) fn run_in_container(
    env: &dyn Env,
    workdir: &str,
    vars: &[(String, String)],
    label: &str,
    cmd: &Cmd,
) -> bool {
    info!("{label}");
    let opts = ExecOpts {
        workdir: Some(workdir),
        env: vars,
        ..ExecOpts::default()
    };
    let run = |argv: &[&str]| -> bool {
        match env.exec(argv, &opts) {
            Ok(()) => true,
            Err(e) => {
                warning!("{label} failed (continuing): {e}");
                false
            }
        }
    };
    match cmd {
        Cmd::Shell(s) => run(&["sh", "-c", s]),
        Cmd::Exec(argv) => run(&argv.iter().map(String::as_str).collect::<Vec<_>>()),
        Cmd::Named(map) => {
            let mut ok = true;
            for (name, one) in map {
                info!("  {name}");
                ok &= match one {
                    OneCmd::Shell(s) => run(&["sh", "-c", s]),
                    OneCmd::Exec(argv) => run(&argv.iter().map(String::as_str).collect::<Vec<_>>()),
                };
            }
            ok
        }
    }
}

// Runs initializeCommand on the host, with the .devcontainer dir as cwd. The
// "host" is wherever the project files live: the local machine, or — with
// --remote — the remote host, over ssh (the dir doesn't exist locally there).
pub(super) fn run_on_host(dir: &Path, label: &str, cmd: &Cmd, target: &Target) -> Result<()> {
    info!("{label} (host)");

    let run_one = |what: &str, one: &OneCmd| -> Result<()> {
        let host_cmd = match one {
            OneCmd::Shell(script) => HostCmd::Shell(script),
            OneCmd::Exec(argv) => HostCmd::Argv(argv),
        };
        if !target.run_in(dir, &host_cmd)?.success() {
            bail!("{what} failed");
        }
        Ok(())
    };

    match cmd {
        Cmd::Shell(s) => run_one(label, &OneCmd::Shell(s.clone())),
        Cmd::Exec(argv) if !argv.is_empty() => run_one(label, &OneCmd::Exec(argv.clone())),
        Cmd::Exec(_) => Ok(()),
        Cmd::Named(map) => {
            for (name, one) in map {
                info!("  {name}");
                match one {
                    OneCmd::Shell(_) => run_one(name, one)?,
                    OneCmd::Exec(argv) if !argv.is_empty() => run_one(name, one)?,
                    OneCmd::Exec(_) => {}
                }
            }
            Ok(())
        }
    }
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
