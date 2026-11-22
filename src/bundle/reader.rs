//! Reading a bundle from its tar, in place.
//!
//! A bundle is never unpacked onto this machine's disk. Its files are Linux
//! files (modes, links, names a Windows filesystem cannot hold), so unpacking
//! them would lose some of that and make hashing and copying depend on what the
//! local filesystem can say. Instead the tar is indexed once, by walking its
//! headers, and everything is read from it where it lies: a [`Bundle`] answers
//! as a [`Snapshot`], and what the headers say (mode, link target) *is* the
//! truth about a file, on every machine.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use super::hash::hash;
use super::manifest::{BIN_DIR, LIB_DIR, MANIFEST_FILE, Manifest, data_file};
use crate::snapshot::{Kind, Node, Snapshot};

/// An entry's path, split into names, so that sorting entries sorts them the way
/// a directory walk visits them: a directory, then what is in it, then its next
/// sibling. (Sorting whole paths as text would put `a-b` between `a` and `a/b`.)
type Names = Vec<String>;

#[derive(Debug, Clone)]
struct Entry {
    kind: Kind,
    mode: u32,
    mtime: u64,
    size: u64,
    /// Where the content starts in the tar.
    offset: u64,
}

/// A bundle: a tar on this machine, indexed, and the manifest it holds.
pub struct Bundle {
    file: PathBuf,
    index: BTreeMap<Names, Entry>,
    manifest: Manifest,
}

impl Bundle {
    /// Indexes the tar at `file` and reads its manifest. The content is not
    /// checked: see [`verified`](Self::verified).
    pub fn open(file: &Path) -> Result<Self> {
        let index = read_index(file)?;
        let entry = index.get(&names_of(MANIFEST_FILE)).with_context(|| {
            format!(
                "{} is not a lodge bundle: it has no {MANIFEST_FILE}",
                file.display()
            )
        })?;
        let mut bytes = Vec::new();
        read_entry(file, entry)?
            .read_to_end(&mut bytes)
            .context("reading the manifest")?;
        let manifest = Manifest::from_json(&bytes)?;
        Ok(Self {
            file: file.to_path_buf(),
            index,
            manifest,
        })
    }

    /// [`open`](Self::open), and every file the manifest lists is present and
    /// matches its recorded hash, so a bundle damaged in transit is caught before
    /// anything is installed from it.
    pub fn verified(file: &Path) -> Result<Self> {
        let bundle = Self::open(file)?;
        bundle.verify()?;
        Ok(bundle)
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The tar this bundle is read from.
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// The binary `name`, as it was packed.
    pub fn bin(&self, name: &str) -> Subtree<'_> {
        self.at(&format!("{BIN_DIR}/{name}"))
    }

    /// The library `name`.
    pub fn lib(&self, name: &str) -> Subtree<'_> {
        self.at(&format!("{LIB_DIR}/{name}"))
    }

    /// The `index`th of the manifest's `data`.
    pub fn data(&self, index: usize) -> Subtree<'_> {
        self.at(&data_file(index))
    }

    /// The content of the file `path` (`bin/hello`).
    pub fn read(&self, path: &str) -> Result<Box<dyn Read + '_>> {
        self.read_names(&names_of(path))
    }

    fn read_names(&self, path: &Names) -> Result<Box<dyn Read + '_>> {
        let entry = self
            .index
            .get(path)
            .with_context(|| format!("the bundle has no {}", path.join("/")))?;
        if entry.kind != Kind::File {
            bail!("{} in the bundle is not a file", path.join("/"));
        }
        read_entry(&self.file, entry)
    }

    fn at(&self, path: &str) -> Subtree<'_> {
        Subtree {
            bundle: self,
            root: names_of(path),
        }
    }

    fn verify(&self) -> Result<()> {
        let check = |relative: String, expected: &str| -> Result<()> {
            let actual = hash(&self.at(&relative))
                .with_context(|| format!("the bundle is missing {relative}"))?;
            if actual != expected {
                bail!("the bundle is damaged: {relative} does not match its recorded hash");
            }
            Ok(())
        };
        let manifest = &self.manifest;
        for lib in &manifest.libs {
            check(format!("{LIB_DIR}/{}", lib.name), &lib.hash)?;
        }
        for binary in &manifest.binaries {
            check(format!("{BIN_DIR}/{}", binary.name), &binary.hash)?;
        }
        for (i, data) in manifest.data.iter().enumerate() {
            check(data_file(i), &data.hash)?;
        }
        Ok(())
    }
}

