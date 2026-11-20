//! `ldg doctor`: what is available on this host, and what each missing piece
//! would break. Reports everything up front instead of failing mid-run.

use anyhow::Result;
use serde_json::json;
use std::path::{Path, PathBuf};

use super::{Ctx, print_json};
use crate::config::{Config, Multiplexer};
use crate::host_tools::{self, Need, Tool};
use crate::output::{info, line, plural, warning};

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Level {
    Ok,
    /// Needed for something lodge was asked to do.
    Missing,
    /// Only needed for an optional feature.
    Optional,
}

struct Check {
    name: String,
    level: Level,
    detail: String,
}

impl Check {
    fn new(name: impl Into<String>, level: Level, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            level,
            detail: detail.into(),
        }
    }
}

/// Runs every check, prints the report, and returns whether all required checks
/// passed.
pub fn run(ctx: &Ctx, path: Option<PathBuf>, as_json: bool) -> Result<bool> {
    let mut checks = Vec::new();
    let remote = ctx.target.ssh_host();
    let place = remote.map_or("local".to_string(), |h| format!("on {h}"));

    let engine = ctx.rt.version();
    checks.push(match &engine {
        Ok(v) => Check::new("container engine", Level::Ok, format!("{v} ({place})")),
        Err(e) => Check::new("container engine", Level::Missing, e.to_string()),
    });

    // The devcontainer here (if any) decides whether compose and oras matter.
    let needs = ctx
        .project(path)
        .and_then(|p| crate::devcontainer::needs(&p, &ctx.target));
    checks.push(match &needs {
        Ok(n) => Check::new(
            "devcontainer",
            Level::Ok,
            n.config_path.display().to_string(),
        ),
        // Not every folder has one: only `up` needs it, and `up` says so.
        Err(e) => Check::new("devcontainer", Level::Optional, format!("{e:#}")),
    });
    let needs_compose = needs.as_ref().is_ok_and(|n| n.compose);
    let needs_oras = needs.as_ref().is_ok_and(|n| n.registry_features);
    if engine.is_ok() {
        checks.push(match ctx.rt.compose().and_then(|c| c.version()) {
            Ok(v) => Check::new("compose", Level::Ok, v),
            Err(e) if needs_compose => Check::new("compose", Level::Missing, format!("{e:#}")),
            Err(_) => Check::new(
                "compose",
                Level::Optional,
                "only needed for devcontainers with dockerComposeFile",
            ),
        });
    }

    let config = Config::load(&ctx.config_path);
    checks.push(config_check(&ctx.config_path, &config));

    // ldd and patchelf are needed only to *pack*: with a bundle, injection needs
    // neither. So they are required only when this config would pack live.
    let packs_live = config
        .as_ref()
        .is_ok_and(|c| c.bundle.is_none() && !c.inject.is_empty());
    let tmux_wanted = config
        .as_ref()
        .is_ok_and(|c| c.session.multiplexer == Multiplexer::Tmux);
    for tool in host_tools::ALL {
        // (shown at all, required) for what this host and config ask of it.
        let (shown, required) = match tool.need {
            Need::Remote => (remote.is_some(), remote.is_some()),
            Need::Pack => (true, packs_live),
            Need::RegistryFeatures => (true, needs_oras),
            Need::Multiplexer => (tmux_wanted, false),
        };
        if shown {
            checks.push(tool_check(tool, required));
        }
    }

    if let Ok(cfg) = &config {
        if let Some(bundle) = &cfg.bundle {
            checks.push(bundle_check(bundle));
        }
        for item in &cfg.inject {
            if let Some(binary) = &item.binary {
                checks.push(binary_check(&item.name, binary));
            }
        }
        for decl in &cfg.secret {
            checks.push(secret_check(decl));
        }
    }

    let failed = checks.iter().filter(|c| c.level == Level::Missing).count();
    if as_json {
        print_json(&json!({
            "ok": failed == 0,
            "checks": checks.iter().map(|c| json!({"name": c.name, "status": c.level, "detail": c.detail})).collect::<Vec<_>>(),
        }))?;
        return Ok(failed == 0);
    }
    info!("doctor");
    for c in &checks {
        let tag = match c.level {
            Level::Ok => "ok      ",
            Level::Missing => "MISSING ",
            Level::Optional => "optional",
        };
        line!("  {tag}  {:<18} {}", c.name, c.detail);
    }
    if failed > 0 {
        warning!(
            "{} failed",
            plural(failed, "required check", "required checks")
        );
    }
    Ok(failed == 0)
}

/// Whether a declared secret's source resolves on this machine. The value is
/// read to find out, then dropped; only the source's kind is shown.
fn secret_check(decl: &crate::secret::SecretDecl) -> Check {
    use crate::secret::Source;
    let source = match &decl.from {
        Source::Env(var) => format!("env {var}"),
        Source::File(path) => format!("file {}", path.display()),
        Source::Command(cmd) => format!("command {}", cmd.split_whitespace().next().unwrap_or("")),
    };
    let name = format!("secret {}", decl.name);
    match crate::secret::resolve(decl) {
        Ok(_) => Check::new(name, Level::Ok, format!("resolves ({source})")),
        Err(e) => Check::new(name, Level::Missing, format!("{e:#}")),
    }
}

fn tool_check(tool: &Tool, required: bool) -> Check {
    match tool.find() {
        Some(path) => Check::new(tool.name, Level::Ok, path.display().to_string()),
        None => Check::new(
            tool.name,
            if required {
                Level::Missing
            } else {
                Level::Optional
            },
            format!("{}; {}", tool.needed_for, tool.hint),
        ),
    }
}

fn config_check(path: &Path, config: &Result<Config>) -> Check {
    match config {
        Ok(c) if path.exists() => Check::new(
            "config",
            Level::Ok,
            format!(
                "{} ({} to inject)",
                path.display(),
                plural(c.inject.len(), "tool", "tools")
            ),
        ),
        Ok(_) => Check::new(
            "config",
            Level::Optional,
            format!(
                "no config at {}; nothing will be injected (`ldg init` writes one)",
                path.display()
            ),
        ),
        Err(e) => Check::new("config", Level::Missing, format!("{e:#}")),
    }
}

fn bundle_check(file: &Path) -> Check {
    match crate::bundle::archive::read_manifest(file) {
        Ok(m) => Check::new(
            "bundle",
            Level::Ok,
            format!(
                "{} ({}, {}, id {})",
                file.display(),
                m.arch,
                plural(m.tools.len(), "tool", "tools"),
                m.id()
            ),
        ),
        Err(e) => Check::new("bundle", Level::Missing, format!("{e:#}")),
    }
}

fn binary_check(name: &str, binary: &Path) -> Check {
    let name = format!("inject {name}");
    if !binary.exists() {
        return Check::new(
            name,
            Level::Missing,
            format!("{} does not exist", binary.display()),
        );
    }
    match crate::bundle::elf::arch_of(binary) {
        Ok(Some(arch)) => Check::new(name, Level::Ok, format!("{} ({arch})", binary.display())),
        Ok(None) => Check::new(
            name,
            Level::Ok,
            format!("{} (not an ELF binary)", binary.display()),
        ),
        Err(e) => Check::new(name, Level::Missing, format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOWHERE: Tool = Tool {
        name: "definitely-not-a-real-tool",
        need: Need::Pack,
        needed_for: "x",
        hint: "install it",
    };

    #[test]
    fn a_missing_required_tool_is_reported_missing_and_an_optional_one_is_not() {
        assert!(tool_check(&NOWHERE, true).level == Level::Missing);
        assert!(tool_check(&NOWHERE, false).level == Level::Optional);
    }
}
