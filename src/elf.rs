//! Just enough ELF to know which CPU a binary is built for.

use anyhow::{Context, Result};
use std::io::Read;
use std::path::Path;

/// The CPU architecture of an ELF file, or of a machine (`uname -m`), reduced to
/// one name so the two can be compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86,
    X86_64,
    Arm,
    Aarch64,
    Riscv,
    Ppc64,
    S390x,
}

impl Arch {
    /// Parses `uname -m` output (`x86_64`, `aarch64`, `arm64`, `armv7l`, …).
    pub fn from_uname(s: &str) -> Option<Self> {
        let s = s.trim();
        Some(match s {
            "x86_64" | "amd64" => Self::X86_64,
            "aarch64" | "arm64" => Self::Aarch64,
            "i386" | "i486" | "i586" | "i686" => Self::X86,
            "s390x" => Self::S390x,
            _ if s.starts_with("armv") => Self::Arm,
            _ if s.starts_with("riscv") => Self::Riscv,
            _ if s.starts_with("ppc64") => Self::Ppc64,
            _ => return None,
        })
    }

    fn from_machine(e_machine: u16) -> Option<Self> {
        Some(match e_machine {
            0x03 => Self::X86,
            0x3E => Self::X86_64,
            0x28 => Self::Arm,
            0xB7 => Self::Aarch64,
            0xF3 => Self::Riscv,
            0x15 => Self::Ppc64,
            0x16 => Self::S390x,
            _ => return None,
        })
    }
}

impl std::fmt::Display for Arch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::X86 => "x86",
            Self::X86_64 => "x86_64",
            Self::Arm => "arm",
            Self::Aarch64 => "aarch64",
            Self::Riscv => "riscv",
            Self::Ppc64 => "ppc64",
            Self::S390x => "s390x",
        })
    }
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
    fn uname_names_are_normalised() {
        assert_eq!(Arch::from_uname("x86_64\n"), Some(Arch::X86_64));
        assert_eq!(Arch::from_uname("arm64"), Some(Arch::Aarch64));
        assert_eq!(Arch::from_uname("armv7l"), Some(Arch::Arm));
        assert_eq!(Arch::from_uname("i686"), Some(Arch::X86));
        assert_eq!(Arch::from_uname("riscv64"), Some(Arch::Riscv));
        assert_eq!(Arch::from_uname("weird"), None);
    }

    #[test]
    fn a_real_binary_has_the_hosts_architecture() {
        // /bin/sh is an ELF file of the machine running the tests (or a script
        // on exotic systems, in which case there is nothing to compare).
        if let Ok(Some(arch)) = arch_of(Path::new("/bin/sh")) {
            let host = Arch::from_uname(std::env::consts::ARCH);
            assert!(host.is_none() || host == Some(arch));
        }
    }
}
