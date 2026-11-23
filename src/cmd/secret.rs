//! `ldg secret`: the values in this machine's credential store, which a
//! `[[secret]]` reads with `from = { store = "name" }`. The value is typed or piped
//! in, never an argument, and is never printed.

use anyhow::{Context, Result, bail};
use std::io::{IsTerminal as _, Read as _};

use super::Ctx;
use crate::cli::SecretCommand;
use crate::config::secret::{Source, validate_name};
use crate::credstore::CredentialStore;
use crate::output::{info, line, note, warning};

/// The most a piped-in value may be: a token or a credential file, not a document.
const MAX_VALUE: u64 = 1 << 20;

pub fn run(ctx: &Ctx, command: SecretCommand) -> Result<()> {
    match command {
        SecretCommand::Set { name } => {
            validate_name(&name)?;
            let value = read_value(ctx, &format!("Value for {name} (not shown): "))?;
            set(ctx.local.store(), &name, value)?;
            info!("stored {name}");
        }
        SecretCommand::Ls => ls(ctx)?,
        SecretCommand::Rm { name } => {
            remove(ctx.local.store(), &name)?;
            info!("removed {name}");
        }
    }
    Ok(())
}

/// A value: typed without echo on a terminal (asking `question`), else read from
/// stdin.
pub(super) fn read_value(ctx: &Ctx, question: &str) -> Result<Vec<u8>> {
    if std::io::stdin().is_terminal() {
        return Ok(ctx.choose.hidden(question)?.into_bytes());
    }
    let mut value = Vec::new();
    std::io::stdin()
        .take(MAX_VALUE + 1)
        .read_to_end(&mut value)
        .context("reading the value from stdin")?;
    if value.len() as u64 > MAX_VALUE {
        bail!("the value is larger than {MAX_VALUE} bytes");
    }
    Ok(value)
}

/// Stores `value` under `name`. A trailing newline is an artefact of how the value
/// was produced (`echo`, an editor), not part of it; an empty value is refused,
/// since it would only fail later as "produced an empty value".
fn set(store: &dyn CredentialStore, name: &str, mut value: Vec<u8>) -> Result<()> {
    validate_name(name)?;
    while matches!(value.last(), Some(b'\n' | b'\r')) {
        value.pop();
    }
    if value.is_empty() {
        bail!("the value is empty; nothing stored");
    }
    store.set(name, &value)
}

fn remove(store: &dyn CredentialStore, name: &str) -> Result<()> {
    validate_name(name)?;
    if !store.remove(name)? {
        bail!("nothing is stored under {name}");
    }
    Ok(())
}

/// Lists the store entries the config's secrets read from, and whether each is
/// set. (The OS stores cannot be listed, so an entry no `[[secret]]` names is not
/// shown here; `ldg secret rm NAME` still removes it.)
fn ls(ctx: &Ctx) -> Result<()> {
    let config = ctx.config()?;
    let store = ctx.local.store();
    let mut rows = Vec::new();
    let mut failure = None;
    for decl in &config.secret {
        let Source::Store(entry) = &decl.from else {
            continue;
        };
        let state = match store.get(entry) {
            Ok(Some(_)) => "set",
            Ok(None) => "not set",
            Err(e) => {
                failure.get_or_insert(e);
                "unavailable"
            }
        };
        rows.push((decl.name.clone(), entry.clone(), state));
    }
    if rows.is_empty() {
        note!("no [[secret]] reads from the store (from = {{ store = \"name\" }})");
        return Ok(());
    }
    line!("{:<28} {:<24} STATE", "SECRET", "STORE ENTRY");
    for (name, entry, state) in rows {
        line!("{name:<28} {entry:<24} {state}");
    }
    if let Some(e) = failure {
        warning!("{e:#}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::Local as _;
    use crate::local::fake::FakeLocal;

    #[test]
    fn a_value_is_stored_without_its_trailing_newline() {
        let local = FakeLocal::new();
        set(local.store(), "tok", b"abc123\n".to_vec()).unwrap();
        assert_eq!(local.stored("tok").as_deref(), Some(&b"abc123"[..]));
        // Inside the value, newlines stay (a credential file has several lines).
        set(local.store(), "file", b"a\nb\r\n".to_vec()).unwrap();
        assert_eq!(local.stored("file").as_deref(), Some(&b"a\nb"[..]));
    }

    #[test]
    fn an_empty_value_or_a_bad_name_is_refused_and_nothing_is_stored() {
        let local = FakeLocal::new();
        assert!(set(local.store(), "tok", b"\n".to_vec()).is_err());
        assert!(set(local.store(), "tok", Vec::new()).is_err());
        let err = set(local.store(), "bad name; rm", b"v".to_vec()).unwrap_err();
        assert!(format!("{err:#}").contains("invalid name"), "{err:#}");
        assert!(local.stored("tok").is_none() && local.stored("bad name; rm").is_none());
    }

    #[test]
    fn removing_says_when_there_was_nothing_and_a_broken_store_is_an_error() {
        let local = FakeLocal::new().with_stored("tok", b"v");
        remove(local.store(), "tok").unwrap();
        assert!(local.stored("tok").is_none());
        let err = remove(local.store(), "tok").unwrap_err();
        assert!(format!("{err:#}").contains("nothing is stored"), "{err:#}");
        let broken = FakeLocal::new().with_unusable_store();
        assert!(set(broken.store(), "tok", b"v".to_vec()).is_err());
        assert!(remove(broken.store(), "tok").is_err());
    }
}
