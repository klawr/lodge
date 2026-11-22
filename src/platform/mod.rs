//! What differs between Unix and Windows, behind one interface.
//!
//! This is the only module with `#[cfg(unix)]` / `#[cfg(windows)]` for operating
//! system differences: the rest of the crate calls these functions and does not
//! know which system it is on. Each system implements the same set in `unix.rs`
//! and `windows.rs`.
//!
//! Windows is a bundle *consumer* (see `docs/architecture.md`): it never packs,
//! so the functions only the pack side needs ([`symlink`], [`set_mode`]) are
//! errors there, not emulations. The rest are what a consumer does.

#[cfg(unix)]
#[path = "unix.rs"]
mod system;
#[cfg(windows)]
#[path = "windows.rs"]
mod system;

use std::ffi::OsString;
use std::fs::{DirBuilder, Metadata, OpenOptions};
use std::path::Path;
use std::process::Command;

/// Makes files created with `opts` readable by this user only (mode 0600).
///
/// On Windows a new file inherits the ACL of its directory, and everything lodge
/// writes lives in the user's own profile or temp directory, which only they (and
/// administrators) can read, so there is nothing to add.
pub fn private_file(opts: &mut OpenOptions) -> &mut OpenOptions {
    system::private_file(opts)
}

/// Makes directories created with `dir` accessible by this user only (mode 0700).
/// Like [`private_file`], a no-op on Windows.
pub fn private_dir(dir: &mut DirBuilder) -> &mut DirBuilder {
    system::private_dir(dir)
}

/// Makes `cmd` start in its own session or process group, so that the terminal's
/// Ctrl-C and the closing of its console do not reach it: a daemon that outlives
/// the command that started it.
pub fn detach(cmd: &mut Command) {
    system::detach(cmd);
}

/// The identity of a directory, equal for two paths that lead to the same one
/// (through a symlink, a junction), so a walk can tell it is going in circles.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DirId(system::DirId);

/// The identity of the directory at `path`, whose metadata is `meta`.
pub fn dir_id(path: &Path, meta: &Metadata) -> std::io::Result<DirId> {
    system::dir_id(path, meta).map(DirId)
}

/// The real path of `path`, as a program that is handed it can use: on Windows
/// without the `\\?\` prefix `std::fs::canonicalize` adds, which `docker.exe` and
/// most other programs do not accept.
pub fn canonicalize(path: &Path) -> std::io::Result<std::path::PathBuf> {
    system::canonicalize(path)
}

/// The permission bits a file has once it is copied into a container (`0o755`,
/// `0o644`, setuid and the like included).
///
/// Windows has no such bits, so a file there is `0o644` and a directory `0o755`,
/// which is what Docker Desktop reports for files it shares from Windows. A
/// script that must run as a program needs its mode set by whoever packed it.
pub fn mode(meta: &Metadata) -> u32 {
    system::mode(meta)
}

/// Whether `path` is a file this machine would run as a program.
pub fn is_executable(path: &Path) -> bool {
    system::is_executable(path)
}

/// A command that runs `script` with this machine's shell: `sh -c` on Unix,
/// `cmd /C` on Windows. The script is written for that shell (a secret's
/// `command` is whatever the user would type at its prompt), and is handed over
/// as it is, with no quoting added.
pub fn shell_command(script: &str) -> Command {
    system::shell_command(script)
}

/// Whether files on this machine belong to a numeric user and group, which a
/// bind mount into a container keeps (see `devcontainer::uid`). On Windows,
/// Docker Desktop shares a folder through its VM and there is nothing to line up.
pub const HAS_UNIX_IDS: bool = system::HAS_UNIX_IDS;

/// Whether this machine has a terminfo database to copy a terminal's entry from
/// (ncurses on Unix). Windows terminals do not, and do not set `TERM`; they speak
/// what `xterm-256color` describes.
pub const HAS_TERMINFO: bool = system::HAS_TERMINFO;

/// Whether `ssh` can share one connection between its invocations
/// (`ControlMaster`). Windows' OpenSSH cannot, so there every step opens its own
/// connection, which must authenticate without a prompt (a key or an agent) except
/// for the first, `connect`.
pub const HAS_SSH_MULTIPLEX: bool = system::HAS_SSH_MULTIPLEX;

/// Whether `pid` is a running `ldg _forward` daemon. Unix reads the command line
/// from `/proc`. Windows has no such view of another process's arguments, so it
/// is any running process with this program's file name.
pub fn is_forward_daemon(pid: u32) -> bool {
    system::is_forward_daemon(pid)
}

/// The bytes of an OS string: the exact ones on Unix, the UTF-8 of its text on
/// Windows (where a lone surrogate, which text cannot hold, becomes U+FFFD).
pub fn os_string_bytes(s: OsString) -> Vec<u8> {
    system::os_string_bytes(s)
}

/// Creates the symlink `link` pointing at `target`. Pack side only: an error on
/// Windows.
pub fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) -> std::io::Result<()> {
    system::symlink(target.as_ref(), link.as_ref())
}

/// Sets the permission bits of `path`. Pack side only: an error on Windows.
pub fn set_mode(path: impl AsRef<Path>, mode: u32) -> std::io::Result<()> {
    system::set_mode(path.as_ref(), mode)
}

/// A process's exit status with the exit code `code`, for the fakes in tests.
#[cfg(test)]
pub fn exit_status(code: i32) -> std::process::ExitStatus {
    system::exit_status(code)
}
