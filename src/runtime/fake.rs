//! In-memory fakes for tests. [`FakeEnv`] is an [`Env`]: a virtual filesystem
//! plus a record of every command that was run, which lets injection logic be
//! tested without a container. [`FakeRuntime`] is a [`Runtime`] (and
//! [`Compose`]) whose containers each run in a `FakeEnv`, which lets
//! `devcontainer::start`'s decisions (create, reuse, recreate) be tested
//! without an engine.
//!
//! It is deliberately strict. It emulates a small set of commands (`cat`, `cp`,
//! `rm`, `test`, `command -v`, [`Env::read_file`], [`Env::exists`],
//! and the no-output file operations), and a test declares anything else it
//! depends on (`with_output`, `with_failure`, `with_commands`). A command it
//! neither emulates nor was told about **panics**, naming the command, so a test
//! can't pass by accident because the fake answered "success" to something the
//! real container might refuse. A container of a [`FakeRuntime`] that is
//! stopped refuses every command, as `docker exec` does.
//!
//! One exception: a `sh -c` script run for its effect (with `exec`, not
//! captured) is recorded and reported as succeeded, since its effect can't be
//! emulated. Tests of such scripts assert on the recorded script and on who it
//! ran as, and scripts whose behaviour matters are also run for real against a
//! temp directory (see `secret/place.rs`).

use anyhow::{Context, Result};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{
    BuildSpec, Compose, Container, ContainerUser, Containers, Env, ExecOpts, Listed, Output, Reach,
    RunSpec, Runtime, User,
};
use crate::host::{Child, Stream};
use crate::tree::{self, Denied, Links};

/// Who a recorded command ran as ([`User`] without the borrow).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Who {
    Default,
    Root,
    Named(String),
}

impl From<User<'_>> for Who {
    fn from(user: User) -> Self {
        match user {
            User::Default => Who::Default,
            User::Root => Who::Root,
            User::Named(n) => Who::Named(n.to_string()),
        }
    }
}

pub struct FakeEnv {
    files: RefCell<HashMap<String, Vec<u8>>>,
    canned: HashMap<Vec<String>, String>,
    failing: RefCell<Vec<Vec<String>>>,
    commands: Vec<String>,
    modes: RefCell<HashMap<String, u32>>,
    /// The `uid:gid` that owns each file copied in with [`Env::put`].
    owners: RefCell<HashMap<String, (u32, u32)>>,
    /// Every command run, with whom it ran as.
    calls: RefCell<Vec<(Vec<String>, Who)>>,
    /// How [`Container::reach`] answers, by peer (`None`: the container itself).
    reach: HashMap<Option<String>, Reach>,
    /// Whether this stands in for a machine that is not thrown away.
    shared: bool,
    /// The relays [`Container::relay`] started: local port, container port, and
    /// whether each was stopped.
    relays: RefCell<Vec<(u16, u16, Arc<AtomicBool>)>>,
}

/// A relay process that is only a flag: stopped when killed.
struct FakeRelay(Arc<AtomicBool>);

impl Child for FakeRelay {
    fn kill(&mut self) -> std::io::Result<()> {
        self.0.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        use std::os::unix::process::ExitStatusExt as _;
        Ok(self
            .0
            .load(Ordering::Relaxed)
            .then(|| std::process::ExitStatus::from_raw(0)))
    }
}

impl FakeEnv {
    pub fn new() -> Self {
        Self {
            files: RefCell::default(),
            canned: HashMap::new(),
            failing: RefCell::default(),
            commands: Vec::new(),
            modes: RefCell::default(),
            owners: RefCell::default(),
            calls: RefCell::default(),
            reach: HashMap::new(),
            shared: false,
            relays: RefCell::default(),
        }
    }

    /// The relays started so far, as (local port, container port, stopped).
    pub fn relays(&self) -> Vec<(u16, u16, bool)> {
        self.relays
            .borrow()
            .iter()
            .map(|(l, p, stopped)| (*l, *p, stopped.load(Ordering::Relaxed)))
            .collect()
    }

    /// Ends the relay started `index`th, as a process that died would.
    pub fn end_relay(&self, index: usize) {
        self.relays.borrow()[index].2.store(true, Ordering::Relaxed);
    }

