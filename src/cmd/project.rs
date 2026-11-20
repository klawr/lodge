//! Commands about a project's container (or all of lodge's): `down`, `status`,
//! `rm`, `ls`, `ports`, `logs`, `clean`.

use anyhow::{Context, Result, bail};
use serde_json::json;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;

use super::{Ctx, print_json};
use crate::devcontainer::{self, ConfigState, Found};
use crate::output::{info, line, note};
use crate::runtime::Container;

/// The project's container, or an error saying there is none.
fn require(ctx: &Ctx, path: Option<PathBuf>) -> Result<Found> {
    let project = ctx.project(path)?;
    devcontainer::find(&project, ctx.rt.as_ref())?.with_context(|| {
        format!(
            "no container for {} (run `ldg up` to create it)",
            project.display()
        )
    })
}

pub fn down(ctx: &Ctx, path: Option<PathBuf>) -> Result<()> {
    let project = ctx.project(path)?;
    match devcontainer::stop(&project, ctx.rt.as_ref())? {
        Some(id) => info!("stopped {id}"),
        None => info!("no container exists for this devcontainer"),
    }
    Ok(())
}

pub fn rm(ctx: &Ctx, path: Option<PathBuf>) -> Result<()> {
    let project = ctx.project(path)?;
    match devcontainer::remove(&project, ctx.rt.as_ref())? {
        Some(id) => info!("removed {id}"),
        None => info!("no container exists for this devcontainer"),
    }
    Ok(())
}

pub fn status(ctx: &Ctx, path: Option<PathBuf>, as_json: bool) -> Result<()> {
    let project = ctx.project(path)?;
    let config = ctx.config()?;
    let status = devcontainer::status(&project, ctx.rt.as_ref())?;
    let Some((found, state)) = status.found else {
        if as_json {
            return print_json(&json!({ "project": project, "container": null }));
        }
        info!("project    {}", project.display());
        info!("container  none (run `ldg up` to create it)");
        return Ok(());
    };
    let c = found.container.as_ref();
    let running = c.is_running();
    let name = c.name().unwrap_or_else(|| c.id().to_string());
    let bundle = crate::inject::installed_bundle(c);
    // Which declared secrets are in place; only a running container can say.
    let secrets: Vec<(String, bool)> = if running && !config.secret.is_empty() {
        let present = crate::secret::place::present_names(c, found.remote_user.as_deref());
        config
            .secret
            .iter()
            .map(|d| (d.name.clone(), present.contains(&d.name)))
            .collect()
    } else {
        Vec::new()
    };

    if as_json {
        return print_json(&json!({
            "project": project,
            "container": {
                "id": c.id(),
                "name": name,
                "running": running,
                "config": state,
                "bundle": bundle,
                "secrets": secrets.iter().map(|(n, p)| json!({"name": n, "placed": p})).collect::<Vec<_>>(),
            },
        }));
    }
    info!("project    {}", project.display());
    info!(
        "container  {name} ({})",
        if running { "running" } else { "stopped" }
    );
    info!(
        "config     {}",
        match state {
            ConfigState::UpToDate => "up to date",
            ConfigState::Changed =>
                "changed since the container was created (`ldg up` offers to recreate it)",
            ConfigState::Unknown => "unknown (the container has no recorded config)",
        }
    );
    if let Some(id) = bundle {
        info!("bundle     {id}");
    }
    if !secrets.is_empty() {
        let shown: Vec<String> = secrets
            .iter()
            .map(|(n, placed)| format!("{n} ({})", if *placed { "placed" } else { "not placed" }))
            .collect();
        info!("secrets    {}", shown.join(", "));
    }
    Ok(())
}

pub fn ls(ctx: &Ctx, as_json: bool) -> Result<()> {
    let listed = ctx.rt.list_labeled(devcontainer::LABEL_PROJECT)?;
    if as_json {
        return print_json(&json!(
            listed
                .iter()
                .map(|l| json!({"id": l.id, "name": l.name, "running": l.running, "project": l.label}))
                .collect::<Vec<_>>()
        ));
    }
    if listed.is_empty() {
        note!(
            "no containers created by lodge{} (containers created before lodge labelled them are not listed)",
            ctx.target
                .ssh_host()
                .map(|h| format!(" on {h}"))
                .unwrap_or_default()
        );
        return Ok(());
    }
    let width = listed.iter().map(|l| l.name.len()).max().unwrap_or(0);
    line!("{:<width$}  {:<7}  PROJECT", "NAME", "STATE");
    for l in &listed {
        let state = if l.running { "running" } else { "stopped" };
        line!("{:<width$}  {state:<7}  {}", l.name, l.label);
    }
    Ok(())
}

pub fn ports(ctx: &Ctx, path: Option<PathBuf>, as_json: bool) -> Result<()> {
    let found = require(ctx, path)?;
    let id = found.container.id();
    let active = crate::forward::active_ports(id);
    if as_json {
        return print_json(&json!({ "daemon_running": active.is_some(), "ports": active }));
    }
    match active {
        None => {
            note!("no port-forwarding daemon is running for this container (`ldg up` starts one)")
        }
        Some(ports) if ports.is_empty() => info!("nothing forwarded yet"),
        Some(ports) => {
            for p in ports {
                line!("127.0.0.1:{:<5}  →  {}", p.local, p.dest);
            }
        }
    }
    Ok(())
}

pub fn logs(ctx: &Ctx, path: Option<PathBuf>, follow: bool) -> Result<()> {
    let found = require(ctx, path)?;
    let log = crate::forward::log_path(found.container.id());
    let mut file = std::fs::File::open(&log)
        .with_context(|| format!("no forwarding log at {} yet", log.display()))?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    crate::output::raw_stdout(&text);
    if !follow {
        return Ok(());
    }
    let mut pos = file.stream_position()?;
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let len = std::fs::metadata(&log)?.len();
        if len < pos {
            // Truncated: a new daemon started over.
            pos = 0;
        }
        if len > pos {
            file.seek(SeekFrom::Start(pos))?;
            let mut more = String::new();
            file.read_to_string(&mut more)?;
            crate::output::raw_stdout(&more);
            pos = file.stream_position()?;
        }
    }
}

pub fn clean(ctx: &Ctx, path: Option<PathBuf>, name: Option<&str>) -> Result<()> {
    let container: Box<dyn Container> = match name {
        Some(n) => ctx.rt.container(n),
        None => require(ctx, path)?.container,
    };
    if !container.is_running() {
        bail!(
            "the container is not running; start it first (`ldg up --no-attach`, or `docker start`)"
        );
    }
    let removed = crate::inject::remove(container.as_ref())?;
    info!(
        "removed lodge from {}",
        container
            .name()
            .unwrap_or_else(|| container.id().to_string())
    );
    for w in &removed.wrappers {
        info!("  wrapper {w}");
    }
    if !removed.configs_kept.is_empty() {
        note!(
            "injected configs were left in place (remove them by hand if unwanted):\n  {}",
            removed.configs_kept.join("\n  ")
        );
    }
    Ok(())
}
