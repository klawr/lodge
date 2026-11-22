//! What injection copies into the prefix: `bin/` and `lib/`, as one snapshot.
//!
//! The binaries and libraries are read straight out of the bundle and sent as
//! they are, except that a dynamic binary is given the interpreter of the prefix
//! it is going to (the bundle's copy holds a placeholder, and is never changed).
//! The libraries and the linker are one copy, so libc and the linker always
//! move together; the binaries run directly against them, with no ld.so wrapper,
//! which keeps `/proc/self/exe` pointing at the binary (Neovim needs that when
//! it re-execs itself with `--embed`).

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::io::{Cursor, Read};

use crate::bundle::elf;
use crate::bundle::reader::Bundle;
use crate::layout::Layout;
use crate::snapshot::{Kind, Node, Snapshot};

pub(super) struct Payload<'a> {
    bundle: &'a Bundle,
    /// The dynamic binaries with the interpreter of this prefix written in. Done
    /// up front, so a prefix that cannot hold it is refused before anything in
    /// the container is touched.
    relinked: BTreeMap<String, Vec<u8>>,
}

impl<'a> Payload<'a> {
    pub(super) fn new(bundle: &'a Bundle, layout: &Layout) -> Result<Self> {
        let manifest = bundle.manifest();
        let mut relinked = BTreeMap::new();
        for binary in manifest.binaries.iter().filter(|b| b.dynamic) {
            let name = &binary.name;
            let linker = manifest
                .linker
                .as_deref()
                .with_context(|| format!("{name} is dynamic but the bundle names no linker"))?;
            let mut bytes = Vec::new();
            bundle
                .read(&format!("bin/{name}"))?
                .read_to_end(&mut bytes)
                .with_context(|| format!("reading {name} from the bundle"))?;
            elf::set_interpreter(&mut bytes, &layout.lib_file(linker))
                .with_context(|| format!("preparing {name} to run from {}", layout.prefix()))?;
            relinked.insert(name.clone(), bytes);
        }
        Ok(Self { bundle, relinked })
    }

    fn dir(&self, relative: &str) -> Node {
        Node {
            relative: relative.into(),
            kind: Kind::Dir,
            mode: 0o755,
            mtime: self.bundle.manifest().created,
            size: 0,
        }
    }
}

