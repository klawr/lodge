//! The format version of what lodge keeps under `/opt/lodge`.

use anyhow::Result;

use crate::output::warning;
use crate::runtime::Env;
// Stamps the injected-state format version, warning when a newer lodge wrote it.
pub(super) fn ensure_state_version(env: &dyn Env) -> Result<()> {
    let found = env
        .exec_capture(&["cat", STATE_VERSION_FILE])
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    match found {
        Some(v) if v > STATE_VERSION => {
            warning!(
                "this container was set up by a newer lodge (state version {v}, \
                 this ldg understands {STATE_VERSION}); consider updating ldg"
            );
            Ok(())
        }
        Some(_) => Ok(()),
        None => env.write_file(STATE_VERSION_FILE, format!("{STATE_VERSION}\n").as_bytes()),
    }
}

// The layout under /opt/lodge is a format. Stamping it lets a future lodge
// migrate it, and keeps an older lodge from silently altering newer state.
const STATE_VERSION: u32 = 1;

const STATE_VERSION_FILE: &str = "/opt/lodge/version";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeEnv;

    #[test]
    fn state_version_is_stamped_once() {
        let env = FakeEnv::new();
        ensure_state_version(&env).unwrap();
        assert_eq!(env.file(STATE_VERSION_FILE).unwrap(), "1\n");
        // A newer stamp is left alone (with a warning), never overwritten.
        let newer = FakeEnv::new().with_file(STATE_VERSION_FILE, "7\n");
        ensure_state_version(&newer).unwrap();
        assert_eq!(newer.file(STATE_VERSION_FILE).unwrap(), "7\n");
    }
}
