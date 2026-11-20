//! `ldg doctor`: what is available on this host, and what each missing piece
//! would break. Reports everything up front instead of failing mid-run.

use crate::output::{info, line, warning};
use anyhow::Result;
use std::path::Path;

use crate::config::{Config, config_path};
use crate::runtime::Runtime;
use crate::target::Target;

enum Status {
    Ok(String),
    /// Needed for something lodge was asked to do.
    Missing(String),
    /// Only needed for an optional feature.
    Optional(String),
}

struct Check {
    name: String,
    status: Status,
}

impl Check {
    fn new(name: impl Into<String>, status: Status) -> Self {
        Self {
            name: name.into(),
            status,
        }
    }
}

/// Runs every check, prints the report, and returns whether all required checks
/// passed.
pub fn run(rt: &dyn Runtime, target: &Target) -> Result<bool> {
    let mut checks = Vec::new();

    let place = match target.ssh_host() {
        Some(host) => format!("on {host}"),
        None => "local".to_string(),
    };
    checks.push(Check::new(
        "container engine",
        match rt.version() {
            Ok(v) => Status::Ok(format!("{v} ({place})")),
            Err(e) => Status::Missing(e.to_string()),
        },
    ));

    if target.ssh_host().is_some() {
        checks.push(tool(
            "ssh",
            "needed for --remote",
            true,
            "install an OpenSSH client",
        ));
    }

    let config = Config::load();
    checks.push(config_check(&config));

    // ldd and patchelf are needed only to *pack*: with a bundle, injection needs
    // neither. So they are required only when this config would pack live.
    let packs_live = config
        .as_ref()
        .is_ok_and(|c| c.bundle.is_none() && !c.inject.is_empty());
    checks.push(tool(
        "patchelf",
        "needed to pack (not needed when injecting from a bundle)",
        packs_live,
        "install patchelf (pacman -S patchelf / apt install patchelf)",
    ));
    checks.push(tool(
        "ldd",
        "needed to pack (not needed when injecting from a bundle)",
        packs_live,
        "install your libc's developer tools (ldd ships with glibc)",
    ));
    if let Ok(cfg) = &config
        && let Some(bundle) = &cfg.bundle
    {
        checks.push(bundle_check(bundle));
    }
    checks.push(tool(
        "oras",
        "only needed for devcontainer features",
        false,
        "install oras from https://oras.land",
    ));

    if let Ok(cfg) = &config {
        for item in &cfg.inject {
            if let Some(binary) = &item.binary {
                checks.push(binary_check(&item.name, binary));
            }
        }
    }

    info!("doctor");
    let mut failed = 0;
    for c in &checks {
        let (tag, detail) = match &c.status {
            Status::Ok(d) => ("ok      ", d),
            Status::Missing(d) => {
                failed += 1;
                ("MISSING ", d)
            }
            Status::Optional(d) => ("optional", d),
        };
        line!("  {tag}  {:<18} {detail}", c.name);
    }
    if failed > 0 {
        warning!("{failed} required check(s) failed");
    }
    Ok(failed == 0)
}

fn tool(name: &str, needed_for: &str, required: bool, hint: &str) -> Check {
    let status = match crate::shell::which(name) {
        Some(path) => Status::Ok(path.display().to_string()),
        None if required => Status::Missing(format!("{needed_for}; {hint}")),
        None => Status::Optional(format!("{needed_for}; {hint}")),
    };
    Check::new(name, status)
}

fn config_check(config: &Result<Config>) -> Check {
    let path = config_path();
    let status = match config {
        Ok(c) if path.exists() => Status::Ok(format!(
            "{} ({} tool{} to inject)",
            path.display(),
            c.inject.len(),
            if c.inject.len() == 1 { "" } else { "s" }
        )),
        Ok(_) => Status::Optional(format!(
            "no config at {}; nothing will be injected{}",
            path.display(),
            old_config_hint()
        )),
        Err(e) => Status::Missing(format!("{e:#}")),
    };
    Check::new("config", status)
}

/// Notes a config left at the pre-rename location (`~/.config/graft`).
fn old_config_hint() -> String {
    let old = dirs::config_dir().map(|d| d.join("graft").join("config.toml"));
    match old {
        Some(p) if p.exists() => format!(
            " (found {}: move the directory to ~/.config/lodge)",
            p.display()
        ),
        _ => String::new(),
    }
}

fn bundle_check(file: &Path) -> Check {
    let status = match crate::bundle::archive::read_manifest(file) {
        Ok(m) => Status::Ok(format!(
            "{} ({}, {} tool(s), id {})",
            file.display(),
            m.arch,
            m.tools.len(),
            m.id()
        )),
        Err(e) => Status::Missing(format!("{e:#}")),
    };
    Check::new("bundle", status)
}

fn binary_check(name: &str, binary: &Path) -> Check {
    let status = if !binary.exists() {
        Status::Missing(format!("{} does not exist", binary.display()))
    } else {
        match crate::elf::arch_of(binary) {
            Ok(Some(arch)) => Status::Ok(format!("{} ({arch})", binary.display())),
            Ok(None) => Status::Ok(format!("{} (not an ELF binary)", binary.display())),
            Err(e) => Status::Missing(format!("{e:#}")),
        }
    };
    Check::new(format!("inject {name}"), status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_required_tool_is_reported_missing_and_an_optional_one_is_not() {
        let required = tool("definitely-not-a-real-tool", "x", true, "install it");
        assert!(matches!(required.status, Status::Missing(_)));
        let optional = tool("definitely-not-a-real-tool", "x", false, "install it");
        assert!(matches!(optional.status, Status::Optional(_)));
    }
}
