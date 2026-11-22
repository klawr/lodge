//! [`super`] on Windows.

use std::ffi::OsString;
use std::fs::{DirBuilder, Metadata, OpenOptions};
use std::io::{Error, ErrorKind};
use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// The real path: `canonicalize` resolves symlinks and junctions.
pub type DirId = PathBuf;

pub const HAS_UNIX_IDS: bool = false;
pub const HAS_TERMINFO: bool = false;
pub const HAS_SSH_MULTIPLEX: bool = false;

/// Asks `tasklist` for the process's image name and compares it with ours.
pub fn is_forward_daemon(pid: u32) -> bool {
    let Some(ours) = std::env::current_exe()
        .ok()
        .and_then(|e| e.file_name().map(|n| n.to_string_lossy().into_owned()))
    else {
        return false;
    };
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .is_ok_and(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim_start()
                .starts_with(&format!("\"{ours}\""))
        })
}

/// `cmd /S /C "script"`: `/S` strips exactly the outer quotes, so the script keeps
/// its own. The argument is raw because `Command` would otherwise escape the
/// quotes for a C program, which `cmd` does not read.
pub fn shell_command(script: &str) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.args(["/S", "/C"]).raw_arg(format!("\"{script}\""));
    cmd
}

pub fn private_file(opts: &mut OpenOptions) -> &mut OpenOptions {
    opts
}

pub fn private_dir(dir: &mut DirBuilder) -> &mut DirBuilder {
    dir
}

pub fn detach(cmd: &mut Command) {
    cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
}

pub fn dir_id(path: &Path, _meta: &Metadata) -> std::io::Result<DirId> {
    std::fs::canonicalize(path)
}

pub fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path).map(|p| without_verbatim_prefix(&p))
}

/// `\\?\C:\x` is `C:\x`, and `\\?\UNC\server\share` is `\\server\share`. Anything
/// else (a device path, say) has no shorter spelling and stays.
fn without_verbatim_prefix(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{unc}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

pub fn mode(meta: &Metadata) -> u32 {
    if meta.is_dir() { 0o755 } else { 0o644 }
}

/// Windows runs a file by its extension, as `PATHEXT` lists them.
pub fn is_executable(path: &Path) -> bool {
    const EXTENSIONS: [&str; 4] = ["exe", "com", "bat", "cmd"];
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
        && path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

pub fn os_string_bytes(s: OsString) -> Vec<u8> {
    s.to_string_lossy().into_owned().into_bytes()
}

#[cfg(test)]
pub fn exit_status(code: i32) -> std::process::ExitStatus {
    use std::os::windows::process::ExitStatusExt as _;
    // The exit code is a `u32` here; the tests only use small ones.
    std::process::ExitStatus::from_raw(code.cast_unsigned())
}

fn pack_only(what: &str) -> Error {
    Error::new(
        ErrorKind::Unsupported,
        format!("{what} is only needed to pack a bundle, which Windows does not do"),
    )
}

pub fn symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(pack_only("creating a symlink"))
}

pub fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Err(pack_only("setting a file mode"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_verbatim_prefix_goes_only_where_a_shorter_spelling_exists() {
        let plain = |s: &str| without_verbatim_prefix(Path::new(s));
        assert_eq!(plain(r"\\?\C:\Users\me"), Path::new(r"C:\Users\me"));
        assert_eq!(plain(r"\\?\UNC\srv\share\p"), Path::new(r"\\srv\share\p"));
        assert_eq!(plain(r"\\?\GLOBALROOT\x"), Path::new(r"\\?\GLOBALROOT\x"));
        assert_eq!(plain(r"C:\Users\me"), Path::new(r"C:\Users\me"));
    }
}
