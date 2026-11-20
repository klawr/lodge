//! Putting the user's environment into a container.
//!
//! [`lodge`] is the entry point. The work is split by what is being injected:
//!
//! Everything is read from a bundle directory ([`crate::bundle::source`]), never
//! from the host's own files, so injecting needs neither `ldd` nor `patchelf`.
//!
//! - [`binary`]: a binary, already relinked, behind a wrapper
//! - [`libs`]: its shared libraries and the dynamic linker, tracked by content
//! - [`installed`]: a hash per installed piece, so unchanged means a cheap no-op
//! - [`config_files`]: config files and directories, and their ownership
//! - [`shell_env`]: `PATH`, aliases, git's `safe.directory`, the terminal
//! - [`state`]: the format version of what is kept under `/opt/lodge`

mod binary;
mod config_files;
mod installed;
mod libs;
mod shell_env;
mod state;

use anyhow::Result;

use crate::bundle::source::Prepared;
use crate::bundle::{LODGE_BIN, LODGE_DIR, LODGE_LIB};
use crate::runtime::Env;
use binary::{check_architecture, inject_binary};
use config_files::{container_home, container_user, ensure_home_writable, inject_config};
use libs::inject_libs;
use shell_env::{configure_git_safe, inject_aliases, inject_docker_path, inject_terminfo};
pub use state::installed_bundle;
use state::{ensure_state_version, record_bundle};

/// Injects `bundle` into the environment: libraries first, then each tool's
/// binary and config, then the shell-level setup.
pub fn lodge(env: &dyn Env, remote_user: Option<&str>, bundle: &Prepared) -> Result<()> {
    check_architecture(env, &bundle.manifest)?;
    env.mkdir_p(&[LODGE_LIB, LODGE_BIN, LODGE_DIR])?;
    ensure_state_version(env)?;

    let home = container_home(env, remote_user);
    let uid_gid = container_user(env, remote_user);
    if let Some(owner) = uid_gid.as_deref() {
        ensure_home_writable(env, &home, remote_user, owner)?;
    }

    inject_libs(env, bundle)?;
    for tool in &bundle.manifest.tools {
        if let Some(binary) = &tool.binary {
            inject_binary(env, &home, bundle, tool, binary)?;
        }
        if let Some(config) = &tool.config {
            inject_config(env, &home, bundle, tool, config, uid_gid.as_deref())?;
        }
    }
    record_bundle(env, &bundle.manifest.id())?;

    inject_docker_path(env)?;
    configure_git_safe(env, &bundle.manifest.git_safe_directories)?;
    inject_aliases(env, &bundle.manifest.aliases)?;
    inject_terminfo(env);

    Ok(())
}
