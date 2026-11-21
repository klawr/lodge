//! An image reference (`ghcr.io/owner/name:1`, `…@sha256:…`) split into what the
//! registry API needs. Pure.

use super::digest::Digest;
use anyhow::{Result, bail};

/// What a reference points at inside its repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Tag(String),
    Digest(Digest),
}

impl Target {
    /// The last path segment of a manifest URL.
    pub fn as_str(&self) -> &str {
        match self {
            Target::Tag(t) => t,
            Target::Digest(d) => d.as_str(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// The host (and port) that serves the registry API.
    pub registry: String,
    pub repository: String,
    pub target: Target,
}

/// Docker Hub is named several ways but served from one host.
const HUB_API: &str = "registry-1.docker.io";

impl Reference {
    pub fn parse(s: &str) -> Result<Self> {
        let (name, digest) = match s.split_once('@') {
            Some((n, d)) => (n, Some(Digest::parse(d)?)),
            None => (s, None),
        };
        // A tag is after the last `:` of the last path segment (an earlier one
        // is a registry's port).
        let (name, tag) = match name.rsplit_once(':') {
            Some((n, t)) if !t.contains('/') => (n, Some(t)),
            _ => (name, None),
        };
        let (host, path) = match name.split_once('/') {
            Some((h, p)) if h.contains('.') || h.contains(':') || h == "localhost" => (h, p),
            _ => ("docker.io", name),
        };
        let registry = match host {
            "docker.io" | "index.docker.io" => HUB_API.to_string(),
            h => h.to_string(),
        };
        let repository = if registry == HUB_API && !path.contains('/') {
            format!("library/{path}")
        } else {
            path.to_string()
        };
        if !valid_host(&registry) {
            bail!("invalid registry {registry:?} in {s:?}");
        }
        if !valid_repository(&repository) {
            bail!("invalid repository {repository:?} in {s:?}");
        }
        let target = match (digest, tag) {
            (Some(d), _) => Target::Digest(d),
            (None, Some(t)) => {
                if !valid_tag(t) {
                    bail!("invalid tag {t:?} in {s:?}");
                }
                Target::Tag(t.to_string())
            }
            (None, None) => Target::Tag("latest".into()),
        };
        Ok(Self {
            registry,
            repository,
            target,
        })
    }
}

// These end up in URLs, so they are checked rather than escaped.
fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
}

fn valid_repository(r: &str) -> bool {
    !r.is_empty()
        && r.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
        })
}

fn valid_tag(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 128
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Reference {
        Reference::parse(s).unwrap()
    }

    #[test]
    fn a_registry_reference_with_a_tag() {
        let r = parse("ghcr.io/devcontainers/features/node:1");
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "devcontainers/features/node");
        assert_eq!(r.target, Target::Tag("1".into()));
    }

    #[test]
    fn no_tag_means_latest_and_a_port_is_not_a_tag() {
        assert_eq!(parse("ghcr.io/a/b").target, Target::Tag("latest".into()));
        let r = parse("localhost:5000/a/b");
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.target, Target::Tag("latest".into()));
        assert_eq!(
            parse("localhost:5000/a/b:2").target,
            Target::Tag("2".into())
        );
    }

    #[test]
    fn a_digest_wins_over_a_tag() {
        let d = Digest::of(b"x");
        let r = parse(&format!("ghcr.io/a/b:1@{d}"));
        assert_eq!(r.target, Target::Digest(d));
    }

    #[test]
    fn docker_hub_names_are_normalised() {
        let r = parse("ubuntu:24.04");
        assert_eq!(
            (r.registry.as_str(), r.repository.as_str()),
            ("registry-1.docker.io", "library/ubuntu")
        );
        assert_eq!(parse("docker.io/me/x").repository, "me/x");
        assert_eq!(parse("me/x").registry, "registry-1.docker.io");
    }

    #[test]
    fn what_would_break_a_url_is_refused() {
        for bad in [
            "ghcr.io/a/../b:1",
            "ghcr.io/A/b:1",
            "ghcr.io/a/b:x y",
            "ghcr.io/a/b?x=1",
            "ghcr.io/a//b",
            "ghcr.io/a/b@sha256:short",
            "gh cr.io/a/b",
        ] {
            assert!(Reference::parse(bad).is_err(), "{bad}");
        }
    }
}
