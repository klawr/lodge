//! Commands about a project's container (or all of lodge's): `down`, `status`,
//! `rm`, `ls`, `ports`, `logs`, `clean`.

use anyhow::{Context, Result, bail};
use serde_json::json;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;

use super::{Ctx, print_json};
use crate::devcontainer::{self, ConfigState, Found};
use crate::output::{info, line, note, warning};
use crate::runtime::{Container, ContainerUser};

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
    let Some((found, state)) = devcontainer::status(&project, ctx.rt.as_ref())? else {
        if as_json {
            return print_json(&json!({ "project": project, "container": null }));
        }
        line!("project    {}", project.display());
        line!("container  none (run `ldg up` to create it)");
        return Ok(());
    };
    let c = found.container.as_ref();
    let running = c.is_running()?;
    let name = c.name().unwrap_or_else(|| c.id().to_string());
    // `None` when the container could not say.
    let bundle = crate::inject::installed_bundle(c)
        .map_err(|e| warning!("could not tell which bundle is installed: {e:#}"))
        .ok();
    // Which declared secrets are in place. They live in memory, so a stopped
    // container has none; `None` when a running one could not say.
    let secrets: Option<Vec<(String, bool)>> = if !running || config.secret.is_empty() {
        Some(
            config
                .secret
                .iter()
                .map(|d| (d.name.clone(), false))
                .collect(),
        )
    } else {
        ContainerUser::resolve(c, found.remote_user.as_deref())
            .and_then(|user| crate::secret::place::status(c, &user, &config.secret))
            .map_err(|e| warning!("could not tell which secrets are placed: {e:#}"))
            .ok()
    };

    if as_json {
        return print_json(&json!({
            "project": project,
            "container": {
                "id": c.id(),
                "name": name,
                "running": running,
                "config": state,
                "bundle": bundle.clone().flatten(),
                "secrets": secrets.as_ref().map(|s| s.iter().map(|(n, p)| json!({"name": n, "placed": p})).collect::<Vec<_>>()),
            },
        }));
    }
    line!("project    {}", project.display());
    line!(
        "container  {name} ({})",
        if running { "running" } else { "stopped" }
    );
    line!(
        "config     {}",
        match state {
            ConfigState::UpToDate => "up to date",
            ConfigState::Changed =>
                "changed since the container was created (`ldg up` offers to recreate it)",
            ConfigState::Unknown => "unknown (no recorded config, or it could not be read)",
        }
    );
    match bundle {
        Some(Some(id)) => line!("bundle     {id}"),
        Some(None) => {}
        None => line!("bundle     unknown"),
    }
    match secrets {
        None => line!("secrets    unknown"),
        Some(s) if s.is_empty() => {}
        Some(s) => {
            let shown: Vec<String> = s
                .iter()
                .map(|(n, placed)| {
                    format!("{n} ({})", if *placed { "placed" } else { "not placed" })
                })
                .collect();
            line!("secrets    {}", shown.join(", "));
        }
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
    if !container.is_running()? {
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
        line!("wrapper  {w}");
    }
    if !removed.configs_kept.is_empty() {
        note!(
            "injected configs were left in place (remove them by hand if unwanted):\n  {}",
            removed.configs_kept.join("\n  ")
        );
    }
    Ok(())
}
