//! The directory a session lands in: the configured `workspaceFolder`, or one
//! the person picks when it doesn't exist inside the container.

use super::Prompt;
use crate::output::warning;
use crate::runtime::Container;

/// `configured` if it exists in the container; otherwise `prompt` decides,
/// offered mount destinations and `/workspaces/*` entries. When existence
/// can't be checked, `configured` is used: only a folder known to be missing is
/// worth a question.
pub(super) fn resolve(container: &dyn Container, configured: &str, prompt: &dyn Prompt) -> String {
    match container.exists(configured) {
        Ok(true) => configured.to_string(),
        Ok(false) => prompt.choose_workdir(configured, &discover_workdirs(container)),
        Err(e) => {
            warning!("could not check that {configured} exists ({e:#}); using it");
            configured.to_string()
        }
    }
}

// Collects workspace candidates: actual mount destinations from `docker inspect`
// (the most relevant — the user's workspace is almost always a bind mount), plus
// any subdirectories found under /workspaces and /workspace (the standard
// devcontainer locations). Sorted and deduplicated.
fn discover_workdirs(container: &dyn Container) -> Vec<String> {
    let mut candidates = container.mounts();

    // `find` exits non-zero when either root is missing (the usual case) but has
    // still listed the other, so its status is ignored.
    if let Ok(out) = container.exec_capture(&[
        "sh",
        "-c",
        "find /workspaces /workspace -maxdepth 1 -mindepth 1 -type d 2>/dev/null; true",
    ]) {
        for line in out.lines().map(str::trim).filter(|s| !s.is_empty()) {
            let s = line.to_string();
            if !candidates.contains(&s) {
                candidates.push(s);
            }
        }
    }

    candidates.sort();
    candidates.dedup();
    candidates
}