    /// Stands in for a real machine, which injection treats with more care.
    pub fn shared(mut self) -> Self {
        self.shared = true;
        self
    }

    /// Makes the container reachable at `reach` (for `peer`, a peer by name;
    /// `None` is the container itself). Any other peer is not found.
    pub fn with_reach(mut self, peer: Option<&str>, reach: Reach) -> Self {
        self.reach.insert(peer.map(str::to_string), reach);
        self
    }

    /// Makes the exact command `argv` print `stdout` and succeed.
    pub fn with_output(mut self, argv: &[&str], stdout: &str) -> Self {
        self.canned
            .insert(argv.iter().map(|s| s.to_string()).collect(), stdout.into());
        self
    }

    /// Makes the exact command `argv` exit non-zero.
    pub fn with_failure(self, argv: &[&str]) -> Self {
        self.failing
            .borrow_mut()
            .push(argv.iter().map(|s| s.to_string()).collect());
        self
    }

    /// Makes `argv`, declared with [`with_failure`](Self::with_failure),
    /// succeed from now on: something that failed once works on a later run.
    pub fn recover(&self, argv: &[&str]) {
        let key: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        self.failing.borrow_mut().retain(|f| *f != key);
    }

    /// Declares which programs exist in the container, for `command -v` checks.
    pub fn with_commands(mut self, names: &[&str]) -> Self {
        self.commands.extend(names.iter().map(|n| n.to_string()));
        self
    }

    /// Pre-populates a file.
    pub fn with_file(self, path: &str, contents: &str) -> Self {
        self.files
            .borrow_mut()
            .insert(path.into(), contents.as_bytes().to_vec());
        self
    }

    /// The text of a file, if it exists.
    pub fn file(&self, path: &str) -> Option<String> {
        self.files
            .borrow()
            .get(path)
            .map(|b| String::from_utf8_lossy(b).into_owned())
    }

    /// The `(uid, gid)` that owns a file copied in with [`Env::put`].
    pub fn owner_of(&self, path: &str) -> Option<(u32, u32)> {
        self.owners.borrow().get(path).copied()
    }

    /// The permission bits a file was last written with, by `write_secret` or
    /// [`Env::put`].
    pub fn mode(&self, path: &str) -> Option<u32> {
        self.modes.borrow().get(path).copied()
    }

    /// True if exactly this command was run (as `exec` or `output`).
    pub fn ran(&self, argv: &[&str]) -> bool {
        self.calls
            .borrow()
            .iter()
            .any(|(c, _)| c.iter().map(String::as_str).eq(argv.iter().copied()))
    }

    /// Who exactly this command ran as, if it ran.
    pub fn ran_as(&self, argv: &[&str]) -> Option<Who> {
        self.calls
            .borrow()
            .iter()
            .find(|(c, _)| c.iter().map(String::as_str).eq(argv.iter().copied()))
            .map(|(_, who)| who.clone())
    }

    /// Every command run so far, with whom it ran as.
    pub fn calls(&self) -> Vec<(Vec<String>, Who)> {
        self.calls.borrow().clone()
    }

    /// True if any command run so far satisfies `pred`.
    pub fn ran_matching(&self, pred: impl Fn(&[String]) -> bool) -> bool {
        self.calls.borrow().iter().any(|(c, _)| pred(c))
    }

