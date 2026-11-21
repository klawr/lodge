//! Which shared libraries a binary needs, and its dynamic linker: the answer
//! comes from a [`DepReader`], this remembers it.

use anyhow::Result;
use std::collections::HashMap;

use crate::tools::DepReader;
pub use crate::tools::Deps;

/// Asks a [`DepReader`] about each binary once, however often it is asked: a live
/// pack needs the answer for the cache key and again to pack, and a missing
/// library is warned about once.
pub struct Resolver<'a> {
    reader: &'a dyn DepReader,
    known: HashMap<String, Deps>,
}

impl<'a> Resolver<'a> {
    pub fn new(reader: &'a dyn DepReader) -> Self {
        Self {
            reader,
            known: HashMap::new(),
        }
    }

    pub fn resolve(&mut self, binary: &str) -> Result<Deps> {
        if let Some(deps) = self.known.get(binary) {
            return Ok(deps.clone());
        }
        let deps = self.reader.read(binary)?;
        self.known.insert(binary.to_string(), deps.clone());
        Ok(deps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::PathBuf;

    struct Counting(Cell<usize>);

    impl DepReader for Counting {
        fn read(&self, _binary: &str) -> Result<Deps> {
            self.0.set(self.0.get() + 1);
            Ok(Deps {
                libs: vec![PathBuf::from("/lib/libc.so.6")],
                linker: None,
            })
        }
    }

    #[test]
    fn each_binary_is_asked_about_once() {
        let reader = Counting(Cell::new(0));
        let mut resolver = Resolver::new(&reader);
        for _ in 0..3 {
            assert_eq!(resolver.resolve("/bin/a").unwrap().libs.len(), 1);
        }
        resolver.resolve("/bin/b").unwrap();
        assert_eq!(reader.0.get(), 2);
    }
}
