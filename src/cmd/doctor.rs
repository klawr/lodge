//! `ldg doctor`: what is available on this host, and what each missing piece
//! would break. Reports everything up front instead of failing mid-run.

use anyhow::Result;
use serde_json::json;
use std::path::{Path, PathBuf};

use super::{Ctx, print_json};
use crate::config::Config;
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
    let remote = ctx.host.label();

    // A plain host or a pod has no engine to check; what else the endpoint needs
    // (a pod: `kubectl`, and that the pod is there) it says itself, below.
    let engine = ctx.endpoints.engine().map(|rt| rt.version());
    checks.push(match &engine {
        Some(Ok(v)) => Check::new(
            "container engine",
            Level::Ok,
            format!("{v}{}", ctx.host.on_host()),
        ),
        Some(Err(e)) => Check::new("container engine", Level::Missing, e.to_string()),
        None => Check::new(
            "container engine",
            Level::Optional,
            format!(
                "none: this target has no container engine{}",
                ctx.host.on_host()
            ),
        ),
    });

    for probe in ctx.endpoints.checks() {
        checks.push(match probe.outcome {
            Ok(detail) => Check::new(
                probe.name,
                Level::Ok,
                format!("{detail}{}", ctx.host.on_host()),
            ),
            Err(why) => Check::new(probe.name, Level::Missing, why),
        });
    }

    // The devcontainer here (if any) decides whether compose matters.
    let needs = ctx
        .project(path)
        .and_then(|p| crate::devcontainer::needs(&p, &ctx.host, ctx.local.as_ref()));
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
    if let Some(rt) = ctx
        .endpoints
        .engine()
        .filter(|_| engine.as_ref().is_some_and(|e| e.is_ok()))
    {
        checks.push(match rt.compose().and_then(|c| c.version()) {
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

    let packs_live = config
        .as_ref()
        .is_ok_and(|c| c.bundle.is_none() && crate::bundle::pack::needs_tools(c));
    checks.extend(host_tool_checks(
        remote.is_some(),
        ctx.tools.packer().map(|_| ()),
        packs_live,
    ));

    if let Ok(cfg) = &config {
        if let Some(bundle) = &cfg.bundle {
            checks.push(bundle_check(bundle, ctx));
        }
        for item in &cfg.binaries {
            checks.push(binary_check(&item.name, &item.path));
            if let Some(missing) = unlisted_data_check(&item.name, &item.path, &cfg.data) {
                checks.push(missing);
            }
        }
        for item in &cfg.data {
            checks.push(data_path_check(&item.path));
        }
        for decl in &cfg.secret {
            checks.push(secret_check(decl, ctx.local.as_ref()));
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
fn secret_check(
    decl: &crate::config::secret::SecretDecl,
    local: &dyn crate::local::Local,
) -> Check {
    use crate::config::secret::Source;
    let source = match &decl.from {
        Source::Env(var) => format!("env {var}"),
        Source::File(path) => format!("file {}", path.display()),
        Source::Command(cmd) => format!("command {}", cmd.split_whitespace().next().unwrap_or("")),
    };
    let name = format!("secret {}", decl.name);
    match crate::secret::resolve(decl, local) {
        Ok(_) => Check::new(name, Level::Ok, format!("resolves ({source})")),
        Err(e) => Check::new(name, Level::Missing, format!("{e:#}")),
    }
}

/// The helper programs lodge runs on this machine, and which of them the work
/// asked for. `ldd` and `patchelf` are needed only to *pack*: with a bundle,
/// injection needs neither, so they are required only when the config would pack
/// live, and not shown on a machine that cannot pack at all (`can_pack` says why
/// not), which needs the bundle instead.
fn host_tool_checks(remote: bool, can_pack: Result<()>, packs_live: bool) -> Vec<Check> {
    let mut checks = Vec::new();
    if packs_live && let Err(e) = &can_pack {
        checks.push(Check::new("bundle", Level::Missing, e.to_string()));
    }
    for tool in host_tools::ALL {
        // (shown at all, required) for what this host and config ask of it.
        let (shown, required) = match tool.need {
            Need::Remote => (remote, remote),
            Need::Pack => (can_pack.is_ok(), packs_live),
        };
        if shown {
            checks.push(tool_check(tool, required));
        }
    }
    checks
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
                "{} ({}, {})",
                path.display(),
                plural(c.binaries.len(), "binary", "binaries"),
                plural(c.data.len(), "data entry", "data entries")
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

/// Looks the bundle the config names up, as `up` would, so a reference that
/// cannot be read (a missing file, a registry that says no) shows up here and not
/// in the middle of `ldg up`. Nothing is fetched or cached: a check has no side
/// effects, so a bundle that is not cached yet is said to be so.
fn bundle_check(reference: &str, ctx: &Ctx) -> Check {
    let found = crate::bundle::cache::root()
        .and_then(|root| crate::bundle::source::look_up(&root, &ctx.bundles, reference));
    match found {
        Ok(found) => {
            let detail = match &found.cached {
                Some(m) => format!(
                    "{reference} ({}{}, {}, id {})",
                    if found.offline {
                        "not reachable, the cached copy from last time: "
                    } else {
                        ""
                    },
                    m.arch,
                    plural(m.binaries.len(), "binary", "binaries"),
                    m.id()
                ),
                None => format!(
                    "{reference} resolves to {} (not cached yet; `ldg up` fetches it)",
                    found.pinned
                ),
            };
            Check::new("bundle", Level::Ok, detail)
        }
        Err(e) => Check::new("bundle", Level::Missing, format!("{e:#}")),
    }
}

fn data_path_check(path: &Path) -> Check {
    let name = format!("data {}", path.display());
    if path.exists() {
        Check::new(name, Level::Ok, "found")
    } else {
        Check::new(
            name,
            Level::Missing,
            format!("{} does not exist", path.display()),
        )
    }
}

/// A program with files beside it that the config does not bring: it may start in
/// the container and then not find them. Said only for the files the filesystem
/// layout's convention puts there (see [`super::candidates::companions`]), and
/// only as a hint, since a program may not need them.
fn unlisted_data_check(
    name: &str,
    binary: &Path,
    data: &[crate::config::DataEntry],
) -> Option<Check> {
    let missing: Vec<String> = super::candidates::companions(binary)
        .into_iter()
        .filter(|place| !data.iter().any(|d| &d.path == place))
        .map(|place| place.display().to_string())
        .collect();
    (!missing.is_empty()).then(|| {
        Check::new(
            format!("binary {name}"),
            Level::Optional,
            format!(
                "{} sits beside it and is not injected; if it needs it: `ldg inject add data {} --for {name}`",
                missing.join(", "),
                missing[0]
            ),
        )
    })
}

fn binary_check(name: &str, binary: &Path) -> Check {
    let name = format!("binary {name}");
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

    fn names(checks: &[Check]) -> Vec<&str> {
        checks.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn a_machine_that_cannot_pack_is_not_asked_for_the_packing_tools_but_for_a_bundle() {
        let cannot = || Err(anyhow::anyhow!("no packing here"));
        // Nothing to pack: nothing to say about it.
        let quiet = host_tool_checks(false, cannot(), false);
        assert!(names(&quiet).is_empty(), "{:?}", names(&quiet));
        // A config that would pack: the missing bundle is the problem, not the tools.
        let loud = host_tool_checks(false, cannot(), true);
        assert_eq!(names(&loud), ["bundle"]);
        assert!(loud[0].level == Level::Missing);
        assert!(loud[0].detail.contains("no packing here"));
        // A machine that can pack lists the tools, and requires them only to pack.
        let can = host_tool_checks(false, Ok(()), false);
        assert_eq!(names(&can), ["ldd", "patchelf"]);
        assert!(can.iter().all(|c| c.level != Level::Missing));
    }

    #[test]
    fn the_remote_tool_is_listed_only_for_a_remote() {
        assert!(
            names(&host_tool_checks(false, Ok(()), false))
                .iter()
                .all(|n| *n != "ssh")
        );
        assert!(names(&host_tool_checks(true, Ok(()), false)).contains(&"ssh"));
    }

    #[test]
    fn files_beside_a_program_that_the_config_leaves_out_are_hinted_at() {
        let dir = crate::tmpfile::TempDir::new("doctor-data").unwrap();
        let share = dir.path().join("share").join("tool");
        std::fs::create_dir_all(&share).unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        let binary = dir.path().join("bin/tool");
        let hint = unlisted_data_check("tool", &binary, &[]).unwrap();
        assert!(hint.level == Level::Optional);
        assert!(
            hint.detail.contains(&share.display().to_string()),
            "{}",
            hint.detail
        );
        let listed = [crate::config::DataEntry {
            path: share,
            target: None,
            binary: None,
        }];
        assert!(unlisted_data_check("tool", &binary, &listed).is_none());
    }

    #[test]
    fn a_missing_required_tool_is_reported_missing_and_an_optional_one_is_not() {
        assert!(tool_check(&NOWHERE, true).level == Level::Missing);
        assert!(tool_check(&NOWHERE, false).level == Level::Optional);
    }
}
