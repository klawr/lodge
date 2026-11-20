//! Resolving and placing the declared secrets for one container, honouring each
//! secret's `targets`. Prints names, never values.

use super::{SecretDecl, place, resolve};
use crate::output::{info, note, warning};
use crate::runtime::Container;
use anyhow::Result;

/// The names this container's host answers to, for matching `targets`: `local`
/// for the local engine, otherwise the `--remote` value and its host part.
pub fn host_names(ssh_host: Option<&str>) -> Vec<String> {
    let Some(remote) = ssh_host else {
        return vec!["local".into()];
    };
    let mut names = vec![remote.to_string()];
    let (host, _port) = crate::ssh::split(remote);
    for n in [
        host.clone(),
        host.rsplit('@').next().unwrap_or("").to_string(),
    ] {
        if !n.is_empty() && !names.contains(&n) {
            names.push(n);
        }
    }
    names
}

/// Places the secrets that apply to `container` and returns the session
/// start-up lines that export the `env` ones (empty when there is nothing).
///
/// A secret that cannot be resolved is reported and skipped rather than
/// blocking the session; the tool that needed it will say it is not logged in.
pub fn deliver(
    container: &dyn Container,
    user: Option<&str>,
    decls: &[SecretDecl],
) -> Result<String> {
    if decls.is_empty() {
        return Ok(String::new());
    }
    let hosts = host_names(container.target().ssh_host());
    let name = container
        .name()
        .unwrap_or_else(|| container.id().to_string());

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_and_remote_host_names() {
        assert_eq!(host_names(None), ["local"]);
        assert_eq!(
            host_names(Some("me@build.example:2222")),
            ["me@build.example:2222", "me@build.example", "build.example"]
        );
        assert_eq!(host_names(Some("build")), ["build"]);
    }
}
