//! Podman: docker's command line with a few differences, and rootless by
//! default, which changes what the host can do with a container.

use std::sync::{Arc, OnceLock};

use super::{CliEngine, Flavor, Podman};
use crate::host::HostRef;
use crate::runtime::RunSpec;

/// The `podman` CLI's part of [`CliEngine`].
#[derive(Clone, Default)]
pub struct PodmanFlavor {
    /// Whether Podman runs without root (asked once).
    rootless: Arc<OnceLock<bool>>,
}

impl Podman {
    pub fn new(host: HostRef) -> Self {
        Self::with(host, PodmanFlavor::default())
    }
}

impl PodmanFlavor {
    fn is_rootless(&self, cli: &CliEngine<Self>) -> bool {
        *self.rootless.get_or_init(|| {
            cli.capture_ok(&["info", "--format", "{{.Host.Security.Rootless}}"])
                .is_some_and(|out| out.trim() == "true")
        })
    }
}

/// What a project bind-mounted into a Podman container needs. With `rootless`,
/// the user's own ids kept inside (`--userns=keep-id`; otherwise the project is
/// owned by a user the container has no mapping for), and SELinux's labelling off
/// (`label=disable`; otherwise the mount is unreadable on a host that enforces
/// it). What the devcontainer itself asks for wins: a `--userns` in `runArgs`, a
/// `label=` security option.
fn mount_flags(spec: &RunSpec, rootless: bool) -> Vec<String> {
    let asked = |prefix: &str| {
        spec.engine_args.iter().any(|a| a.contains(prefix))
            || spec.security_opt.iter().any(|a| a.starts_with(prefix))
    };
    let mut flags = Vec::new();
    if rootless && !asked("--userns") {
        flags.push("--userns=keep-id".into());
    }
    if !asked("label=") {
        flags.extend(["--security-opt".into(), "label=disable".into()]);
    }
    flags
}

impl Flavor for PodmanFlavor {
    fn program(&self) -> &'static str {
        "podman"
    }

    fn extra_run_flags(&self, cli: &CliEngine<Self>, spec: &RunSpec) -> Vec<String> {
        mount_flags(spec, self.is_rootless(cli))
    }

    fn cp_in_args<'a>(&self, target: &'a str) -> Vec<&'a str> {
        // Podman's `-a` is on by default and *chowns* everything to the
        // container's user, the opposite of what is wanted. It prints nothing.
        vec!["cp", "--archive=false", "-", target]
    }

    fn says_path_is_missing(&self, stderr: &str) -> bool {
        stderr.contains("could not be found on container")
    }

    fn compose_version_args(&self) -> &'static [&'static str] {
        // `podman compose` runs whichever provider is installed (docker-compose
        // or podman-compose), and not every provider knows `--short`.
        &["compose", "version"]
    }

    fn addresses_are_reachable(&self, cli: &CliEngine<Self>) -> bool {
        // A rootless container has an address, but not one the host can connect to.
        !self.is_rootless(cli)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rootless_keeps_the_users_ids_and_selinux_labelling_is_off_unless_told_otherwise() {
        assert_eq!(
            mount_flags(&RunSpec::default(), true),
            ["--userns=keep-id", "--security-opt", "label=disable"]
        );
        // Rootful: the ids are the host's already, so only the label.
        assert_eq!(
            mount_flags(&RunSpec::default(), false),
            ["--security-opt", "label=disable"]
        );
        // What the devcontainer asks for wins.
        let own = RunSpec {
            security_opt: vec!["label=type:container_t".into()],
            engine_args: vec!["--userns=host".into()],
            ..Default::default()
        };
        assert!(mount_flags(&own, true).is_empty());
    }

    #[test]
    fn the_tar_is_copied_in_keeping_its_owners_and_a_missing_file_is_told_apart() {
        let podman = PodmanFlavor::default();
        assert_eq!(
            podman.cp_in_args("c:/d"),
            ["cp", "--archive=false", "-", "c:/d"]
        );
        assert!(podman.says_path_is_missing(
            "Error: \"/nope/x\" could not be found on container c: no such file or directory"
        ));
        assert!(!podman.says_path_is_missing("Error: no such container c"));
        assert!(!podman.says_path_is_missing("ssh: connect to host h port 22: Connection refused"));
    }
}
