//! `${…}` variable substitution in devcontainer.json values.
//!
//! Resolves the host-side variables of the spec. `${containerEnv:X}` is not
//! handled here: it is rewritten for the container's own shell to expand (see
//! `spec::normalize_env_value`).

use crate::local::Local;
use std::path::Path;

use crate::fingerprint::Fingerprint;

/// Short, stable tag derived from a project's path: `${devcontainerId}`, and
/// appended to container and session names so two projects that share a
/// basename never collide.
pub(super) fn path_tag(path: &Path) -> String {
    let h = Fingerprint::new()
        .put(path.to_string_lossy().as_bytes())
        .value();
    format!("{:06x}", h & 0xff_ffff)
}

pub(super) struct Vars<'a> {
    project: &'a Path,
    container_workspace: Option<&'a str>,
    /// Where `${localEnv:…}` is read from.
    local: &'a dyn Local,
}

impl<'a> Vars<'a> {
    pub(super) fn new(project: &'a Path, local: &'a dyn Local) -> Self {
        Self {
            project,
            container_workspace: None,
            local,
        }
    }

    /// Makes `${containerWorkspaceFolder}` resolvable. Unset while the
    /// workspace folder itself is being computed.
    pub(super) fn with_container_workspace(mut self, folder: &'a str) -> Self {
        self.container_workspace = Some(folder);
        self
    }

    pub(super) fn apply(&self, s: &str) -> String {
        let basename = |p: &Path| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        };

        let mut out = s
            .replace("${localWorkspaceFolder}", &self.project.to_string_lossy())
            .replace("${localWorkspaceFolderBasename}", &basename(self.project))
            .replace("${devcontainerId}", &path_tag(self.project));
        if let Some(folder) = self.container_workspace {
            out = out.replace("${containerWorkspaceFolder}", folder).replace(
                "${containerWorkspaceFolderBasename}",
                &basename(Path::new(folder)),
            );
        }

        // ${localEnv:NAME} and ${localEnv:NAME:default}
        let mut from = 0;
        while let Some(rel) = out[from..].find("${localEnv:") {
            let start = from + rel;
            let Some(end_rel) = out[start..].find('}') else {
                break;
            };
            let end = start + end_rel;
            let spec = &out[start + "${localEnv:".len()..end];
            let (name, default) = spec.split_once(':').unwrap_or((spec, ""));
            let value = self
                .local
                .var(name)
                .and_then(|v| v.into_string().ok())
                .unwrap_or_else(|| default.to_string());
            out.replace_range(start..=end, &value);
            from = start + value.len();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::fake::FakeLocal;

    #[test]
    fn local_workspace_variables() {
        let p = Path::new("/home/me/proj");
        let local = FakeLocal::new().with_var("PATH", "/usr/bin");
        let v = Vars::new(p, &local);
        assert_eq!(v.apply("${localWorkspaceFolder}/x"), "/home/me/proj/x");
        assert_eq!(v.apply("${localWorkspaceFolderBasename}"), "proj");
    }

    #[test]
    fn local_env_resolves_and_missing_is_empty() {
        let local = FakeLocal::new().with_var("PATH", "/usr/bin");
        let v = Vars::new(Path::new("/p"), &local);
        // PATH is reliably set.
        assert_eq!(v.apply("${localEnv:PATH}"), "/usr/bin");
        assert_eq!(v.apply("a${localEnv:LODGE_DEFINITELY_UNSET}b"), "ab");
    }

    #[test]
    fn local_env_default_is_used_when_unset() {
        let local = FakeLocal::new().with_var("PATH", "/usr/bin");
        let v = Vars::new(Path::new("/p"), &local);
        assert_eq!(
            v.apply("${localEnv:LODGE_DEFINITELY_UNSET:fallback}"),
            "fallback"
        );
        assert_eq!(v.apply("${localEnv:LODGE_DEFINITELY_UNSET:}"), "");
        // A default may itself contain a colon.
        assert_eq!(v.apply("${localEnv:LODGE_DEFINITELY_UNSET:a:b}"), "a:b");
        // The variable wins over the default when it is set.
        assert_eq!(v.apply("${localEnv:PATH:fallback}"), "/usr/bin");
    }

    #[test]
    fn several_variables_in_one_value() {
        let local = FakeLocal::new().with_var("PATH", "/usr/bin");
        let v = Vars::new(Path::new("/p"), &local);
        assert_eq!(
            v.apply("${localEnv:LODGE_UNSET_A:x}-${localEnv:LODGE_UNSET_B:y}"),
            "x-y"
        );
    }

    #[test]
    fn container_workspace_variables_need_the_folder() {
        let p = Path::new("/home/me/proj");
        assert_eq!(
            Vars::new(p, &FakeLocal::new()).apply("${containerWorkspaceFolder}"),
            "${containerWorkspaceFolder}",
            "left alone until the folder is known"
        );
        let local = FakeLocal::new();
        let v = Vars::new(p, &local).with_container_workspace("/workspaces/proj");
        assert_eq!(
            v.apply("${containerWorkspaceFolder}/src"),
            "/workspaces/proj/src"
        );
        assert_eq!(v.apply("${containerWorkspaceFolderBasename}"), "proj");
    }

    #[test]
    fn devcontainer_id_is_stable_and_path_scoped() {
        let a = Vars::new(Path::new("/a/proj"), &FakeLocal::new()).apply("${devcontainerId}");
        assert_eq!(
            a,
            Vars::new(Path::new("/a/proj"), &FakeLocal::new()).apply("${devcontainerId}")
        );
        assert_ne!(
            a,
            Vars::new(Path::new("/b/proj"), &FakeLocal::new()).apply("${devcontainerId}")
        );
        assert!(!a.contains("${"));
    }

    #[test]
    fn an_unterminated_reference_is_left_as_is() {
        let local = FakeLocal::new().with_var("PATH", "/usr/bin");
        let v = Vars::new(Path::new("/p"), &local);
        assert_eq!(v.apply("x${localEnv:PATH"), "x${localEnv:PATH");
    }
}
