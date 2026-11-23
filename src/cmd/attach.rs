//! `ldg up`: get the project's container ready, inject if needed, open a session.

use anyhow::Result;
use std::path::PathBuf;

use super::Ctx;
use super::bundle as bundle_cmd;
use super::prompt::Choose;
use super::table::{Cell, Table, Tone};
use crate::bundle::choice;
use crate::config::Config;
use crate::config::client::Client;
use crate::history::Place;
use crate::layout::Layout;
use crate::output::{note, warning};
use crate::provision::Attached;
use crate::runtime::{Container, ContainerUser};
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
    let settings = ctx.client()?;
    let place = Place::Project {
        path: project.clone(),
    };
    // Which bundle comes first, and is readied before anything starts, so a
    // problem with it fails before a container is made.
    let bundle = choose_bundle(ctx, &settings, bundle, &place, reinject)?
        .or_else(|| settings.bundle.clone());
    let prepared = bundle::source::prepare(&config, bundle.as_deref(), &ctx.bundles, &ctx.tools)?;
    // How an endpoint is made, or found, is the endpoints' business; which
    // endpoints is chosen once, in `cmd::run`.
    let attached = ctx.endpoints.provision(&project, build)?;
    attach(
        ctx,
        &attached,
        (&config, &settings),
        &Injection {
            prepared: &prepared,
            used: bundle.as_deref(),
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
    settings: &Client,
    explicit: Option<&str>,
    place: &Place,
    reinject: bool,
) -> Result<Option<String>> {
    if explicit.is_some() {
        return Ok(explicit.map(str::to_string));
    }
    let default = choice::default(settings);
    let offered = choice::options(&default, &settings.bundles);
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
    let settings = ctx.client()?;
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
    let bundle = choose_bundle(ctx, &settings, bundle, &place, reinject)?
        .or_else(|| settings.bundle.clone());
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
    attach(
        ctx,
        &attached,
        (&config, &settings),
        &Injection {
            prepared: &prepared,
            used: bundle.as_deref(),
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
    (config, settings): (&Config, &Client),
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
    // The container is one person's workspace and the secrets are about to become
    // this client's, so a session an earlier client left running goes first. Asked
    // before anything is recorded or run for this attach, since the answer may be no.
    let client = session::lease::client(ctx.local.as_ref());
    if !make_room(ctx.choose.as_ref(), container, &user, &layout, &client)? {
        note!("not attaching; the other session is still there");
        return Ok(());
    }
    ctx.remember(place, attached.name.as_deref(), used);
    attached.run_post_attach();
    let session_id = session::lease::new_id();
    let setup = secret::deliver::deliver(
        container,
        ctx.host.as_ref(),
        &user,
        &settings.secret,
        ctx.local.as_ref(),
    )?;
    let result = session::enter(
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
            client: &client,
        },
    );
    // The secrets last as long as the session: now they go, unless another client
    // has taken the container over, and they are its secrets. This runs whether the
    // shell ended well or not.
    let secrets = crate::layout::secrets_dir(&user.uid.to_string());
    if let Err(e) = session::lease::release(container, &user, &layout, &session_id, &[&secrets]) {
        warning!(
            "could not remove the secrets from {}: {e:#}; they stay until it stops",
            container.display_name()
        );
    }
    // A shell that was hung up because another client attached would only look like a
    // failure: say who took over instead. (After the release above, which must not be
    // undone by the check: it only gives up what this session still holds.)
    match result {
        Err(e)
            if e.downcast_ref::<session::Ended>()
                .is_some_and(|e| e.hung_up()) =>
        {
            let wait = std::time::Duration::from_millis(400);
            match session::lease::holder_soon(container, &user, &layout, 6, wait) {
                Some(client) => {
                    note!(
                        "this session was ended: {client} attached to {}",
                        container.display_name()
                    );
                    Ok(())
                }
                None => Err(e),
            }
        }
        other => other,
    }
}

/// Ends the session that holds the container so this client's can open. A
/// session from this same client (same user, same machine) is replaced without
/// asking: it is almost always one that was left behind. One from another client
/// is asked about first, since ending it also takes its secrets away. False when
/// the person says no. An error when there is a session of another client and
/// nobody to ask.
fn make_room(
    choose: &dyn Choose,
    container: &dyn Container,
    user: &ContainerUser,
    layout: &Layout,
    client: &str,
) -> Result<bool> {
    let name = container.display_name();
    match session::lease::holder(container, user, layout)? {
        None => return Ok(true),
        Some(holder) if holder == client => {
            note!("replacing the session this machine left open in {name}");
        }
        Some(holder) => {
            if !choose.can_ask() {
                anyhow::bail!(
                    "{holder} has a session open in {name}; attaching would end it and \
                     remove its secrets. Run this from a terminal to confirm"
                );
            }
            let options = Table::new(
                vec!["DO"],
                vec![
                    vec![Cell::new("end it and attach", Tone::Strong)],
                    vec![Cell::new("cancel", Tone::Muted)],
                ],
                0,
            );
            let question = format!(
                "{holder} has a session open in {name}. Attaching ends it and removes the \
                 secrets placed for it (a connection that is only stale is fine to end)"
            );
            if choose.choose(&question, &options)? != Some(0) {
                return Ok(false);
            }
        }
    }
    if session::lease::take_over(container, user, layout)? {
        note!("ended the session that was open in {name}");
    }
    Ok(true)
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
            paths: crate::config::Paths::beside(
                dir.path().join("config.toml"),
                crate::files::disk(),
            ),
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
            (&Config::default(), &Client::default()),
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
            paths: crate::config::Paths::beside(
                dir.path().join("config.toml"),
                crate::files::disk(),
            ),
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

    /// Whether a command that ends a session ran in container `c`.
    fn ended_a_session(rt: &FakeRuntime) -> bool {
        rt.calls("c")
            .iter()
            .any(|(argv, _)| argv.iter().any(|a| a.contains("kill -HUP")))
    }

    fn make_room_with(holder: Option<&str>, choose: &Picks) -> (Result<bool>, bool) {
        let layout = Layout::new("/home/dev/.local/share/lodge");
        let env = session::lease::with_holder(FakeEnv::new(), &layout, holder);
        let rt = FakeRuntime::new(FakeEnv::new).with_container("c", env);
        let container = rt.container("c");
        let go = make_room(choose, container.as_ref(), &user(), &layout, "kai@laptop");
        (go, ended_a_session(&rt))
    }

    #[test]
    fn nobody_in_the_container_means_nothing_to_end_or_ask() {
        let (go, ended) = make_room_with(None, &Picks::nobody());
        assert!(go.unwrap());
        assert!(!ended);
    }

    #[test]
    fn a_session_this_client_left_is_replaced_without_asking() {
        // `nobody` would be an error if it were asked.
        let (go, ended) = make_room_with(Some("kai@laptop"), &Picks::nobody());
        assert!(go.unwrap());
        assert!(ended);
    }

    #[test]
    fn another_clients_session_is_ended_only_when_the_person_agrees() {
        let (agree, asked) = Picks::answering(Some(0));
        let (go, ended) = make_room_with(Some("kai@desktop"), &agree);
        assert!(go.unwrap() && ended);
        assert_eq!(asked.borrow().len(), 1);

        let (decline, _) = Picks::answering(Some(1));
        let (go, ended) = make_room_with(Some("kai@desktop"), &decline);
        assert!(!go.unwrap() && !ended, "declining leaves the session alone");
        let (leave, _) = Picks::answering(None);
        let (go, ended) = make_room_with(Some("kai@desktop"), &leave);
        assert!(!go.unwrap() && !ended);
    }

    #[test]
    fn another_clients_session_with_nobody_to_ask_is_an_error_that_names_it() {
        let (go, ended) = make_room_with(Some("kai@desktop"), &Picks::nobody());
        let err = format!("{:#}", go.unwrap_err());
        assert!(
            err.contains("kai@desktop") && err.contains("terminal"),
            "{err}"
        );
        assert!(!ended);
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

    fn offering(default: Option<&str>, others: &[&str]) -> Client {
        Client {
            bundle: default.map(Into::into),
            bundles: others.iter().map(|o| o.to_string()).collect(),
            ..Client::default()
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