    /// Runs one command against the fake. `capture` is whether the caller wants
    /// its output (as opposed to just running it).
    fn handle(&self, argv: &[&str], user: User, capture: bool) -> Output {
        self.record(argv, user);
        let ok = |stdout: String| Output {
            success: true,
            stdout,
            stderr: String::new(),
        };
        let fail = || Output {
            success: false,
            stdout: String::new(),
            stderr: String::new(),
        };
        let key: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        if self.failing.borrow().contains(&key) {
            return fail();
        }
        if let Some(stdout) = self.canned.get(&key) {
            return ok(stdout.clone());
        }
        match argv {
            ["cat", path] => match self.file(path) {
                Some(text) => ok(text),
                None => fail(),
            },
            ["cp", src, dst] => {
                let bytes = self.files.borrow().get(*src).cloned();
                match bytes {
                    Some(b) => {
                        self.files.borrow_mut().insert((*dst).into(), b);
                        ok(String::new())
                    }
                    None => fail(),
                }
            }
            ["test", "-f", path] => {
                if self.files.borrow().contains_key(*path) {
                    ok(String::new())
                } else {
                    fail()
                }
            }
            ["test", "-e", path] => {
                if self.path_exists(path) {
                    ok(String::new())
                } else {
                    fail()
                }
            }
            ["sh", "-c", script, "sh", path] if *script == super::EXISTS_SCRIPT => {
                let found = self.path_exists(path);
                ok(if found { "y" } else { "n" }.into())
            }
            ["sh", "-c", script, "sh", path] if *script == super::READ_FILE_SCRIPT => {
                match self.file(path) {
                    Some(text) => ok(format!("y{text}")),
                    None => ok("n".into()),
                }
            }
            ["rm", .., path] => {
                let below = format!("{}/", path.trim_end_matches('/'));
                self.files
                    .borrow_mut()
                    .retain(|p, _| p != path && !p.starts_with(&below));
                ok(String::new())
            }
            // File operations with nothing to report.
            ["mkdir" | "chmod" | "chown" | "chgrp", ..] => ok(String::new()),
            // `command -v <name> …`, as `Env::has_command` runs it.
            ["sh", "-c", script] if script.starts_with("command -v ") => {
                let name = script["command -v ".len()..]
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_matches('\'');
                if self.commands.iter().any(|c| c == name) {
                    ok(String::new())
                } else {
                    fail()
                }
            }
            // A script run for its effect (as root, say). Its effect can't be
            // emulated, so it is recorded and assumed to work; a test that cares
            // asserts on the recorded script.
            ["sh", "-c", _, ..] if !capture => ok(String::new()),
            _ => panic!(
                "FakeEnv: unexpected command {argv:?}; declare it with \
                 with_output / with_failure / with_commands"
            ),
        }
    }

    /// Whether `path` is a file, or a directory holding one.
    fn path_exists(&self, path: &str) -> bool {
        let below = format!("{}/", path.trim_end_matches('/'));
        self.files
            .borrow()
            .keys()
            .any(|p| p == path || p.starts_with(&below))
    }

    fn record(&self, argv: &[&str], user: User) {
        self.calls
            .borrow_mut()
            .push((argv.iter().map(|s| s.to_string()).collect(), user.into()));
    }
}

impl Env for FakeEnv {
    fn is_disposable(&self) -> bool {
        !self.shared
    }

    fn exec(&self, argv: &[&str], opts: &ExecOpts) -> Result<()> {
        let out = self.handle(argv, opts.user, false);
        if out.success {
            Ok(())
        } else {
            anyhow::bail!("fake exec failed: {argv:?}")
        }
    }

    fn output(&self, argv: &[&str], opts: &ExecOpts) -> Result<Output> {
        Ok(self.handle(argv, opts.user, true))
    }

    /// A file becomes `dest`; a directory's files are merged into `dest`, as
    /// [`Env::put`] promises, and the owner owns them.
    fn put(&self, src: &str, dest: &str, owner: &ContainerUser) -> Result<()> {
        // The directory the real one unpacks into is made as the owner first.
        let into = if Path::new(src).is_dir() {
            dest
        } else {
            Path::new(dest)
                .parent()
                .and_then(Path::to_str)
                .filter(|p| !p.is_empty())
                .unwrap_or("/")
        };
        self.record(&["mkdir", "-p", into], owner.exec_as());
        let mut copied = Vec::new();
        tree::walk(Path::new(src), Links::Keep, Denied::Fail, &mut |e| {
            if !e.meta.is_dir() {
                let to = if e.relative.is_empty() {
                    dest.to_string()
                } else {
                    format!("{}/{}", dest.trim_end_matches('/'), e.relative)
                };
                let mode = e.meta.permissions().mode() & 0o777;
                copied.push((to, std::fs::read(e.path)?, mode));
            }
            Ok(())
        })
        .with_context(|| format!("reading {src}"))?;
        let mut owners = self.owners.borrow_mut();
        for (path, _, mode) in &copied {
            owners.insert(path.clone(), (owner.uid, owner.gid));
            self.modes.borrow_mut().insert(path.clone(), *mode);
        }
        self.files
            .borrow_mut()
            .extend(copied.into_iter().map(|(path, bytes, _)| (path, bytes)));
        Ok(())
    }

