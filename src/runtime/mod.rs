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

use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::shell::shell_quote;
use crate::target::Target;
use crate::tmpfile::TempFile;

/// Where lodge keeps one-shot task markers inside an environment.
const STATE_DIR: &str = "/opt/lodge/state";

/// Who a command runs as.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum User<'a> {
    /// The environment's default user (a container's `USER`).
    #[default]
    Default,
    /// uid 0, regardless of the default. lodge needs this for setup that writes
    /// to root-owned locations (`/opt/lodge`, `/lodge`, `/etc/profile.d`).
    Root,
    Named(&'a str),
}

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

    /// Copies `src` from the environment to the local path `local_dst`.
    fn cp_out(&self, src: &str, local_dst: &str) -> Result<()>;

    /// A local shell command line that opens an interactive session in the
    /// environment, running `inner` (a `sh -c` script) as `user` from `workdir`.
    fn interactive_command(&self, user: Option<&str>, workdir: &str, inner: &str) -> String;

    // ── provided, built on the above ───────────────────────────────────────

    /// Runs `argv` as root.
    fn exec_root(&self, argv: &[&str]) -> Result<()> {
        self.exec(argv, &ExecOpts::root())
    }

    /// Runs `argv` as the default user and returns stdout; errors on non-zero exit.
    fn exec_capture(&self, argv: &[&str]) -> Result<String> {
        self.exec_capture_as(None, argv)
    }

    /// Like [`exec_capture`](Self::exec_capture) as `user` (`None` = default).
    fn exec_capture_as(&self, user: Option<&str>, argv: &[&str]) -> Result<String> {
        let opts = ExecOpts {
            user: user.map_or(User::Default, User::Named),
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

    /// True if the one-shot task `key` already completed here. Lets create-time
    /// hooks and feature installs retry on a later run until they succeed.
    fn task_done(&self, key: &str) -> bool {
        self.path_exists(&task_marker(key))
    }

    /// Records that the one-shot task `key` completed.
    fn mark_task_done(&self, key: &str) -> Result<()> {
        self.write_file(&task_marker(key), b"")
    }
}

fn task_marker(key: &str) -> String {
    let safe: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{STATE_DIR}/{safe}")
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

/// A container engine.
pub trait Runtime {
    /// Where the engine runs (and so where project files must be read).
    fn target(&self) -> &Target;

    /// A handle for the container with this id or name.
    fn container(&self, id: &str) -> Box<dyn Container>;

    /// The engine's server version; errors if the engine can't be reached.
    fn version(&self) -> Result<String>;

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

    /// The first container id for `service`, or "" if there is none. With `all`,
    /// includes stopped containers.
    fn compose_ps(
        &self,
        dir: &Path,
        project: &str,
        files: &[String],
        service: &str,
        all: bool,
    ) -> Result<String>;
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
    fn task_markers_make_hooks_run_once() {
        let env = FakeEnv::new();
        assert!(!env.task_done("postCreate"));
        env.mark_task_done("postCreate").unwrap();
        assert!(env.task_done("postCreate"));
        assert!(!env.task_done("onCreate"));
    }

    #[test]
    fn task_keys_are_made_path_safe() {
        let env = FakeEnv::new();
        env.mark_task_done("ghcr.io/x/feat:1").unwrap();
        assert!(env.file_exists("/opt/lodge/state/ghcr.io_x_feat_1"));
    }

    #[test]
    fn write_file_stores_the_contents() {
        let env = FakeEnv::new();
        env.write_file("/a/b/c.txt", b"hi").unwrap();
        assert_eq!(env.file("/a/b/c.txt").unwrap(), "hi");
    }

    #[test]
    fn a_marker_from_an_earlier_run_is_respected() {
        let env = FakeEnv::new().with_file("/opt/lodge/state/onCreate", "");
        assert!(env.task_done("onCreate"));
    }
}
