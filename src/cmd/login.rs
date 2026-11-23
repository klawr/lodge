//! Registry logins on this machine: `ldg registry login|logout`, and the question
//! put to the person when a registry refuses a pull or a push. The login is
//! kept in the credential store ([`crate::secret::registry`]), never in the config.

use anyhow::{Result, bail};
use std::sync::Arc;

use super::Ctx;
use super::prompt::{Ask, Choose, Terminal};
use crate::cli::RegistryCommand;
use crate::credstore::CredentialStore;
use crate::local::Local;
use crate::output::{info, note, warning};
use crate::secret::registry::{forget, save};
use crate::tools::{Login, LoginPrompt};

pub fn run(ctx: &Ctx, command: RegistryCommand) -> Result<()> {
    match command {
        RegistryCommand::Login { registry, username } => {
            let username = match username {
                Some(u) => u,
                None => match Terminal::default().ask("User name: ")? {
                    Some(u) => u,
                    None => bail!("no user name given; pass -u"),
                },
            };
            let question = format!("Password or access token for {username} (not shown): ");
            let password = super::secret::read_value(ctx, &question)?;
            keep(ctx.local.store(), &registry, username, password)?;
            info!("stored the login for {registry}");
        }
        RegistryCommand::Logout { registry } => {
            drop_login(ctx.local.store(), &registry)?;
            info!("forgot the login for {registry}");
        }
    }
    Ok(())
}

/// Keeps what was typed or piped in as the login. A trailing newline is an
/// artefact of how the password was produced (`echo`), not part of it.
fn keep(
    store: &dyn CredentialStore,
    registry: &str,
    username: String,
    mut password: Vec<u8>,
) -> Result<()> {
    while matches!(password.last(), Some(b'\n' | b'\r')) {
        password.pop();
    }
    let password =
        String::from_utf8(password).map_err(|_| anyhow::anyhow!("the password is not text"))?;
    save(store, registry, &Login { username, password })
}

fn drop_login(store: &dyn CredentialStore, registry: &str) -> Result<()> {
    if !forget(store, registry)? {
        bail!("no login is kept for {registry}");
    }
    Ok(())
}

/// Asks the person at the terminal; nobody to ask when there is none.
pub struct TerminalLogin<T> {
    terminal: T,
    local: Arc<dyn Local>,
}

impl<T> TerminalLogin<T> {
    pub fn new(terminal: T, local: Arc<dyn Local>) -> Self {
        Self { terminal, local }
    }
}

impl<T: Choose + Ask + Send + Sync> LoginPrompt for TerminalLogin<T> {
    fn ask(&self, registry: &str, rejected: bool) -> Result<Option<Login>> {
        if !self.terminal.can_ask() {
            return Ok(None);
        }
        if rejected {
            warning!("{registry} refused the login it was given");
        } else {
            note!("{registry} needs a login");
        }
        note!("with two-factor authentication on, use an access token as the password");
        // Giving up (empty, Esc, Ctrl-C) leaves the registry's own refusal to report.
        let Some(username) = self.terminal.ask("User name (empty to give up): ")? else {
            return Ok(None);
        };
        let Ok(password) = self
            .terminal
            .hidden("Password or access token (not shown): ")
        else {
            return Ok(None);
        };
        if password.is_empty() {
            return Ok(None);
        }
        Ok(Some(Login { username, password }))
    }

