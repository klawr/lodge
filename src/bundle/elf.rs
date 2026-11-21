//! Just enough ELF for bundles: which CPU a binary is built for, and the
//! program interpreter that makes a packed binary independent of where it is
//! installed.
//!
//! A dynamic binary names its dynamic linker (`PT_INTERP`) as an absolute path
//! that the kernel does not expand, so a binary relinked to run against a bundled
//! glibc would be tied to one install prefix. `ldg pack` therefore writes a
//! placeholder interpreter that fills a fixed number of reserved bytes
//! ([`INTERP_CAPACITY`]), and injection overwrites those bytes in place with the
//! real path ([`set_interpreter`]). Nothing moves, so no `patchelf` is needed
//! where a bundle is injected, and the same bundle fits any prefix.

use anyhow::{Context, Result, bail};
use std::io::Read;
use std::ops::Range;
use std::path::Path;

use crate::arch::Arch;

/// Bytes reserved for the interpreter path of a packed binary, counting the
/// terminating NUL. A path may use all but one of them.
pub const INTERP_CAPACITY: usize = 512;

/// `PT_INTERP`, the program header type of the interpreter.
const PT_INTERP: u64 = 3;

/// The interpreter `ldg pack` writes: exactly [`INTERP_CAPACITY`] bytes with the
/// NUL, and a name that says what is wrong if the binary is run before it was
/// injected.
pub fn placeholder_interpreter() -> String {
    const HEAD: &str = "/ldg-bundle-binary-not-injected/";
    let mut path = String::from(HEAD);
    path.extend(std::iter::repeat_n('_', INTERP_CAPACITY - 1 - HEAD.len()));
    path
}

/// Overwrites the program interpreter of the ELF image `data` with `path`,
/// NUL-padded to the space the binary reserves. The image keeps its size and
/// layout.
pub fn set_interpreter(data: &mut [u8], path: &str) -> Result<()> {
    if path.contains('\0') {
        bail!("an interpreter path cannot contain a NUL");
    }
    let range = interpreter_range(data)?.context("the binary has no program interpreter")?;
    let capacity = range.len();
    if path.len() + 1 > capacity {
        bail!(
            "the interpreter path {path} is {} bytes, but the binary reserves {capacity} \
             (a path of up to {} bytes fits); use a shorter install prefix",
            path.len() + 1,
            capacity - 1
        );
    }
    let slot = &mut data[range];
    slot[..path.len()].copy_from_slice(path.as_bytes());
    slot[path.len()..].fill(0);
    Ok(())
}

/// The program interpreter of the ELF image `data`: `None` for a static binary.
/// For checking what `ldg pack` and injection wrote.
#[cfg(test)]
pub fn interpreter(data: &[u8]) -> Result<Option<String>> {
    Ok(interpreter_range(data)?.map(|range| {
        let slot = &data[range];
        let end = slot.iter().position(|b| *b == 0).unwrap_or(slot.len());
        String::from_utf8_lossy(&slot[..end]).into_owned()
    }))
}

/// Where the interpreter string lives in `data`, `None` for an image that has
/// none (a static binary).
fn interpreter_range(data: &[u8]) -> Result<Option<Range<usize>>> {
    if data.get(..4) != Some(b"\x7fELF".as_slice()) {
        bail!("not an ELF file");
    }
    let wide = match data.get(4) {
        Some(1) => false,
        Some(2) => true,
        other => bail!("unknown ELF class {other:?}"),
    };
    let big = match data.get(5) {
        Some(1) => false,
        Some(2) => true,
        other => bail!("unknown ELF byte order {other:?}"),
    };
    let uint = |offset: u64, size: usize| read_uint(data, offset, size, big);
    let (table, entry_size, count) = if wide {
        (uint(0x20, 8)?, uint(0x36, 2)?, uint(0x38, 2)?)
    } else {
        (uint(0x1C, 4)?, uint(0x2A, 2)?, uint(0x2C, 2)?)
    };
    for i in 0..count {
        let header = table
            .checked_add(i * entry_size)
            .context("the program header table is out of range")?;
        if uint(header, 4)? != PT_INTERP {
            continue;
        }
        let (offset, size) = if wide {
            (uint(header + 8, 8)?, uint(header + 32, 8)?)
        } else {
            (uint(header + 4, 4)?, uint(header + 16, 4)?)
        };
        let end = offset
            .checked_add(size)
            .filter(|end| *end <= data.len() as u64)
            .context("the program interpreter lies outside the file")?;
        return Ok(Some(offset as usize..end as usize));
    }
    Ok(None)
}

