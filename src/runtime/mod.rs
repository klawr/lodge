//! The seam between lodge and whatever runs the workload.
//!
//! Three small traits, from narrow to broad, plus one for compose:
//!
//! - [`Env`]: a place you can run commands in and copy files into. Injection,
//!   features, lifecycle hooks and the stored config hash only need this, so
//!   they work the same for a container, and later for a plain host.
//! - [`Container`]: an `Env` that can also be inspected (name, mounts, how to
//!   reach its ports, files even while stopped). Port forwarding, workdir selection, drift and
//!   status need this.
//! - [`Runtime`]: the engine itself: find, build, run, start, stop. Devcontainer
//!   backends need this.
//! - [`Compose`]: an engine's compose support, which only the compose backend
//!   needs.
//!
//! [`cli`] implements all four over the docker-compatible command line (`docker`
//! or `podman`, locally or over SSH).
//! `fake` (in tests) has the test doubles: `FakeEnv`, an in-memory `Env`, and
//! `FakeRuntime`, an engine (with compose) whose containers each run in one.

pub mod cli;
#[cfg(test)]
pub mod fake;
mod inside;
pub(crate) mod tarball;
mod user;

pub use inside::Inside;
#[cfg(test)]
pub(crate) use user::PROBE_SCRIPT;
pub use user::{ContainerUser, User};

use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use crate::shell::{script_argv, shell_quote};
use crate::tmpfile::TempFile;

/// The script behind [`Env::write_secret`]: `$1` is the destination, the value
/// arrives on stdin and `%MODE%` is replaced by an octal mode. Written to a
/// sibling temp file under `umask 077`, then moved into place.
pub const WRITE_SECRET_SCRIPT: &str = "umask 077; t=\"$1.tmp.$$\"; \
     { cat > \"$t\" && chmod %MODE% \"$t\" && mv -f \"$t\" \"$1\"; } || { rm -f \"$t\"; exit 1; }";

/// The script behind [`Env::read_file`]: `$1` is the path. Prints `y` and the
/// contents, or `n` when there is no such file, and exits 0 either way, so a
/// non-zero exit means the read itself failed.
pub const READ_FILE_SCRIPT: &str = "if [ -f \"$1\" ]; then printf y; cat \"$1\"; else printf n; fi";

/// The script behind [`Env::exists`]: `$1` is the path. Prints `y` or `n` and
/// exits 0 either way, so a non-zero exit means the check itself failed.
pub const EXISTS_SCRIPT: &str = "if [ -e \"$1\" ]; then printf y; else printf n; fi";

/// The script behind [`Env::can_run_in`]: `$1` is a directory; `$2` is `own` when
/// the caller must own it (a shared `/tmp` is not trusted otherwise). Makes the
/// directory, refuses a symlink, and checks that a script can be written there
/// and run from there (a `noexec` mount refuses the run even when the write
/// works). Exits 0 only if all of that works.
pub const RUN_PROBE_SCRIPT: &str = r#"d=$1
mkdir -p "$d" 2>/dev/null || exit 1
[ -d "$d" ] && [ ! -L "$d" ] || exit 1
if [ "$2" = own ]; then
  [ -O "$d" ] && chmod 700 "$d" 2>/dev/null || exit 1
fi
t="$d/.probe.$$"
printf '#!/bin/sh\nexit 0\n' > "$t" 2>/dev/null || exit 1
chmod +x "$t" 2>/dev/null || { rm -f "$t"; exit 1; }
"$t" 2>/dev/null; r=$?
rm -f "$t"
exit $r"#;

/// How a command is run: as whom, from where, with what extra environment.
#[derive(Debug, Default, Clone, Copy)]
pub struct ExecOpts<'a> {
    pub user: User<'a>,
    pub workdir: Option<&'a str>,
    pub env: &'a [(String, String)],
    /// Whether the command may be run again if the connection to a remote
    /// engine drops while it runs. Off unless the caller knows that running it
    /// twice is harmless.
    pub repeat: crate::host::Repeat,
}