    // Records the destination and mode, never the value (as the real one keeps
    // it out of argv).
    fn write_secret(&self, dest: &str, contents: &[u8], mode: u32, user: User) -> Result<()> {
        self.record(&["write_secret", dest, &format!("{mode:o}")], user);
        self.files
            .borrow_mut()
            .insert(dest.into(), contents.to_vec());
        self.modes.borrow_mut().insert(dest.into(), mode);
        Ok(())
    }

    fn interactive(
        &self,
        user: User,
        workdir: &str,
        inner: &str,
    ) -> std::io::Result<std::process::ExitStatus> {
        use std::os::unix::process::ExitStatusExt as _;
        self.record(&["interactive", workdir, inner], user);
        Ok(std::process::ExitStatus::from_raw(0))
    }
}

/// A [`Runtime`] that keeps containers by name (a container's id is its name),
/// each running in its own [`FakeEnv`]. A container it creates gets a fresh
/// environment from the factory it was made with, so a recreated container
/// starts empty, as a real one does. Records every engine operation as a line
/// (`run <name>`, `start <id>`, `remove <id>`, …). Like the engine, it refuses
/// to act on a container that doesn't exist, or to create one that does.
pub struct FakeRuntime {
    new_env: Box<dyn Fn() -> FakeEnv>,
    containers: RefCell<BTreeMap<String, Rc<Fake>>>,
    /// Compose projects that are up.
    projects: RefCell<Vec<String>>,
    ops: RefCell<Vec<String>>,
    runs: RefCell<Vec<RunSpec>>,
}

impl FakeRuntime {
    /// An engine with no containers; each one it creates runs in `new_env()`.
    pub fn new(new_env: impl Fn() -> FakeEnv + 'static) -> Self {
        Self {
            new_env: Box::new(new_env),
            containers: RefCell::default(),
            projects: RefCell::default(),
            ops: RefCell::default(),
            runs: RefCell::default(),
        }
    }

    /// Adds an existing, running container `name`, running in `env`.
    pub fn with_container(self, name: &str, env: FakeEnv) -> Self {
        self.containers
            .borrow_mut()
            .insert(name.to_string(), Fake::running(env));
        self
    }

    fn create(&self) -> Rc<Fake> {
        Fake::running((self.new_env)())
    }

    fn set_running(&self, id: &str, running: bool) -> Result<()> {
        let containers = self.containers.borrow();
        let c = containers
            .get(id)
            .with_context(|| format!("no such container: {id}"))?;
        c.running.set(running);
        Ok(())
    }

    /// The relays started for container `name`; see [`FakeEnv::relays`].
    pub fn relays(&self, name: &str) -> Vec<(u16, u16, bool)> {
        self.containers.borrow()[name].env.relays()
    }

    /// Ends the `index`th relay of container `name`, as a process that died would.
    pub fn end_relay(&self, name: &str, index: usize) {
        self.containers.borrow()[name].env.end_relay(index);
    }

    /// What each container created so far was asked to be.
    pub fn runs(&self) -> Vec<RunSpec> {
        self.runs.borrow().clone()
    }

    /// The engine operations so far.
    pub fn ops(&self) -> Vec<String> {
        self.ops.borrow().clone()
    }

    fn op(&self, line: String) {
        self.ops.borrow_mut().push(line);
    }

    fn exists(&self, id: &str) -> Result<()> {
        if self.containers.borrow().contains_key(id) {
            Ok(())
        } else {
            anyhow::bail!("no such container: {id}")
        }
    }
}

/// One container of a [`FakeRuntime`]: its environment, and whether it runs.
struct Fake {
    env: FakeEnv,
    running: Cell<bool>,
}

impl Fake {
    fn running(env: FakeEnv) -> Rc<Self> {
        Rc::new(Self {
            env,
            running: Cell::new(true),
        })
    }
}