/// The `size`-byte unsigned integer at `offset`, in the file's byte order.
fn read_uint(data: &[u8], offset: u64, size: usize, big: bool) -> Result<u64> {
    let start = usize::try_from(offset).context("an ELF offset is out of range")?;
    let bytes = start
        .checked_add(size)
        .and_then(|end| data.get(start..end))
        .context("the ELF headers are cut short")?;
    let most_significant_first = |acc: u64, byte: &u8| (acc << 8) | u64::from(*byte);
    Ok(if big {
        bytes.iter().fold(0, most_significant_first)
    } else {
        bytes.iter().rev().fold(0, most_significant_first)
    })
}

/// The architecture of the ELF file at `path`: `None` if it is not an ELF file
/// (a script, say) or an architecture we don't know.
pub fn arch_of(path: &Path) -> Result<Option<Arch>> {
    let mut header = [0u8; 20];
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    if file.read_exact(&mut header).is_err() {
        return Ok(None); // shorter than an ELF header
    }
    Ok(arch_from_header(&header))
}

/// Refuses a binary built for another CPU than `host` (this machine's): it
/// cannot be inspected with `ldd` here, and a bundle holds one architecture. A
/// file that isn't ELF (a script) passes.
pub fn check_arch(tool: &str, binary: &Path, host: Arch) -> Result<()> {
    if let Some(arch) = arch_of(binary)?
        && arch != host
    {
        bail!(
            "{tool}: {} is built for {arch}, but this machine is {host}; \
             pack on a {arch} machine",
            binary.display()
        );
    }
    Ok(())
}

