//! Commands about a project's container (or all of lodge's): `down`, `status`,
//! `rm`, `ls`, `ports`, `logs`, `clean`.

use anyhow::{Context, Result, bail};
use serde_json::json;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Ctx, print_json};
use crate::config::Config;
use crate::output::{info, line, note, warning};
use crate::provision::{ConfigState, Endpoints, Found, Lifecycle};
use crate::runtime::ContainerUser;

/// The project's container, or an error saying there is none.
fn require(ctx: &Ctx, path: Option<PathBuf>) -> Result<Found> {
    let project = ctx.project(path)?;
    ctx.endpoints.find(&project)?.with_context(|| {
        format!(
            "no container for {} (run `ldg up` to create it)",
            project.display()
        )
    })
}

/// What it takes to `what` (stop, remove, list) the target's endpoints, or an
/// error saying this target has none of that.
fn lifecycle<'a>(ctx: &'a Ctx, what: &str) -> Result<&'a dyn Lifecycle> {
    ctx.endpoints.lifecycle().with_context(|| {
        format!("this target has nothing to {what}; `ldg clean` takes lodge out of it")
    })
}

pub fn down(ctx: &Ctx, path: Option<PathBuf>) -> Result<()> {
    let project = ctx.project(path)?;
    match lifecycle(ctx, "stop")?.stop(&project)? {
        Some(id) => info!("stopped {id}"),
        None => info!("no container exists for this devcontainer"),
    }
    Ok(())
}

pub fn rm(ctx: &Ctx, path: Option<PathBuf>) -> Result<()> {
    let project = ctx.project(path)?;
    match lifecycle(ctx, "remove")?.remove(&project)? {
        Some(id) => info!("removed {id}"),
        None => info!("no container exists for this devcontainer"),
    }
    Ok(())
}

pub fn status(ctx: &Ctx, path: Option<PathBuf>, as_json: bool) -> Result<()> {
    let project = ctx.project(path)?;
    let config = ctx.config()?;
    let report = StatusReport::gather(&project, ctx.endpoints.as_ref(), &config)?;
    if as_json {
        return print_json(&report.to_json());
    }
    for l in report.lines() {
        line!("{l}");
    }
    Ok(())
}

/// What `ldg status` reports about a project.
struct StatusReport {
    project: PathBuf,
    /// `None` when the project has no container.
    container: Option<ContainerStatus>,
}

struct ContainerStatus {
    id: String,
    name: String,
    running: bool,
    /// `None` for an endpoint that is made from no config (a plain host).
    config: Option<ConfigState>,
    /// The bundle installed (`Some(None)`: none); `None` when the container
    /// could not say.
    bundle: Option<Option<String>>,
    /// Each declared secret and whether it is placed; `None` when a running
    /// container could not say.
    secrets: Option<Vec<(String, bool)>>,
}

impl StatusReport {
    fn gather(project: &Path, endpoints: &dyn Endpoints, config: &Config) -> Result<Self> {
        let Some(status) = endpoints.status(project)? else {
            return Ok(Self {
                project: project.to_path_buf(),
                container: None,
            });
        };
        let (found, running) = (&status.found, status.running);
        let c = found.container.as_ref();
        // A stopped container can't be asked who its user is, so it is looked up
        // in its passwd file; either way the user tells where lodge installed.
        let user = if running {
            ContainerUser::resolve(c, found.remote_user.as_deref())
        } else {
            ContainerUser::from_stored(c, found.remote_user.as_deref())
                .and_then(|u| u.context("the container has no such user"))
        };
        let bundle = match &user {
            Ok(user) => crate::inject::installed(c, user)
                .map(|found| found.map(|i| i.bundle))
                .map_err(|e| warning!("could not tell which bundle is installed: {e:#}"))
                .ok(),
            Err(e) => {
                warning!("could not tell which bundle is installed: {e:#}");
                None
            }
        };
        // Secrets live in memory, so a stopped container has none.
        let secrets = if !running || config.secret.is_empty() {
            Some(
                config
                    .secret
                    .iter()
                    .map(|d| (d.name.clone(), false))
                    .collect(),
            )
        } else {
            user.and_then(|user| crate::secret::place::status(c, &user, &config.secret))
                .map_err(|e| warning!("could not tell which secrets are placed: {e:#}"))
                .ok()
        };
        Ok(Self {
            project: project.to_path_buf(),
            container: Some(ContainerStatus {
                id: c.id().to_string(),
                name: c.display_name(),
                running,
                config: status.config,
                bundle,
                secrets,
            }),
        })
    }

