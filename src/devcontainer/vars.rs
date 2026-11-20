//! `${…}` variable substitution in devcontainer.json values.
//!
//! Resolves the host-side variables of the spec. `${containerEnv:X}` is not
//! handled here: it is rewritten for the container's own shell to expand (see
//! `config::normalize_env_value`).

use std::path::Path;

use super::hash::path_tag;

pub(super) struct Vars<'a> {
    project: &'a Path,
    container_workspace: Option<&'a str>,
}

impl<'a> Vars<'a> {
    pub(super) fn new(project: &'a Path) -> Self {
        Self {
            project,
            container_workspace: None,
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
            let value = std::env::var(name).unwrap_or_else(|_| default.to_string());
            out.replace_range(start..=end, &value);
            from = start + value.len();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_workspace_variables() {
        let p = Path::new("/home/me/proj");
        let v = Vars::new(p);
        assert_eq!(v.apply("${localWorkspaceFolder}/x"), "/home/me/proj/x");
        assert_eq!(v.apply("${localWorkspaceFolderBasename}"), "proj");
    }

    #[test]
    fn local_env_resolves_and_missing_is_empty() {
        let v = Vars::new(Path::new("/p"));
        // PATH is reliably set.
        assert_eq!(v.apply("${localEnv:PATH}"), std::env::var("PATH").unwrap());
        assert_eq!(v.apply("a${localEnv:LODGE_DEFINITELY_UNSET}b"), "ab");
    }

    #[test]
    fn local_env_default_is_used_when_unset() {
        let v = Vars::new(Path::new("/p"));
        assert_eq!(
            v.apply("${localEnv:LODGE_DEFINITELY_UNSET:fallback}"),
            "fallback"
        );
        assert_eq!(v.apply("${localEnv:LODGE_DEFINITELY_UNSET:}"), "");
        // A default may itself contain a colon.
        assert_eq!(v.apply("${localEnv:LODGE_DEFINITELY_UNSET:a:b}"), "a:b");
        // The variable wins over the default when it is set.
        assert_eq!(
            v.apply("${localEnv:PATH:fallback}"),
            std::env::var("PATH").unwrap()
        );
    }

    #[test]
    fn several_variables_in_one_value() {
        let v = Vars::new(Path::new("/p"));
        assert_eq!(
            v.apply("${localEnv:LODGE_UNSET_A:x}-${localEnv:LODGE_UNSET_B:y}"),
            "x-y"
        );
    }

    #[test]
    fn container_workspace_variables_need_the_folder() {
        let p = Path::new("/home/me/proj");
        assert_eq!(
            Vars::new(p).apply("${containerWorkspaceFolder}"),
            "${containerWorkspaceFolder}",
            "left alone until the folder is known"
        );
        let v = Vars::new(p).with_container_workspace("/workspaces/proj");
        assert_eq!(
            v.apply("${containerWorkspaceFolder}/src"),
            "/workspaces/proj/src"
        );
        assert_eq!(v.apply("${containerWorkspaceFolderBasename}"), "proj");
    }

    #[test]
    fn devcontainer_id_is_stable_and_path_scoped() {
        let a = Vars::new(Path::new("/a/proj")).apply("${devcontainerId}");
        assert_eq!(
            a,
            Vars::new(Path::new("/a/proj")).apply("${devcontainerId}")
        );
        assert_ne!(
            a,
            Vars::new(Path::new("/b/proj")).apply("${devcontainerId}")
        );
        assert!(!a.contains("${"));
    }

    #[test]
    fn an_unterminated_reference_is_left_as_is() {
        let v = Vars::new(Path::new("/p"));
        assert_eq!(v.apply("x${localEnv:PATH"), "x${localEnv:PATH");
    }
}
