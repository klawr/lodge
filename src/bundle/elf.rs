//! Just enough ELF to know which CPU a binary is built for.

use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::Path;

use super::arch::Arch;

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