    fn to_json(&self) -> serde_json::Value {
        let Some(c) = &self.container else {
            return json!({ "project": self.project, "container": null });
        };
        json!({
            "project": self.project,
            "container": {
                "id": c.id,
                "name": c.name,
                "running": c.running,
                "config": c.config,
                "bundle": c.bundle.clone().flatten(),
                "secrets": c.secrets.as_ref().map(|s| {
                    s.iter()
                        .map(|(n, p)| json!({"name": n, "placed": p}))
                        .collect::<Vec<_>>()
                }),
            },
        })
    }

    /// The text report, one line each.
    fn lines(&self) -> Vec<String> {
        let mut out = vec![format!("project    {}", self.project.display())];
        let Some(c) = &self.container else {
            out.push("container  none (run `ldg up` to create it)".into());
            return out;
        };
        let state = if c.running { "running" } else { "stopped" };
        out.push(format!("container  {} ({state})", c.name));
        if let Some(config) = c.config {
            out.push(format!(
                "config     {}",
                match config {
                    ConfigState::UpToDate => "up to date",
                    ConfigState::Changed =>
                        "changed since the container was created (`ldg up` offers to recreate it)",
                    ConfigState::Unknown => "unknown (no recorded config, or it could not be read)",
                }
            ));
        }
        match &c.bundle {
            Some(Some(id)) => out.push(format!("bundle     {id}")),
            Some(None) => {}
            None => out.push("bundle     unknown".into()),
        }
        match &c.secrets {
            None => out.push("secrets    unknown".into()),
            Some(s) if s.is_empty() => {}
            Some(s) => {
                let shown: Vec<String> = s
                    .iter()
                    .map(|(n, placed)| {
                        format!("{n} ({})", if *placed { "placed" } else { "not placed" })
                    })
                    .collect();
                out.push(format!("secrets    {}", shown.join(", ")));
            }
        }
        out
    }
}