impl Runtime for FakeRuntime {
    /// Panics for a container that doesn't exist: lodge only asks for handles
    /// to containers it found.
    fn container(&self, id: &str) -> Box<dyn Container> {
        let fake = self
            .containers
            .borrow()
            .get(id)
            .cloned()
            .unwrap_or_else(|| panic!("FakeRuntime: no container {id}"));
        Box::new(FakeContainer {
            id: id.to_string(),
            fake,
        })
    }

    fn version(&self) -> Result<String> {
        Ok("fake".into())
    }

    fn compose(&self) -> Result<&dyn Compose> {
        Ok(self)
    }

    fn list_labeled(&self, _label: &str) -> Result<Vec<Listed>> {
        Ok(Vec::new())
    }

    fn find_by_name(&self, name: &str) -> Result<Option<String>> {
        Ok(self
            .containers
            .borrow()
            .contains_key(name)
            .then(|| name.to_string()))
    }

    fn build_image(&self, spec: &BuildSpec) -> Result<()> {
        self.op(format!("build {}", spec.tag));
        Ok(())
    }

    fn run_detached(&self, spec: &RunSpec) -> Result<()> {
        let name = spec.name.as_str();
        if self.exists(name).is_ok() {
            anyhow::bail!("the container name {name} is already in use");
        }
        self.op(format!("run {name}"));
        self.runs.borrow_mut().push(spec.clone());
        self.containers
            .borrow_mut()
            .insert(name.to_string(), self.create());
        Ok(())
    }

    fn start(&self, id: &str) -> Result<()> {
        self.set_running(id, true)?;
        self.op(format!("start {id}"));
        Ok(())
    }

    fn stop(&self, id: &str) -> Result<()> {
        self.set_running(id, false)?;
        self.op(format!("stop {id}"));
        Ok(())
    }

    fn remove(&self, id: &str) -> Result<()> {
        self.exists(id)?;
        self.op(format!("remove {id}"));
        self.containers.borrow_mut().remove(id);
        Ok(())
    }
}

/// Compose with one container per service, named `<project>-<service>-1` as
/// compose names them, created when a project that is up is first asked about.
/// `up` starts the project's containers again.
/// A fake engine shared between the code under test, which owns it as a
/// `Box<dyn Runtime>`, and the test, which looks at what it did.
impl Runtime for Rc<FakeRuntime> {
    fn container(&self, id: &str) -> Box<dyn Container> {
        self.as_ref().container(id)
    }

    fn version(&self) -> Result<String> {
        Runtime::version(self.as_ref())
    }

    fn compose(&self) -> Result<&dyn Compose> {
        self.as_ref().compose()
    }

    fn list_labeled(&self, label: &str) -> Result<Vec<Listed>> {
        self.as_ref().list_labeled(label)
    }

    fn find_by_name(&self, name: &str) -> Result<Option<String>> {
        self.as_ref().find_by_name(name)
    }

    fn build_image(&self, spec: &BuildSpec) -> Result<()> {
        self.as_ref().build_image(spec)
    }

    fn run_detached(&self, spec: &RunSpec) -> Result<()> {
        self.as_ref().run_detached(spec)
    }

    fn start(&self, id: &str) -> Result<()> {
        self.as_ref().start(id)
    }

    fn stop(&self, id: &str) -> Result<()> {
        self.as_ref().stop(id)
    }

    fn remove(&self, id: &str) -> Result<()> {
        self.as_ref().remove(id)
    }
}

impl Compose for FakeRuntime {
    fn version(&self) -> Result<String> {
        Ok("fake".into())
    }

    fn up(&self, _: &Path, project: &str, _: &[String], _: &[&str]) -> Result<()> {
        self.op(format!("compose up {project}"));
        let mut projects = self.projects.borrow_mut();
        if !projects.iter().any(|p| p == project) {
            projects.push(project.to_string());
        }
        let prefix = format!("{project}-");
        for (name, c) in self.containers.borrow().iter() {
            if name.starts_with(&prefix) {
                c.running.set(true);
            }
        }
        Ok(())
    }

