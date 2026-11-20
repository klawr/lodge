//! `ldg up` and `ldg exec`: get a container ready, inject, open a session.

use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

use super::Ctx;
use crate::config::Config;
use crate::devcontainer::{self, StartOpts};
use crate::runtime::{Container, ContainerUser};
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
    let started = devcontainer::start(&project, opts, ctx.rt.as_ref(), &super::prompt::Terminal)?;
    let container = started.container.as_ref();
    // After `start`, which may have changed the user's uid.
    let user = ContainerUser::resolve(container, started.remote_user.as_deref())?;
    forward::spawn_daemon(container.id(), &ctx.target, &started.forward_ports);
    inject::run(container, &user, &prepared)?;
    if no_attach {
        return Ok(());
    }
    started.run_post_attach();
    attach(
        container,
        &config,
        &user,
        &started.session_name,
        &started.workdir,
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
    let user = ContainerUser::resolve(container.as_ref(), user)?;
    inject::run(container.as_ref(), &user, &prepared)?;
    if no_attach {
        return Ok(());
    }
    attach(
        container.as_ref(),
        &config,
        &user,
        &format!("lodge-{name}"),
        workdir,
    )
}

/// The part `up` and `exec` share: deliver secrets, then open the session.
fn attach(
    container: &dyn Container,
    config: &Config,
    user: &ContainerUser,
    name: &str,
    workdir: &str,
) -> Result<()> {
    let setup = secret::deliver::deliver(container, user, &config.secret)?;
    session::enter(
        container,
        &config.session,
        &Session {
            name,
            workdir,
            user: user.exec_as(),
            setup: &setup,
        },
    )
}
