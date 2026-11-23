//! `ldg up`: get the project's container ready, inject if needed, open a session.

use anyhow::Result;
use std::path::PathBuf;

use super::Ctx;
use super::bundle as bundle_cmd;
use crate::bundle::choice;
use crate::config::Config;
use crate::history::Place;
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
    let place = Place::Project {
        path: project.clone(),
    };
    // Which bundle comes first, and is readied before anything starts, so a
    // problem with it fails before a container is made.
    let bundle = choose_bundle(ctx, &config, bundle, &place, reinject)?;
    let prepared = bundle::source::prepare(&config, bundle.as_deref(), &ctx.bundles, &ctx.tools)?;
    // How an endpoint is made, or found, is the endpoints' business; which
    // endpoints is chosen once, in `cmd::run`.
    let attached = ctx.endpoints.provision(&project, build)?;
    let used = bundle.or_else(|| config.bundle.clone());
    attach(
        ctx,
        &attached,
        &config,
        &Injection {
            prepared: &prepared,
            used: used.as_deref(),
            reinject,
            no_attach,
        },
        place,
    )
}

/// A bundle for a message: its id, and where it came from when that is known.
fn describe(id: &str, source: Option<&str>) -> String {
    match source {
        Some(source) => format!("bundle {id} ({source})"),
        None => format!("bundle {id}"),
    }
}

/// Which bundle this run injects from, as a reference that overrides the config's
/// `bundle` (`None`: the config's default). What was asked for with `--bundle` is
/// not questioned. Otherwise, when an injection is about to happen into a place
/// not opened before (or `--reinject` says to inject again) and the config puts
/// more than one bundle on offer, the person is asked which. Nobody to ask, or
/// declining, is the default.
fn choose_bundle(
    ctx: &Ctx,
    config: &Config,
    explicit: Option<&str>,
    place: &Place,
    reinject: bool,
) -> Result<Option<String>> {
    if explicit.is_some() {
        return Ok(explicit.map(str::to_string));
    }
    let default = choice::default(config);
    let offered = choice::options(&default, &config.bundles);
    if offered.len() < 2 || !(reinject || !ctx.knows(place)) || !ctx.choose.can_ask() {
        return Ok(None);
    }
    let question = format!("Which bundle for {}?", place.fallback_name());
    let chosen = bundle_cmd::pick(ctx.choose.as_ref(), &question, &offered, &default)?;
    Ok(match chosen {
        Some(c) if c != default => c.reference().map(str::to_string),
        _ => None,
    })
}

/// Works in a container that is running and that lodge did not make: the same
/// forwarding, injection, secrets and session as `up`, with nothing created
/// first. The container is somebody's, so it is neither started nor changed
/// beyond what is injected.
pub fn attach_to(
    ctx: &Ctx,
    name: &str,
    workdir: Option<String>,
    reinject: bool,
    no_attach: bool,
    bundle: Option<&str>,
) -> Result<()> {
    let config = ctx.config()?;
    let container = ctx.existing_container(name)?;
    if !container.is_running()? {
        anyhow::bail!(
            "{name} is not running, or there is no such container; lodge does not start \
             containers it did not make"
        );
    }
    let place = Place::Container {
        name: name.to_string(),
        workdir: workdir.clone(),
    };
    let bundle = choose_bundle(ctx, &config, bundle, &place, reinject)?;
    let prepared = bundle::source::prepare(&config, bundle.as_deref(), &ctx.bundles, &ctx.tools)?;
    let workdir = match workdir {
        Some(dir) => dir,
        None => ContainerUser::resolve(container.as_ref(), None)?.home,
    };
    let attached = Attached {
        container,
        // Made by somebody else, so whether anything is injected is for the record
        // in its prefix to say.
        created: false,
        workdir,
        name: None,
        forward_ports: Vec::new(),
        remote_user: None,
        post_attach: None,
    };
    let used = bundle.or_else(|| config.bundle.clone());
    attach(
        ctx,
        &attached,
        &config,
        &Injection {
            prepared: &prepared,
            used: used.as_deref(),
            reinject,
            no_attach,
        },
        place,
    )
}

