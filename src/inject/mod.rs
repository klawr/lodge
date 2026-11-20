//! Putting the user's environment into a container.
//!
//! [`run`] is the entry point. The work is split by what is being injected:
//!
//! Everything is read from a bundle directory ([`crate::bundle::source`]), never
//! from the host's own files, so injecting needs neither `ldd` nor `patchelf`.
//!
//! - [`binary`]: a binary, already relinked, behind a wrapper
//! - [`libs`]: its shared libraries and the dynamic linker, tracked by content
//! - [`installed`]: a hash per installed piece, so unchanged means a cheap no-op
//! - [`config_files`]: config files and directories
//! - [`home`]: the user's home directory: writable, and owning what is put there
//! - [`shell_env`]: `PATH`, aliases, git's `safe.directory`
//! - [`state`]: the format version of what is kept under `/opt/lodge`
//! - [`remove`]: taking it all back out (`ldg clean`)

mod binary;
mod config_files;
mod home;
mod installed;
mod libs;
mod remove;
mod shell_env;
mod state;

use anyhow::Result;

use crate::bundle::source::Prepared;
use crate::layout;
use crate::runtime::{ContainerUser, Env};
use binary::{check_architecture, inject_binary};
use config_files::inject_config;
use home::ensure_home_writable;
use libs::inject_libs;
pub use remove::remove;
use shell_env::{configure_git_safe, inject_aliases, preserve_path};
pub use state::installed_bundle;
use state::{ensure_state_version, record_bundle};

/// Injects `bundle` into the environment for `user`: libraries first, then each
/// tool's binary and config, then the shell-level setup.
pub fn run(env: &dyn Env, user: &ContainerUser, bundle: &Prepared) -> Result<()> {
    check_architecture(env, &bundle.manifest)?;
    env.mkdir_p(&[layout::LIB, layout::BIN, layout::LINKER_DIR])?;
    ensure_state_version(env)?;
    ensure_home_writable(env, user)?;

    inject_libs(env, bundle)?;
    for tool in &bundle.manifest.tools {
        if let Some(binary) = &tool.binary {
            inject_binary(env, user, bundle, tool, binary)?;
        }
        if let Some(config) = &tool.config {
            inject_config(env, user, bundle, tool, config)?;
        }
    }
    record_bundle(env, &bundle.manifest.id())?;

    preserve_path(env)?;
    configure_git_safe(env, &bundle.manifest.git_safe_directories, user.exec_as())?;
    inject_aliases(env, &bundle.manifest.aliases)?;

    Ok(())
}
