//! `ldg up`: get the project's container ready, inject if needed, open a session.

use anyhow::Result;
use std::path::{Path, PathBuf};

use super::Ctx;
use crate::config::Config;
use crate::devcontainer::Devcontainer;
use crate::output::note;
use crate::provision::{Attached, Provisioner};
use crate::runtime::ContainerUser;
use crate::session::{self, Session};
use crate::{bundle, inject, secret};

/// Brings the project's container up and opens a session in it: `provision`,
/// then `attach`. Anything already injected is left alone (just reported when it
/// is out of date), unless the container was just created or `reinject` asks for
/// a fresh injection.
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
    let prepared = bundle::source::prepare(&config, bundle_file, &ctx.tools)?;
    // The one place that picks how an endpoint is made. A devcontainer is the only
    // provisioner so far; a second one is chosen here, from the project's signals
    // and `--engine`, and nothing below changes.
    let provisioner = Devcontainer {
        host: &ctx.host,
        rt: ctx.rt.as_ref(),
        oci: ctx.tools.oci.as_ref(),
        prompt: &super::prompt::Terminal,
    };
    let attached = provisioner.provision(&project, build)?;
    attach(ctx, &attached, &config, &prepared, reinject, no_attach)
}

/// Works in an endpoint that is up: forwards its ports, injects (unless that was
/// done already), delivers the secrets and opens the session. Knows nothing of
/// how the endpoint was made.
fn attach(
    ctx: &Ctx,
    attached: &Attached,
    config: &Config,
    prepared: &bundle::source::Prepared,
    reinject: bool,
    no_attach: bool,
) -> Result<()> {
    let container = attached.container.as_ref();
    // After provisioning, which may have changed the user's uid.
    let user = ContainerUser::resolve(container, attached.remote_user.as_deref())?;
    ctx.forwarder.start(container.id(), &attached.forward_ports);

    let installed = if attached.created || reinject {
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
            inject::run(container, &user, &layout, prepared)?;
            layout
        }
    };
    if no_attach {
        return Ok(());
    }
    attached.run_post_attach();
    let setup = secret::deliver::deliver(container, ctx.host.as_ref(), &user, &config.secret)?;
    session::enter(
        container,
        ctx.host.as_ref(),
        &config.session,
        &Session {
            workdir: &attached.workdir,
            user: &user,
            layout: &layout,
            setup: &setup,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::manifest::Manifest;
    use crate::bundle::source::{Origin, Prepared};
    use crate::forward::ForwardDaemon;
    use crate::host::local;
    use crate::port::PortForward;
    use crate::runtime::RUN_PROBE_SCRIPT;
    use crate::runtime::fake::{FakeEnv, FakeRuntime};
    use crate::shell::script_argv;
    use crate::tmpfile::TempDir;
    use crate::tools;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Records what it was asked to forward instead of starting a process.
    struct RecordingDaemon(Rc<RefCell<Vec<String>>>);

    impl ForwardDaemon for RecordingDaemon {
        fn start(&self, container: &str, ports: &[PortForward]) {
            self.0
                .borrow_mut()
                .push(format!("{container} {}", ports.len()));
        }
    }

    fn user() -> ContainerUser {
        ContainerUser {
            requested: None,
            name: "dev".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/dev".into(),
        }
    }

    fn manifest() -> Manifest {
        Manifest {
            lodge_version: "0".into(),
            created: 0,
            arch: "x86_64".into(),
            linker: None,
            libs: vec![],
            tools: vec![],
            git_safe_directories: vec![],
        }
    }

    #[test]
    fn attaching_to_a_container_that_exists_forwards_and_injects_without_a_provisioner() {
        let dir = TempDir::new("attach").unwrap();
        let env = FakeEnv::new()
            .with_output(
                &["sh", "-c", crate::runtime::PROBE_SCRIPT],
                "1000\n1000\ndev\n/home/dev\n",
            )
            .with_output(
                &script_argv(RUN_PROBE_SCRIPT, &["/home/dev/.local/share/lodge", "any"]),
                "",
            )
            .with_output(&["uname", "-m"], "x86_64\n")
            .with_commands(&["git"]);
        let rt = FakeRuntime::new(FakeEnv::new).with_container("c1", env);
        let forwarded = Rc::new(RefCell::new(Vec::new()));
        let ctx = Ctx {
            host: local(),
            forwarder: Box::new(RecordingDaemon(forwarded.clone())),
            rt: Box::new(rt),
            tools: tools::external(),
            config_path: dir.path().join("config.toml"),
        };
        let attached = Attached {
            container: ctx.rt.container("c1"),
            created: false,
            workdir: "/w".into(),
            forward_ports: Vec::new(),
            remote_user: None,
            post_attach: None,
        };
        let prepared = Prepared {
            dir: dir.path().to_path_buf(),
            manifest: manifest(),
            origin: Origin::Packed,
        };

        attach(&ctx, &attached, &Config::default(), &prepared, false, true).unwrap();

        assert_eq!(*forwarded.borrow(), ["c1 0"]);
        // Nothing was injected before, so the bundle was injected now.
        assert!(
            inject::installed(attached.container.as_ref(), &user())
                .unwrap()
                .is_some()
        );
    }
}
