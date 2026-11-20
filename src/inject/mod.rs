//! Putting the user's environment into a container.
//!
//! [`lodge`] is the entry point. The work is split by what is being injected:
//!
//! - [`binary`]: a binary, relinked to the injected glibc, behind a wrapper
//! - [`libs`]: its shared libraries and the dynamic linker, tracked by content
//! - [`config_files`]: config files and directories, and their ownership
//! - [`shell_env`]: `PATH`, aliases, git's `safe.directory`, the terminal
//! - [`state`]: the format version of what is kept under `/opt/lodge`

mod binary;
mod config_files;
mod libs;
mod shell_env;
mod state;

use anyhow::Result;

use crate::config::Config;
use crate::runtime::Env;
use binary::inject_binary;
use config_files::{container_home, container_user, ensure_home_writable, inject_config};
use shell_env::{configure_git_safe, inject_aliases, inject_docker_path, inject_terminfo};
use state::ensure_state_version;

const LODGE_LIB: &str = "/opt/lodge/lib";

const LODGE_BIN: &str = "/opt/lodge/bin";

// The dynamic linker is also placed here so a binary whose PT_INTERP we patch
// to /lodge/<ld> resolves its interpreter when run directly. The prefix is
// 7 bytes, matching /lib64/, so PT_INTERP can be patched in place.
const LODGE_DIR: &str = "/lodge";

pub fn lodge(env: &dyn Env, remote_user: Option<&str>) -> Result<()> {
    let config = Config::load()?;
    env.mkdir_p(&[LODGE_LIB, LODGE_BIN, LODGE_DIR])?;
    ensure_state_version(env)?;

    let home = container_home(env, remote_user);
    let uid_gid = container_user(env, remote_user);
    if let Some(owner) = uid_gid.as_deref() {
        ensure_home_writable(env, &home, remote_user, owner)?;
    }

    for item in &config.inject {
        if let Some(binary) = &item.binary {
            inject_binary(env, &home, item, binary.to_string_lossy().as_ref())?;
        }
        if let Some(cfg) = &item.config {
            inject_config(
                env,
                &home,
                item,
                cfg.to_string_lossy().as_ref(),
                uid_gid.as_deref(),
            )?;
        }
    }

    inject_docker_path(env)?;
    configure_git_safe(env, &config.git.safe_directories)?;
    inject_aliases(env, &config.aliases)?;
    inject_terminfo(env);

    Ok(())
}