/// One file or directory of a bundle, and what is below it, as a snapshot rooted
/// there: `data/0` is a tree whose root is that entry, whatever it is called in
/// the tar.
pub struct Subtree<'a> {
    bundle: &'a Bundle,
    root: Names,
}

impl Subtree<'_> {
    fn entry(&self) -> Result<&Entry> {
        self.bundle
            .index
            .get(&self.root)
            .with_context(|| format!("the bundle has no {}", self.root.join("/")))
    }
}

fn node(relative: String, entry: &Entry) -> Node {
    Node {
        relative,
        kind: entry.kind.clone(),
        mode: entry.mode,
        mtime: entry.mtime,
        size: entry.size,
    }
}

impl Snapshot for Subtree<'_> {
    fn root(&self) -> Result<Node> {
        Ok(node(String::new(), self.entry()?))
    }

    fn walk(&self, visit: &mut dyn FnMut(&Node) -> Result<()>) -> Result<()> {
        visit(&self.root()?)?;
        // Everything below the root follows it directly in this order.
        let below = self
            .bundle
            .index
            .range(self.root.clone()..)
            .skip(1)
            .take_while(|(path, _)| path.starts_with(&self.root));
        for (path, entry) in below {
            visit(&node(path[self.root.len()..].join("/"), entry))?;
        }
        Ok(())
    }

    fn open(&self, relative: &str) -> Result<Box<dyn Read + '_>> {
        let mut path = self.root.clone();
        path.extend(
            relative
                .split('/')
                .filter(|n| !n.is_empty())
                .map(String::from),
        );
        self.bundle.read_names(&path)
    }
}

fn names_of(path: &str) -> Names {
    path.split('/').map(String::from).collect()
}

/// The content of `entry` in the tar at `file`.
fn read_entry<'a>(file: &Path, entry: &Entry) -> Result<Box<dyn Read + 'a>> {
    let mut handle =
        std::fs::File::open(file).with_context(|| format!("opening {}", file.display()))?;
    handle.seek(SeekFrom::Start(entry.offset))?;
    Ok(Box::new(handle.take(entry.size)))
}

/// The names in `path` as a bundle may hold them: relative, with no `..`.
fn entry_path(path: &Path) -> Result<Names> {
    let mut names = Vec::new();
    for part in path.components() {
        match part {
            Component::Normal(name) => names.push(
                name.to_str()
                    .with_context(|| format!("{} is not a name lodge can read", path.display()))?
                    .to_string(),
            ),
            Component::CurDir => {}
            _ => bail!("the bundle holds a path that leaves it: {}", path.display()),
        }
    }
    if names.is_empty() {
        bail!("the bundle holds an entry with no name");
    }
    Ok(names)
}

fn read_index(file: &Path) -> Result<BTreeMap<Names, Entry>> {
    let handle =
        std::fs::File::open(file).with_context(|| format!("opening {}", file.display()))?;
    let mut archive = tar::Archive::new(handle);
    let mut index = BTreeMap::new();
    for entry in archive
        .entries_with_seek()
        .with_context(|| format!("reading {}", file.display()))?
    {
        let entry = entry.with_context(|| format!("reading {}", file.display()))?;
        let header = entry.header();
        let path = entry_path(&entry.path()?)?;
        let kind = match header.entry_type() {
            tar::EntryType::Regular => Kind::File,
            tar::EntryType::Directory => Kind::Dir,
            tar::EntryType::Symlink => Kind::Link(
                entry
                    .link_name()?
                    .with_context(|| format!("{} is a link to nothing", path.join("/")))?
                    .to_string_lossy()
                    .into_owned(),
            ),
            other => bail!(
                "{} is a {other:?}: a bundle holds files, directories and links only",
                path.join("/")
            ),
        };
        let found = Entry {
            size: if kind == Kind::File {
                header.size()?
            } else {
                0
            },
            mode: header.mode()? & 0o7777,
            mtime: header.mtime().unwrap_or(0),
            offset: entry.raw_file_position(),
            kind,
        };
        if index.insert(path.clone(), found).is_some() {
            bail!("the bundle holds {} twice", path.join("/"));
        }
    }
    add_implied_directories(&mut index)?;
    Ok(index)
}