impl<'a> ExecOpts<'a> {
    /// As `user`, from the default directory, with no extra environment.
    pub fn as_user(user: User<'a>) -> Self {
        ExecOpts {
            user,
            workdir: None,
            env: &[],
            repeat: crate::host::Repeat::Never,
        }
    }

    /// This command is safe to run again; see [`ExecOpts::repeat`].
    pub fn repeatable(self) -> Self {
        ExecOpts {
            repeat: crate::host::Repeat::Safe,
            ..self
        }
    }

    pub fn root() -> ExecOpts<'static> {
        ExecOpts::as_user(User::Root)
    }
}

/// A finished command: whether it exited zero, and what it printed (decoded as
/// UTF-8, an invalid sequence replaced, so odd output is never an error).
#[derive(Debug, Clone)]
pub struct Output {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// A place commands can run and files can be copied into.
///
/// What lodge does *for a user* (injecting tools, writing their state, opening
/// their session) goes through an [`Acting`] view of the environment, so it runs
/// as that user and owns what it creates, with no root involved. The plain
/// environment acts as root, which is what provisioning a devcontainer uses.
pub trait Env {
    /// Runs `argv` with inherited stdio. Errors if it exits non-zero.
    fn exec(&self, argv: &[&str], opts: &ExecOpts) -> Result<()>;

    /// Runs `argv` capturing its output. Errors only if it could not be run; a
    /// non-zero exit is reported in [`Output::success`].
    fn output(&self, argv: &[&str], opts: &ExecOpts) -> Result<Output>;

    /// Copies the local file or directory `src` to `dest` inside the
    /// environment, keeping file modes, with `owner` owning everything that is
    /// created, parent directories included. A file becomes `dest`; a
    /// directory's *contents* are merged into `dest`. Missing parent directories
    /// (of `dest`, for a file; `dest` itself, for a directory) are made. Files
    /// already in `dest` that `src` lacks are kept, so remove `dest` first for an
    /// exact copy.
    fn put(&self, src: &str, dest: &str, owner: &ContainerUser) -> Result<()>;

    /// Writes `contents` to `dest` inside the environment with permission bits
    /// `mode`, as `user`. The bytes travel on the command's **stdin**, never in
    /// argv, so they show up in neither `ps` nor the `-v` trace (use this, not
    /// `exec` with `-e K=V`, for anything secret). The file is created
    /// owner-only, so it is never readable at a looser mode, and replaced
    /// atomically. The parent directory must already exist.
    fn write_secret(&self, dest: &str, contents: &[u8], mode: u32, user: User) -> Result<()>;

    /// Opens an interactive session in the environment, running `inner` (a
    /// `sh -c` script) as `user` from `workdir`, and waits until it ends. Can be
    /// called again to start another.
    fn interactive(&self, user: User, workdir: &str, inner: &str) -> std::io::Result<ExitStatus>;

    /// Whether `user` can write a program into `dir` (made if need be) and run it
    /// from there; with `private`, also that `user` owns `dir`, which is then
    /// made owner-only (a shared directory is not trusted otherwise). An error
    /// means it could not be found out, which is not "no". This is a capability:
    /// the default asks by trying, which any environment that can run commands
    /// can do; one that knows better (a read-only root, say) overrides it.
    fn can_run_in(&self, dir: &str, private: bool, user: &ContainerUser) -> Result<bool> {
        let mode = if private { "own" } else { "any" };
        let opts = ExecOpts::as_user(user.exec_as()).repeatable();
        let out = self.output(&script_argv(RUN_PROBE_SCRIPT, &[dir, mode]), &opts)?;
        Ok(out.success)
    }

    /// Whether this is a place made for lodge's use and thrown away when done (a
    /// container), so replacing what is there is fine. A real machine is not:
    /// injection leaves a config lodge didn't place alone and does not loosen
    /// git's ownership check there.
    fn is_disposable(&self) -> bool {
        true
    }

