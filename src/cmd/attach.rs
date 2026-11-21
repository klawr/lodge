//! `ldg up`: get the project's container ready, inject if needed, open a session.

use anyhow::Result;
use std::path::PathBuf;

use super::Ctx;
use crate::config::Config;
use crate::output::note;
use crate::provision::Attached;
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
    bundle: Option<&str>,
) -> Result<()> {
    let project = ctx.project(path)?;
    let config = ctx.config()?;
    // Ready the bundle first, so a problem with it fails before anything starts.
    let prepared = bundle::source::prepare(&config, bundle, &ctx.bundles, &ctx.tools)?;
    // How an endpoint is made, or found, is the endpoints' business; which
    // endpoints is chosen once, in `cmd::run`.
    let attached = ctx.endpoints.provision(&project, build)?;
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
    if container.forwards_ports() {
        ctx.forwarder.start(container.id(), &attached.forward_ports);
    }

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
    let setup = secret::deliver::deliver(
        container,
        ctx.host.as_ref(),
        &user,
        &config.secret,
        ctx.local.as_ref(),
    )?;
    session::enter(
        container,
        ctx.host.as_ref(),
        ctx.local.as_ref(),
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
    use crate::devcontainer::{Devcontainer, Prompt};
    use crate::forward::ForwardDaemon;
    use crate::host::fake;
    use crate::local::fake::FakeLocal;
    use crate::port::PortForward;
    use crate::runtime::RUN_PROBE_SCRIPT;
    use crate::runtime::Runtime as _;
    use crate::runtime::fake::{FakeEnv, FakeRuntime};
    use crate::shell::script_argv;
    use crate::tmpfile::TempDir;
    use crate::tools;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;

    /// Records what it was asked to forward instead of starting a process.
    struct RecordingDaemon(Rc<RefCell<Vec<String>>>);

    impl ForwardDaemon for RecordingDaemon {
        fn start(&self, container: &str, ports: &[PortForward]) {
            self.0
                .borrow_mut()
                .push(format!("{container} {}", ports.len()));
        }

        fn stop(&self, container: &str) {
            self.0.borrow_mut().push(format!("stop {container}"));
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

    struct NoQuestions;

    impl Prompt for NoQuestions {
        fn choose_workdir(&self, _: &str, _: &[String]) -> String {
            panic!("attaching asks nothing")
        }
    }

    /// Attaches to a container made from `env` and says what was forwarded for it
    /// and whether the bundle was injected.
    fn attach_to(env: FakeEnv) -> (Vec<String>, bool) {
        let dir = TempDir::new("attach").unwrap();
        let env = env
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
        let rt = Rc::new(FakeRuntime::new(FakeEnv::new).with_container("c1", env));
        let forwarded = Rc::new(RefCell::new(Vec::new()));
        let ctx = Ctx {
            host: fake(),
            local: Arc::new(FakeLocal::new()),
            forwarder: Box::new(RecordingDaemon(forwarded.clone())),
            endpoints: Box::new(Devcontainer::new(
                fake(),
                Arc::new(FakeLocal::new()),
                Box::new(rt.clone()),
                Arc::new(tools::NoRegistry),
                Arc::new(NoQuestions),
            )),
            tools: tools::external(),
            bundles: crate::bundle::store::Stores::new(vec![]),
            sinks: crate::bundle::store::Sinks::new(vec![]),
            config_path: dir.path().join("config.toml"),
        };
        let attached = Attached {
            container: rt.container("c1"),
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

        let injected = inject::installed(attached.container.as_ref(), &user())
            .unwrap()
            .is_some();
        let forwarded = forwarded.borrow().clone();
        (forwarded, injected)
    }

    #[test]
    fn attaching_to_a_container_that_exists_forwards_and_injects_without_a_provisioner() {
        let (forwarded, injected) = attach_to(FakeEnv::new());
        assert_eq!(forwarded, ["c1 0"]);
        // Nothing was injected before, so the bundle was injected now.
        assert!(injected);
    }

    #[test]
    fn an_endpoint_whose_ports_are_already_here_gets_no_forwarding_daemon() {
        let (forwarded, injected) = attach_to(FakeEnv::new().without_forwarding());
        assert!(forwarded.is_empty(), "{forwarded:?}");
        assert!(injected);
    }
}
