//! Resolving and placing the declared secrets for one container, honouring each
//! secret's `targets`. Prints names, never values.

use super::{place, resolve};
use crate::config::secret::SecretDecl;
use crate::host::Host;
use crate::output::{info, note, warning};
use crate::runtime::{Container, ContainerUser};
use anyhow::Result;

/// Places the secrets that apply to `container` and returns the session
/// start-up lines that export the `env` ones (empty when there is nothing).
///
/// A secret that cannot be resolved is reported and skipped rather than
/// blocking the session; the tool that needed it will say it is not logged in.
pub fn deliver(
    container: &dyn Container,
    host: &dyn Host,
    user: &ContainerUser,
    decls: &[SecretDecl],
) -> Result<String> {
    if decls.is_empty() {
        // Nothing declared (any more): take away whatever an earlier run placed.
        place::clear(container, user)?;
        return Ok(String::new());
    }
    let hosts = host.names();
    let name = container.display_name();

    let (wanted, skipped): (Vec<&SecretDecl>, Vec<&SecretDecl>) =
        decls.iter().partition(|d| d.applies_to(&hosts, &name));
    if !skipped.is_empty() {
        let names: Vec<&str> = skipped.iter().map(|d| d.name.as_str()).collect();
        note!("secrets not for this target: {}", names.join(", "));
    }

    let mut resolved = Vec::new();
    for decl in wanted {
        match resolve(decl) {
            Ok(value) => resolved.push((decl, value)),
            Err(e) => warning!("secret not delivered: {e:#}"),
        }
    }
    let items: Vec<_> = resolved.iter().map(|(d, v)| (*d, v)).collect();
    // Always place, even an empty set: it prunes secrets that are no longer wanted.
    let placed = place::place(container, user, &items)?;
    let names: Vec<&str> = placed.names().collect();
    if !names.is_empty() {
        info!("secrets: {}", names.join(", "));
    }
    Ok(placed.session_setup())
}