/// A tar need not have an entry for each directory its files are in. They exist
/// all the same, so they are added (and what is in the way of one is refused).
fn add_implied_directories(index: &mut BTreeMap<Names, Entry>) -> Result<()> {
    let parents: Vec<(Names, u64)> = index
        .iter()
        .flat_map(|(path, entry)| (1..path.len()).map(move |n| (path[..n].to_vec(), entry.mtime)))
        .collect();
    for (parent, mtime) in parents {
        match index.get(&parent) {
            Some(entry) if entry.kind == Kind::Dir => {}
            Some(_) => bail!(
                "the bundle holds {} as a file and has things inside it",
                parent.join("/")
            ),
            None => {
                index.insert(
                    parent,
                    Entry {
                        kind: Kind::Dir,
                        mode: 0o755,
                        mtime,
                        size: 0,
                        offset: 0,
                    },
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::super::testing::{bundle_dir, tar_of};
    use super::*;
    #[cfg(unix)]
    use crate::snapshot::DiskSnapshot;
    use crate::tmpfile::TempDir;

    #[cfg(unix)]
    fn walked(snapshot: &dyn Snapshot) -> Vec<String> {
        let mut seen = Vec::new();
        snapshot
            .walk(&mut |n| {
                seen.push(n.relative.clone());
                Ok(())
            })
            .unwrap();
        seen
    }

    #[cfg(unix)]
    #[test]
    fn a_bundle_is_read_in_place_and_verifies() {
        let (dir, manifest) = bundle_dir();
        let (_keep, file) = tar_of(dir.path(), "rt");

        let bundle = Bundle::verified(&file).unwrap();
        assert_eq!(bundle.manifest(), &manifest);
        let mut text = String::new();
        bundle
            .data(0)
            .open("sub/rc")
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "set x");
    }

    #[cfg(unix)]
    #[test]
    fn a_part_of_the_bundle_hashes_as_the_directory_it_was_packed_from() {
        // The manifest's hashes were made from directories; reading the same
        // bytes out of the tar must give the same answer.
        let (dir, _) = bundle_dir();
        let (_keep, file) = tar_of(dir.path(), "same-hash");
        let bundle = Bundle::open(&file).unwrap();
        for (inside, on_disk) in [("data/0", "data/0"), ("bin/hello", "bin/hello")] {
            let part = bundle.at(inside);
            assert_eq!(
                hash(&part).unwrap(),
                hash(&DiskSnapshot::at(&dir.path().join(on_disk))).unwrap(),
                "{inside}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_subtree_is_walked_like_a_directory_and_keeps_modes_and_links() {
        let (dir, _) = bundle_dir();
        let (_keep, file) = tar_of(dir.path(), "walk");
        let bundle = Bundle::open(&file).unwrap();

        // `a-b` sorts between `a` and `a/b` as text, but after `a`'s contents in a walk.
        assert_eq!(
            walked(&bundle.data(0)),
            ["", "a-b", "link", "sub", "sub/rc"]
        );
        let mut modes = Vec::new();
        bundle
            .bin("hello")
            .walk(&mut |n| {
                modes.push((n.mode & 0o777, n.size));
                Ok(())
            })
            .unwrap();
        assert_eq!(modes, [(0o755, 18)]);
        let mut links = Vec::new();
        bundle
            .data(0)
            .walk(&mut |n| {
                if let Kind::Link(to) = &n.kind {
                    links.push(to.clone());
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(links, ["sub/rc"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_damaged_file_is_caught() {
        let (dir, _) = bundle_dir();
        std::fs::write(dir.path().join("lib/libx.so.1"), "tampered").unwrap();
        let (_keep, file) = tar_of(dir.path(), "damaged");
        let err = Bundle::verified(&file).err().unwrap().to_string();
        assert!(
            err.contains("damaged") && err.contains("lib/libx.so.1"),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_file_is_caught_too() {
        let (dir, _) = bundle_dir();
        std::fs::remove_file(dir.path().join("bin/hello")).unwrap();
        let (_keep, file) = tar_of(dir.path(), "missing");
        let err = format!("{:#}", Bundle::verified(&file).err().unwrap());
        assert!(err.contains("bin/hello"), "{err}");
    }

    /// A tar holding the given entries, named by raw bytes so that a name the
    /// tar crate refuses to write can still be put in.
    fn raw_tar(entries: &[(&str, tar::EntryType, &str)]) -> (TempDir, PathBuf) {
        let dir = TempDir::new("raw-tar").unwrap();
        let file = dir.path().join("x.tar");
        let mut out = Vec::new();
        for (name, kind, body) in entries {
            let mut header = tar::Header::new_gnu();
            header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
            header.set_entry_type(*kind);
            header.set_mode(0o644);
            header.set_size(body.len() as u64);
            if *kind == tar::EntryType::Symlink {
                header.set_link_name("t").unwrap();
                header.set_size(0);
            }
            header.set_cksum();
            out.extend_from_slice(header.as_bytes());
            if *kind == tar::EntryType::Regular {
                out.extend_from_slice(body.as_bytes());
                out.resize(out.len().next_multiple_of(512), 0);
            }
        }
        out.extend_from_slice(&[0; 1024]);
        std::fs::write(&file, out).unwrap();
        (dir, file)
    }

    #[test]
    fn something_that_is_not_a_bundle_says_so() {
        let (_dir, file) = raw_tar(&[("readme", tar::EntryType::Regular, "hi")]);
        let err = Bundle::open(&file).err().unwrap().to_string();
        assert!(err.contains("not a lodge bundle"), "{err}");
    }

    #[test]
    fn a_path_that_leaves_the_bundle_is_refused() {
        for name in ["../x", "/etc/passwd", "a/../../x"] {
            let (_dir, file) = raw_tar(&[(name, tar::EntryType::Regular, "x")]);
            let err = Bundle::open(&file).err().unwrap().to_string();
            assert!(err.contains("leaves it"), "{name}: {err}");
        }
    }

    #[test]
    fn what_a_bundle_may_not_hold_is_refused() {
        let (_dir, hard) = raw_tar(&[("a", tar::EntryType::Link, "")]);
        assert!(
            Bundle::open(&hard)
                .err()
                .unwrap()
                .to_string()
                .contains("files, directories and links only")
        );
        let (_dir, twice) = raw_tar(&[
            ("a", tar::EntryType::Regular, "1"),
            ("a", tar::EntryType::Regular, "2"),
        ]);
        assert!(
            Bundle::open(&twice)
                .err()
                .unwrap()
                .to_string()
                .contains("twice")
        );
        let (_dir, blocked) = raw_tar(&[
            ("a", tar::EntryType::Regular, "1"),
            ("a/b", tar::EntryType::Regular, "2"),
        ]);
        assert!(
            Bundle::open(&blocked)
                .err()
                .unwrap()
                .to_string()
                .contains("as a file")
        );
    }

    #[test]
    fn directories_a_tar_leaves_out_are_implied() {
        let (_dir, file) = raw_tar(&[
            ("data/0/sub/rc", tar::EntryType::Regular, "x"),
            ("manifest.json", tar::EntryType::Regular, "{}"),
        ]);
        // (Not a bundle: the manifest is no manifest; the index is what is looked at.)
        let index = read_index(&file).unwrap();
        for dir in ["data", "data/0", "data/0/sub"] {
            assert_eq!(index[&names_of(dir)].kind, Kind::Dir, "{dir}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_part_the_bundle_lacks_is_an_error_not_an_empty_tree() {
        let (dir, _) = bundle_dir();
        let (_keep, file) = tar_of(dir.path(), "absent");
        let bundle = Bundle::open(&file).unwrap();
        assert!(bundle.data(7).walk(&mut |_| Ok(())).is_err());
        assert!(bundle.bin("nope").open("").is_err());
        // A directory has no content to open.
        assert!(bundle.data(0).open("sub").is_err());
    }
}
