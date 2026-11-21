//! The seam between making an endpoint and working in one.
//!
//! [`Endpoints`] gets something ready (a devcontainer built and started
//! today; a pod, a Podman container or a plain host later) and hands back an
//! [`Attached`]: plain data plus the endpoint. Everything after that (the
//! forwarder, injection, secrets, the session) takes only an [`Attached`], so it
//! works the same for every provisioner, and it can be tested with a fake
//! endpoint and no provisioner at all.

use anyhow::Result;
use std::path::Path;
use std::sync::Arc;

use crate::host::HostRef;
use crate::local::Local;
use crate::port::PortForward;
use crate::runtime::{Container, Listed, Runtime};
use crate::tools::OciFetcher;

/// What runs after lodge has injected, right before the session opens. It is
/// given the endpoint; a failure is the hook's own to report, never fatal.
pub type PostAttach = Box<dyn Fn(&dyn Container)>;

/// An endpoint that is up, and what is needed to work in it.
pub struct Attached {
    pub container: Box<dyn Container>,
    /// The endpoint was created by this call (new, or rebuilt), so nothing has
    /// been injected into it yet.
    pub created: bool,
    pub workdir: String,
    pub forward_ports: Vec<PortForward>,
    /// The user to run as. `None` means the endpoint's default user.
    pub remote_user: Option<String>,
    pub post_attach: Option<PostAttach>,
}

impl Attached {
    /// Runs the post-attach hook, if there is one.
    pub fn run_post_attach(&self) {
        if let Some(hook) = &self.post_attach {
            hook(self.container.as_ref());
        }
    }
}

/// What a target is built from: the machine the project is on, the machine lodge
/// runs on, and the registry client for devcontainer features. Handed to a
/// [`TargetKind`] by the command layer.
pub struct Wiring {
    pub host: HostRef,
    pub local: Arc<dyn Local>,
    pub oci: Arc<dyn OciFetcher>,
}

/// What a [`TargetKind`] is made of, for the architecture test: the concrete types
/// it builds, and the places that may name them (besides the command layer).
#[cfg(test)]
pub struct Made {
    pub types: &'static [&'static str],
    pub home: &'static [&'static str],
}

/// One kind of endpoint the user can name (`--target`, `target` in the config): its
/// name, what it says about itself, and how to build its [`Endpoints`] from the
/// [`Wiring`] and the settings the user gave it (`[targets.<name>]`).
///
/// A kind is the *whole* of what is particular to a target outside its endpoint:
/// the name, the settings it understands, and which concrete types make it up (and
/// so which of them the architecture test guards). It
/// is registered in one list in the command layer (`cmd/targets.rs`), so a new
/// target is a new `TargetKind` and one line there; nothing else knows how many
/// targets there are.
pub trait TargetKind {
    /// What the user types: `docker`.
    fn name(&self) -> &'static str;

    /// One line for the help.
    fn about(&self) -> &'static str;

    /// Builds the endpoints. `settings` is the `[targets.<name>]` table, empty when
    /// the user gave none; a kind that takes none refuses a key, and one that does
    /// checks them against its own settings type.
    fn build(&self, with: Wiring, settings: &toml::Table) -> Result<Box<dyn Endpoints>>;

    /// The concrete types this target is made of and where they live, for the
    /// architecture test that nothing else names them. Required, so a new target
    /// cannot leave itself unguarded.
    #[cfg(test)]
    fn made_of(&self) -> Made;

    /// The schema of its settings, for the config file's JSON schema (None when it
    /// takes none).
    #[cfg(test)]
    fn settings_schema(&self) -> Option<schemars::Schema> {
        None
    }
}

/// Whether the config an endpoint was made from changed since.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigState {
    UpToDate,
    Changed,
    /// No recorded config, or it could not be read. Not knowing is no reason to
    /// throw an endpoint away, so this is not drift.
    Unknown,
}

/// The project's endpoint.
pub struct Found {
    pub container: Box<dyn Container>,
    /// The user sessions and hooks run as (`remoteUser`); `None` for the
    /// endpoint's default.
    pub remote_user: Option<String>,
}

/// What an endpoint's status says.
pub struct Status {
    pub found: Found,
    pub running: bool,
    /// Whether the config drifted; `None` for an endpoint that is made from none.
    pub config: Option<ConfigState>,
}

/// The endpoints lodge made, and how to get rid of them: only for an endpoint
/// that lodge creates and can throw away.
pub trait Lifecycle {
    /// Stops the project's endpoint; its name if there was one.
    fn stop(&self, project: &Path) -> Result<Option<String>>;

    /// Removes the project's endpoint; its name if there was one.
    fn remove(&self, project: &Path) -> Result<Option<String>>;

    /// Every endpoint lodge made.
    fn list(&self) -> Result<Vec<Listed>>;
}

/// Where a project's work happens, and how to get there: what every command
/// that is about "the project's container" asks, whatever the endpoint is (a
/// devcontainer on an engine, a plain host, later a pod). Chosen in the command
/// layer; everything else takes this.
pub trait Endpoints {
    /// Brings the project's endpoint up (creating it, or recreating with
    /// `rebuild`) and says where and as whom to work in it.
    fn provision(&self, project: &Path, rebuild: bool) -> Result<Attached>;

    /// The project's endpoint, if it has one.
    fn find(&self, project: &Path) -> Result<Option<Found>>;

    /// The project's endpoint and its state, if it has one.
    fn status(&self, project: &Path) -> Result<Option<Status>>;

    /// What it takes to stop, remove and list the endpoints lodge made, for an
    /// endpoint that is made by lodge and can be thrown away. `None` for one that
    /// is not (a machine that was there before lodge).
    fn lifecycle(&self) -> Option<&dyn Lifecycle>;

    /// A handle for the endpoint with this id, for the forwarding daemon, which
    /// is started with only the id.
    fn container(&self, id: &str) -> Box<dyn Container>;

    /// The container engine behind it, if there is one (what `doctor` checks).
    fn engine(&self) -> Option<&dyn Runtime>;

    /// Whether `--remote` has to be told which folder the project is: an endpoint
    /// that is a place to work in anywhere (a plain host) does not.
    fn needs_project_path(&self) -> bool;

    /// What `doctor` should check about this endpoint beyond the engine: the
    /// programs it needs, and that what it points at is there. Nothing, for one
    /// that needs nothing it can check.
    fn checks(&self) -> Vec<Probe> {
        Vec::new()
    }
}

/// One thing `doctor` checked about an endpoint.
pub struct Probe {
    pub name: String,
    /// What is good about it, or what is wrong.
    pub outcome: std::result::Result<String, String>,
}
