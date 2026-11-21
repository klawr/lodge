//! What a manifest says: the layers of an image, or the images of an index and
//! which one is for which machine. Pure.

use super::digest::Digest;
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

/// The media types of what lodge writes.
pub const IMAGE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const IMAGE_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const EMPTY_CONFIG: &str = "application/vnd.oci.empty.v1+json";

/// An image manifest for an artifact: an empty config and one layer. The bytes
/// are the same for the same input (keys in order), so publishing the same thing
/// twice is the same digest.
pub fn image(
    artifact_type: &str,
    layer_media_type: &str,
    config: (&Digest, u64),
    layer: (&Digest, u64),
) -> Result<Vec<u8>> {
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": IMAGE_MANIFEST,
        "artifactType": artifact_type,
        "config": {"mediaType": EMPTY_CONFIG, "digest": config.0.as_str(), "size": config.1},
        "layers": [{"mediaType": layer_media_type, "digest": layer.0.as_str(), "size": layer.1}],
    });
    Ok(serde_json::to_vec(&manifest)?)
}

/// The index a tag should hold once `image` (for `arch`, on Linux) is published
/// to it. `existing` is what the tag holds now: an index keeps every entry but
/// the one for `arch`, which is replaced where it stood (or added at the end), so
/// publishing from several machines builds one index. Entries are kept as they
/// were read, whatever else they say. Anything else a tag held (nothing, or a
/// single image, whose machine nothing says) is replaced by an index of one. The
/// same input is the same bytes.
pub fn merged_index(
    existing: Option<&[u8]>,
    arch: Arch,
    artifact_type: &str,
    image: (&Digest, u64),
) -> Result<Vec<u8>> {
    let mut index = match existing {
        Some(bytes) => match Manifest::parse(bytes)? {
            Manifest::Index(_) => serde_json::from_slice(bytes).context("parsing the index")?,
            Manifest::Image(_) => serde_json::json!({}),
        },
        None => serde_json::json!({}),
    };
    let entry = serde_json::json!({
        "mediaType": IMAGE_MANIFEST,
        "digest": image.0.as_str(),
        "size": image.1,
        "artifactType": artifact_type,
        "platform": {"architecture": oci_arch(arch), "os": "linux"},
    });
    let is_for_arch = |e: &serde_json::Value| {
        let platform = &e["platform"];
        platform["os"] == "linux" && platform["architecture"] == oci_arch(arch)
    };
    let mut entries = match index["manifests"].take() {
        serde_json::Value::Array(entries) => entries,
        _ => Vec::new(),
    };
    let at = entries.iter().position(is_for_arch);
    entries.retain(|e| !is_for_arch(e));
    entries.insert(at.unwrap_or(entries.len()), entry);
    index["schemaVersion"] = 2.into();
    index["mediaType"] = IMAGE_INDEX.into();
    index["manifests"] = entries.into();
    Ok(serde_json::to_vec(&index)?)
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
    fn what_is_written_reads_back_as_one_image_with_one_layer() {
        let (config, layer) = (Digest::of(b"{}"), Digest::of(b"tar"));
        let bytes = image("a/type", "a/layer", (&config, 2), (&layer, 3)).unwrap();
        let Manifest::Image(layers) = Manifest::parse(&bytes).unwrap() else {
            panic!("not an image");
        };
        assert_eq!(layers.len(), 1);
        assert_eq!(
            (layers[0].digest.as_str(), layers[0].size),
            (layer.as_str(), 3)
        );
        // The same input is the same bytes, hence the same digest.
        assert_eq!(
            bytes,
            image("a/type", "a/layer", (&config, 2), (&layer, 3)).unwrap()
        );
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\"artifactType\":\"a/type\""), "{text}");
        assert!(text.contains(IMAGE_MANIFEST), "{text}");
    }

    fn merged(existing: Option<&[u8]>, arch: Arch, image: &str) -> Vec<u8> {
        merged_index(existing, arch, "a/type", (&Digest::of(image.as_bytes()), 7)).unwrap()
    }

    /// The `(os/architecture, digest)` of each entry, in order.
    fn entries(index: &[u8]) -> Vec<(String, String)> {
        let Manifest::Index(entries) = Manifest::parse(index).unwrap() else {
            panic!("not an index");
        };
        entries
            .into_iter()
            .map(|d| {
                let p = d.platform.unwrap();
                (format!("{}/{}", p.os, p.architecture), d.digest)
            })
            .collect()
    }

    #[test]
    fn a_tag_with_nothing_gets_an_index_of_one() {
        let index = merged(None, Arch::X86_64, "one");
        let want = Digest::of(b"one").to_string();
        assert_eq!(entries(&index), vec![("linux/amd64".into(), want)]);
        let text = String::from_utf8(index).unwrap();
        assert!(text.contains(IMAGE_INDEX) && text.contains("\"artifactType\":\"a/type\""));
    }

    #[test]
    fn another_architecture_is_added_and_the_same_one_replaced_in_place() {
        let first = merged(None, Arch::X86_64, "one");
        let both = merged(Some(&first), Arch::Aarch64, "two");
        let ids: Vec<String> = ["one", "two"]
            .iter()
            .map(|s| Digest::of(s.as_bytes()).to_string())
            .collect();
        assert_eq!(
            entries(&both),
            vec![
                ("linux/amd64".into(), ids[0].clone()),
                ("linux/arm64".into(), ids[1].clone())
            ]
        );
        // Publishing amd64 again changes its entry and nothing else, not even the order.
        let again = merged(Some(&both), Arch::X86_64, "three");
        assert_eq!(
            entries(&again),
            vec![
                ("linux/amd64".into(), Digest::of(b"three").to_string()),
                ("linux/arm64".into(), ids[1].clone())
            ]
        );
        // The same input is the same bytes.
        assert_eq!(again, merged(Some(&both), Arch::X86_64, "three"));
    }

    #[test]
    fn what_another_tool_put_in_an_index_is_kept_as_it_was() {
        let foreign = br#"{"schemaVersion": 2, "annotations": {"a": "b"}, "manifests": [
            {"mediaType": "m", "digest": "sha256:att", "size": 1, "annotations": {"x": "y"},
             "platform": {"architecture": "unknown", "os": "unknown"}},
            {"mediaType": "m", "digest": "sha256:win", "size": 1,
             "platform": {"architecture": "amd64", "os": "windows"}}]}"#;
        let out = merged(Some(foreign), Arch::X86_64, "one");
        let text = String::from_utf8(out.clone()).unwrap();
        assert!(text.contains("\"annotations\":{\"x\":\"y\"}"), "{text}");
        assert!(text.contains("\"annotations\":{\"a\":\"b\"}"), "{text}");
        // A windows image is not the linux one for the same architecture.
        let kinds: Vec<String> = entries(&out).into_iter().map(|e| e.0).collect();
        assert_eq!(kinds, ["unknown/unknown", "windows/amd64", "linux/amd64"]);
    }

    #[test]
    fn a_single_image_under_the_tag_is_replaced_and_garbage_is_an_error() {
        let image = br#"{"layers": [{"digest": "sha256:a", "size": 3}]}"#;
        let out = merged(Some(image), Arch::Aarch64, "two");
        assert_eq!(entries(&out).len(), 1);
        let err = merged_index(Some(b"{}"), Arch::Aarch64, "t", (&Digest::of(b"x"), 1));
        assert!(err.is_err());
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