    fn accepted(&self, registry: &str, login: &Login) {
        let keep = self
            .terminal
            .ask_or("Keep this login in the credential store? [Y/n] ", "y");
        if !matches!(keep, Ok(Some(a)) if !a.to_lowercase().starts_with('n')) {
            return;
        }
        match save(self.local.store(), registry, login) {
            Ok(()) => info!("stored the login for {registry}"),
            Err(e) => warning!("could not keep the login: {e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::table::Table;
    use crate::local::fake::FakeLocal;
    use std::sync::Mutex;

    /// A person (or nobody) at a terminal: what they type, in turn, and what they
    /// were asked.
    struct Person {
        present: bool,
        typed: Mutex<Vec<Option<String>>>,
        hidden: Mutex<Vec<Option<String>>>,
        asked: Mutex<Vec<String>>,
    }

    impl Person {
        fn at_terminal(typed: &[Option<&str>], hidden: &[Option<&str>]) -> Self {
            let own = |l: &[Option<&str>]| l.iter().map(|s| s.map(String::from)).collect();
            Self {
                present: true,
                typed: Mutex::new(own(typed)),
                hidden: Mutex::new(own(hidden)),
                asked: Mutex::default(),
            }
        }

        fn nobody() -> Self {
            Self {
                present: false,
                ..Self::at_terminal(&[], &[])
            }
        }
    }

    impl Choose for Person {
        fn can_ask(&self) -> bool {
            self.present
        }

        fn hidden(&self, question: &str) -> Result<String> {
            self.asked.lock().unwrap().push(question.into());
            match self.hidden.lock().unwrap().remove(0) {
                Some(line) => Ok(line),
                None => bail!("cancelled"),
            }
        }

        fn choose(&self, _: &str, _: &Table) -> Result<Option<usize>> {
            unreachable!("a login is never picked from a list")
        }
    }

    impl Ask for Person {
        fn ask(&self, question: &str) -> Result<Option<String>> {
            self.asked.lock().unwrap().push(question.into());
            Ok(self
                .typed
                .lock()
                .unwrap()
                .remove(0)
                .filter(|s| !s.is_empty()))
        }

        fn ask_or(&self, question: &str, default: &str) -> Result<Option<String>> {
            self.asked.lock().unwrap().push(question.into());
            Ok(Some(
                self.typed
                    .lock()
                    .unwrap()
                    .remove(0)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| default.into()),
            ))
        }
    }

    fn prompt(person: Person) -> (TerminalLogin<Person>, Arc<FakeLocal>) {
        let local = Arc::new(FakeLocal::new());
        (TerminalLogin::new(person, local.clone()), local)
    }

    fn kai() -> Login {
        Login {
            username: "kai".into(),
            password: "tok".into(),
        }
    }

    #[test]
    fn nobody_at_the_terminal_is_nobody_to_ask() {
        let (p, _) = prompt(Person::nobody());
        assert!(p.ask("ghcr.io", false).unwrap().is_none());
        assert!(p.terminal.asked.lock().unwrap().is_empty());
    }

    #[test]
    fn a_user_name_and_a_password_are_the_login() {
        let (p, _) = prompt(Person::at_terminal(&[Some("kai")], &[Some("tok")]));
        let login = p.ask("ghcr.io", false).unwrap().unwrap();
        assert_eq!(
            (login.username.as_str(), login.password.as_str()),
            ("kai", "tok")
        );
        // The password is asked for hidden.
        let asked = p.terminal.asked.lock().unwrap().join("|");
        assert!(asked.contains("not shown"), "{asked}");
    }

    #[test]
    fn giving_up_at_either_question_is_no_login() {
        // An empty user name.
        let (p, _) = prompt(Person::at_terminal(&[None], &[]));
        assert!(p.ask("ghcr.io", false).unwrap().is_none());
        // Esc or Ctrl-C at the password, which the terminal reports as an error.
        let (p, _) = prompt(Person::at_terminal(&[Some("kai")], &[None]));
        assert!(p.ask("ghcr.io", false).unwrap().is_none());
        // An empty password.
        let (p, _) = prompt(Person::at_terminal(&[Some("kai")], &[Some("")]));
        assert!(p.ask("ghcr.io", true).unwrap().is_none());
    }

    #[test]
    fn a_login_that_worked_is_kept_unless_the_person_says_no() {
        for (answer, kept) in [(Some("y"), true), (None, true), (Some("N"), false)] {
            let (p, local) = prompt(Person::at_terminal(&[answer], &[]));
            p.accepted("ghcr.io", &kai());
            assert_eq!(
                local.stored("registry:ghcr.io").is_some(),
                kept,
                "{answer:?}"
            );
        }
    }

    #[test]
    fn a_store_that_cannot_keep_it_does_not_fail_the_pull() {
        let local = Arc::new(FakeLocal::new().with_unusable_store());
        let p = TerminalLogin::new(Person::at_terminal(&[Some("y")], &[]), local);
        // The pull has already worked; only a warning is left to say so.
        p.accepted("ghcr.io", &kai());
    }

    #[test]
    fn a_piped_password_loses_its_newline_and_must_be_text() {
        let local = FakeLocal::new();
        keep(local.store(), "ghcr.io", "kai".into(), b"tok\r\n".to_vec()).unwrap();
        assert_eq!(
            local.stored("registry:ghcr.io").as_deref(),
            Some(&b"kai\ntok"[..])
        );
        let err = keep(local.store(), "ghcr.io", "kai".into(), vec![0xff, 0xfe]).unwrap_err();
        assert!(err.to_string().contains("not text"), "{err}");
        let err = keep(local.store(), "ghcr.io", "kai".into(), b"\n".to_vec()).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn logging_out_of_a_registry_with_no_login_says_so() {
        let local = FakeLocal::new();
        let err = drop_login(local.store(), "ghcr.io").unwrap_err();
        assert!(
            err.to_string().contains("no login is kept for ghcr.io"),
            "{err}"
        );
        keep(local.store(), "ghcr.io", "kai".into(), b"tok".to_vec()).unwrap();
        drop_login(local.store(), "ghcr.io").unwrap();
        assert!(local.stored("registry:ghcr.io").is_none());
    }
}
