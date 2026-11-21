//! `ldg up`: get the project's container ready, inject if needed, open a session.

use anyhow::Result;
use std::path::{Path, PathBuf};

use super::Ctx;
use crate::config::Config;
use crate::devcontainer;
use crate::layout::Layout;
use crate::output::note;
use crate::runtime::{Container, ContainerUser};
use crate::session::{self, Session};
use crate::{bundle, forward, inject, secret};

/// Brings the project's container up and opens a session in it. Anything
/// already injected is left alone (just reported when it is out of date), unless
/// the container was just created or `reinject` asks for a fresh injection.
pub fn up(
    ctx: &Ctx,
    path: Option<PathBuf>,
    build: bool,
    reinject: bool,
    no_attach: bool,
    bundle_file: Option<&Path>,
) -> Result<()> {
    let project = ctx.project(path)?;
    let config = ctx.config()?;
    // Ready the bundle first, so a problem with it fails before anything starts.
    let prepared = bundle::source::prepare(&config, bundle_file)?;
    let started = devcontainer::start(
        &project,
        &ctx.host,
        build,
        ctx.rt.as_ref(),
        &super::prompt::Terminal,
    )?;
    let container = started.container.as_ref();
    // After `start`, which may have changed the user's uid.
    let user = ContainerUser::resolve(container, started.remote_user.as_deref())?;
    forward::spawn_daemon(
        container.id(),
        ctx.host.as_ref(),
        ctx.rt.as_ref(),
        &started.forward_ports,
    );

    let installed = if started.created || reinject {
        None
    } else {
        inject::installed(container, &user)?
    };
    let layout = match installed {
        Some(found) => {
            if found.bundle != prepared.manifest.id() {
                note!(
                    "what is injected is out of date (your config or ldg changed); \
                     `ldg up --reinject` updates it"
                );
            }
            found.layout
        }
        None => {
            let layout = inject::resolve_layout(container, &user)?;
            inject::run(container, &user, &layout, &prepared)?;
            layout
        }
    };
    if no_attach {
        return Ok(());
    }
    started.run_post_attach();
    attach(ctx, container, &config, &user, &layout, &started.workdir)
}

/// Delivers the secrets, then opens the session.
fn attach(
    ctx: &Ctx,
    container: &dyn Container,
    config: &Config,
    user: &ContainerUser,
    layout: &Layout,
    workdir: &str,
) -> Result<()> {
    let setup = secret::deliver::deliver(container, ctx.host.as_ref(), user, &config.secret)?;
    session::enter(
        container,
        ctx.host.as_ref(),
        &config.session,
        &Session {
            workdir,
            user,
            layout,
            setup: &setup,
        },
    )
}