/// What this run injects, and whether it may be skipped or must be done again.
struct Injection<'a> {
    prepared: &'a bundle::source::Prepared,
    /// The reference the bundle was asked for by, for the history; `None` for the
    /// config packed on the spot.
    used: Option<&'a str>,
    reinject: bool,
    no_attach: bool,
}

/// Works in an endpoint that is up: forwards its ports, injects (unless that was
/// done already), delivers the secrets and opens the session. Knows nothing of
/// how the endpoint was made, except `place`, which is what the history is told
/// once a session is about to open.
fn attach(
    ctx: &Ctx,
    attached: &Attached,
    config: &Config,
    injection: &Injection,
    place: Place,
) -> Result<()> {
    let Injection {
        prepared,
        used,
        reinject,
        no_attach,
    } = *injection;
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
            if found.bundle != prepared.bundle.manifest().id() {
                note!(
                    "what is injected is out of date: the container has {}, this run uses {}; \
                     `ldg up --reinject` updates it",
                    describe(&found.bundle, found.source.as_deref()),
                    describe(
                        prepared.bundle.manifest().id().as_str(),
                        prepared.source.as_deref()
                    ),
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
    ctx.remember(place, attached.name.as_deref(), used);
    attached.run_post_attach();
    // The container is one person's workspace and the secrets are about to become
    // this client's, so a session an earlier client left running goes first.
    if session::lease::take_over(container, &user, &layout)? {
        note!("ended the session another client had in this container");
    }
    let session_id = session::lease::new_id();
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
            id: &session_id,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::testing::{manifest, prepared};
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
            history: Box::new(crate::history::Memory::default()),
            choose: Box::new(Picks::nobody()),
            route: crate::history::Route {
                target: "docker".into(),
                remote: None,
            },
            config_path: dir.path().join("config.toml"),
        };
        let attached = Attached {
            container: rt.container("c1"),
            created: false,
            workdir: "/w".into(),
            name: None,
            forward_ports: Vec::new(),
            remote_user: None,
            post_attach: None,
        };
        let (_bundle, prepared) = prepared(&manifest("x86_64"), &[]);

        let place = Place::Project { path: "/p".into() };
        attach(
            &ctx,
            &attached,
            &Config::default(),
            &Injection {
                prepared: &prepared,
                used: None,
                reinject: false,
                no_attach: true,
            },
            place,
        )
        .unwrap();
        // Setting up without a shell is not a session, so it is not remembered.
        assert!(ctx.history.entries().unwrap().is_empty());

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

    /// A context whose engine has the running container `c1` and no other, built
    /// on `endpoints`.
    fn ctx_with(endpoints: Box<dyn crate::provision::Endpoints>, dir: &TempDir) -> Ctx {
        Ctx {
            host: fake(),
            local: Arc::new(FakeLocal::new()),
            forwarder: Box::new(RecordingDaemon(Rc::default())),
            endpoints,
            tools: tools::external(),
            bundles: crate::bundle::store::Stores::new(vec![]),
            sinks: crate::bundle::store::Sinks::new(vec![]),
            history: Box::new(crate::history::Memory::default()),
            choose: Box::new(Picks::nobody()),
            route: crate::history::Route {
                target: "docker".into(),
                remote: None,
            },
            config_path: dir.path().join("config.toml"),
        }
    }

    #[test]
    fn a_session_is_remembered_under_the_endpoints_name_else_the_places_own() {
        let dir = TempDir::new("attach-remember").unwrap();
        let rt = Rc::new(FakeRuntime::new(FakeEnv::new));
        let ctx = ctx_with(
            Box::new(Devcontainer::new(
                fake(),
                Arc::new(FakeLocal::new()),
                Box::new(rt),
                Arc::new(tools::NoRegistry),
                Arc::new(NoQuestions),
            )),
            &dir,
        );
        let project = |path: &str| Place::Project { path: path.into() };
        ctx.remember(project("/work/api"), Some("Demo API"), Some("a.tar"));
        ctx.remember(project("/work/web"), None, None);
        let entries = ctx.history.entries().unwrap();
        let seen: Vec<_> = entries
            .iter()
            .map(|e| (e.name.as_str(), e.route.target.as_str()))
            .collect();
        assert_eq!(seen, [("web", "docker"), ("Demo API", "docker")]);
        // What each session was injected from is kept with it.
        let bundles: Vec<_> = entries.iter().map(|e| e.bundle.as_deref()).collect();
        assert_eq!(bundles, [None, Some("a.tar")]);
    }

    /// Answers a question with a row, or says there is nobody to ask, and keeps
    /// what it was asked.
    struct Picks {
        row: Option<usize>,
        can_ask: bool,
        offered: Rc<RefCell<Vec<Vec<String>>>>,
    }

    impl Picks {
        fn nobody() -> Self {
            Picks {
                row: None,
                can_ask: false,
                offered: Rc::default(),
            }
        }

        fn answering(row: Option<usize>) -> (Self, Rc<RefCell<Vec<Vec<String>>>>) {
            let offered = Rc::new(RefCell::new(Vec::new()));
            let picks = Picks {
                row,
                can_ask: true,
                offered: offered.clone(),
            };
            (picks, offered)
        }
    }

    impl crate::cmd::prompt::Choose for Picks {
        fn can_ask(&self) -> bool {
            self.can_ask
        }

        fn choose(&self, _: &str, options: &crate::cmd::table::Table) -> Result<Option<usize>> {
            self.offered.borrow_mut().push(options.natural());
            Ok(self.row)
        }
    }

    /// A context that asks through `choose`, with `history` already holding the
    /// places in it.
    fn ctx_choosing(choose: Picks, known: &[&str], dir: &TempDir) -> Ctx {
        let mut ctx = ctx_with(
            Box::new(Devcontainer::new(
                fake(),
                Arc::new(FakeLocal::new()),
                Box::new(Rc::new(FakeRuntime::new(FakeEnv::new))),
                Arc::new(tools::NoRegistry),
                Arc::new(NoQuestions),
            )),
            dir,
        );
        ctx.choose = Box::new(choose);
        for path in known {
            ctx.remember(
                Place::Project {
                    path: (*path).into(),
                },
                None,
                None,
            );
        }
        ctx
    }

    fn offering(default: Option<&str>, others: &[&str]) -> Config {
        Config {
            bundle: default.map(Into::into),
            bundles: others.iter().map(|o| o.to_string()).collect(),
            ..Config::default()
        }
    }

    fn project(path: &str) -> Place {
        Place::Project { path: path.into() }
    }

    #[test]
    fn a_new_place_with_bundles_on_offer_asks_and_a_pick_other_than_the_default_is_used() {
        let dir = TempDir::new("choose-new").unwrap();
        let (picks, offered) = Picks::answering(Some(1));
        let ctx = ctx_choosing(picks, &[], &dir);
        let config = offering(Some("a.tar"), &["b.tar"]);
        let chosen = choose_bundle(&ctx, &config, None, &project("/new"), false).unwrap();
        assert_eq!(chosen.as_deref(), Some("b.tar"));
        let asked = offered.borrow();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].len(), 2, "{asked:?}");
    }

    #[test]
    fn picking_the_default_leaves_the_config_to_say_it() {
        let dir = TempDir::new("choose-default").unwrap();
        let (picks, _) = Picks::answering(Some(0));
        let ctx = ctx_choosing(picks, &[], &dir);
        let config = offering(Some("a.tar"), &["b.tar"]);
        assert_eq!(
            choose_bundle(&ctx, &config, None, &project("/new"), false).unwrap(),
            None
        );
    }

    #[test]
    fn declining_the_question_is_the_default_too() {
        let dir = TempDir::new("choose-decline").unwrap();
        let (picks, offered) = Picks::answering(None);
        let ctx = ctx_choosing(picks, &[], &dir);
        let config = offering(Some("a.tar"), &["b.tar"]);
        assert_eq!(
            choose_bundle(&ctx, &config, None, &project("/new"), false).unwrap(),
            None
        );
        assert_eq!(offered.borrow().len(), 1);
    }

    #[test]
    fn a_bundle_asked_for_is_not_questioned() {
        let dir = TempDir::new("choose-flag").unwrap();
        let (picks, offered) = Picks::answering(Some(1));
        let ctx = ctx_choosing(picks, &[], &dir);
        let config = offering(Some("a.tar"), &["b.tar"]);
        let chosen = choose_bundle(&ctx, &config, Some("c.tar"), &project("/new"), true).unwrap();
        assert_eq!(chosen.as_deref(), Some("c.tar"));
        assert!(offered.borrow().is_empty());
    }

    #[test]
    fn with_one_bundle_there_is_nothing_to_ask() {
        let dir = TempDir::new("choose-one").unwrap();
        let (picks, offered) = Picks::answering(Some(0));
        let ctx = ctx_choosing(picks, &[], &dir);
        for config in [offering(Some("a.tar"), &[]), offering(None, &[])] {
            assert_eq!(
                choose_bundle(&ctx, &config, None, &project("/new"), true).unwrap(),
                None
            );
        }
        assert!(offered.borrow().is_empty());
    }

    #[test]
    fn a_place_opened_before_is_not_asked_about_unless_it_is_injected_again() {
        let dir = TempDir::new("choose-known").unwrap();
        let (picks, offered) = Picks::answering(Some(1));
        let ctx = ctx_choosing(picks, &["/known"], &dir);
        let config = offering(Some("a.tar"), &["b.tar"]);
        let place = project("/known");
        assert_eq!(
            choose_bundle(&ctx, &config, None, &place, false).unwrap(),
            None
        );
        assert!(offered.borrow().is_empty());
        let again = choose_bundle(&ctx, &config, None, &place, true).unwrap();
        assert_eq!(again.as_deref(), Some("b.tar"));
        assert_eq!(offered.borrow().len(), 1);
    }

    #[test]
    fn without_anybody_to_ask_the_default_is_used_and_nothing_is_asked() {
        let dir = TempDir::new("choose-nobody").unwrap();
        let ctx = ctx_choosing(Picks::nobody(), &[], &dir);
        let config = offering(Some("a.tar"), &["b.tar"]);
        assert_eq!(
            choose_bundle(&ctx, &config, None, &project("/new"), true).unwrap(),
            None
        );
    }

    #[test]
    fn the_packed_config_is_a_choice_when_it_is_the_default() {
        let dir = TempDir::new("choose-live").unwrap();
        let (picks, _) = Picks::answering(Some(1));
        let ctx = ctx_choosing(picks, &[], &dir);
        let config = offering(None, &["b.tar"]);
        let chosen = choose_bundle(&ctx, &config, None, &project("/new"), false).unwrap();
        assert_eq!(chosen.as_deref(), Some("b.tar"));
    }

    #[test]
    fn a_container_that_is_stopped_or_not_there_is_refused_and_never_started() {
        let dir = TempDir::new("attach-gone").unwrap();
        let rt = Rc::new(FakeRuntime::new(FakeEnv::new).with_container("c1", FakeEnv::new()));
        let ctx = ctx_with(
            Box::new(Devcontainer::new(
                fake(),
                Arc::new(FakeLocal::new()),
                Box::new(rt.clone()),
                Arc::new(tools::NoRegistry),
                Arc::new(NoQuestions),
            )),
            &dir,
        );
        let err = super::attach_to(&ctx, "nope", None, false, true, None)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("nope is not running") && err.contains("did not make"),
            "{err}"
        );
        assert!(rt.ops().is_empty(), "nothing was started: {:?}", rt.ops());
    }

    #[test]
    fn a_target_without_an_engine_has_nothing_to_attach_to() {
        let dir = TempDir::new("attach-none").unwrap();
        let ctx = ctx_with(
            Box::new(crate::plain_host::PlainHostEndpoints::new(fake())),
            &dir,
        );
        let err = super::attach_to(&ctx, "c1", None, false, true, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no container engine"), "{err}");
        assert!(ctx.existing_container("c1").is_err());
    }
}