    fn down(&self, _: &Path, project: &str, _: &[String]) -> Result<()> {
        self.op(format!("compose down {project}"));
        self.projects.borrow_mut().retain(|p| p != project);
        let prefix = format!("{project}-");
        self.containers
            .borrow_mut()
            .retain(|name, _| !name.starts_with(&prefix));
        Ok(())
    }

    fn ps(
        &self,
        _: &Path,
        project: &str,
        _: &[String],
        service: &str,
        _: Containers,
    ) -> Result<Option<String>> {
        let name = format!("{project}-{service}-1");
        if self.projects.borrow().iter().any(|p| p == project) {
            self.containers
                .borrow_mut()
                .entry(name.clone())
                .or_insert_with(|| self.create());
        }
        self.find_by_name(&name)
    }
}

/// The handle [`FakeRuntime::container`] returns: its commands go to the
/// runtime's shared [`FakeEnv`], and are refused while it is stopped.
struct FakeContainer {
    id: String,
    fake: Rc<Fake>,
}

impl FakeContainer {
    /// The environment, if the container runs, as `docker exec` requires.
    fn env(&self) -> Result<&FakeEnv> {
        if self.fake.running.get() {
            Ok(&self.fake.env)
        } else {
            anyhow::bail!("container {} is not running", self.id)
        }
    }
}

impl Env for FakeContainer {
    fn exec(&self, argv: &[&str], opts: &ExecOpts) -> Result<()> {
        self.env()?.exec(argv, opts)
    }

    /// Like `docker exec`, a stopped container is a failed command, not a
    /// failure to run one.
    fn output(&self, argv: &[&str], opts: &ExecOpts) -> Result<Output> {
        match self.env() {
            Ok(env) => env.output(argv, opts),
            Err(e) => Ok(Output {
                success: false,
                stdout: String::new(),
                stderr: e.to_string(),
            }),
        }
    }

    fn put(&self, src: &str, dest: &str, owner: &ContainerUser) -> Result<()> {
        self.env()?.put(src, dest, owner)
    }

    fn write_secret(&self, dest: &str, contents: &[u8], mode: u32, user: User) -> Result<()> {
        self.env()?.write_secret(dest, contents, mode, user)
    }

    fn interactive(
        &self,
        user: User,
        workdir: &str,
        inner: &str,
    ) -> std::io::Result<std::process::ExitStatus> {
        self.fake.env.interactive(user, workdir, inner)
    }
}

impl Container for FakeContainer {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> Option<String> {
        Some(self.id.clone())
    }

    fn configured_user(&self) -> Option<String> {
        None
    }

    fn mounts(&self) -> Vec<String> {
        Vec::new()
    }

    fn is_running(&self) -> Result<bool> {
        Ok(self.fake.running.get())
    }

    /// Answered as the environment answers [`Env::read_file`] (including a
    /// failure declared for it), whether the container runs or not.
    fn read_stored_file(&self, path: &str) -> Result<Option<String>> {
        self.fake.env.read_file(path)
    }

    fn reach(&self, peer: Option<&str>) -> Option<Reach> {
        self.fake.env.reach.get(&peer.map(str::to_string)).cloned()
    }