pub fn ls(ctx: &Ctx, as_json: bool) -> Result<()> {
    let listed = lifecycle(ctx, "list")?.list()?;
    if as_json {
        return print_json(&json!(
            listed
                .iter()
                .map(|l| json!({"id": l.id, "name": l.name, "running": l.running, "project": l.label}))
                .collect::<Vec<_>>()
        ));
    }
    if listed.is_empty() {
        note!("no containers created by lodge{}", ctx.host.on_host());
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
    let active = crate::forward::active_ports(id)?;
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
    let log = crate::forward::log_path(found.container.id())?;
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

pub fn clean(ctx: &Ctx, path: Option<PathBuf>, existing: Option<&str>) -> Result<()> {
    let Found {
        container,
        remote_user,
    } = match existing {
        Some(name) => Found {
            container: ctx.existing_container(name)?,
            remote_user: None,
        },
        None => require(ctx, path)?,
    };
    // Lodge's forwarding is part of what is taken out, and on a machine that is
    // always up nothing else would end it.
    ctx.forwarder.stop(container.id());
    if !container.is_running()? {
        bail!("the container is not running; start it first (`ldg up --no-attach`)");
    }
    let user = ContainerUser::resolve(container.as_ref(), remote_user.as_deref())?;
    let Some(installed) = crate::inject::installed(container.as_ref(), &user)? else {
        info!("nothing is injected into {}", container.display_name());
        return Ok(());
    };
    let removed = crate::inject::remove(container.as_ref(), &user, &installed)?;
    crate::secret::place::clear(container.as_ref(), &user)?;
    info!("removed lodge from {}", container.display_name());

    for w in &removed.wrappers {
        line!("wrapper  {w}");
    }
    for d in &removed.safe_directories {
        line!("git safe.directory  {d}");
    }
    if !removed.configs_kept.is_empty() {
        note!(
            "injected configs were left in place (remove them by hand if unwanted):\n  {}",
            removed.configs_kept.join("\n  ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devcontainer::Devcontainer;
    use crate::devcontainer::Prompt;
    use crate::host::HostRef;
    use crate::host::fake::FakeHost;
    use crate::local::fake::FakeLocal;
    use crate::runtime::fake::{FakeEnv, FakeRuntime};
    use crate::tools::NoRegistry;
    use std::rc::Rc;
    use std::sync::Arc;

    struct NoQuestions;

    impl Prompt for NoQuestions {
        fn choose_workdir(&self, _: &str, _: &[String]) -> String {
            panic!("the workspace exists")
        }
    }

    /// A project with an image-based devcontainer, and the host its files are on.
    struct Project {
        host: HostRef,
    }

    impl Project {
        fn path(&self) -> &std::path::Path {
            std::path::Path::new("/proj")
        }
    }

    fn project() -> Project {
        Project {
            host: Arc::new(FakeHost::new().with_file(
                "/proj/.devcontainer/devcontainer.json",
                r#"{"image": "debian", "workspaceFolder": "/w", "updateRemoteUserUID": false}"#,
            )),
        }
    }

    /// An engine that stays in the test's hands, whatever owns it.
    fn engine(new_env: impl Fn() -> FakeEnv + 'static) -> Rc<FakeRuntime> {
        Rc::new(FakeRuntime::new(new_env))
    }

    /// The project's endpoints on `rt`.
    fn endpoints(dir: &Project, rt: &Rc<FakeRuntime>) -> Devcontainer {
        Devcontainer::new(
            dir.host.clone(),
            Arc::new(FakeLocal::new()),
            Box::new(rt.clone()),
            Arc::new(NoRegistry),
            Arc::new(NoQuestions),
        )
    }

    #[test]
    fn a_project_without_a_container_says_so() {
        let dir = project();
        let rt = engine(FakeEnv::new);
        let r =
            StatusReport::gather(dir.path(), &endpoints(&dir, &rt), &Config::default()).unwrap();
        assert_eq!(r.lines()[1], "container  none (run `ldg up` to create it)");
        assert_eq!(r.to_json()["container"], serde_json::Value::Null);
    }

    fn start(dir: &Project, rt: &Rc<FakeRuntime>) -> Box<dyn crate::runtime::Container> {
        endpoints(dir, rt)
            .provision(dir.path(), false)
            .unwrap()
            .container
    }

    fn one_secret() -> Config {
        Config::parse(
            "[[secret]]\nname = \"T\"\nfrom = { env = \"X\" }\nas = \"env\"\n",
            Path::new("/c.toml"),
        )
        .unwrap()
    }

    const PROBE: [&str; 3] = ["sh", "-c", crate::runtime::PROBE_SCRIPT];

    #[test]
    fn a_running_container_reports_its_config_bundle_and_secrets() {
        let dir = project();
        let list = crate::shell::script_argv(
            crate::secret::place::LIST_SCRIPT,
            &["/dev/shm/lodge-secrets-1000"],
        );
        let rt = engine(move || {
            FakeEnv::new()
                .with_file("/w", "")
                .with_output(&PROBE, "1000\n1000\ndev\n/home/dev\n")
                .with_output(&list, "T\n")
        });
        let c = start(&dir, &rt);
        c.write_file(
            "/home/dev/.local/share/lodge/injected",
            b"bundle 0123456789abcdef\n",
        )
        .unwrap();

        let r = StatusReport::gather(dir.path(), &endpoints(&dir, &rt), &one_secret()).unwrap();
        assert_eq!(
            r.lines()[1..],
            [
                format!("container  {} (running)", c.id()),
                "config     up to date".into(),
                "bundle     0123456789abcdef".into(),
                "secrets    T (placed)".into(),
            ]
        );
        let json = r.to_json();
        assert_eq!(json["container"]["config"], "up_to_date");
        assert_eq!(json["container"]["bundle"], "0123456789abcdef");
        assert_eq!(json["container"]["secrets"][0]["placed"], true);
    }

    #[test]
    fn a_stopped_container_is_reported_in_full_without_running_anything_in_it() {
        let dir = project();
        // A stopped container can't say who its user is, so it is looked up in its
        // passwd file (root, as the image sets no other user).
        let rt = engine(|| {
            FakeEnv::new()
                .with_file("/w", "")
                .with_file("/etc/passwd", "root:x:0:0:root:/root:/bin/sh\n")
        });
        let c = start(&dir, &rt);
        c.write_file(
            "/root/.local/share/lodge/injected",
            b"bundle 0123456789abcdef\n",
        )
        .unwrap();
        crate::runtime::Runtime::stop(&rt, c.id()).unwrap();
        // The fake refuses commands in a stopped container, so a command would
        // show up as "unknown" (and a warning).
        let r = StatusReport::gather(dir.path(), &endpoints(&dir, &rt), &one_secret()).unwrap();
        assert_eq!(
            r.lines()[1..],
            [
                format!("container  {} (stopped)", c.id()),
                "config     up to date".into(),
                "bundle     0123456789abcdef".into(),
                "secrets    T (not placed)".into(),
            ]
        );
    }

    #[test]
    fn secrets_are_unknown_when_the_container_user_cannot_be_looked_up() {
        let dir = project();
        let rt = engine(|| FakeEnv::new().with_file("/w", "").with_failure(&PROBE));
        start(&dir, &rt);
        let r = StatusReport::gather(dir.path(), &endpoints(&dir, &rt), &one_secret()).unwrap();
        assert_eq!(r.lines().last().unwrap(), "secrets    unknown");
        assert_eq!(r.to_json()["container"]["secrets"], serde_json::Value::Null);
    }
}
