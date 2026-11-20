//! `ldg up` and `ldg exec`: get a container ready, inject, open a session.

use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

use super::Ctx;
use crate::config::Config;
use crate::devcontainer::{self, StartOpts};
use crate::runtime::Container;
use crate::session::{self, Session};
use crate::{bundle, forward, inject, secret};

pub fn up(
    ctx: &Ctx,
    path: Option<PathBuf>,
    opts: StartOpts,
    no_attach: bool,
    bundle_file: Option<&Path>,
) -> Result<()> {
    let project = ctx.project(path)?;
    let config = ctx.config()?;
    // Ready the bundle first, so a problem with it fails before anything starts.
    let prepared = bundle::source::prepare(&config, bundle_file)?;
    let started = devcontainer::start(&project, opts, ctx.rt.as_ref())?;
    let container = started.container.as_ref();
    let user = started.remote_user.as_deref();
    forward::spawn_daemon(container.id(), &ctx.target, &started.forward_ports);
    inject::lodge(container, user, &prepared)?;
    if no_attach {
        return Ok(());
    }
    started.run_post_attach();
    attach(
        container,
        &config,
        &started.session_name,
        &started.workdir,
        user,
    )
}

pub fn exec(
    ctx: &Ctx,
    name: &str,
    user: Option<&str>,
    workdir: &str,
    no_attach: bool,
    bundle_file: Option<&Path>,
) -> Result<()> {
    let config = ctx.config()?;
    let prepared = bundle::source::prepare(&config, bundle_file)?;
    let container = ctx.rt.container(name);
    if !container.is_running() {
        bail!("container {name} is not running (check the name with `docker ps`)");
    }
    inject::lodge(container.as_ref(), user, &prepared)?;
    if no_attach {
        return Ok(());
    }
    attach(
        container.as_ref(),
        &config,
        &format!("lodge-{name}"),
        workdir,
        user,
    )
}

/// The part `up` and `exec` share: deliver secrets, then open the session.
fn attach(
    container: &dyn Container,
    config: &Config,
    name: &str,
    workdir: &str,
    user: Option<&str>,
) -> Result<()> {
    let setup = secret::deliver::deliver(container, user, &config.secret)?;
    session::enter(
        container,
        &config.session,
        &Session {
            name,
            workdir,
            user,
            setup: &setup,
        },
    )
}