    /// Who the provided helpers below act as, and who owns what they create:
    /// root, unless this is an [`Acting`] view of another user.
    fn actor(&self) -> ContainerUser {
        ContainerUser::root()
    }

    // ── provided, built on the above ───────────────────────────────────────

    /// Runs `argv` as root, whoever [`actor`](Self::actor) is. For provisioning,
    /// where the engine allows it; what lodge does for a user never needs it.
    fn exec_root(&self, argv: &[&str]) -> Result<()> {
        self.exec(argv, &ExecOpts::root())
    }

    /// Runs `argv` and returns stdout; errors on non-zero exit. For a read or a
    /// probe, ask for [`ExecOpts::repeatable`].
    fn exec_capture_with(&self, argv: &[&str], opts: &ExecOpts) -> Result<String> {
        let out = self.output(argv, opts)?;
        if !out.success {
            bail!("exec failed: {argv:?}\n{}", out.stderr.trim());
        }
        Ok(out.stdout)
    }

    /// True if `argv` runs and exits zero.
    fn succeeds(&self, argv: &[&str]) -> bool {
        self.output(argv, &ExecOpts::default())
            .is_ok_and(|o| o.success)
    }

    /// Whether `path` exists (file or directory), as the actor. An error means
    /// it could not be found out, which callers must not mistake for "missing".
    fn exists(&self, path: &str) -> Result<bool> {
        let actor = self.actor();
        let opts = ExecOpts::as_user(actor.exec_as()).repeatable();
        let out = self.output(&script_argv(EXISTS_SCRIPT, &[path]), &opts)?;
        match (out.success, out.stdout.as_str()) {
            (true, "y") => Ok(true),
            (true, "n") => Ok(false),
            (true, _) => bail!("checking {path} failed: unexpected output"),
            (false, _) => bail!("checking {path} failed: {}", out.stderr.trim()),
        }
    }

    /// True if `cmd` is on `PATH`.
    fn has_command(&self, cmd: &str) -> bool {
        let probe = format!("command -v {} >/dev/null 2>&1", shell_quote(cmd));
        self.output(&["sh", "-c", &probe], &ExecOpts::default().repeatable())
            .is_ok_and(|o| o.success)
    }

    /// `mkdir -p` as the actor.
    fn mkdir_p(&self, dirs: &[&str]) -> Result<()> {
        let mut argv = vec!["mkdir", "-p"];
        argv.extend_from_slice(dirs);
        let actor = self.actor();
        self.exec(&argv, &ExecOpts::as_user(actor.exec_as()).repeatable())
    }

    /// Copies `src` into the environment, owned by the actor; see
    /// [`put`](Self::put).
    fn cp_in(&self, src: &str, dest: &str) -> Result<()> {
        self.put(src, dest, &self.actor())
    }

    /// Creates `dest` with `contents` and mode 0644, owned by the actor, making
    /// parent directories first.
    fn write_file(&self, dest: &str, contents: &[u8]) -> Result<()> {
        self.write_file_mode(dest, contents, 0o644)
    }

    /// Like [`write_file`](Self::write_file) with the permission bits `mode` (a
    /// script that must run, say). Stages the bytes in a local temp file and
    /// sends them with [`put`](Self::put).
    fn write_file_mode(&self, dest: &str, contents: &[u8], mode: u32) -> Result<()> {
        let name = Path::new(dest)
            .file_name()
            .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
        let tmp = TempFile::write(&name, contents)?;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(mode))?;
        self.cp_in(&tmp.path().to_string_lossy(), dest)
    }

    /// The text of the file `path` (read as the actor), `None` if there is no
    /// such file. An error means it could not be found out, which callers must
    /// not mistake for "missing".
    fn read_file(&self, path: &str) -> Result<Option<String>> {
        let actor = self.actor();
        let opts = ExecOpts::as_user(actor.exec_as()).repeatable();
        let out = self.output(&script_argv(READ_FILE_SCRIPT, &[path]), &opts)?;
        if !out.success {
            bail!("reading {path} failed: {}", out.stderr.trim());
        }
        match out.stdout.split_at_checked(1) {
            Some(("y", text)) => Ok(Some(text.to_string())),
            Some(("n", _)) => Ok(None),
            _ => bail!("reading {path} failed: unexpected output"),
        }
    }
}