fn arch_from_header(h: &[u8; 20]) -> Option<Arch> {
    if &h[..4] != b"\x7fELF" {
        return None;
    }
    let machine = [h[18], h[19]];
    let e_machine = match h[5] {
        1 => u16::from_le_bytes(machine),
        2 => u16::from_be_bytes(machine),
        _ => return None,
    };
    Arch::from_machine(e_machine)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(data: u8, machine: [u8; 2]) -> [u8; 20] {
        let mut h = [0u8; 20];
        h[..4].copy_from_slice(b"\x7fELF");
        h[5] = data;
        h[18] = machine[0];
        h[19] = machine[1];
        h
    }

    #[test]
    fn reads_little_endian_machines() {
        assert_eq!(arch_from_header(&header(1, [0x3E, 0])), Some(Arch::X86_64));
        assert_eq!(arch_from_header(&header(1, [0xB7, 0])), Some(Arch::Aarch64));
    }

    #[test]
    fn reads_big_endian_machines() {
        assert_eq!(arch_from_header(&header(2, [0, 0x16])), Some(Arch::S390x));
    }

    #[test]
    fn scripts_and_unknown_machines_are_not_architectures() {
        let mut script = [0u8; 20];
        script[..2].copy_from_slice(b"#!");
        assert_eq!(arch_from_header(&script), None);
        assert_eq!(arch_from_header(&header(1, [0xFF, 0xFF])), None);
    }

    /// A minimal ELF image with one `PT_INTERP` program header whose string
    /// (`interp`, NUL-terminated) has `capacity` bytes, followed by a marker.
    fn image(wide: bool, big: bool, interp: &str, capacity: usize) -> Vec<u8> {
        let put = |v: &mut Vec<u8>, at: usize, value: u64, size: usize| {
            let bytes = if big {
                value.to_be_bytes()[8 - size..].to_vec()
            } else {
                value.to_le_bytes()[..size].to_vec()
            };
            v[at..at + size].copy_from_slice(&bytes);
        };
        let (phoff, entry, data_at) = if wide { (64, 56, 192) } else { (52, 32, 128) };
        let mut v = vec![0u8; data_at + capacity + 4];
        v[..4].copy_from_slice(b"\x7fELF");
        v[4] = if wide { 2 } else { 1 };
        v[5] = if big { 2 } else { 1 };
        if wide {
            put(&mut v, 0x20, phoff as u64, 8);
            put(&mut v, 0x36, entry as u64, 2);
            put(&mut v, 0x38, 2, 2);
        } else {
            put(&mut v, 0x1C, phoff as u64, 4);
            put(&mut v, 0x2A, entry as u64, 2);
            put(&mut v, 0x2C, 2, 2);
        }
        // Header 0 is a PT_LOAD that must be skipped; header 1 is the interpreter.
        put(&mut v, phoff, 1, 4);
        let h = phoff + entry;
        put(&mut v, h, PT_INTERP, 4);
        if wide {
            put(&mut v, h + 8, data_at as u64, 8);
            put(&mut v, h + 32, capacity as u64, 8);
        } else {
            put(&mut v, h + 4, data_at as u64, 4);
            put(&mut v, h + 16, capacity as u64, 4);
        }
        v[data_at..data_at + interp.len()].copy_from_slice(interp.as_bytes());
        v[data_at + capacity..].copy_from_slice(b"TAIL");
        v
    }

    fn interp_of(v: &[u8]) -> String {
        let range = interpreter_range(v).unwrap().unwrap();
        let slot = &v[range];
        let end = slot.iter().position(|b| *b == 0).unwrap_or(slot.len());
        String::from_utf8_lossy(&slot[..end]).into_owned()
    }

    #[test]
    fn the_placeholder_fills_the_reserved_space_exactly() {
        let p = placeholder_interpreter();
        assert_eq!(p.len() + 1, INTERP_CAPACITY);
        assert!(p.starts_with('/'));
    }

    #[test]
    fn the_interpreter_is_replaced_in_place_for_every_class_and_byte_order() {
        for (wide, big) in [(true, false), (true, true), (false, false), (false, true)] {
            let mut v = image(wide, big, &placeholder_interpreter(), INTERP_CAPACITY);
            let before = v.len();
            set_interpreter(&mut v, "/home/dev/.local/share/lodge/lib/ld.so").unwrap();
            assert_eq!(interp_of(&v), "/home/dev/.local/share/lodge/lib/ld.so");
            assert_eq!(v.len(), before, "nothing moves");
            assert_eq!(
                &v[v.len() - 4..],
                b"TAIL",
                "nothing past the slot is touched"
            );
            // And again, for another prefix, longer and shorter.
            set_interpreter(&mut v, "/x").unwrap();
            assert_eq!(interp_of(&v), "/x");
            let slot = interpreter_range(&v).unwrap().unwrap();
            assert!(v[slot.start + 3..slot.end].iter().all(|b| *b == 0));
        }
    }

    #[test]
    fn a_path_that_does_not_fit_is_refused_with_the_limit() {
        let mut v = image(true, false, "/lib64/ld.so", 28);
        let exact = format!("/{}", "a".repeat(26)); // 27 bytes + NUL
        set_interpreter(&mut v, &exact).unwrap();
        let err = set_interpreter(&mut v, &format!("{exact}b"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("28") && err.contains("shorter"), "{err}");
        assert_eq!(interp_of(&v), exact, "a refusal changes nothing");
    }

    #[test]
    fn a_binary_without_an_interpreter_or_cut_short_is_an_error_not_a_panic() {
        let mut v = image(true, false, "/x", 16);
        // Turn the interpreter header into a PT_NULL.
        v[64 + 56..64 + 56 + 4].copy_from_slice(&[0, 0, 0, 0]);
        assert!(interpreter_range(&v).unwrap().is_none());
        assert!(
            set_interpreter(&mut v, "/y")
                .unwrap_err()
                .to_string()
                .contains("no program interpreter")
        );
        assert!(set_interpreter(&mut b"#!/bin/sh\n".to_vec(), "/y").is_err());
        let ok = image(true, false, "/x", 16);
        for cut in [3, 10, 63, 100, 130, 190] {
            let mut short = ok[..cut].to_vec();
            assert!(set_interpreter(&mut short, "/y").is_err(), "cut at {cut}");
        }
        assert!(set_interpreter(&mut ok.clone(), "a\0b").is_err());
    }

    #[test]
    fn a_real_dynamic_binary_keeps_its_interpreter_when_set_to_itself() {
        let Ok(mut data) = std::fs::read("/bin/sh") else {
            return;
        };
        let Ok(Some(range)) = interpreter_range(&data) else {
            return; // a script, or a static shell
        };
        let original = data.clone();
        let slot = &data[range];
        let end = slot.iter().position(|b| *b == 0).unwrap();
        let path = String::from_utf8(slot[..end].to_vec()).unwrap();
        set_interpreter(&mut data, &path).unwrap();
        assert_eq!(data, original);
    }

    #[test]
    fn a_real_binary_has_the_hosts_architecture() {
        // /bin/sh is an ELF file of the machine running the tests (or a script
        // on exotic systems, in which case there is nothing to compare).
        if let Ok(Some(arch)) = arch_of(Path::new("/bin/sh")) {
            let host = Arch::host().ok();
            assert!(host.is_none() || host == Some(arch));
        }
    }
}