    fn relay(
        &self,
        local: u16,
        port: u16,
        _chatter: Stream,
    ) -> Option<std::io::Result<Box<dyn Child>>> {
        let stopped = Arc::new(AtomicBool::new(false));
        self.fake
            .env
            .relays
            .borrow_mut()
            .push((local, port, stopped.clone()));
        Some(Ok(Box::new(FakeRelay(stopped))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Env;

    #[test]
    #[should_panic(expected = "FakeEnv: unexpected command")]
    fn an_undeclared_command_panics_instead_of_succeeding() {
        let env = FakeEnv::new();
        let _ = env.exec_capture_with(&["some-tool", "--flag"], &ExecOpts::default());
    }

    #[test]
    fn declared_commands_answer_and_undeclared_programs_are_absent() {
        let env = FakeEnv::new().with_commands(&["git"]);
        assert!(env.has_command("git"));
        assert!(!env.has_command("sed"));
    }

    #[test]
    fn declared_output_and_failures_are_returned() {
        let env = FakeEnv::new()
            .with_output(&["uname", "-m"], "x86_64\n")
            .with_failure(&["infocmp", "nope"]);
        assert_eq!(
            env.exec_capture_with(&["uname", "-m"], &ExecOpts::default())
                .unwrap(),
            "x86_64\n"
        );
        assert!(!env.succeeds(&["infocmp", "nope"]));
    }

    #[test]
    fn write_secret_stores_bytes_and_mode_but_records_no_value() {
        let env = FakeEnv::new();
        env.write_secret("/dev/shm/s/TOKEN", b"hunter2", 0o600, User::Default)
            .unwrap();
        assert_eq!(env.file("/dev/shm/s/TOKEN").as_deref(), Some("hunter2"));
        assert_eq!(env.mode("/dev/shm/s/TOKEN"), Some(0o600));
        assert!(env.ran(&["write_secret", "/dev/shm/s/TOKEN", "600"]));
        assert!(!env.ran_matching(|c| c.iter().any(|a| a.contains("hunter2"))));
    }

    #[test]
    fn a_directory_is_merged_into_the_destination_and_removed_whole() {
        let src = crate::tmpfile::TempDir::new("fake-cp").unwrap();
        std::fs::create_dir_all(src.path().join("sub")).unwrap();
        std::fs::write(src.path().join("a"), "A").unwrap();
        std::fs::write(src.path().join("sub/b"), "B").unwrap();
        let env = FakeEnv::new().with_file("/d/old", "kept");
        env.cp_in(&src.path().to_string_lossy(), "/d").unwrap();
        assert_eq!(env.file("/d/a").as_deref(), Some("A"));
        assert_eq!(env.file("/d/sub/b").as_deref(), Some("B"));
        assert_eq!(env.file("/d/old").as_deref(), Some("kept"));
        assert!(env.exists("/d/sub").unwrap());
        env.exec_root(&["rm", "-rf", "/d"]).unwrap();
        assert!(!env.exists("/d").unwrap() && env.file("/d/sub/b").is_none());
    }

    #[test]
    fn what_is_put_in_belongs_to_the_owner_and_its_parent_is_made_as_them() {
        let owner = ContainerUser {
            requested: Some("dev".into()),
            name: "dev".into(),
            uid: 1000,
            gid: 1001,
            home: "/home/dev".into(),
        };
        let src = crate::tmpfile::TempDir::new("fake-put").unwrap();
        std::fs::write(src.path().join("f"), "x").unwrap();
        let env = FakeEnv::new();
        env.put(
            &src.path().join("f").to_string_lossy(),
            "/home/dev/.local/f",
            &owner,
        )
        .unwrap();
        assert_eq!(env.owner_of("/home/dev/.local/f"), Some((1000, 1001)));
        assert_eq!(
            env.ran_as(&["mkdir", "-p", "/home/dev/.local"]),
            Some(Who::Named("dev".into()))
        );
    }

    #[test]
    fn a_declared_failure_can_recover() {
        let env = FakeEnv::new().with_failure(&["sh", "-c", "make"]);
        assert!(env.exec_root(&["sh", "-c", "make"]).is_err());
        env.recover(&["sh", "-c", "make"]);
        assert!(env.exec_root(&["sh", "-c", "make"]).is_ok());
    }

    #[test]
    fn a_stopped_container_refuses_commands_until_started() {
        let rt = FakeRuntime::new(FakeEnv::new).with_container("c", FakeEnv::new());
        rt.stop("c").unwrap();
        let c = rt.container("c");
        assert!(!c.is_running().unwrap());
        assert!(c.read_file("/x").is_err());
        rt.start("c").unwrap();
        assert_eq!(c.read_file("/x").unwrap(), None);
    }

    #[test]
    fn files_written_are_visible_to_later_commands() {
        let env = FakeEnv::new();
        env.write_file("/etc/x", b"hello").unwrap();
        assert!(env.exists("/etc/x").unwrap());
        assert_eq!(
            env.exec_capture_with(&["cat", "/etc/x"], &ExecOpts::default())
                .unwrap(),
            "hello"
        );
        env.exec_root(&["rm", "-f", "/etc/x"]).unwrap();
        assert!(!env.exists("/etc/x").unwrap());
    }
}