/// An environment seen as one user: what lodge does for that user, with no root
/// involved. Commands that don't name a user run as them, the provided helpers
/// ([`Env::mkdir_p`], [`Env::write_file`], [`Env::read_file`], …) act as them, and
/// what is copied in is theirs. Root is still available by asking for it
/// ([`Env::exec_root`]), which nothing done for a user does.
pub struct Acting<'a> {
    env: &'a dyn Env,
    user: &'a ContainerUser,
}

impl<'a> Acting<'a> {
    pub fn new(env: &'a dyn Env, user: &'a ContainerUser) -> Self {
        Self { env, user }
    }

    /// `opts`, with the default user replaced by the one acted as.
    fn resolve<'o>(&'o self, opts: &ExecOpts<'o>) -> ExecOpts<'o> {
        let user = match opts.user {
            User::Default => self.user.exec_as(),
            explicit => explicit,
        };
        ExecOpts { user, ..*opts }
    }
}

impl Env for Acting<'_> {
    fn exec(&self, argv: &[&str], opts: &ExecOpts) -> Result<()> {
        self.env.exec(argv, &self.resolve(opts))
    }

    fn output(&self, argv: &[&str], opts: &ExecOpts) -> Result<Output> {
        self.env.output(argv, &self.resolve(opts))
    }

    fn put(&self, src: &str, dest: &str, owner: &ContainerUser) -> Result<()> {
        self.env.put(src, dest, owner)
    }

    fn write_secret(&self, dest: &str, contents: &[u8], mode: u32, user: User) -> Result<()> {
        self.env.write_secret(dest, contents, mode, user)
    }

    fn interactive(&self, user: User, workdir: &str, inner: &str) -> std::io::Result<ExitStatus> {
        self.env.interactive(user, workdir, inner)
    }

    fn can_run_in(&self, dir: &str, private: bool, user: &ContainerUser) -> Result<bool> {
        self.env.can_run_in(dir, private, user)
    }

    fn actor(&self) -> ContainerUser {
        self.user.clone()
    }

    fn is_disposable(&self) -> bool {
        self.env.is_disposable()
    }
}

/// An [`Env`] that can be inspected.
pub trait Container: Env {
    fn id(&self) -> &str;

    /// The container's name, without the leading `/`.
    fn name(&self) -> Option<String>;

    /// Mount destination paths inside the container.
    fn mounts(&self) -> Vec<String>;

    /// Whether the container is running: `false` when it is stopped or gone,
    /// an error when the engine could not be asked (which is not "stopped").
    fn is_running(&self) -> Result<bool>;

    /// The text of the regular file `path` in the container, `None` if there is
    /// no such file. Read through the engine rather than by running a command
    /// in it, so a stopped container can be read too. An error means it could
    /// not be found out, which callers must not mistake for "missing".
    fn read_stored_file(&self, path: &str) -> Result<Option<String>>;

    /// The user the image or the container is configured to run as (`USER`, a
    /// name, uid or `user:group`), `None` when it sets none, which means root.
    /// Read through the engine, so it works for a stopped container.
    fn configured_user(&self) -> Option<String>;

    /// How to reach this container's ports (`peer` is `None`), or a peer's (a
    /// compose service, container name or network alias), from the host that
    /// runs the engine; `None` when that can't be told (not up yet). Asked
    /// again over time, because a restarted container may come back elsewhere.
    fn reach(&self, peer: Option<&str>) -> Option<Reach>;

    /// The name to show for it: its name, or its id when it has none.
    fn display_name(&self) -> String {
        self.name().unwrap_or_else(|| self.id().to_string())
    }
}

