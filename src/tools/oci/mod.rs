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
use crate::tools::{Artifact, OciPusher, Pinned, Replace};
use anyhow::{Context, Result, bail};
use digest::Digest;
use manifest::Manifest;
use reference::{Reference, Target};
use registry::{Access, Registry};
use std::path::Path;

pub struct NativeOci {
    http: Box<dyn Http>,
    credentials: Box<dyn Credentials>,
    waits: Vec<std::time::Duration>,
}

impl NativeOci {
    pub fn new(http: Box<dyn Http>, credentials: Box<dyn Credentials>) -> Self {
        Self {
            http,
            credentials,
            waits: registry::RETRY_WAITS.to_vec(),
        }
    }

    /// How long to wait before each new try of a request the registry answered
    /// with "not now" (a rate limit, an overloaded server). The real waits are
    /// seconds; a test has none.
    #[cfg(test)]
    pub fn retrying(mut self, waits: &[std::time::Duration]) -> Self {
        self.waits = waits.to_vec();
        self
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
        )
        .retrying(&self.waits);
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
        )
        .retrying(&self.waits);
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
        )
        .retrying(&self.waits);
        // A refusal comes before anything is sent.
        if artifact.replace == Replace::No
            && let Some(held) =
                manifest::held_for(registry.current_manifest(tag)?.as_deref(), artifact.arch)?
        {
            bail!(
                "{reference} already holds an image for {} ({held}); publish without \
                 --no-clobber to replace it",
                artifact.arch
            );
        }
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
        let image = manifest::image(
            artifact.artifact_type,
            artifact.layer_media_type,
            (&config_digest, config_size),
            (&layer_digest, layer_size),
        )?;
        let image_size = image.len() as u64;
        // The image by its digest, then the tag's index naming it: an index names
        // only what is there. The tag is read as late as possible, so a push from
        // another machine in between is lost as rarely as it can be.
        let published = (|| {
            let image_digest = registry.put_manifest(
                Digest::of(&image).as_str(),
                manifest::IMAGE_MANIFEST,
                image,
            )?;
            let index = manifest::merged_index(
                registry.current_manifest(tag)?.as_deref(),
                artifact.arch,
                artifact.artifact_type,
                (&image_digest, image_size),
            )?;
            registry.put_manifest(tag, manifest::IMAGE_INDEX, index)?;
            Ok::<_, anyhow::Error>(image_digest)
        })()
        .with_context(|| format!("publishing {reference}"))?;
        // What can be used is the image for this machine, which is what `pin` of
        // the tag answers here.
        Ok(pinned(&r, &published))
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
