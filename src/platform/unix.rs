//! [`super`] on Unix.

use std::ffi::OsString;
use std::fs::{DirBuilder, Metadata, OpenOptions, Permissions};
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::Command;

/// (device, inode).
pub type DirId = (u64, u64);

pub const HAS_UNIX_IDS: bool = true;
pub const HAS_TERMINFO: bool = true;
pub const HAS_SSH_MULTIPLEX: bool = true;

pub fn is_forward_daemon(pid: u32) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .is_ok_and(|c| c.split(|&b| b == 0).any(|a| a == b"_forward"))
}

pub fn shell_command(script: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script);
    cmd
}

pub fn private_file(opts: &mut OpenOptions) -> &mut OpenOptions {
    opts.mode(0o600)
}

pub fn private_dir(dir: &mut DirBuilder) -> &mut DirBuilder {
    dir.mode(0o700)
}

pub fn detach(cmd: &mut Command) {
    cmd.process_group(0);
}

pub fn dir_id(_path: &Path, meta: &Metadata) -> std::io::Result<DirId> {
    Ok((meta.dev(), meta.ino()))
}

pub fn canonicalize(path: &Path) -> std::io::Result<std::path::PathBuf> {
    std::fs::canonicalize(path)
}

pub fn mode(meta: &Metadata) -> u32 {
    meta.permissions().mode() & 0o7777
}

pub fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

pub fn os_string_bytes(s: OsString) -> Vec<u8> {
    s.into_vec()
}

#[cfg(test)]
pub fn exit_status(code: i32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt as _;
    std::process::ExitStatus::from_raw(code << 8)
}

pub fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

pub fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    std::fs::set_permissions(path, Permissions::from_mode(mode))
}
