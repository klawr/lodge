//! The seam between lodge and whatever runs the workload.
//!
//! Three small traits, from narrow to broad:
//!
//! - [`Env`]: a place you can run commands in and copy files into. Injection,
//!   features, lifecycle hooks and the stored config hash only need this, so
//!   they work the same for a container, and later for a plain host.
//! - [`Container`]: an `Env` that can also be inspected (name, mounts, address).
//!   Port forwarding and workdir selection need this.
//! - [`Runtime`]: the engine itself: find, build, run, start, stop, compose.
//!   Devcontainer backends need this.
//!
//! [`docker`] implements all three over the `docker` CLI (locally or over SSH);
//! [`fake`] is an in-memory `Env` for tests.

pub mod docker;
#[cfg(test)]
pub mod fake;
mod user;

#[cfg(test)]
pub(crate) use user::PROBE_SCRIPT;
pub use user::{ContainerUser, User};

use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::shell::shell_quote;
use crate::target::Target;
use crate::tmpfile::TempFile;

/// The `sh -c` script behind [`Env::write_secret`]: `$0` is the destination, the
/// value arrives on stdin and `%MODE%` is replaced by an octal mode. Written to a
/// sibling temp file under `umask 077`, then moved into place.
pub const WRITE_SECRET_SCRIPT: &str = "umask 077; t=\"$0.tmp.$$\"; \
     { cat > \"$t\" && chmod %MODE% \"$t\" && mv -f \"$t\" \"$0\"; } || { rm -f \"$t\"; exit 1; }";

/// The `sh -c` script behind [`Env::read_file`]: `$0` is the path. Prints `y`
/// and the contents, or `n` when there is no such file, and exits 0 either way,
/// so a non-zero exit means the read itself failed.
pub const READ_FILE_SCRIPT: &str = "if [ -f \"$0\" ]; then printf y; cat \"$0\"; else printf n; fi";

/// How a command is run: as whom, from where, with what extra environment.
#[derive(Debug, Default, Clone, Copy)]
pub struct ExecOpts<'a> {
    pub user: User<'a>,
    pub workdir: Option<&'a str>,
    pub env: &'a [(String, String)],
}

impl ExecOpts<'_> {
    pub fn root() -> ExecOpts<'static> {
        ExecOpts {
            user: User::Root,
            workdir: None,
            env: &[],
        }
    }
}

/// A finished command: whether it exited zero, and what it printed.
#[derive(Debug, Clone)]
pub struct Output {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// A place commands can run and files can be copied into.
pub trait Env {
    /// Where the environment lives: this machine or a remote host.
    fn target(&self) -> &Target;

    /// Runs `argv` with inherited stdio. Errors if it exits non-zero.
    fn exec(&self, argv: &[&str], opts: &ExecOpts) -> Result<()>;

    /// Runs `argv` capturing its output. Errors only if it could not be run; a
    /// non-zero exit is reported in [`Output::success`].
    fn output(&self, argv: &[&str], opts: &ExecOpts) -> Result<Output>;

    /// Copies the local file or directory `src` to `dest` inside the environment.
    fn cp_in(&self, src: &str, dest: &str) -> Result<()>;

    /// Writes `contents` to `dest` inside the environment with permission bits
    /// `mode`, as `user`. The bytes travel on the command's **stdin**, never in
    /// argv, so they show up in neither `ps` nor the `-v` trace (use this, not
    /// `exec` with `-e K=V`, for anything secret). The file is created
    /// owner-only, so it is never readable at a looser mode, and replaced
    /// atomically. The parent directory must already exist.
    fn write_secret(&self, dest: &str, contents: &[u8], mode: u32, user: User) -> Result<()>;

    /// A local shell command line that opens an interactive session in the
    /// environment, running `inner` (a `sh -c` script) as `user` from `workdir`.
    fn interactive_command(&self, user: User, workdir: &str, inner: &str) -> String;

    // ── provided, built on the above ───────────────────────────────────────

    /// Runs `argv` as root.
    fn exec_root(&self, argv: &[&str]) -> Result<()> {
        self.exec(argv, &ExecOpts::root())
    }

    /// Runs `argv` as the default user and returns stdout; errors on non-zero exit.
    fn exec_capture(&self, argv: &[&str]) -> Result<String> {
        self.exec_capture_as(User::Default, argv)
    }

