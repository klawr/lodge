//! CPU architectures: what a bundle is built for, what a binary is built for
//! and what a machine runs, reduced to one name so they can be compared.

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
    /// This machine's architecture: what `ldg pack` builds for. An error on a
    /// CPU lodge doesn't know.
    pub fn host() -> anyhow::Result<Self> {
        use anyhow::Context as _;
        Self::from_uname(std::env::consts::ARCH)
            .with_context(|| format!("unsupported host architecture {}", std::env::consts::ARCH))
    }

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

    /// The architecture an ELF `e_machine` value names.
    pub fn from_machine(e_machine: u16) -> Option<Self> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uname_names_are_normalised() {
        assert_eq!(Arch::from_uname("x86_64\n"), Some(Arch::X86_64));
        assert_eq!(Arch::from_uname("arm64"), Some(Arch::Aarch64));
        assert_eq!(Arch::from_uname("armv7l"), Some(Arch::Arm));
        assert_eq!(Arch::from_uname("i686"), Some(Arch::X86));
        assert_eq!(Arch::from_uname("riscv64"), Some(Arch::Riscv));
        assert_eq!(Arch::from_uname("weird"), None);
    }
}