/// How the ports of a container can be reached from the host that runs its
/// engine. What a forward does with it (the built-in proxy, or the host's own
/// tunnel) is decided from the host, not from the answer. A way that is a
/// *process* (`kubectl port-forward`) is not an address at all and would be
/// another variant, with the `Host` hook that starts it, when an engine needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// Every port the container listens on is open at this address (an IP on
    /// the container network).
    Address(String),
    /// Only the ports the container publishes are reachable, each at the address
    /// the engine's host has for it (a rootless engine, whose containers have an
    /// address the host cannot connect to). Container port to `host:port`.
    Published(BTreeMap<u16, String>),
}

impl Reach {
    /// `host:port` to connect to for `port` of the container, `None` when this
    /// way of reaching it does not reach that port (one that is not published).
    pub fn destination(&self, port: u16) -> Option<String> {
        match self {
            Reach::Address(ip) => Some(format!("{ip}:{port}")),
            Reach::Published(ports) => ports.get(&port).cloned(),
        }
    }
}

impl std::fmt::Display for Reach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reach::Address(ip) => f.write_str(ip),
            Reach::Published(ports) => {
                let shown: Vec<String> = ports.keys().map(u16::to_string).collect();
                write!(f, "published ports [{}]", shown.join(", "))
            }
        }
    }
}

/// An image to build before running a devcontainer.
pub struct BuildSpec {
    pub context: PathBuf,
    pub dockerfile: Option<String>,
    /// The stage to build (`docker build --target`).
    pub stage: Option<String>,
    pub args: BTreeMap<String, String>,
    pub tag: String,
}

/// A container to create and start, in terms every engine has. An engine turns
/// it into its own flags or API calls; nothing outside it spells those.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RunSpec {
    pub name: String,
    pub image: String,
    /// Replaces the image's own command; empty keeps it.
    pub command: Vec<String>,
    pub labels: Vec<(String, String)>,
    pub workdir: Option<String>,
    /// Bind mounts, as (source on the engine's host, target in the container).
    pub binds: Vec<(String, String)>,
    /// Mounts in the devcontainer spec's own syntax
    /// (`type=bind,source=/a,target=/b`).
    pub mounts: Vec<String>,
    pub privileged: bool,
    pub init: bool,
    pub cap_add: Vec<String>,
    pub security_opt: Vec<String>,
    /// The user to run as (`-u`): a name, uid or `user:group`.
    pub user: Option<String>,
    /// Published ports as `[ip:]host:container`.
    pub publish: Vec<String>,
    /// Arguments for the engine's own command line, passed through after all
    /// the above so they can override them: a devcontainer's `runArgs`, which
    /// the spec defines as the engine's CLI flags.
    pub engine_args: Vec<String>,
}

/// Which containers a lookup considers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Containers {
    /// Running ones only.
    Running,
    /// Stopped ones too.
    All,
}

/// A container found by [`Runtime::list_labeled`].
#[derive(Debug, Clone)]
pub struct Listed {
    pub id: String,
    pub name: String,
    pub running: bool,
    /// The value of the label that was asked for.
    pub label: String,
}

/// A container engine.
pub trait Runtime {
    /// A handle for the container with this id or name.
    fn container(&self, id: &str) -> Box<dyn Container>;

    /// The engine's server version; errors if the engine can't be reached.
    fn version(&self) -> Result<String>;

    /// The engine's compose support, for devcontainers with
    /// `dockerComposeFile`; errors if the engine has none.
    fn compose(&self) -> Result<&dyn Compose>;

    /// Every container (running or stopped) that carries `label`.
    fn list_labeled(&self, label: &str) -> Result<Vec<Listed>>;

    /// The id of the container (running or stopped) with exactly this name.
    fn find_by_name(&self, name: &str) -> Result<Option<String>>;

    /// Builds an image. Reads the build context on the engine's host.
    fn build_image(&self, spec: &BuildSpec) -> Result<()>;

    /// Creates and starts a detached container.
    fn run_detached(&self, spec: &RunSpec) -> Result<()>;

