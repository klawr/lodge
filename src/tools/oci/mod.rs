//! Talking to an OCI registry natively, to pull from it and to publish to it: no
//! `oras`, no other program.
//!
//! [`NativeOci`] is an [`OciFetcher`](super::OciFetcher) and an [`OciPusher`], two interfaces of their own so that what only
//! reads is never given a way to write. It is built from two smaller interfaces,
//! each in its own file and each with a fake: [`Http`] (the network, the only thing
//! that names an HTTP crate) and [`Credentials`] (where a login comes from). It
//! knows registries and nothing else: what is in a layer is its caller's
//! business, so devcontainer features and bundles use it unchanged.

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
use crate::tools::{Artifact, OciPusher, Pinned};
use anyhow::{Context, Result, bail};
use digest::Digest;
use manifest::Manifest;
use reference::{Reference, Target};
use registry::{Access, Registry};
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
            Access::Pull,
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
            Access::Pull,
        );
        let bytes = registry.manifest(&r.target)?;
        let digest = match Manifest::parse(&bytes)? {
            Manifest::Image(_) => Digest::of(&bytes),
            Manifest::Index(entries) => Digest::parse(&manifest::select(&entries, arch)?.digest)?,
        };
        Ok(pinned(&r, &digest))
    }
}

impl OciPusher for NativeOci {
    fn push(&self, reference: &str, artifact: &Artifact) -> Result<Pinned> {
        let r = Reference::parse(reference)?;
        let Target::Tag(tag) = &r.target else {
            bail!("a bundle is published to a tag, and {reference} names a digest");
        };
        let mut registry = Registry::new(
            self.http.as_ref(),
            self.credentials.as_ref(),
            &r.registry,
            &r.repository,
            Access::Push,
        );
        // The blobs first, the manifest last: a manifest names only what is there.
        let config = b"{}".to_vec();
        let config_digest = Digest::of(&config);
        let config_size = config.len() as u64;
        let (layer_digest, layer_size) = digest::of_file(artifact.layer)?;
        registry.upload_blob(&config_digest, http::Body::Bytes(config))?;
        registry.upload_blob(
            &layer_digest,
            http::Body::File(artifact.layer.to_path_buf()),
        )?;
        let manifest = manifest::image(
            artifact.artifact_type,
            artifact.layer_media_type,
            (&config_digest, config_size),
            (&layer_digest, layer_size),
        )?;
        let digest = registry
            .put_manifest(tag, manifest::IMAGE_MANIFEST, manifest)
            .with_context(|| format!("publishing {reference}"))?;
        Ok(pinned(&r, &digest))
    }
}

/// `registry/repository@digest`: what `pull` takes, for exactly this content.
fn pinned(r: &Reference, digest: &Digest) -> Pinned {
    Pinned {
        reference: format!("{}/{}@{digest}", r.registry, r.repository),
        digest: digest.to_string(),
    }
}

#[cfg(test)]
mod tests;
