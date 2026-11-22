//! Docker: the engine the rest of this module was written for. What is its own
//! is only what [`Flavor`] asks for.

use serde::{Deserialize, Serialize};

use super::{CliEngine, Docker, Flavor};
use crate::host::HostRef;
use crate::runtime::RunSpec;

/// What the user can say about docker, under `[targets.docker]` in the config.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct DockerOptions {
    /// The docker context to use (`docker --context NAME`): which engine to talk
    /// to when you have several (a remote one, Colima, Docker Desktop). Default:
    /// the one `docker context use` selected.
    #[serde(default)]
    pub context: Option<String>,
}

/// The `docker` CLI's part of [`CliEngine`].
#[derive(Clone, Debug, Default)]
pub struct DockerFlavor {
    options: DockerOptions,
}

impl Docker {
    #[cfg(test)]
    pub fn new(host: HostRef) -> Self {
        Self::with_options(host, DockerOptions::default())
    }

    pub fn with_options(host: HostRef, options: DockerOptions) -> Self {
        Self::with(host, DockerFlavor { options })
    }
}

impl Flavor for DockerFlavor {
    fn program(&self) -> &'static str {
        "docker"
    }

    fn global_args(&self) -> Vec<String> {
        self.options
            .context
            .iter()
            .flat_map(|c| ["--context".to_string(), c.clone()])
            .collect()
    }

    fn extra_run_flags(&self, _: &CliEngine<Self>, _: &RunSpec) -> Vec<String> {
        Vec::new()
    }

    fn cp_in_args<'a>(&self, target: &'a str) -> Vec<&'a str> {
        // `-q`: on a terminal docker otherwise prints "Successfully copied …" for
        // every file, which would bury lodge's own output. `-a` does not keep the
        // archive's owners: the daemon gives everything to the container's
        // *configured* user (`Config.User`), and to root when that is set to root
        // (a devcontainer's `user: root`, with another `remoteUser`). Without `-a`
        // it is root's too. Either way the owners are put right afterwards.
        vec!["cp", "-q", "-a", "-", target]
    }

    fn cp_in_keeps_owners(&self) -> bool {
        false
    }

    fn says_path_is_missing(&self, stderr: &str) -> bool {
        stderr.contains("Could not find the file")
    }

    fn compose_version_args(&self) -> &'static [&'static str] {
        &["compose", "version", "--short"]
    }

    fn addresses_are_reachable(&self, _: &CliEngine<Self>) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tar_is_copied_in_keeping_its_owners_and_a_missing_file_is_told_apart() {
        let docker = DockerFlavor::default();
        assert_eq!(docker.cp_in_args("c:/d"), ["cp", "-q", "-a", "-", "c:/d"]);
        assert!(docker.says_path_is_missing(
            "Error response from daemon: Could not find the file /nope/x in container f4b2"
        ));
        assert!(!docker.says_path_is_missing("Error response from daemon: No such container: c"));
        assert!(!docker.says_path_is_missing("ssh: connect to host h port 22: Connection refused"));
    }
}