    fn start(&self, id: &str) -> Result<()>;
    fn stop(&self, id: &str) -> Result<()>;
    fn remove(&self, id: &str) -> Result<()>;
}

/// Multi-container projects described by compose files, which live in `dir` on
/// the engine's host. Separate from [`Runtime`] because not every engine has it,
/// and only the compose backend needs it.
pub trait Compose {
    /// The compose provider's version; errors if it isn't usable.
    fn version(&self) -> Result<String>;

    /// `compose up -d` for the project, with `extra` arguments.
    fn up(&self, dir: &Path, project: &str, files: &[String], extra: &[&str]) -> Result<()>;

    /// `compose down` for the project: removes its containers and network.
    fn down(&self, dir: &Path, project: &str, files: &[String]) -> Result<()>;

    /// The first container id for `service` among `which`, if there is one.
    fn ps(
        &self,
        dir: &Path,
        project: &str,
        files: &[String],
        service: &str,
        which: Containers,
    ) -> Result<Option<String>>;
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeEnv, Who};
    use super::{Acting, ContainerUser, Env, ExecOpts, Reach, User};

    #[test]
    fn a_way_in_gives_the_destination_of_each_port_it_reaches() {
        let address = Reach::Address("10.0.0.2".into());
        assert_eq!(address.destination(80).as_deref(), Some("10.0.0.2:80"));
        let published = Reach::Published([(80, "127.0.0.1:8080".to_string())].into());
        assert_eq!(published.destination(80).as_deref(), Some("127.0.0.1:8080"));
        assert_eq!(published.destination(443), None, "not published");
        assert_eq!(address.to_string(), "10.0.0.2");
        assert_eq!(published.to_string(), "published ports [80]");
    }

    fn dev() -> ContainerUser {
        ContainerUser {
            requested: Some("dev".into()),
            name: "dev".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/dev".into(),
        }
    }

    #[test]
    fn write_file_makes_the_parent_as_root_and_copies_the_contents_in() {
        let env = FakeEnv::new();
        env.write_file("/a/b/c.txt", b"hi").unwrap();
        assert_eq!(env.file("/a/b/c.txt").unwrap(), "hi");
        assert_eq!(env.ran_as(&["mkdir", "-p", "/a/b"]), Some(Who::Root));
        assert_eq!(env.owner_of("/a/b/c.txt"), Some((0, 0)));
    }

    #[test]
    fn an_acting_env_does_everything_as_its_user_and_owns_what_it_creates() {
        let env = FakeEnv::new().with_file("/home/dev/x", "old");
        let user = dev();
        let acting = Acting::new(&env, &user);

        acting
            .write_file("/home/dev/.local/lodge/f", b"hi")
            .unwrap();
        acting.mkdir_p(&["/home/dev/d"]).unwrap();
        assert!(acting.exists("/home/dev/x").unwrap());
        assert_eq!(
            acting.read_file("/home/dev/x").unwrap().as_deref(),
            Some("old")
        );

        assert_eq!(env.owner_of("/home/dev/.local/lodge/f"), Some((1000, 1000)));
        for call in env.calls() {
            assert_eq!(
                call.1,
                Who::Named("dev".into()),
                "{:?} ran as {:?}",
                call.0,
                call.1
            );
        }
        assert!(!env.calls().is_empty());
    }

    #[test]
    fn an_acting_env_still_runs_as_root_when_asked_to_and_as_the_user_otherwise() {
        let env = FakeEnv::new();
        let user = dev();
        let acting = Acting::new(&env, &user);
        acting
            .exec(&["sh", "-c", "a"], &ExecOpts::default())
            .unwrap();
        acting.exec_root(&["sh", "-c", "b"]).unwrap();
        acting
            .exec(&["sh", "-c", "c"], &ExecOpts::as_user(User::Named("other")))
            .unwrap();
        assert_eq!(
            env.ran_as(&["sh", "-c", "a"]),
            Some(Who::Named("dev".into()))
        );
        assert_eq!(env.ran_as(&["sh", "-c", "b"]), Some(Who::Root));
        assert_eq!(
            env.ran_as(&["sh", "-c", "c"]),
            Some(Who::Named("other".into()))
        );
    }

    #[test]
    fn a_user_without_a_name_acts_through_the_default_user() {
        let env = FakeEnv::new();
        let user = ContainerUser {
            requested: None,
            ..dev()
        };
        let acting = Acting::new(&env, &user);
        acting.mkdir_p(&["/x"]).unwrap();
        assert_eq!(env.ran_as(&["mkdir", "-p", "/x"]), Some(Who::Default));
    }

    /// An environment that answers [`Env::can_run_in`] itself and can run nothing.
    struct Sealed;

    impl Env for Sealed {
        fn exec(&self, _: &[&str], _: &ExecOpts) -> anyhow::Result<()> {
            unreachable!("nothing runs here")
        }
        fn output(&self, _: &[&str], _: &ExecOpts) -> anyhow::Result<super::Output> {
            unreachable!("nothing runs here")
        }
        fn put(&self, _: &str, _: &str, _: &ContainerUser) -> anyhow::Result<()> {
            unreachable!()
        }
        fn write_secret(&self, _: &str, _: &[u8], _: u32, _: User) -> anyhow::Result<()> {
            unreachable!()
        }
        fn interactive(
            &self,
            _: User,
            _: &str,
            _: &str,
        ) -> std::io::Result<std::process::ExitStatus> {
            unreachable!()
        }
        fn can_run_in(
            &self,
            dir: &str,
            private: bool,
            user: &ContainerUser,
        ) -> anyhow::Result<bool> {
            Ok(dir == "/opt" && !private && user.name == "dev")
        }
    }

    #[test]
    fn an_acting_env_leaves_the_capability_to_the_env_it_wraps() {
        let user = dev();
        let acting = Acting::new(&Sealed, &user);
        assert!(acting.can_run_in("/opt", false, &user).unwrap());
        assert!(!acting.can_run_in("/opt", true, &user).unwrap());
    }

    #[test]
    fn an_acting_root_acts_as_root() {
        let env = FakeEnv::new();
        let root = ContainerUser::root();
        Acting::new(&env, &root).mkdir_p(&["/x"]).unwrap();
        assert_eq!(env.ran_as(&["mkdir", "-p", "/x"]), Some(Who::Root));
    }
}

