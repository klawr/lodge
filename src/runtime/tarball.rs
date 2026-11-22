//! Packing a snapshot as a tar stream whose entries belong to a given user.
//!
//! Copying into an environment as a user, without root, means the archive itself
//! says who owns what it creates (an engine that unpacks it as `docker cp -a`
//! does honours that). So the ownership fields are set here, in-process, instead
//! of fixed afterwards with `chown`, and nothing needs a `tar` in the target.

use anyhow::{Context, Result};
use std::io::Write;

use super::ContainerUser;
use crate::snapshot::{Kind, Snapshot};

/// Writes `src` to `out` as a tar archive owned by `owner`.
///
/// A directory becomes its *contents*, each entry named relative to it, so that
/// unpacking into `dest` fills `dest`. A file becomes the single entry `name`.
/// Modes and modification times are kept, and symlinks *inside* a directory stay
/// symlinks.
pub fn pack(src: &dyn Snapshot, name: &str, owner: &ContainerUser, out: impl Write) -> Result<()> {
    let mut builder = tar::Builder::new(out);
    src.walk(&mut |node| {
        let mut header = tar::Header::new_gnu();
        header.set_uid(u64::from(owner.uid));
        header.set_gid(u64::from(owner.gid));
        header.set_mode(node.mode);
        header.set_mtime(node.mtime);
        let entry_name = if node.relative.is_empty() {
            name
        } else {
            &node.relative
        };
        let packing = || format!("packing {entry_name}");
        match &node.kind {
            Kind::Dir => {
                if node.relative.is_empty() {
                    return Ok(()); // the directory itself already exists at `dest`
                }
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                builder
                    .append_data(&mut header, format!("{entry_name}/"), std::io::empty())
                    .with_context(packing)
            }
            Kind::Link(target) => {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                builder
                    .append_link(&mut header, entry_name, target)
                    .with_context(packing)
            }
            Kind::File => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(node.size);
                builder
                    .append_data(&mut header, entry_name, src.open(&node.relative)?)
                    .with_context(packing)
            }
        }
    })?;
    builder.into_inner().context("finishing the archive")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::DiskSnapshot;
    use crate::tmpfile::TempDir;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;

    fn owner() -> ContainerUser {
        ContainerUser {
            requested: Some("dev".into()),
            name: "dev".into(),
            uid: 1234,
            gid: 4321,
            home: "/home/dev".into(),
        }
    }

    /// (path, type, mode, uid, gid, link target or contents) per entry.
    type Row = (String, tar::EntryType, u32, u64, u64, String);

    fn rows(archive: &[u8]) -> Vec<Row> {
        let mut out = Vec::new();
        for entry in tar::Archive::new(archive).entries().unwrap() {
            let mut entry = entry.unwrap();
            let header = entry.header().clone();
            let mut body = String::new();
            if header.entry_type().is_file() {
                entry.read_to_string(&mut body).unwrap();
            } else if let Some(link) = header.link_name().unwrap() {
                body = link.to_string_lossy().into_owned();
            }
            out.push((
                entry.path().unwrap().to_string_lossy().into_owned(),
                header.entry_type(),
                header.mode().unwrap(),
                header.uid().unwrap(),
                header.gid().unwrap(),
                body,
            ));
        }
        out
    }

    #[test]
    fn a_directory_is_packed_as_its_contents_owned_by_the_owner() {
        let dir = TempDir::new("tarball-dir").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("a"), "A").unwrap();
        std::fs::write(dir.path().join("sub/run"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            dir.path().join("sub/run"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("a", dir.path().join("link")).unwrap();

        let mut buf = Vec::new();
        pack(&DiskSnapshot::at(dir.path()), "ignored", &owner(), &mut buf).unwrap();
        let rows = rows(&buf);

        let names: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(names, ["a", "link", "sub/", "sub/run"]);
        for row in &rows {
            assert_eq!((row.3, row.4), (1234, 4321), "{} is the owner's", row.0);
        }
        let run = rows.iter().find(|r| r.0 == "sub/run").unwrap();
        assert_eq!(run.2 & 0o777, 0o755, "modes are kept");
        let link = rows.iter().find(|r| r.0 == "link").unwrap();
        assert_eq!((link.1, link.5.as_str()), (tar::EntryType::Symlink, "a"));
    }

    #[test]
    fn a_file_is_packed_as_one_entry_under_the_name_it_is_given() {
        let dir = TempDir::new("tarball-file").unwrap();
        let file = dir.path().join("source-name");
        std::fs::write(&file, "hello").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700)).unwrap();

        let mut buf = Vec::new();
        pack(&DiskSnapshot::at(&file), "dest-name", &owner(), &mut buf).unwrap();
        let rows = rows(&buf);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "dest-name");
        assert_eq!(rows[0].2 & 0o777, 0o700);
        assert_eq!(rows[0].5, "hello");
        assert_eq!((rows[0].3, rows[0].4), (1234, 4321));
    }

    #[test]
    fn a_source_that_is_itself_a_link_is_copied_as_what_it_points_at() {
        let dir = TempDir::new("tarball-link").unwrap();
        let real = dir.path().join("real");
        std::fs::write(&real, "contents").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut buf = Vec::new();
        pack(
            &DiskSnapshot::resolved(&link).unwrap(),
            "dest",
            &owner(),
            &mut buf,
        )
        .unwrap();
        let file = rows(&buf);
        assert_eq!(file.len(), 1);
        assert_eq!(
            (file[0].1, file[0].5.as_str()),
            (tar::EntryType::Regular, "contents")
        );

        // The same for a directory reached through a link.
        let sub = dir.path().join("d");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("f"), "x").unwrap();
        let dir_link = dir.path().join("dl");
        std::os::unix::fs::symlink(&sub, &dir_link).unwrap();
        let mut buf = Vec::new();
        pack(
            &DiskSnapshot::resolved(&dir_link).unwrap(),
            "ignored",
            &owner(),
            &mut buf,
        )
        .unwrap();
        assert_eq!(
            rows(&buf).iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            ["f"]
        );
    }
}
