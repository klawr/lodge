//! Config probing over SSH, for `--remote`: the project's files live on the
//! remote host, so the config has to be found there.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

// One SSH call: probe the candidate config locations (same precedence as the
// local find_config) and cat the first one that exists. Output format:
// NUL-terminated path, then raw file bytes. Exit code 1 = no config found;
// anything else is an ssh/connection failure and is reported as such.
pub(super) fn find_and_read_config(host: &str, project_path: &Path) -> Result<(PathBuf, Vec<u8>)> {
    let dc = crate::shell::shell_quote(
        &project_path
            .join(".devcontainer")
            .join("devcontainer.json")
            .to_string_lossy(),
    );
    let flat =
        crate::shell::shell_quote(&project_path.join(".devcontainer.json").to_string_lossy());
    // Quoted dir + unquoted glob, so the shell expands */devcontainer.json.
    let sub_glob = format!(
        "{}/*/devcontainer.json",
        crate::shell::shell_quote(&project_path.join(".devcontainer").to_string_lossy())
    );
    let script = format!(
        "for f in {dc} {flat} {sub_glob}; do [ -f \"$f\" ] && {{ printf '%s\\0' \"$f\"; cat \"$f\"; exit 0; }}; done; exit 1"
    );
    let out = crate::ssh::sh_output(host, &script)?;
    if !out.status.success() {
        // ssh exits with the remote command's status; our probe script exits 1
        // when nothing was found. Anything else (e.g. 255) means ssh itself
        // failed — auth, unknown host, … — and must not masquerade as a
        // missing config.
        if out.status.code() != Some(1) {
            bail!(
                "ssh to {host} failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        bail!(
            "no devcontainer config found in {} on {host} (looked for \
             .devcontainer/devcontainer.json, .devcontainer.json, and \
             .devcontainer/*/devcontainer.json)",
            project_path.display()
        );
    }
    let nul = out
        .stdout
        .iter()
        .position(|&b| b == 0)
        .context("unexpected output from remote config probe")?;
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout[..nul]).as_ref());
    let content = out.stdout[nul + 1..].to_vec();
    Ok((path, content))
}