#[cfg(test)]
mod write_secret_script_tests {
    use super::WRITE_SECRET_SCRIPT;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    fn run(dest: &str, mode: u32, input: &[u8]) -> bool {
        let script = WRITE_SECRET_SCRIPT.replace("%MODE%", &format!("{mode:o}"));
        let argv = crate::shell::script_argv(&script, &[dest]);
        let mut child = Command::new(argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait().unwrap().success()
    }

    #[test]
    fn writes_bytes_with_the_mode_and_replaces_read_only_files() {
        let tmp = crate::tmpfile::TempDir::new("write-secret").unwrap();
        let dir = tmp.path();
        let dest = dir.join("it's a 'token'");
        let dest = dest.to_str().unwrap();
        assert!(run(dest, 0o600, b"one\0two"));
        assert_eq!(std::fs::read(dest).unwrap(), b"one\0two");
        assert_eq!(
            std::fs::metadata(dest).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // A more restrictive mode, over an existing file, still works.
        assert!(run(dest, 0o400, b"new"));
        assert!(run(dest, 0o400, b"newer"));
        assert_eq!(std::fs::read(dest).unwrap(), b"newer");
        assert_eq!(
            std::fs::metadata(dest).unwrap().permissions().mode() & 0o777,
            0o400
        );
        // No leftovers; a missing directory fails cleanly.
        assert_eq!(std::fs::read_dir(dir).unwrap().count(), 1);
        assert!(!run(dir.join("nope/x").to_str().unwrap(), 0o600, b"x"));
    }
}
