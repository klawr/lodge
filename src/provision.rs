//! The seam between making an endpoint and working in one.
//!
//! A [`Provisioner`] gets something ready (a devcontainer built and started
//! today; a pod, a Podman container or a plain host later) and hands back an
//! [`Attached`]: plain data plus the endpoint. Everything after that (the
//! forwarder, injection, secrets, the session) takes only an [`Attached`], so it
//! works the same for every provisioner, and it can be tested with a fake
//! endpoint and no provisioner at all.

use anyhow::Result;
use std::path::Path;

use crate::port::PortForward;
use crate::runtime::Container;

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

/// Gets an endpoint ready for a project. The only part of `ldg up` that needs
/// an engine's provisioning side; chosen in the command layer.
pub trait Provisioner {
    /// Brings the project's endpoint up (creating it, or recreating with
    /// `rebuild`) and says where and as whom to work in it.
    fn provision(&self, project: &Path, rebuild: bool) -> Result<Attached>;
}
