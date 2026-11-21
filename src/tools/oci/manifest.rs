//! What a manifest says: the layers of an image, or the images of an index and
//! which one is for which machine. Pure.

use crate::arch::Arch;
use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Descriptor {
    pub digest: String,
    pub size: u64,
    #[serde(default)]
    pub platform: Option<Platform>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Platform {
    pub architecture: String,
    pub os: String,
}

#[derive(Deserialize)]
struct Raw {
    layers: Option<Vec<Descriptor>>,
    manifests: Option<Vec<Descriptor>>,
}

#[derive(Debug)]
pub enum Manifest {
    /// One image: its layers, in order.
    Image(Vec<Descriptor>),
    /// Images for several platforms.
    Index(Vec<Descriptor>),
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let raw: Raw = serde_json::from_slice(bytes).context("parsing the manifest")?;
        match (raw.layers, raw.manifests) {
            (Some(layers), None) => Ok(Self::Image(layers)),
            (None, Some(manifests)) => Ok(Self::Index(manifests)),
            _ => bail!("the manifest is neither an image nor an index"),
        }
    }
}

/// Go's name for an architecture, which is what registries use.
pub fn oci_arch(arch: Arch) -> &'static str {
    match arch {
        Arch::X86 => "386",
        Arch::X86_64 => "amd64",
        Arch::Arm => "arm",
        Arch::Aarch64 => "arm64",
        Arch::Riscv => "riscv64",
        // Little-endian is the only one registries carry in practice.
        Arch::Ppc64 => "ppc64le",
        Arch::S390x => "s390x",
    }
}

/// The entry of an index for `arch` on Linux. Without an architecture it must be
/// unambiguous: an index with one image has an answer, one with several does not.
/// (Entries that are not images, such as attestations, say `unknown` and are
/// skipped.)
pub fn select(entries: &[Descriptor], arch: Option<Arch>) -> Result<&Descriptor> {
    let images: Vec<&Descriptor> = entries
        .iter()
        .filter(|d| d.platform.as_ref().is_none_or(|p| p.os != "unknown"))
        .collect();
    match arch {
        Some(arch) => images
            .iter()
            .find(|d| {
                d.platform
                    .as_ref()
                    .is_some_and(|p| p.os == "linux" && p.architecture == oci_arch(arch))
            })
            .copied()
            .with_context(|| {
                let has: Vec<String> = images
                    .iter()
                    .filter_map(|d| d.platform.as_ref())
                    .map(|p| format!("{}/{}", p.os, p.architecture))
                    .collect();
                format!(
                    "no image for linux/{} (it has {})",
                    oci_arch(arch),
                    has.join(", ")
                )
            }),
        None => match images.as_slice() {
            [one] => Ok(one),
            _ => bail!(
                "the reference is an index of {} images and no architecture was asked for",
                images.len()
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(arch: &str, os: &str) -> Descriptor {
        Descriptor {
            digest: format!("sha256:{arch}"),
            size: 1,
            platform: Some(Platform {
                architecture: arch.into(),
                os: os.into(),
            }),
        }
    }

    #[test]
    fn an_image_and_an_index_are_told_apart() {
        let image = br#"{"layers": [{"digest": "sha256:a", "size": 3}]}"#;
        assert!(matches!(Manifest::parse(image).unwrap(), Manifest::Image(l) if l.len() == 1));
        let index = br#"{"manifests": [{"digest": "sha256:a", "size": 3,
            "platform": {"architecture": "amd64", "os": "linux"}}]}"#;
        assert!(matches!(Manifest::parse(index).unwrap(), Manifest::Index(m) if m.len() == 1));
        assert!(Manifest::parse(b"{}").is_err());
        assert!(Manifest::parse(b"nope").is_err());
    }

    #[test]
    fn the_entry_for_the_architecture_is_chosen() {
        let entries = [entry("amd64", "linux"), entry("arm64", "linux")];
        assert_eq!(
            select(&entries, Some(Arch::Aarch64)).unwrap().digest,
            "sha256:arm64"
        );
        let err = select(&entries, Some(Arch::Riscv)).unwrap_err();
        assert!(
            format!("{err:#}").contains("linux/riscv64 (it has linux/amd64"),
            "{err:#}"
        );
    }

    #[test]
    fn a_wrong_os_never_matches_and_attestations_are_skipped() {
        let entries = [entry("amd64", "windows"), entry("unknown", "unknown")];
        assert!(select(&entries, Some(Arch::X86_64)).is_err());
        // With the attestation skipped, one image is left, so it is unambiguous.
        assert!(select(&entries, None).is_ok());
    }

    #[test]
    fn without_an_architecture_only_a_single_image_is_unambiguous() {
        assert!(select(&[entry("amd64", "linux")], None).is_ok());
        assert!(select(&[entry("amd64", "linux"), entry("arm64", "linux")], None).is_err());
    }
}