    /// Like [`exec_capture`](Self::exec_capture) as `user`.
    fn exec_capture_as(&self, user: User, argv: &[&str]) -> Result<String> {
        let opts = ExecOpts {
            user,
            ..ExecOpts::default()
        };
        let out = self.output(argv, &opts)?;
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

    /// True if `path` is a regular file.
    fn file_exists(&self, path: &str) -> bool {
        self.succeeds(&["test", "-f", path])
    }

    /// True if `path` exists (file or directory).
    fn path_exists(&self, path: &str) -> bool {
        self.succeeds(&["test", "-e", path])
    }

    /// True if `cmd` is on `PATH`.
    fn has_command(&self, cmd: &str) -> bool {
        let probe = format!("command -v {} >/dev/null 2>&1", shell_quote(cmd));
        self.succeeds(&["sh", "-c", &probe])
    }

    /// `mkdir -p` as root. lodge's directories live under root-owned trees.
    fn mkdir_p(&self, dirs: &[&str]) -> Result<()> {
        let mut argv = vec!["mkdir", "-p"];
        argv.extend_from_slice(dirs);
        self.exec_root(&argv)
    }

    /// Creates `dest` with `contents`, making parent directories first. Stages
    /// the bytes in a local temp file and sends them with [`cp_in`](Self::cp_in).
    fn write_file(&self, dest: &str, contents: &[u8]) -> Result<()> {
        let path = Path::new(dest);
        if let Some(parent) = path
            .parent()
            .and_then(Path::to_str)
            .filter(|p| !p.is_empty())
        {
            self.mkdir_p(&[parent])?;
        }
        let name = path
            .file_name()
            .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
        let tmp = TempFile::write(&name, contents)?;
        self.cp_in(&tmp.path().to_string_lossy(), dest)
    }

    /// The text of the file `path` (read as root), `None` if there is no such
    /// file. An error means it could not be found out, which callers must not
    /// mistake for "missing".
    fn read_file(&self, path: &str) -> Result<Option<String>> {
        let out = self.output(&["sh", "-c", READ_FILE_SCRIPT, path], &ExecOpts::root())?;
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

/// An [`Env`] that can be inspected.
pub trait Container: Env {
    fn id(&self) -> &str;

    /// The container's name, without the leading `/`.
    fn name(&self) -> Option<String>;

    /// Mount destination paths inside the container.
    fn mounts(&self) -> Vec<String>;

    fn is_running(&self) -> bool;

    /// The container's address on its first network, reachable from the host
    /// that runs the engine.
    fn ip_address(&self) -> Option<String>;

    /// Resolves a peer by name (a compose service, container name or network
    /// alias) to an address reachable from the engine host.
    fn resolve_peer(&self, host: &str) -> Option<String>;
}

/// An image to build before running a devcontainer.
pub struct BuildSpec {
    pub context: PathBuf,
    pub dockerfile: Option<String>,
    pub target: Option<String>,
    pub args: BTreeMap<String, String>,
    pub tag: String,
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
    /// Where the engine runs (and so where project files must be read).
    fn target(&self) -> &Target;

    /// A handle for the container with this id or name.
    fn container(&self, id: &str) -> Box<dyn Container>;

    /// The engine's server version; errors if the engine can't be reached.
    fn version(&self) -> Result<String>;

    /// The compose provider's version; errors if compose isn't available.
    fn compose_version(&self) -> Result<String>;

    /// Every container (running or stopped) that carries `label`.
    fn list_labeled(&self, label: &str) -> Result<Vec<Listed>>;

    /// The id of the container (running or stopped) with exactly this name.
    fn find_by_name(&self, name: &str) -> Result<Option<String>>;

    /// Builds an image. Reads the build context on the engine's host.
    fn build_image(&self, spec: &BuildSpec) -> Result<()>;

    /// Creates and starts a detached container. `run_args` are engine CLI flags
    /// (a devcontainer's `runArgs` are passed through).
    fn run_detached(
        &self,
        name: &str,
        run_args: &[String],
        image: &str,
        command: &[&str],
    ) -> Result<()>;

    fn start(&self, id: &str) -> Result<()>;
    fn stop(&self, id: &str) -> Result<()>;
    fn remove(&self, id: &str) -> Result<()>;

    /// `compose up -d` for a project whose files live in `dir` on the engine host.
    fn compose_up(&self, dir: &Path, project: &str, files: &[String], extra: &[&str])
    -> Result<()>;

    /// `compose down` for the project: removes its containers and network.
    fn compose_down(&self, dir: &Path, project: &str, files: &[String]) -> Result<()>;

    /// The first container id for `service`, if there is one. With `all`,
    /// includes stopped containers.
    fn compose_ps(
        &self,
        dir: &Path,
        project: &str,
        files: &[String],
        service: &str,
        all: bool,
    ) -> Result<Option<String>>;
}

/// Connects to the container engine at `target`.
pub fn connect(target: &Target) -> Box<dyn Runtime> {
    Box::new(docker::DockerCli::new(target))
}

#[cfg(test)]
mod tests {
    use super::Env;
    use super::fake::FakeEnv;

    #[test]
    fn write_file_makes_the_parent_as_root_and_copies_the_contents_in() {
        let env = FakeEnv::new();
        env.write_file("/a/b/c.txt", b"hi").unwrap();
        assert_eq!(env.file("/a/b/c.txt").unwrap(), "hi");
        assert_eq!(
            env.ran_as(&["mkdir", "-p", "/a/b"]),
            Some(super::fake::Who::Root)
        );
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
        let mut child = Command::new("sh")
            .args(["-c", &script, dest])
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
