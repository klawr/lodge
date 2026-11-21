//! Docker: the engine the rest of this module was written for. What is its own
//! is only what [`Flavor`] asks for.

use super::{CliEngine, Docker, Flavor};
use crate::host::HostRef;
use crate::runtime::RunSpec;

/// The `docker` CLI's part of [`CliEngine`].
#[derive(Clone, Copy, Debug, Default)]
pub struct DockerFlavor;

impl Docker {
    pub fn new(host: HostRef) -> Self {
        Self::with(host, DockerFlavor)
    }
}

impl Flavor for DockerFlavor {
    fn program(&self) -> &'static str {
        "docker"
    }

    fn extra_run_flags(&self, _: &CliEngine<Self>, _: &RunSpec) -> Vec<String> {
        Vec::new()
    }

    fn cp_in_args<'a>(&self, target: &'a str) -> Vec<&'a str> {
        // `-q`: on a terminal docker otherwise prints "Successfully copied …" for
        // every file, which would bury lodge's own output. `-a` makes the daemon
        // honour the archive's owners instead of making everything root's.
        vec!["cp", "-q", "-a", "-", target]
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
        let docker = DockerFlavor;
        assert_eq!(docker.cp_in_args("c:/d"), ["cp", "-q", "-a", "-", "c:/d"]);
        assert!(docker.says_path_is_missing(
            "Error response from daemon: Could not find the file /nope/x in container f4b2"
        ));
        assert!(!docker.says_path_is_missing("Error response from daemon: No such container: c"));
        assert!(!docker.says_path_is_missing("ssh: connect to host h port 22: Connection refused"));
    }
}
