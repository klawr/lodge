//! Pulling from an OCI registry natively: no `oras`, no other program.
//!
//! [`NativeOci`] is an [`OciFetcher`](super::OciFetcher) built from two smaller
//! interfaces, each in its own file and each with a fake: [`Http`] (the network,
//! the only thing that names an HTTP crate) and [`Credentials`] (where a login
//! comes from). It knows registries and nothing else: what is in a layer is its
//! caller's business, so devcontainer features use it today and a bundle can
//! tomorrow.

mod auth;
mod credentials;
mod digest;
mod http;
mod manifest;
mod reference;
mod registry;

pub use credentials::{Credentials, DockerConfig};
pub use http::{Http, UreqHttp};

use crate::arch::Arch;
use crate::tools::Pinned;
use anyhow::{Context, Result, bail};
use digest::Digest;
use manifest::Manifest;
use reference::Reference;
use registry::Registry;
use std::path::Path;

pub struct NativeOci {
    http: Box<dyn Http>,
    credentials: Box<dyn Credentials>,
}

impl NativeOci {
    pub fn new(http: Box<dyn Http>, credentials: Box<dyn Credentials>) -> Self {
        Self { http, credentials }
    }
}

impl super::OciFetcher for NativeOci {
    fn pull(&self, reference: &str, arch: Option<Arch>, into: &Path) -> Result<()> {
        let r = Reference::parse(reference)?;
        let mut registry = Registry::new(
            self.http.as_ref(),
            self.credentials.as_ref(),
            &r.registry,
            &r.repository,
        );
        let mut bytes = registry.manifest(&r.target)?;
        let mut parsed = Manifest::parse(&bytes)?;
        if let Manifest::Index(entries) = &parsed {
            let chosen = manifest::select(entries, arch)?.clone();
            bytes = registry.manifest_of(&chosen)?;
            parsed = Manifest::parse(&bytes)?;
        }
        let Manifest::Image(layers) = parsed else {
            bail!("{reference} is an index of indexes");
        };
        if layers.is_empty() {
            bail!("{reference} has no layers");
        }
        for layer in &layers {
            let name = digest::Digest::parse(&layer.digest)?.file_name();
            registry
                .blob_to(layer, &into.join(name))
                .with_context(|| format!("pulling {reference}"))?;
        }
        Ok(())
    }

    fn pin(&self, reference: &str, arch: Option<Arch>) -> Result<Pinned> {
        let r = Reference::parse(reference)?;
        let mut registry = Registry::new(
            self.http.as_ref(),
            self.credentials.as_ref(),
            &r.registry,
            &r.repository,
        );
        let bytes = registry.manifest(&r.target)?;
        let digest = match Manifest::parse(&bytes)? {
            Manifest::Image(_) => Digest::of(&bytes),
            Manifest::Index(entries) => Digest::parse(&manifest::select(&entries, arch)?.digest)?,
        };
        Ok(Pinned {
            reference: format!("{}/{}@{digest}", r.registry, r.repository),
            digest: digest.to_string(),
        })
    }
}

#[cfg(test)]
mod tests;