impl Snapshot for Payload<'_> {
    fn root(&self) -> Result<Node> {
        Ok(self.dir(""))
    }

    fn walk(&self, visit: &mut dyn FnMut(&Node) -> Result<()>) -> Result<()> {
        let manifest = self.bundle.manifest();
        visit(&self.dir(""))?;
        let mut binaries: Vec<&str> = manifest.binaries.iter().map(|b| b.name.as_str()).collect();
        binaries.sort_unstable();
        if !binaries.is_empty() {
            visit(&self.dir("bin"))?;
        }
        for name in binaries {
            let mut node = self.bundle.bin(name).root()?;
            if self.relinked.contains_key(name) {
                node.mode = 0o755;
            }
            visit(&node.renamed(format!("bin/{name}")))?;
        }
        let mut libs: Vec<&str> = manifest.libs.iter().map(|l| l.name.as_str()).collect();
        libs.sort_unstable();
        if !libs.is_empty() {
            visit(&self.dir("lib"))?;
        }
        for name in libs {
            visit(&self.bundle.lib(name).root()?.renamed(format!("lib/{name}")))?;
        }
        Ok(())
    }

    fn open(&self, relative: &str) -> Result<Box<dyn Read + '_>> {
        match relative.split_once('/') {
            // (`elf::set_interpreter` keeps the image's size, so it fits its header.)
            Some(("bin", name)) => match self.relinked.get(name) {
                Some(bytes) => Ok(Box::new(Cursor::new(bytes.as_slice()))),
                None => self.bundle.read(&format!("bin/{name}")),
            },
            Some(("lib", name)) => self.bundle.read(&format!("lib/{name}")),
            _ => bail!("the payload has nothing at {relative}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::manifest::{Binary, Lib};
    use crate::bundle::source::Prepared;
    use crate::bundle::testing::{manifest, prepared};

    fn hello(dynamic: bool) -> Binary {
        Binary {
            name: "hello".into(),
            dest: None,
            dynamic,
            hash: "aaaaaaaaaaaaaaaa".into(),
        }
    }

    fn lib(name: &str) -> Lib {
        Lib {
            name: name.into(),
            hash: "bbbbbbbbbbbbbbbb".into(),
        }
    }

    /// An ELF64 image with a placeholder interpreter, as `ldg pack` leaves it.
    fn dynamic_image() -> Vec<u8> {
        let cap = elf::INTERP_CAPACITY;
        let mut v = vec![0u8; 128 + cap];
        v[..4].copy_from_slice(b"\x7fELF");
        v[4] = 2;
        v[5] = 1;
        v[0x20..0x28].copy_from_slice(&64u64.to_le_bytes());
        v[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        v[0x38..0x3A].copy_from_slice(&1u16.to_le_bytes());
        v[64..68].copy_from_slice(&3u32.to_le_bytes());
        v[72..80].copy_from_slice(&128u64.to_le_bytes());
        v[96..104].copy_from_slice(&(cap as u64).to_le_bytes());
        let placeholder = elf::placeholder_interpreter();
        v[128..128 + placeholder.len()].copy_from_slice(placeholder.as_bytes());
        v
    }

    fn read(payload: &Payload, relative: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        payload
            .open(relative)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        bytes
    }

    fn names(payload: &Payload) -> Vec<String> {
        let mut seen = Vec::new();
        payload
            .walk(&mut |n| {
                seen.push(n.relative.clone());
                Ok(())
            })
            .unwrap();
        seen
    }

    fn with_dynamic_binary() -> (crate::tmpfile::TempDir, Prepared) {
        let mut m = manifest("x86_64");
        m.linker = Some("ld-linux-x86-64.so.2".into());
        m.libs = vec![lib("ld-linux-x86-64.so.2")];
        m.binaries = vec![hello(true)];
        prepared(
            &m,
            &[
                ("bin/hello", &dynamic_image()),
                ("lib/ld-linux-x86-64.so.2", b"linker"),
            ],
        )
    }

    #[test]
    fn binaries_and_libraries_are_one_tree_with_the_linker_beside_its_libraries() {
        let mut m = manifest("x86_64");
        m.binaries = vec![hello(false)];
        m.libs = vec![lib("libc.so.6"), lib("ld-linux.so.2")];
        let (_dir, p) = prepared(
            &m,
            &[
                ("bin/hello", b"#!/bin/sh\necho hi\n"),
                ("lib/libc.so.6", b"libc"),
                ("lib/ld-linux.so.2", b"linker"),
            ],
        );
        let payload = Payload::new(&p.bundle, &Layout::new("/p")).unwrap();

        assert_eq!(
            names(&payload),
            [
                "",
                "bin",
                "bin/hello",
                "lib",
                "lib/ld-linux.so.2",
                "lib/libc.so.6"
            ]
        );
        assert_eq!(read(&payload, "bin/hello"), b"#!/bin/sh\necho hi\n");
        assert_eq!(read(&payload, "lib/libc.so.6"), b"libc");
        assert_eq!(read(&payload, "lib/ld-linux.so.2"), b"linker");
    }

    #[test]
    fn a_bundle_without_binaries_or_libraries_sends_no_empty_folders() {
        let (_dir, p) = prepared(&manifest("x86_64"), &[]);
        let payload = Payload::new(&p.bundle, &Layout::new("/p")).unwrap();
        assert_eq!(names(&payload), [""]);
    }

    #[test]
    fn a_dynamic_binary_is_given_the_interpreter_of_the_prefix_it_goes_to() {
        let (_dir, p) = with_dynamic_binary();
        for prefix in ["/p", "/home/dev/.local/share/lodge"] {
            let payload = Payload::new(&p.bundle, &Layout::new(prefix)).unwrap();
            let image = read(&payload, "bin/hello");
            assert_eq!(
                elf::interpreter(&image).unwrap().unwrap(),
                format!("{prefix}/lib/ld-linux-x86-64.so.2")
            );
            let mut mode = 0;
            payload
                .walk(&mut |n| {
                    if n.relative == "bin/hello" {
                        mode = n.mode;
                    }
                    Ok(())
                })
                .unwrap();
            assert_eq!(mode, 0o755);
        }
        // The bundle's own copy still holds the placeholder.
        let mut kept = Vec::new();
        p.bundle
            .bin("hello")
            .open("")
            .unwrap()
            .read_to_end(&mut kept)
            .unwrap();
        assert_eq!(
            elf::interpreter(&kept).unwrap(),
            Some(elf::placeholder_interpreter())
        );
    }

    #[test]
    fn a_prefix_too_long_for_the_reserved_space_is_refused_by_name_before_anything_is_sent() {
        let (_dir, p) = with_dynamic_binary();
        let long = Layout::new(format!("/{}", "a".repeat(600)));
        let err = format!("{:#}", Payload::new(&p.bundle, &long).err().unwrap());
        assert!(err.contains("hello") && err.contains("shorter"), "{err}");
    }
}
