//! The config screen, reached with `c` from a bare `ldg`: the whole config file as a
//! form, every value a line that can be changed, programs and data chosen from
//! what this machine has, and the bundle packed or published from it. Each step is
//! a question put through [`Choose`] and [`Ask`], and each change is made by
//! [`crate::config::edit`], so what is written is checked the way `ldg inject`
//! checks it. A mistake is said on the first line and the form comes back; leaving
//! is the only way out.
//!
//! Left out so far: the `[targets.<name>]` tables, whose keys each target defines.

use anyhow::Result;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::add::add_binary;
use super::browse::{self, Browsed, Want};
use super::candidates::{self, Candidate, name_from};
use super::init;
use super::prompt::{
    Adding, Ask, Choice, Choose, Doc, DocLine, Entered, Hotkey, Item, Pause, Pick, Selection,
    TextRow,
};
use super::table::{Cell, Table, Tone};
use crate::bundle::choice::{self, Offer};
use crate::bundle::store::Replace;
use crate::config::Config;
use crate::config::edit::{
    self, Field, Kind, NewEntry, NewSecret, RawEntry, RawSecret, SecretAs, SecretFrom, Setting,
};
use crate::config::unknown::Unknown;
use crate::layout::tilde;
use crate::output::{plural, warning};

const KEYS: &[Hotkey] = &[
    Hotkey {
        key: 'b',
        label: "+binary",
    },
    Hotkey {
        key: 'd',
        label: "+data",
    },
    Hotkey {
        key: 's',
        label: "+secret",
    },
    Hotkey {
        key: 'r',
        label: "remove",
    },
    Hotkey {
        key: 'x',
        label: "clear",
    },
    Hotkey {
        key: 'p',
        label: "pack",
    },
    Hotkey {
        key: 'P',
        label: "publish",
    },
    Hotkey {
        key: 'o',
        label: "open file",
    },
];

/// The keys while choosing which bundle file to pack into.
const PACK_KEYS: &[Hotkey] = &[Hotkey {
    key: 'x',
    label: "delete",
}];

/// Packing a bundle, which needs the helper programs and the registry client: the
/// composition root has them, the screen only asks.
pub trait Bundles {
    /// Packs the config at `config` into `output`: a file, or an `oci://` reference
    /// to publish to (a file named for this machine, here, when none).
    /// Returns what was made, in a line.
    fn pack(&self, config: &Path, output: Option<&str>, replace: Replace) -> Result<String>;
    /// The file a bundle is written to when none is named.
    fn default_file(&self) -> Result<String>;
    /// Whether the bundle file `reference` is there.
    fn exists(&self, reference: &str) -> bool;
    /// Deletes the bundle file `reference`; a registry's bundles are not deleted here.
    fn delete(&self, reference: &str) -> Result<()>;
    /// Whether `reference` is a registry (somewhere to publish to) and not a file.
    fn is_registry(&self, reference: &str) -> bool;
    /// Whether this machine can pack the config, so "the config as it is" is a
    /// bundle to choose.
    fn can_pack(&self) -> bool;
}

/// What the screen works with on this machine.
pub struct Machine<'a> {
    pub home: &'a Path,
    pub path_var: &'a OsStr,
    /// The folder `ldg` was started in: config files are looked for there.
    pub cwd: &'a Path,
    pub bundles: &'a dyn Bundles,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecretField {
    Name,
    From,
    As,
    Targets,
}

impl SecretField {
    const ALL: [SecretField; 4] = [
        SecretField::Name,
        SecretField::From,
        SecretField::As,
        SecretField::Targets,
    ];

    fn key(self) -> &'static str {
        match self {
            SecretField::Name => "name",
            SecretField::From => "from",
            SecretField::As => "as",
            SecretField::Targets => "targets",
        }
    }
}

/// A line of the form that can be selected: which value of what.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Setting(Setting),
    Entry {
        kind: Kind,
        entry: usize,
        field: Field,
    },
    Secret {
        entry: usize,
        field: SecretField,
    },
    /// Something in the file that this version of lodge does not know.
    Unknown(usize),
}

/// What the file holds, as written.
struct File {
    settings: Vec<(Setting, Option<String>)>,
    binaries: Vec<RawEntry>,
    data: Vec<RawEntry>,
    secrets: Vec<RawSecret>,
    /// The bundles listed besides the default, as written.
    bundles: Vec<String>,
    /// What lodge does not know in it, to be shown and taken out.
    unknown: Vec<Unknown>,
    /// Why it does not load, when it does not: the form is shown all the same.
    problem: Option<String>,
}

impl File {
    fn read(path: &Path) -> Result<File> {
        let mut settings = Vec::new();
        for which in Setting::ALL {
            settings.push((which, edit::setting(path, which)?));
        }
        Ok(File {
            settings,
            binaries: edit::list(path, Kind::Binary)?,
            data: edit::list(path, Kind::Data)?,
            secrets: edit::list_secrets(path)?,
            bundles: edit::bundles(path)?,
            unknown: edit::unknown(path)?,
            problem: Config::load(path).err().map(|e| why(&e)),
        })
    }

    fn entries(&self, kind: Kind) -> &[RawEntry] {
        match kind {
            Kind::Binary => &self.binaries,
            Kind::Data => &self.data,
        }
    }

    /// The programs, as `(name, host path)`: what their data is looked for beside.
    fn programs(&self) -> Vec<(String, PathBuf)> {
        self.binaries
            .iter()
            .filter_map(|b| {
                let path = crate::config::expand_tilde(PathBuf::from(b.path.as_deref()?));
                Some((b.name.clone()?, path))
            })
            .collect()
    }

    fn setting(&self, which: Setting) -> Option<&str> {
        self.settings
            .iter()
            .find(|(s, _)| *s == which)
            .and_then(|(_, v)| v.as_deref())
    }
}

pub fn run(
    path: &Path,
    machine: &Machine,
    choose: &dyn Choose,
    ask: &dyn Ask,
    wait: &dyn Pause,
) -> Result<()> {
    let mut path = path.to_path_buf();
    let mut status = String::new();
    loop {
        // A file that is not TOML at all has no form to show; one that does not load
        // as a config does, with what is wrong with it, so it can be mended here.
        let file = match File::read(&path) {
            Ok(file) => file,
            Err(e) => {
                warning!("{e:#}");
                return Ok(());
            }
        };
        let title = match status.as_str() {
            "" => format!("Config {}", path.display()),
            // First, so a long path cannot push it off the line.
            said => format!("{said}   {}", path.display()),
        };
        let (doc, stops) = form(&file);
        let done = match choose.view(&title, &doc, KEYS)? {
            Some(Choice::Left) => return Ok(()),
            Some(choice) => act(
                &mut path,
                choice,
                &stops,
                &file,
                machine,
                choose,
                &Talk { ask, wait },
            ),
            None => match table(&path, &file, machine, choose, ask)? {
                Some(done) => done,
                None => return Ok(()),
            },
        };
        status = match done {
            Ok(said) if said.is_empty() => String::new(),
            Ok(said) => format!("✓ {said}"),
            Err(e) => format!("✗ {}", one_line(&e)),
        };
    }
}

/// What is wrong with a config that does not load, on one line.
fn why(e: &anyhow::Error) -> String {
    e.root_cause()
        .to_string()
        .trim()
        .replace("\n  - ", " · ")
        .trim_start_matches(" · ")
        .to_string()
}

/// What is said of a line that lodge does not know, when it is changed.
const NOT_USED: &str = "this version of lodge does not use it: press r to remove it";

/// What the person is asked in words, and waited for.
struct Talk<'a> {
    ask: &'a dyn Ask,
    wait: &'a dyn Pause,
}

/// What a key or Enter on a line of the form does.
fn act(
    path: &mut PathBuf,
    choice: Choice,
    stops: &[Stop],
    file: &File,
    machine: &Machine,
    choose: &dyn Choose,
    talk: &Talk,
) -> Result<String> {
    let Talk { ask, wait } = *talk;
    let (key, row) = match choice {
        Choice::Key(key, row) => (Some(key), row),
        Choice::Row(row) => (None, row),
        Choice::Left => return Ok(String::new()),
    };
    let stop = stops.get(row).copied();
    match key {
        Some('b') => add(path, Kind::Binary, file, machine, choose, ask),
        Some('d') => add(path, Kind::Data, file, machine, choose, ask),
        Some('s') => add_secret(path, choose, ask),
        Some('p') => pack(path, file, machine, choose, ask, wait),
        Some('P') => publish(path, file, machine, ask, wait),
        Some('o') => open_file(path, machine, choose, ask),
        Some('r') => match stop {
            Some(Stop::Entry { kind, entry, .. }) => {
                let key = file.entries(kind)[entry].key();
                edit::remove(path, kind, key)?;
                Ok(format!("removed {} `{key}`", kind.key()))
            }
            Some(Stop::Secret { entry, .. }) => {
                let name = &file.secrets[entry].name;
                edit::remove_secret(path, name)?;
                Ok(format!("removed secret `{name}`"))
            }
            Some(Stop::Setting(which)) => clear_setting(path, which),
            Some(Stop::Unknown(i)) => {
                let which = &file.unknown[i];
                edit::remove_unknown(path, which)?;
                Ok(format!("removed {}", which.label))
            }
            None => Ok(String::new()),
        },
        Some(_) => match stop {
            Some(Stop::Setting(which)) => clear_setting(path, which),
            Some(Stop::Entry { kind, entry, field }) => {
                clear(path, &file.entries(kind)[entry], field)
            }
            Some(Stop::Secret { entry, field }) => clear_secret(path, &file.secrets[entry], field),
            Some(Stop::Unknown(_)) => anyhow::bail!(NOT_USED),
            None => Ok(String::new()),
        },
        None => match stop {
            Some(Stop::Setting(Setting::Bundle)) => edit_bundle(path, file, machine, choose, ask),
            Some(Stop::Setting(which)) => edit_setting(path, which, file, choose, ask),
            Some(Stop::Entry { kind, entry, field }) => edit_field(
                path,
                &file.entries(kind)[entry],
                field,
                file,
                machine,
                choose,
                ask,
            ),
            Some(Stop::Secret { entry, field }) => {
                edit_secret(path, &file.secrets[entry], field, choose, ask)
            }
            Some(Stop::Unknown(_)) => anyhow::bail!(NOT_USED),
            None => Ok(String::new()),
        },
    }
}

/// Builds the form: its lines, and which of them can be selected.
#[derive(Default)]
struct Builder {
    lines: Vec<DocLine>,
    /// What each selectable line stands for, and where it is in `lines`.
    stops: Vec<Stop>,
    at: Vec<usize>,
}

impl Builder {
    fn text(&mut self, text: impl Into<String>, tone: Tone) {
        self.lines.push(DocLine::new(text, tone));
    }

    fn blank(&mut self) {
        self.text("", Tone::Normal);
    }

    /// A line that can be selected, as it is.
    fn pick(&mut self, stop: Stop, text: impl Into<String>, tone: Tone) {
        self.stops.push(stop);
        self.at.push(self.lines.len());
        self.text(text, tone);
    }

    /// A `key = value` line that can be selected; `unset` says what a missing
    /// value stands for.
    fn value(&mut self, stop: Stop, key: &str, width: usize, value: Option<String>, unset: String) {
        let (text, tone) = match value {
            Some(v) => (v, Tone::Normal),
            None => (unset, Tone::Muted),
        };
        self.stops.push(stop);
        self.at.push(self.lines.len());
        self.text(format!("{key:<width$} = {text}"), tone);
    }
}

/// The file as a form shaped like the TOML: the single values first (`target`,
/// `bundle`, then `[session]` and `[git]`), then each `[[binary]]`, `[[data]]`
/// and `[[secret]]` with all of its fields under it, the ones that are not set
/// shown as such. Every value is a line that can be selected. A kind with no
/// entries shows the key that adds one.
fn form(file: &File) -> (Doc, Vec<Stop>) {
    let mut f = Builder::default();
    let unset = |which: Setting| match which {
        Setting::Target => format!("(default {})", super::targets::default_name()),
        Setting::Bundle => "(not set)".to_string(),
        Setting::Shell => "(default \"/bin/bash\")".to_string(),
        Setting::SafeDirs => "(default [\"*\"])".to_string(),
    };
    let setting = |f: &mut Builder, which: Setting, width: usize| {
        let value = file.setting(which).map(String::from);
        f.value(
            Stop::Setting(which),
            which.key(),
            width,
            value,
            unset(which),
        );
    };
    if let Some(problem) = &file.problem {
        f.text(format!("# does not load: {problem}"), Tone::Accent);
        f.text(
            "# change what is wrong, or take out what is not known (below): each step that leaves fewer problems is kept",
            Tone::Muted,
        );
        f.blank();
    }
    setting(&mut f, Setting::Target, 6);
    setting(&mut f, Setting::Bundle, 6);
    if file.setting(Setting::Bundle).is_some() {
        f.text(
            "# a bundle is used instead of the entries below: pack again to use a change",
            Tone::Muted,
        );
    }
    if !file.bundles.is_empty() {
        f.text(
            format!("# also on offer: {}", file.bundles.join(", ")),
            Tone::Muted,
        );
    }
    for (section, which, width) in [
        ("[session]", Setting::Shell, 5),
        ("[git]", Setting::SafeDirs, 16),
    ] {
        f.blank();
        f.text(section, Tone::Accent);
        setting(&mut f, which, width);
    }

    for kind in [Kind::Binary, Kind::Data] {
        f.blank();
        let list = file.entries(kind);
        if list.is_empty() {
            let hint = match kind {
                Kind::Binary => "press b to add a program",
                Kind::Data => "press d to add data",
            };
            f.text(format!("# [[{}]]  {hint}", kind.key()), Tone::Muted);
        }
        let width = kind
            .fields()
            .iter()
            .map(|k| k.key().len())
            .max()
            .unwrap_or(0);
        for (n, e) in list.iter().enumerate() {
            if n > 0 {
                f.blank();
            }
            f.text(format!("[[{}]]", kind.key()), Tone::Strong);
            for &field in kind.fields() {
                let value = match field {
                    Field::Name => e.name.as_deref().map(quote),
                    Field::Path => e.path.as_deref().map(quote),
                    Field::Target => e.target.as_deref().map(quote),
                    Field::CopyDeps => e.copy_deps.map(|b| b.to_string()),
                    Field::For => e.binary.as_deref().map(quote),
                };
                let stop = Stop::Entry {
                    kind,
                    entry: n,
                    field,
                };
                f.value(stop, field.key(), width, value, unset_field(kind, field));
            }
        }
    }

    f.blank();
    if file.secrets.is_empty() {
        f.text("# [[secret]]  press s to add a secret", Tone::Muted);
    }
    for (n, s) in file.secrets.iter().enumerate() {
        if n > 0 {
            f.blank();
        }
        f.text("[[secret]]", Tone::Strong);
        for field in SecretField::ALL {
            let (value, unset) = match field {
                SecretField::Name => (Some(quote(&s.name)), String::new()),
                SecretField::From => (s.from.clone(), "(not set)".to_string()),
                SecretField::As => (s.as_.clone(), "(not set)".to_string()),
                SecretField::Targets => (s.targets.clone(), "(anywhere)".to_string()),
            };
            f.value(
                Stop::Secret { entry: n, field },
                field.key(),
                7,
                value,
                unset,
            );
        }
    }
    if !file.unknown.is_empty() {
        f.blank();
        f.text(
            "# not known to this version of lodge: r removes",
            Tone::Accent,
        );
        for (n, u) in file.unknown.iter().enumerate() {
            f.pick(Stop::Unknown(n), u.label.clone(), Tone::Normal);
        }
    }
    (
        Doc {
            lines: f.lines,
            stops: f.at,
        },
        f.stops,
    )
}

fn quote(text: &str) -> String {
    format!("\"{text}\"")
}

/// What a field that is not set stands for.
fn unset_field(kind: Kind, field: Field) -> String {
    match (kind, field) {
        (Kind::Data, Field::Target) => "(the same place as here)".into(),
        (Kind::Binary, Field::Target) => "(not set: on PATH)".into(),
        (_, Field::CopyDeps) => "(default true)".into(),
        _ => "(not set)".into(),
    }
}

/// An error as one line, for the status line.
fn one_line(e: &anyhow::Error) -> String {
    format!("{e:#}").replace('\n', "; ")
}

/// Asks for one of a few things, each with what it means.
fn pick_one(
    choose: &dyn Choose,
    question: &str,
    options: &[(&str, &str)],
) -> Result<Option<usize>> {
    let rows = options
        .iter()
        .map(|(name, about)| {
            vec![
                Cell::new(*name, Tone::Strong),
                Cell::new(*about, Tone::Muted),
            ]
        })
        .collect();
    choose.choose(question, &Table::new(vec!["", ""], rows, 1))
}

// ── single values ────────────────────────────────────────────────────────────

fn edit_setting(
    path: &Path,
    which: Setting,
    file: &File,
    choose: &dyn Choose,
    ask: &dyn Ask,
) -> Result<String> {
    let typed = match which {
        Setting::Target => {
            let targets = super::targets::all();
            let options: Vec<(&str, &str)> =
                targets.iter().map(|t| (t.name(), t.about())).collect();
            match pick_one(choose, "Where to work", &options)? {
                Some(i) => options[i].0.to_string(),
                None => return Ok(String::new()),
            }
        }
        // Chosen, not typed, in `edit_bundle`.
        Setting::Bundle => anyhow::bail!("the bundle is chosen with `edit_bundle`"),
        Setting::Shell => match ask.ask("shell in the container")? {
            Some(t) => t,
            None => return Ok(String::new()),
        },
        Setting::SafeDirs => match ask.ask("folders, comma-separated (`none` switches it off)")? {
            Some(t) => t,
            None => return Ok(String::new()),
        },
    };
    let _ = file;
    edit::set_setting(path, which, Some(&typed))?;
    Ok(format!("{} is {typed}", which.key()))
}

/// Chooses the bundle in use: from the default and the others on offer (and the
/// config as it is, where it can be packed), or one typed. With nothing to choose
/// from, it is typed. The bundle that was in use stays on offer.
fn edit_bundle(
    path: &Path,
    file: &File,
    machine: &Machine,
    choose: &dyn Choose,
    ask: &dyn Ask,
) -> Result<String> {
    let current = file
        .setting(Setting::Bundle)
        .map(|b| b.trim_matches('"').to_string())
        .map_or(Offer::Live, Offer::Reference);
    let options = choice::selectable(&current, &file.bundles, machine.bundles.can_pack());
    let typed = || ask.ask("bundle: a file, or oci://… (a registry)");
    let new = if options.len() < 2 {
        match typed()? {
            Some(reference) => Offer::Reference(reference),
            None => return Ok(String::new()),
        }
    } else {
        let mut rows: Vec<(&str, &str)> = options
            .iter()
            .map(|c| (c.label(), if *c == current { "in use" } else { "" }))
            .collect();
        rows.push(("(type one)", "a file, or oci://… (a registry)"));
        match pick_one(choose, "Use which bundle", &rows)? {
            None => return Ok(String::new()),
            Some(i) if i < options.len() => options[i].clone(),
            Some(_) => match typed()? {
                Some(reference) => Offer::Reference(reference),
                None => return Ok(String::new()),
            },
        }
    };
    if new == current {
        return Ok(format!("{} is in use already", new.label()));
    }
    edit::set_setting(path, Setting::Bundle, new.reference())?;
    let kept = choice::after_switch(&file.bundles, &current, &new);
    if kept != file.bundles {
        edit::set_bundles(path, &kept)?;
    }
    Ok(format!("bundle is {}", new.label()))
}

fn clear_setting(path: &Path, which: Setting) -> Result<String> {
    edit::set_setting(path, which, None)?;
    Ok(format!("{} is back to its default", which.key()))
}

// ── binaries and configs ─────────────────────────────────────────────────────

/// Takes a field out of an entry (`copy_deps` goes back to its default).
fn clear(path: &Path, entry: &RawEntry, field: Field) -> Result<String> {
    let key = entry.key();
    match field {
        Field::CopyDeps => edit::set_copy_deps(path, key, true)?,
        _ => edit::set_field(path, entry.kind, key, field, None)?,
    }
    Ok(format!("cleared {} of `{key}`", field.key()))
}

/// Changes one field of an entry: the path is chosen from the machine's own, a
/// name or a target is typed, `for` is chosen among the programs, `copy_deps` is
/// turned over.
fn edit_field(
    path: &Path,
    entry: &RawEntry,
    field: Field,
    file: &File,
    machine: &Machine,
    choose: &dyn Choose,
    ask: &dyn Ask,
) -> Result<String> {
    let (kind, key) = (entry.kind, entry.key());
    match field {
        Field::CopyDeps => {
            let on = !entry.copy_deps.unwrap_or(true);
            edit::set_copy_deps(path, key, on)?;
            Ok(format!("copy_deps of `{key}` is now {on}"))
        }
        Field::Path => {
            let Some(chosen) = pick_path(kind, file, machine, choose, ask)? else {
                return Ok(String::new());
            };
            edit::set_field(path, kind, key, Field::Path, Some(&chosen))?;
            Ok(format!("path of `{key}` is now {chosen}"))
        }
        Field::For => {
            let names: Vec<&str> = file
                .binaries
                .iter()
                .filter_map(|b| b.name.as_deref())
                .collect();
            if names.is_empty() {
                anyhow::bail!("there is no [[binary]] for it to be for: press b to add one");
            }
            let options: Vec<(&str, &str)> = names.iter().map(|n| (*n, "")).collect();
            let Some(i) = pick_one(choose, &format!("`{key}` is for"), &options)? else {
                return Ok(String::new());
            };
            edit::set_field(path, kind, key, Field::For, Some(names[i]))?;
            Ok(format!("`{key}` is for `{}`", names[i]))
        }
        Field::Name | Field::Target => {
            let Some(typed) = ask.ask(&format!("{} of `{key}`", field.key()))? else {
                return Ok(String::new());
            };
            edit::set_field(path, kind, key, field, Some(&typed))?;
            Ok(format!("{} of `{key}` is now {typed}", field.key()))
        }
    }
}

/// What this machine has of `kind`, as items to look through, and the
/// candidates they stand for. What is in the config already is on offer too,
/// shown marked.
fn on_offer(
    kind: Kind,
    file: &File,
    machine: &Machine,
    scope: &browse::Scope,
    added: &HashSet<String>,
) -> (Vec<Candidate>, Vec<Item>) {
    let taken = file.entries(kind);
    let names: HashSet<&str> = taken.iter().filter_map(|e| e.name.as_deref()).collect();
    let paths: HashSet<&str> = taken
        .iter()
        .filter_map(|e| e.path.as_deref())
        .chain(added.iter().map(String::as_str))
        .collect();
    let found = match kind {
        Kind::Binary => {
            let known = init::detect(machine.home, machine.path_var);
            candidates::programs(machine.home, machine.path_var, &known)
        }
        Kind::Data => candidates::data(machine.home, &file.programs()),
    };
    let items = found
        .iter()
        .map(|c| Item {
            added: (kind == Kind::Binary && names.contains(c.name.as_str()))
                || paths.contains(c.entry.path()),
            tab: 0,
            name: c.name.clone(),
            kind: c.kind,
            detail: c.detail.clone(),
        })
        .collect();
    let (mut found, mut items): (Vec<Candidate>, Vec<Item>) = (found, items);
    // Then everything else the scope reaches, in the same list.
    let want = match kind {
        Kind::Binary => Want::File,
        Kind::Data => Want::Any,
    };
    // What the list above has is not offered again.
    let have: HashSet<String> = found.iter().map(|c| c.entry.path().to_string()).collect();
    let more: Vec<PathBuf> = browse::walk(scope, want)
        .into_iter()
        .filter(|p| kind == Kind::Data || crate::platform::is_executable(p))
        .filter(|p| !have.contains(&tilde(p, machine.home)))
        .collect();
    for (path, item) in more
        .iter()
        .zip(browse::items(&more, scope, want, machine.home))
    {
        let place = tilde(path, machine.home);
        items.push(Item {
            added: paths.contains(place.as_str()),
            ..item
        });
        found.push(Candidate {
            name: name_from(path),
            kind: item.kind,
            detail: String::new(),
            entry: match kind {
                Kind::Binary => NewEntry::binary(&name_from(path), place),
                Kind::Data => NewEntry::data(place, None, None),
            },
        });
    }
    (found, items)
}

/// Where the files of this machine are looked for, to begin with: the
/// directories of the programs on `PATH` for a program, `~/.config` for the
/// rest, leaving out credentials and state. It is changed from inside the list.
fn scope(machine: &Machine, kind: Option<Kind>) -> browse::Scope {
    let roots = match kind {
        Some(Kind::Binary) => std::env::split_paths(machine.path_var).collect(),
        _ => vec![machine.home.join(".config")],
    };
    browse::Scope::new(roots, candidates::withheld(machine.home))
}

fn list_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Binary => "Programs",
        Kind::Data => "Data",
    }
}

/// A path typed in full, as the config writes it; a path that is not there is
/// refused.
fn resolve(typed: &str, home: &Path) -> Result<String> {
    let file = crate::config::expand_tilde(PathBuf::from(typed));
    let file = std::path::absolute(&file).unwrap_or(file);
    if !file.exists() {
        anyhow::bail!("{} does not exist", file.display());
    }
    Ok(tilde(&file, home))
}

/// A program or data from what this machine has, or a path typed in full, as the
/// config writes it.
fn pick_path(
    kind: Kind,
    file: &File,
    machine: &Machine,
    choose: &dyn Choose,
    ask: &dyn Ask,
) -> Result<Option<String>> {
    let title = format!("{} (type to filter, or a path)", kind.key());
    let mut scope = scope(machine, Some(kind));
    let chosen = loop {
        let (found, items) = on_offer(kind, file, machine, &scope, &HashSet::new());
        let tabs = [list_name(kind)];
        let knobs = scope.knobs(machine.home);
        match choose.find_knobs(&title, &tabs, &items, &knobs)? {
            Selection::Left => return Ok(None),
            Selection::Edited(knob, text) => scope = scope.edited(knob, &text),
            Selection::Items(chosen) => {
                let Some(c) = chosen.first().and_then(|&i| found.get(i)) else {
                    return Ok(None);
                };
                return Ok(Some(c.entry.path().to_string()));
            }
            Selection::Path(typed) => {
                break browse::settle(choose, kind.key(), &typed, Want::Any, &scope, machine.home)?;
            }
            Selection::Unsupported => break Browsed::Unsupported,
        }
    };
    let Some(typed) = chosen.or_ask(ask, "path on this machine")? else {
        return Ok(None);
    };
    resolve(&typed.to_string_lossy(), machine.home).map(Some)
}

/// Adds entries of `kind`, chosen from what this machine has: each Enter adds the
/// one selected and marks it, and the list stays open for the next.
fn add(
    path: &Path,
    kind: Kind,
    file: &File,
    machine: &Machine,
    choose: &dyn Choose,
    ask: &dyn Ask,
) -> Result<String> {
    let title = format!("Add {} (type to filter, or a path)", kind.key());
    let count = std::cell::Cell::new(0);
    // What was added, so it is shown marked when the list is made again.
    let added = std::cell::RefCell::new(HashSet::new());
    let mut scope = scope(machine, Some(kind));
    loop {
        let (found, items) = on_offer(kind, file, machine, &scope, &added.borrow());
        let mut add_one = |pick: Pick| -> std::result::Result<String, String> {
            let typed;
            let entry = match &pick {
                Pick::Item(i) => &found.get(*i).ok_or("nothing there")?.entry,
                Pick::Path(p) => {
                    typed = entry_for(kind, machine.home, p).map_err(|e| one_line(&e))?;
                    &typed
                }
            };
            let said = place(path, entry, machine.home).map_err(|e| one_line(&e))?;
            count.set(count.get() + 1);
            added.borrow_mut().insert(entry.path().to_string());
            Ok(said)
        };
        let tabs = [list_name(kind)];
        let knobs = scope.knobs(machine.home);
        match choose.find_adding_knobs(&title, &tabs, &items, &knobs, &mut add_one)? {
            Adding::Done => break,
            Adding::Edited(knob, text) => scope = scope.edited(knob, &text),
            Adding::Unsupported => {
                // A terminal that can't show the list: the path is asked for in words.
                let Some(typed) =
                    ask.ask(&format!("path of the {} on this machine", kind.key()))?
                else {
                    return Ok(String::new());
                };
                return place(path, &entry_for(kind, machine.home, &typed)?, machine.home);
            }
        }
    }
    let (one, many) = match kind {
        Kind::Binary => ("binary", "binaries"),
        Kind::Data => ("data entry", "data entries"),
    };
    Ok(match count.get() {
        0 => String::new(),
        n => format!("added {}", plural(n, one, many)),
    })
}

/// Writes `entry` into the config, and says what was added. A program brings what
/// sits beside it as data, as `ldg inject add binary` does.
fn place(path: &Path, entry: &NewEntry, home: &Path) -> Result<String> {
    match entry.kind() {
        Kind::Binary => {
            let data = add_binary(path, entry.clone(), home, true)?;
            Ok(match data.len() {
                0 => format!("added `{}`", entry.key()),
                n => format!("added `{}` and {n} data beside it", entry.key()),
            })
        }
        Kind::Data => {
            edit::add(path, entry)?;
            Ok(format!("added `{}`", entry.key()))
        }
    }
}

/// The entry that adding the path `typed` makes.
fn entry_for(kind: Kind, home: &Path, typed: &str) -> Result<NewEntry> {
    let place = resolve(typed, home)?;
    Ok(match kind {
        Kind::Binary => NewEntry::binary(&name_from(Path::new(typed)), place),
        Kind::Data => NewEntry::data(place, None, None),
    })
}

// ── secrets ──────────────────────────────────────────────────────────────────

const FROM_KINDS: [(&str, &str); 3] = [
    ("env", "an environment variable of the shell lodge runs in"),
    ("file", "a file on this machine"),
    ("command", "a command whose output is the value"),
];

/// Where a secret comes from: the kind chosen, the value typed.
fn ask_from(choose: &dyn Choose, ask: &dyn Ask) -> Result<Option<SecretFrom>> {
    let Some(kind) = pick_one(choose, "Where the value comes from", &FROM_KINDS)? else {
        return Ok(None);
    };
    let what = ["name of the variable", "path of the file", "command"][kind];
    let Some(typed) = ask.ask(what)? else {
        return Ok(None);
    };
    Ok(Some(match kind {
        0 => SecretFrom::Env(typed),
        1 => SecretFrom::File(typed),
        _ => SecretFrom::Command(typed),
    }))
}

/// How a secret is presented in the container: an environment variable, or a file.
fn ask_as(choose: &dyn Choose, ask: &dyn Ask) -> Result<Option<SecretAs>> {
    let options = [
        ("env", "an environment variable named like the secret"),
        ("file", "a file in the container"),
    ];
    match pick_one(choose, "How it appears in the container", &options)? {
        Some(0) => Ok(Some(SecretAs::Env)),
        Some(_) => {
            let Some(file) = ask.ask("path in the container")? else {
                return Ok(None);
            };
            let Some(typed) = ask.ask_or("permissions [0600]", "0600")? else {
                return Ok(None);
            };
            let mode = (typed != "0600").then_some(typed);
            Ok(Some(SecretAs::File { path: file, mode }))
        }
        None => Ok(None),
    }
}

fn add_secret(path: &Path, choose: &dyn Choose, ask: &dyn Ask) -> Result<String> {
    let Some(name) = ask.ask("name of the secret (it is the variable's name too)")? else {
        return Ok(String::new());
    };
    let Some(from) = ask_from(choose, ask)? else {
        return Ok(String::new());
    };
    let new = NewSecret {
        name: name.clone(),
        from,
        as_: SecretAs::Env,
    };
    edit::add_secret(path, &new)?;
    Ok(format!(
        "added secret `{name}` (as env: Enter on `as` to change)"
    ))
}

fn edit_secret(
    path: &Path,
    secret: &RawSecret,
    field: SecretField,
    choose: &dyn Choose,
    ask: &dyn Ask,
) -> Result<String> {
    let name = &secret.name;
    match field {
        SecretField::Name => {
            let Some(typed) = ask.ask(&format!("name of `{name}`"))? else {
                return Ok(String::new());
            };
            edit::rename_secret(path, name, &typed)?;
            Ok(format!("`{name}` is now `{typed}`"))
        }
        SecretField::From => match ask_from(choose, ask)? {
            Some(from) => {
                edit::set_secret_from(path, name, &from)?;
                Ok(format!("`{name}` comes from a {}", from.kind()))
            }
            None => Ok(String::new()),
        },
        SecretField::As => match ask_as(choose, ask)? {
            Some(as_) => {
                edit::set_secret_as(path, name, &as_)?;
                Ok(format!("`{name}` is placed as set"))
            }
            None => Ok(String::new()),
        },
        SecretField::Targets => {
            let Some(typed) = ask.ask("hosts or containers it may go to, comma-separated")? else {
                return Ok(String::new());
            };
            let list: Vec<String> = typed
                .split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
            edit::set_secret_targets(path, name, &list)?;
            Ok(format!("`{name}` may go to {}", list.join(", ")))
        }
    }
}

fn clear_secret(path: &Path, secret: &RawSecret, field: SecretField) -> Result<String> {
    match field {
        SecretField::Targets => {
            edit::set_secret_targets(path, &secret.name, &[])?;
            Ok(format!("`{}` may go anywhere", secret.name))
        }
        _ => anyhow::bail!("a secret needs its {}: change it with Enter", field.key()),
    }
}

// ── the bundle ───────────────────────────────────────────────────────────────

/// Packs the config at `path` into `output`, and says what was made. What went
/// wrong is the error. Warnings were printed on the terminal's main screen, which
/// the form is about to cover, so those (and only those) are waited on.
fn pack_into(
    path: &Path,
    machine: &Machine,
    wait: &dyn Pause,
    output: &str,
    replace: Replace,
) -> Result<String> {
    let warned = crate::output::warnings();
    let made = machine.bundles.pack(path, Some(output), replace);
    if crate::output::warnings() != warned {
        wait.pause();
    }
    made
}

/// What `p` shows: the bundle files there are already, to pack into (replacing
/// one) or delete with `x`, and a row to type the name of a new file on.
fn pack(
    path: &Path,
    file: &File,
    machine: &Machine,
    choose: &dyn Choose,
    ask: &dyn Ask,
    wait: &dyn Pause,
) -> Result<String> {
    let default = machine.bundles.default_file()?;
    let mut existing: Vec<String> = Vec::new();
    let in_use = file
        .setting(Setting::Bundle)
        .map(|b| b.trim_matches('"').to_string());
    let known = in_use.iter().chain(file.bundles.iter());
    for reference in known.chain(machine.bundles.exists(&default).then_some(&default)) {
        if !machine.bundles.is_registry(reference) && !existing.contains(reference) {
            existing.push(reference.clone());
        }
    }
    let mut rows: Vec<Vec<Cell>> = existing
        .iter()
        .map(|e| {
            let about = if in_use.as_deref() == Some(e) {
                "in use · replace it"
            } else {
                "replace it"
            };
            vec![Cell::new(e, Tone::Strong), Cell::new(about, Tone::Muted)]
        })
        .collect();
    let new_row = rows.len();
    rows.push(vec![
        Cell::new("(new file)", Tone::Strong),
        Cell::new(&default, Tone::Muted),
    ]);
    let table = Table::new(vec!["", ""], rows, 1);
    // A new name does not take the place of a file that is there.
    let new_file = |name: &str| pack_into(path, machine, wait, name, Replace::No);
    // What fails stays in the list, in red, with what was typed: it is tried again
    // or left, and the form is not back until then.
    let mut notice: Option<String> = None;
    let mut typed = String::new();
    loop {
        let field = TextRow {
            row: new_row,
            column: 1,
            text: typed.clone(),
            placeholder: default.clone(),
        };
        let entered =
            choose.choose_typing("Pack into", &table, PACK_KEYS, &field, notice.as_deref())?;
        let tried = match entered {
            Entered::Text(name) => {
                let tried = new_file(&name);
                typed = name;
                tried
            }
            Entered::Chose(Choice::Key('x', row)) => match existing.get(row) {
                Some(reference) => delete_bundle(path, file, machine, choose, reference),
                None => return Ok(String::new()),
            },
            Entered::Chose(Choice::Row(row)) => match existing.get(row) {
                Some(reference) => pack_into(path, machine, wait, reference, Replace::Yes),
                // The row to type on, where the terminal can't type on it.
                None => match ask.ask_or(&format!("write the bundle to [{default}]"), &default)? {
                    Some(name) => {
                        let tried = new_file(&name);
                        typed = name;
                        tried
                    }
                    None => return Ok(String::new()),
                },
            },
            Entered::Chose(_) => return Ok(String::new()),
        };
        match tried {
            Ok(said) => return Ok(said),
            Err(e) => notice = Some(one_line(&e)),
        }
    }
}

/// Deletes the bundle file `reference` after the person agrees, and takes it off
/// the list. The one in use is kept: it is changed first.
fn delete_bundle(
    path: &Path,
    file: &File,
    machine: &Machine,
    choose: &dyn Choose,
    reference: &str,
) -> Result<String> {
    let in_use = file.setting(Setting::Bundle).map(|b| b.trim_matches('"'));
    if in_use == Some(reference) {
        anyhow::bail!("{reference} is in use: choose another bundle first");
    }
    let sure = pick_one(
        choose,
        &format!("Delete {reference}"),
        &[("delete", "the file is removed"), ("keep", "")],
    )?;
    if sure != Some(0) {
        return Ok(String::new());
    }
    machine.bundles.delete(reference)?;
    edit::remove_bundle(path, reference)?;
    Ok(format!("deleted {reference}"))
}

/// Publishes the bundle to a registry: the reference the config already names,
/// when it names one, or one typed.
fn publish(
    path: &Path,
    file: &File,
    machine: &Machine,
    ask: &dyn Ask,
    wait: &dyn Pause,
) -> Result<String> {
    let current = file
        .setting(Setting::Bundle)
        .map(|b| b.trim_matches('"').to_string())
        .filter(|b| machine.bundles.is_registry(b));
    let question = match &current {
        Some(c) => format!("publish to [{c}]"),
        None => "publish to (oci://registry/name:tag)".to_string(),
    };
    let typed = match &current {
        Some(c) => ask.ask_or(&question, c)?,
        None => ask.ask(&question)?,
    };
    let Some(reference) = typed else {
        return Ok(String::new());
    };
    if !machine.bundles.is_registry(&reference) {
        anyhow::bail!("`{reference}` is a file, not a registry: use `p` to pack to a file");
    }
    pack_into(path, machine, wait, &reference, Replace::Yes)?;
    Ok(format!("published {reference}"))
}

// ── which file ───────────────────────────────────────────────────────────────

/// The config files that could be meant: the one being edited, the default one,
/// the `.toml` files beside it and in the folder `ldg` was started in, and a
/// `config.toml` in each folder under it.
fn config_files(current: &Path, machine: &Machine) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = vec![current.to_path_buf()];
    let add = |p: PathBuf, found: &mut Vec<PathBuf>| {
        if p.is_file() && !found.contains(&p) {
            found.push(p);
        }
    };
    let mut folders = vec![machine.cwd.to_path_buf()];
    if let Ok(default) = crate::config::default_path() {
        add(default.clone(), &mut found);
        folders.extend(default.parent().map(Path::to_path_buf));
    }
    for folder in &folders {
        let mut files: Vec<PathBuf> = std::fs::read_dir(folder)
            .into_iter()
            .flatten()
            .filter_map(|e| Some(e.ok()?.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        files.sort();
        for p in files {
            add(p, &mut found);
        }
    }
    let mut subfolders: Vec<PathBuf> = std::fs::read_dir(machine.cwd)
        .into_iter()
        .flatten()
        .filter_map(|e| Some(e.ok()?.path()))
        .filter(|p| {
            p.is_dir()
                && !p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        })
        .collect();
    subfolders.sort();
    for folder in subfolders {
        add(folder.join("config.toml"), &mut found);
    }
    found
}

/// Chooses which config file the screen works on.
fn open_file(
    path: &mut PathBuf,
    machine: &Machine,
    choose: &dyn Choose,
    ask: &dyn Ask,
) -> Result<String> {
    let known = config_files(path, machine);
    let mut scope = scope(machine, None);
    let chosen = loop {
        // The config files that are known, then any other file the scope reaches.
        let more: Vec<PathBuf> = browse::walk(&scope, Want::File)
            .into_iter()
            .filter(|f| !known.contains(f))
            .collect();
        let mut items: Vec<Item> = known
            .iter()
            .map(|f| Item {
                added: false,
                tab: 0,
                name: tilde(f, machine.home),
                kind: if f == path { "current" } else { "" },
                detail: String::new(),
            })
            .collect();
        items.extend(browse::items(&more, &scope, Want::File, machine.home));
        let files: Vec<&PathBuf> = known.iter().chain(&more).collect();
        let knobs = scope.knobs(machine.home);
        match choose.find_knobs(
            "Config file (type to filter, or a path)",
            &["Files"],
            &items,
            &knobs,
        )? {
            Selection::Left => return Ok(String::new()),
            Selection::Edited(knob, text) => scope = scope.edited(knob, &text),
            Selection::Items(chosen) => match chosen.first().and_then(|&i| files.get(i)) {
                Some(f) => break (*f).clone(),
                None => return Ok(String::new()),
            },
            Selection::Path(typed) => {
                match browse::settle(
                    choose,
                    "Config file",
                    &typed,
                    Want::File,
                    &scope,
                    machine.home,
                )? {
                    Browsed::Chose(file) => break file,
                    Browsed::Left => return Ok(String::new()),
                    Browsed::Unsupported => match ask.ask("path of the config file")? {
                        Some(typed) => break PathBuf::from(typed),
                        None => return Ok(String::new()),
                    },
                }
            }
            Selection::Unsupported => match ask.ask("path of the config file")? {
                Some(typed) => break PathBuf::from(typed),
                None => return Ok(String::new()),
            },
        }
    };
    // A file that is not there yet is made by the first thing added to it.
    let chosen = crate::config::expand_tilde(chosen);
    *path = std::path::absolute(&chosen).unwrap_or(chosen);
    Ok(format!("editing {}", path.display()))
}

// ── without the form ─────────────────────────────────────────────────────────

/// A row of the table that stands in for the form.
enum Row {
    Entry(Kind, String),
    Unknown(usize),
    Add(Kind),
}

/// The entries and two ways to add one, for a terminal that can't show the form:
/// programs and data only.
fn table(
    path: &Path,
    file: &File,
    machine: &Machine,
    choose: &dyn Choose,
    ask: &dyn Ask,
) -> Result<Option<Result<String>>> {
    let mut rows = Vec::new();
    let mut cells = Vec::new();
    for e in file.binaries.iter().chain(&file.data) {
        cells.push(vec![
            Cell::new(e.kind.key(), Tone::Muted),
            Cell::new(e.name.clone().unwrap_or_default(), Tone::Strong),
            Cell::new(e.path.clone().unwrap_or_default(), Tone::Normal),
        ]);
        rows.push(Row::Entry(e.kind, e.key().to_string()));
    }
    for (n, u) in file.unknown.iter().enumerate() {
        cells.push(vec![
            Cell::new("unknown", Tone::Accent),
            Cell::new("", Tone::Strong),
            Cell::new(u.label.clone(), Tone::Normal),
        ]);
        rows.push(Row::Unknown(n));
    }
    for (kind, text) in [
        (Kind::Binary, "+ add a program"),
        (Kind::Data, "+ add data"),
    ] {
        cells.push(vec![
            Cell::new("", Tone::Muted),
            Cell::new(text, Tone::Accent),
            Cell::new("", Tone::Muted),
        ]);
        rows.push(Row::Add(kind));
    }
    let table = Table::new(vec!["KIND", "NAME", "PATH"], cells, 2);
    let title = format!("Config {}", path.display());
    Ok(match choose.choose_with(&title, &table, &[])? {
        Choice::Row(i) => Some(match rows.get(i) {
            Some(Row::Entry(kind, name)) => {
                let options = [("remove it", ""), ("back", "")];
                if pick_one(choose, name, &options)? == Some(0) {
                    edit::remove(path, *kind, name)
                        .map(|()| format!("removed {} `{name}`", kind.key()))
                } else {
                    Ok(String::new())
                }
            }
            Some(Row::Unknown(i)) => {
                let which = &file.unknown[*i];
                let options = [
                    ("remove it", "this version of lodge does not use it"),
                    ("back", ""),
                ];
                if pick_one(choose, &which.label, &options)? == Some(0) {
                    edit::remove_unknown(path, which).map(|()| format!("removed {}", which.label))
                } else {
                    Ok(String::new())
                }
            }
            Some(Row::Add(kind)) => add(path, *kind, file, machine, choose, ask),
            None => Ok(String::new()),
        }),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmpfile::TempDir;
    use std::cell::RefCell;

    /// Packs nothing, and remembers what it was asked to.
    #[derive(Default)]
    struct Packs {
        asked: RefCell<Vec<(PathBuf, Option<String>)>>,
        deleted: RefCell<Vec<String>>,
        replaced: RefCell<Vec<Replace>>,
        fail: bool,
        can_pack: bool,
    }
    impl Bundles for Packs {
        fn pack(&self, config: &Path, output: Option<&str>, replace: Replace) -> Result<String> {
            self.replaced.borrow_mut().push(replace);
            self.asked
                .borrow_mut()
                .push((config.to_path_buf(), output.map(String::from)));
            if self.fail {
                anyhow::bail!("ldd is missing");
            }
            Ok(format!("packed into {}", output.unwrap_or_default()))
        }
        fn default_file(&self) -> Result<String> {
            Ok("lodge-x86_64.tar".into())
        }
        fn exists(&self, _: &str) -> bool {
            false
        }
        fn delete(&self, reference: &str) -> Result<()> {
            self.deleted.borrow_mut().push(reference.to_string());
            Ok(())
        }
        fn is_registry(&self, reference: &str) -> bool {
            reference.contains("://")
        }
        fn can_pack(&self) -> bool {
            self.can_pack
        }
    }

    /// Shows the form: records what it was shown, answers from scripts.
    #[derive(Default)]
    struct Screen {
        views: RefCell<Vec<Choice>>,
        picks: RefCell<Vec<Option<usize>>>,
        /// For each question with hotkeys, the key pressed on the row picked.
        keys: RefCell<Vec<char>>,
        /// What is typed on the row of a list that can be typed on.
        typed: RefCell<Vec<String>>,
        /// The notice each question with a row to type on was given.
        notices: RefCell<Vec<Option<String>>>,
        finds: RefCell<Vec<Selection>>,
        adds: RefCell<Vec<Vec<Pick>>>,
        results: RefCell<Vec<Result<String, String>>>,
        shown: RefCell<Vec<(String, Vec<String>)>>,
        offered: RefCell<Vec<String>>,
        /// Whether the form can be shown at all.
        no_form: bool,
    }
    impl Choose for Screen {
        fn choose(&self, _: &str, _: &Table) -> Result<Option<usize>> {
            Ok(self.picks.borrow_mut().remove(0))
        }
        fn choose_with(&self, _: &str, _: &Table, _: &[Hotkey]) -> Result<Choice> {
            Ok(match self.picks.borrow_mut().remove(0) {
                Some(i) if !self.keys.borrow().is_empty() => {
                    Choice::Key(self.keys.borrow_mut().remove(0), i)
                }
                Some(i) => Choice::Row(i),
                None => Choice::Left,
            })
        }
        fn choose_typing(
            &self,
            q: &str,
            t: &Table,
            k: &[Hotkey],
            _: &TextRow,
            notice: Option<&str>,
        ) -> Result<Entered> {
            self.notices.borrow_mut().push(notice.map(String::from));
            if self.typed.borrow().is_empty() {
                return self.choose_with(q, t, k).map(Entered::Chose);
            }
            Ok(Entered::Text(self.typed.borrow_mut().remove(0)))
        }
        fn view(&self, title: &str, doc: &Doc, keys: &[Hotkey]) -> Result<Option<Choice>> {
            assert_eq!(keys, KEYS);
            if self.no_form {
                return Ok(None);
            }
            self.shown.borrow_mut().push((
                title.to_string(),
                doc.lines.iter().map(|l| l.text.clone()).collect(),
            ));
            Ok(Some(self.views.borrow_mut().remove(0)))
        }
        fn find(&self, title: &str, _: &[&str], items: &[Item]) -> Result<Selection> {
            self.offered.borrow_mut().push(format!(
                "{title}: {}",
                items
                    .iter()
                    .map(|i| format!("{}{}", i.name, if i.added { "*" } else { "" }))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            Ok(self.finds.borrow_mut().remove(0))
        }
        fn find_adding(
            &self,
            title: &str,
            _: &[&str],
            items: &[Item],
            add: &mut dyn FnMut(Pick) -> Result<String, String>,
        ) -> Result<bool> {
            self.offered.borrow_mut().push(format!(
                "{title}: {}",
                items
                    .iter()
                    .map(|i| format!("{}{}", i.name, if i.added { "*" } else { "" }))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            for pick in self.adds.borrow_mut().remove(0) {
                self.results.borrow_mut().push(add(pick));
            }
            Ok(true)
        }
    }

    struct NoWait;
    impl Pause for NoWait {
        fn pause(&self) {}
    }

    struct Typed(RefCell<Vec<Option<&'static str>>>);
    /// What is typed to say that Esc was pressed, not Enter.
    const ESC: &str = "\u{1b}";

    impl Ask for Typed {
        fn ask(&self, _: &str) -> Result<Option<String>> {
            let mut left = self.0.borrow_mut();
            Ok(if left.is_empty() {
                None
            } else {
                left.remove(0).filter(|t| *t != ESC).map(Into::into)
            })
        }

        fn ask_or(&self, _: &str, default: &str) -> Result<Option<String>> {
            let mut left = self.0.borrow_mut();
            Ok(
                match if left.is_empty() {
                    None
                } else {
                    left.remove(0)
                } {
                    Some(ESC) => None,
                    Some(typed) => Some(typed.into()),
                    None => Some(default.into()),
                },
            )
        }
    }

    struct Run {
        config: String,
        screen: Screen,
        packs: Packs,
        dir: TempDir,
    }

    fn drive(text: &str, screen: Screen, typed: Vec<Option<&'static str>>) -> Run {
        drive_with(text, screen, typed, Packs::default())
    }

    fn drive_with(
        text: &str,
        screen: Screen,
        typed: Vec<Option<&'static str>>,
        packs: Packs,
    ) -> Run {
        let dir = TempDir::new("configure").unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(".bashrc"), "x").unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, text).unwrap();
        let machine = Machine {
            home: &home,
            path_var: OsStr::new(""),
            cwd: dir.path(),
            bundles: &packs,
        };
        run(
            &config,
            &machine,
            &screen,
            &Typed(RefCell::new(typed)),
            &NoWait,
        )
        .unwrap();
        Run {
            config: std::fs::read_to_string(&config).unwrap(),
            screen,
            packs,
            dir,
        }
    }

    fn views(choices: Vec<Choice>) -> Screen {
        Screen {
            views: RefCell::new(choices),
            ..Screen::default()
        }
    }

    const FILE: &str = "# keep\ntarget = \"podman\"\n\n[session]\nshell = \"/bin/zsh\"\n\n[[binary]]\nname = \"a\"\npath = \"/bin/sh\"\n\n[[binary]]\nname = \"b\"\npath = \"/bin/ls\"\n\n[[data]]\npath = \"/etc/hostname\"\n\n[[secret]]\nname = \"TOKEN\"\nfrom = { env = \"HOST_TOKEN\" }\nas = \"env\"\n";

    // Stops of FILE: 0 target, 1 bundle, 2 shell, 3 safe_directories; binary a
    // 4-7, binary b 8-11 (name, path, target, copy_deps); data 12-14 (path,
    // target, for); secret 15-18 (name, from, as, targets).

    #[test]
    fn the_form_is_the_toml_with_every_value_and_says_what_is_not_set() {
        let r = drive(FILE, views(vec![Choice::Left]), vec![]);
        let (title, text) = &r.screen.shown.borrow()[0].clone();
        assert!(
            title.starts_with("Config ") && title.ends_with("config.toml"),
            "{title}"
        );
        let text: Vec<&str> = text.iter().map(String::as_str).collect();
        assert_eq!(
            text[..13],
            [
                "target = \"podman\"",
                "bundle = (not set)",
                "",
                "[session]",
                "shell = \"/bin/zsh\"",
                "",
                "[git]",
                "safe_directories = (default [\"*\"])",
                "",
                "[[binary]]",
                "name      = \"a\"",
                "path      = \"/bin/sh\"",
                "target    = (not set: on PATH)",
            ]
        );
        assert!(
            text.contains(&"target = (the same place as here)"),
            "{text:?}"
        );
        assert!(
            text.contains(&"from    = { env = \"HOST_TOKEN\" }"),
            "{text:?}"
        );
        assert!(text.contains(&"targets = (anywhere)"), "{text:?}");
        // Nothing yet: the keys that add one are named.
        let r = drive("", views(vec![Choice::Left]), vec![]);
        let text = r.screen.shown.borrow()[0].1.join("\n");
        for hint in [
            "press b to add a program",
            "press d to add data",
            "press s to add a secret",
        ] {
            assert!(text.contains(hint), "{text}");
        }
    }

    #[test]
    fn a_bundle_in_the_config_is_said_to_replace_the_entries() {
        let r = drive(
            "bundle = \"oci://r/x:1\"\n",
            views(vec![Choice::Left]),
            vec![],
        );
        let text = r.screen.shown.borrow()[0].1.join("\n");
        assert!(
            text.contains("a bundle is used instead of the entries"),
            "{text}"
        );
    }

    const WITH_BUNDLES: &str = "bundle = \"a.tar\"\nbundles = [\"b.tar\"]\n";

    #[test]
    fn the_bundle_in_use_is_chosen_from_those_on_offer_and_the_old_one_stays_on_offer() {
        let screen = Screen {
            views: RefCell::new(vec![Choice::Row(1), Choice::Left]),
            picks: RefCell::new(vec![Some(1)]),
            ..Screen::default()
        };
        let r = drive(WITH_BUNDLES, screen, vec![]);
        assert!(r.config.contains("bundle = \"b.tar\""), "{}", r.config);
        assert!(r.config.contains("bundles = ["), "{}", r.config);
        assert_eq!(
            edit::bundles(&r.dir.path().join("config.toml")).unwrap(),
            ["a.tar"]
        );
        // The form says what else is on offer.
        let shown = r.screen.shown.borrow();
        assert!(
            shown[0].1.iter().any(|l| l == "# also on offer: b.tar"),
            "{:?}",
            shown[0].1
        );
    }

    #[test]
    fn a_bundle_can_still_be_typed_when_others_are_on_offer() {
        let screen = Screen {
            views: RefCell::new(vec![Choice::Row(1), Choice::Left]),
            // a.tar, b.tar, then "(type one)".
            picks: RefCell::new(vec![Some(2)]),
            ..Screen::default()
        };
        let r = drive(WITH_BUNDLES, screen, vec![Some("c.tar")]);
        assert!(r.config.contains("bundle = \"c.tar\""), "{}", r.config);
        assert_eq!(
            edit::bundles(&r.dir.path().join("config.toml")).unwrap(),
            ["b.tar", "a.tar"]
        );
    }

    #[test]
    fn the_config_as_it_is_is_a_choice_where_it_can_be_packed() {
        let screen = Screen {
            views: RefCell::new(vec![Choice::Row(1), Choice::Left]),
            // a.tar, b.tar, the config, "(type one)".
            picks: RefCell::new(vec![Some(2)]),
            ..Screen::default()
        };
        let packs = Packs {
            can_pack: true,
            ..Packs::default()
        };
        let r = drive_with(WITH_BUNDLES, screen, vec![], packs);
        assert!(!r.config.contains("bundle = "), "{}", r.config);
        assert_eq!(
            edit::bundles(&r.dir.path().join("config.toml")).unwrap(),
            ["b.tar", "a.tar"]
        );
    }

    #[test]
    fn enter_changes_the_value_x_clears_it_r_removes_the_entry() {
        let screen = Screen {
            views: RefCell::new(vec![
                Choice::Row(2),      // shell: typed
                Choice::Row(3),      // safe_directories: typed
                Choice::Row(0),      // target: chosen from the list (the third)
                Choice::Row(1),      // bundle: typed
                Choice::Key('x', 2), // shell: back to the default
                Choice::Row(6),      // target of binary a: typed
                Choice::Row(7),      // copy_deps of a: turned off
                Choice::Key('x', 6), // target of a: cleared
                Choice::Key('r', 9), // an entry of b: removed
                Choice::Left,
            ]),
            picks: RefCell::new(vec![Some(2)]),
            ..Screen::default()
        };
        let r = drive(
            FILE,
            screen,
            vec![
                Some("/bin/fish"),
                Some("/work, /srv"),
                Some("deploy/bundle.tar"),
                Some("~/.local/bin/a"),
            ],
        );
        assert!(r.config.contains("# keep"), "{}", r.config);
        assert!(r.config.contains("target = \"host\""), "{}", r.config);
        assert!(
            r.config.contains("bundle = \"deploy/bundle.tar\""),
            "{}",
            r.config
        );
        assert!(
            r.config
                .contains("safe_directories = [\"/work\", \"/srv\"]"),
            "{}",
            r.config
        );
        assert!(
            !r.config.contains("shell"),
            "back to its default: {}",
            r.config
        );
        assert!(
            r.config
                .contains("name = \"a\"\npath = \"/bin/sh\"\ncopy_deps = false"),
            "{}",
            r.config
        );
        assert!(!r.config.contains("name = \"b\""), "{}", r.config);
        assert!(
            r.config.contains("[[data]]\npath = \"/etc/hostname\""),
            "{}",
            r.config
        );
        // What each did is said on the first line of the next form.
        let titles: Vec<String> = r
            .screen
            .shown
            .borrow()
            .iter()
            .map(|s| s.0.clone())
            .collect();
        assert!(
            titles[1].ends_with("✓ shell is \"/bin/fish\"")
                || titles[1].contains("✓ shell is /bin/fish"),
            "{titles:?}"
        );
        assert!(
            titles[5].contains("✓ shell is back to its default"),
            "{titles:?}"
        );
    }

    #[test]
    fn b_and_d_add_one_enter_at_a_time_and_what_is_there_already_is_marked() {
        let screen = Screen {
            views: RefCell::new(vec![Choice::Key('d', 0), Choice::Key('b', 0), Choice::Left]),
            adds: RefCell::new(vec![
                vec![
                    Pick::Item(0),
                    Pick::Item(0),
                    Pick::Path("/no/such/path".into()),
                ],
                vec![Pick::Path("/bin/true".into())],
            ]),
            ..Screen::default()
        };
        let r = drive("[[data]]\npath = \"/etc/hostname\"\n", screen, vec![]);
        let offered = r.screen.offered.borrow();
        assert!(
            offered[0].starts_with("Add data") && offered[0].ends_with(": bashrc"),
            "{offered:?}"
        );
        assert!(offered[1].starts_with("Add binary"), "{offered:?}");
        let results = r.screen.results.borrow();
        assert_eq!(results[0], Ok("added `~/.bashrc`".to_string()));
        assert!(
            results[1].as_ref().unwrap_err().contains("already in"),
            "{results:?}"
        );
        assert!(
            results[2].as_ref().unwrap_err().contains("does not exist"),
            "{results:?}"
        );
        assert!(
            r.config.contains("[[data]]\npath = \"~/.bashrc\"\n"),
            "{}",
            r.config
        );
        assert!(
            r.config
                .contains("[[binary]]\nname = \"true\"\npath = \"/bin/true\""),
            "{}",
            r.config
        );
        // The form after each says how many were added.
        let titles: Vec<String> = r
            .screen
            .shown
            .borrow()
            .iter()
            .map(|s| s.0.clone())
            .collect();
        assert!(titles[1].contains("✓ added 1 data entry"), "{titles:?}");
    }

    #[test]
    fn what_the_config_has_already_is_marked_in_the_list() {
        let screen = Screen {
            views: RefCell::new(vec![Choice::Key('d', 0), Choice::Left]),
            adds: RefCell::new(vec![vec![]]),
            ..Screen::default()
        };
        let r = drive("[[data]]\npath = \"~/.bashrc\"\n", screen, vec![]);
        assert!(
            r.screen.offered.borrow()[0].ends_with(": bashrc*"),
            "{:?}",
            r.screen.offered
        );
    }

    #[test]
    fn secrets_are_added_from_a_kind_and_a_value_then_changed_field_by_field() {
        // FILE's secret TOKEN is stops 15-18 (name, from, as, targets); the one
        // added comes after it, at 19-22.
        let screen = Screen {
            views: RefCell::new(vec![
                Choice::Key('s', 0),  // add: name, kind (a command), value
                Choice::Row(21),      // as: a file in the container, and its mode
                Choice::Row(22),      // targets
                Choice::Row(16),      // from of TOKEN: an environment variable
                Choice::Key('x', 22), // targets cleared
                Choice::Key('x', 20), // from can't be cleared
                Choice::Key('r', 15), // TOKEN removed
                Choice::Left,
            ]),
            picks: RefCell::new(vec![Some(2), Some(1), Some(0)]),
            ..Screen::default()
        };
        let r = drive(
            FILE,
            screen,
            vec![
                Some("GH_TOKEN"),
                Some("gh auth token"),
                Some("/run/gh"),
                Some("0400"),
                Some("me@box, dev-*"),
                Some("GITHUB_TOKEN"),
            ],
        );
        assert!(
            r.config.contains("from = { command = \"gh auth token\" }"),
            "{}",
            r.config
        );
        assert!(
            r.config
                .contains("as = { file = \"/run/gh\", mode = \"0400\" }"),
            "{}",
            r.config
        );
        assert!(!r.config.contains("targets"), "cleared: {}", r.config);
        assert!(
            !r.config.contains("name = \"TOKEN\""),
            "removed: {}",
            r.config
        );
        let titles: Vec<String> = r
            .screen
            .shown
            .borrow()
            .iter()
            .map(|s| s.0.clone())
            .collect();
        assert!(
            titles[3].contains("✓ `GH_TOKEN` may go to me@box, dev-*"),
            "{titles:?}"
        );
        assert!(
            titles[6].contains("✗ a secret needs its from"),
            "{titles:?}"
        );
    }

    #[test]
    fn p_packs_into_a_file_and_a_failure_is_shown_and_said() {
        // With no bundle yet the one row is the new file, which the terminal
        // cannot type on here, so it is asked for in words.
        let screen = Screen {
            picks: RefCell::new(vec![Some(0), Some(0)]),
            ..views(vec![Choice::Key('p', 0), Choice::Key('p', 0), Choice::Left])
        };
        let r = drive(FILE, screen, vec![None, Some("out/mine.tar")]);
        let asked = r.packs.asked.borrow();
        assert_eq!(
            asked[0].1.as_deref(),
            Some("lodge-x86_64.tar"),
            "Enter takes the default"
        );
        assert_eq!(asked[1].1.as_deref(), Some("out/mine.tar"));
        assert_eq!(asked[0].0, r.dir.path().join("config.toml"));
        let titles: Vec<String> = r
            .screen
            .shown
            .borrow()
            .iter()
            .map(|s| s.0.clone())
            .collect();
        assert!(
            titles[1].contains("✓ packed into lodge-x86_64.tar"),
            "{titles:?}"
        );
        let failing = Packs {
            fail: true,
            ..Packs::default()
        };
        // The name is asked for in words here; the list is left after the failure.
        let screen = Screen {
            picks: RefCell::new(vec![Some(0), None]),
            ..views(vec![Choice::Key('p', 0), Choice::Left])
        };
        let r = drive_with(FILE, screen, vec![Some("x.tar")], failing);
        // The list stays, asked again with why, and the form has nothing to say.
        let notices = r.screen.notices.borrow();
        assert_eq!(*notices, [None, Some("ldd is missing".to_string())]);
        let shown = titles_of(&r);
        assert!(!shown[1].contains('✗'), "{shown:?}");
    }

    fn titles_of(r: &Run) -> Vec<String> {
        r.screen
            .shown
            .borrow()
            .iter()
            .map(|s| s.0.clone())
            .collect()
    }

    #[test]
    fn p_shows_the_bundles_there_are_and_replaces_the_one_chosen() {
        let screen = Screen {
            views: RefCell::new(vec![Choice::Key('p', 0), Choice::Left]),
            picks: RefCell::new(vec![Some(1)]),
            ..Screen::default()
        };
        let r = drive(WITH_BUNDLES, screen, vec![]);
        assert_eq!(r.packs.asked.borrow()[0].1.as_deref(), Some("b.tar"));
        assert!(r.packs.replaced.borrow()[0] == Replace::Yes);
    }

    #[test]
    fn a_new_file_is_never_written_over_what_is_there() {
        let screen = Screen {
            views: RefCell::new(vec![Choice::Key('p', 0), Choice::Left]),
            picks: RefCell::new(vec![Some(2)]),
            ..Screen::default()
        };
        let r = drive(WITH_BUNDLES, screen, vec![Some("c.tar")]);
        assert_eq!(r.packs.asked.borrow()[0].1.as_deref(), Some("c.tar"));
        assert!(r.packs.replaced.borrow()[0] == Replace::No);
    }

    #[test]
    fn x_deletes_a_bundle_that_is_not_in_use_once_agreed_and_takes_it_off_the_list() {
        let screen = Screen {
            views: RefCell::new(vec![Choice::Key('p', 0), Choice::Left]),
            // The row of b.tar, then "delete" at the question.
            picks: RefCell::new(vec![Some(1), Some(0)]),
            keys: RefCell::new(vec!['x']),
            ..Screen::default()
        };
        let r = drive(WITH_BUNDLES, screen, vec![]);
        assert_eq!(*r.packs.deleted.borrow(), ["b.tar"]);
        assert!(r.packs.asked.borrow().is_empty());
        assert!(
            edit::bundles(&r.dir.path().join("config.toml"))
                .unwrap()
                .is_empty()
        );
        assert!(
            titles_of(&r)[1].contains("✓ deleted b.tar"),
            "{:?}",
            titles_of(&r)
        );
    }

    #[test]
    fn declining_keeps_the_bundle_and_the_one_in_use_cannot_be_deleted() {
        let screen = Screen {
            views: RefCell::new(vec![Choice::Key('p', 0), Choice::Left]),
            picks: RefCell::new(vec![Some(1), Some(1)]),
            keys: RefCell::new(vec!['x']),
            ..Screen::default()
        };
        let r = drive(WITH_BUNDLES, screen, vec![]);
        assert!(r.packs.deleted.borrow().is_empty());
        assert_eq!(
            edit::bundles(&r.dir.path().join("config.toml")).unwrap(),
            ["b.tar"]
        );
        let screen = Screen {
            views: RefCell::new(vec![Choice::Key('p', 0), Choice::Left]),
            // The row of a.tar, which is in use; then the list is left.
            picks: RefCell::new(vec![Some(0), None]),
            keys: RefCell::new(vec!['x']),
            ..Screen::default()
        };
        let r = drive(WITH_BUNDLES, screen, vec![]);
        assert!(r.packs.deleted.borrow().is_empty());
        // It stays in the list, which says why.
        assert_eq!(
            r.screen.notices.borrow()[1].as_deref(),
            Some("a.tar is in use: choose another bundle first")
        );
    }

    #[test]
    fn a_name_typed_in_the_list_is_a_new_file_and_what_was_made_is_on_the_status_line() {
        let screen = Screen {
            typed: RefCell::new(vec!["mine.tar".into()]),
            ..views(vec![Choice::Key('p', 0), Choice::Left])
        };
        let r = drive(WITH_BUNDLES, screen, vec![]);
        assert_eq!(r.packs.asked.borrow()[0].1.as_deref(), Some("mine.tar"));
        assert!(r.packs.replaced.borrow()[0] == Replace::No);
        assert!(
            titles_of(&r)[1].contains("✓ packed into mine.tar"),
            "{:?}",
            titles_of(&r)
        );
    }

    #[test]
    fn esc_at_a_question_with_a_default_backs_out_where_enter_takes_the_default() {
        let leave = || Screen {
            views: RefCell::new(vec![Choice::Key('p', 0), Choice::Key('P', 0), Choice::Left]),
            picks: RefCell::new(vec![Some(0)]),
            ..Screen::default()
        };
        let r = drive(FILE, leave(), vec![Some(ESC), Some(ESC)]);
        assert!(r.packs.asked.borrow().is_empty(), "nothing was packed");
        let r = drive(FILE, leave(), vec![None, Some(ESC)]);
        assert_eq!(r.packs.asked.borrow().len(), 1, "Enter took the default");
    }

    #[test]
    fn shift_p_publishes_to_the_reference_in_the_config_or_one_typed_and_only_to_a_registry() {
        let r = drive(
            "bundle = \"oci://ghcr.io/me/lodge:1\"\n[[binary]]\nname = \"a\"\npath = \"/bin/sh\"\n",
            views(vec![Choice::Key('P', 0), Choice::Key('P', 0), Choice::Left]),
            vec![None, Some("oci://other/x:2")],
        );
        let asked = r.packs.asked.borrow();
        assert_eq!(
            asked[0].1.as_deref(),
            Some("oci://ghcr.io/me/lodge:1"),
            "the config's own"
        );
        assert_eq!(asked[1].1.as_deref(), Some("oci://other/x:2"));
        // Not a registry: refused before anything is packed. With nothing named
        // and nothing typed, nothing happens.
        let r = drive(
            FILE,
            views(vec![Choice::Key('P', 0), Choice::Key('P', 0), Choice::Left]),
            vec![Some("lodge.tar"), None],
        );
        assert!(r.packs.asked.borrow().is_empty());
        let titles: Vec<String> = r
            .screen
            .shown
            .borrow()
            .iter()
            .map(|s| s.0.clone())
            .collect();
        assert!(
            titles[1].contains("✗ `lodge.tar` is a file, not a registry"),
            "{titles:?}"
        );
    }

    #[test]
    fn o_opens_another_config_file_and_the_form_is_then_that_files() {
        let dir = TempDir::new("configure-open").unwrap();
        let other = dir.path().join("deploy.toml");
        std::fs::write(&other, "[[binary]]\nname = \"other\"\npath = \"/bin/ls\"\n").unwrap();
        let screen = Screen {
            views: RefCell::new(vec![Choice::Key('o', 0), Choice::Left]),
            finds: RefCell::new(vec![Selection::Path(other.display().to_string())]),
            ..Screen::default()
        };
        let r = drive(FILE, screen, vec![]);
        let shown = r.screen.shown.borrow();
        assert!(shown[0].1.contains(&"name      = \"a\"".to_string()));
        assert!(shown[1].0.contains("deploy.toml"), "{}", shown[1].0);
        assert!(
            shown[1].1.contains(&"name      = \"other\"".to_string()),
            "{:?}",
            shown[1].1
        );
        // The first thing offered is the file being edited.
        assert!(
            r.screen.offered.borrow()[0].contains("config.toml"),
            "{:?}",
            r.screen.offered
        );
    }

    #[test]
    fn a_config_that_does_not_load_is_shown_with_why_and_what_it_does_not_know() {
        // The stops are the four settings, the four of binary a, then what lodge does not know.
        let r = drive(
            "# mine\ntarget = \"podman\"\n\n[[inject]]\nname = \"x\"\n\n[[binary]]\nname = \"a\"\npath = \"/bin/sh\"\ncopy_dep = true\n",
            views(vec![Choice::Row(8), Choice::Key('x', 9), Choice::Left]),
            vec![],
        );
        let shown = r.screen.shown.borrow();
        let text = shown[0].1.join("\n");
        assert!(
            text.contains("# does not load: unknown field `inject`"),
            "{text}"
        );
        assert!(
            text.contains("# not known to this version of lodge"),
            "{text}"
        );
        assert!(text.contains("[[inject]]\n"), "{text}");
        assert!(text.contains("copy_dep = true  in [[binary]]"), "{text}");
        // Changing one says what to do, and changes nothing.
        assert!(
            shown[1]
                .0
                .contains("✗ this version of lodge does not use it"),
            "{:?}",
            shown[1].0
        );
        assert!(
            shown[2]
                .0
                .contains("✗ this version of lodge does not use it"),
            "{:?}",
            shown[2].0
        );
        assert!(r.config.contains("[[inject]]") && r.config.contains("copy_dep"));
    }

    #[test]
    fn what_a_config_does_not_know_is_removed_with_r_until_it_loads_again() {
        let r = drive(
            "# mine\ntarget = \"podman\"\n\n[[inject]]\nname = \"x\"\n\n[[binary]]\nname = \"a\"\npath = \"/bin/sh\"\ncopy_dep = true\n",
            views(vec![Choice::Key('r', 8), Choice::Key('r', 8), Choice::Left]),
            vec![],
        );
        assert!(
            r.config.starts_with("# mine\ntarget = \"podman\""),
            "{}",
            r.config
        );
        assert!(
            !r.config.contains("inject") && !r.config.contains("copy_dep"),
            "{}",
            r.config
        );
        assert!(
            r.config.contains("[[binary]]\nname = \"a\""),
            "{}",
            r.config
        );
        let shown = r.screen.shown.borrow();
        assert!(
            shown[1].0.contains("✓ removed [[inject]]"),
            "{:?}",
            shown[1].0
        );
        assert!(
            shown[2]
                .0
                .contains("✓ removed copy_dep = true  in [[binary]]"),
            "{:?}",
            shown[2].0
        );
        // Once it loads, the banner and the group are gone.
        let last = shown[2].1.join("\n");
        assert!(
            !last.contains("does not load") && !last.contains("not known"),
            "{last}"
        );
        Config::load(&r.dir.path().join("config.toml")).unwrap();
    }

    #[test]
    fn a_file_that_is_not_toml_has_no_form_and_is_said() {
        let r = drive("= not toml", views(vec![]), vec![]);
        assert!(r.screen.shown.borrow().is_empty());
    }

    #[test]
    fn without_the_form_a_table_lists_programs_and_data_and_two_rows_add() {
        // Rows: a, b, the data, add a program, add data. Remove `b`, add data by typing.
        let screen = Screen {
            no_form: true,
            picks: RefCell::new(vec![Some(1), Some(0), Some(3), None]),
            adds: RefCell::new(vec![]),
            ..Screen::default()
        };
        // (The terminal that has no form has no list to add from either, so the
        // path is asked for in words.)
        struct Words(Screen);
        impl Choose for Words {
            fn choose(&self, q: &str, t: &Table) -> Result<Option<usize>> {
                self.0.choose(q, t)
            }
            fn choose_with(&self, q: &str, t: &Table, k: &[Hotkey]) -> Result<Choice> {
                self.0.choose_with(q, t, k)
            }
            fn view(&self, _: &str, _: &Doc, _: &[Hotkey]) -> Result<Option<Choice>> {
                Ok(None)
            }
        }
        let dir = TempDir::new("configure-table").unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, FILE).unwrap();
        let machine = Machine {
            home: dir.path(),
            path_var: OsStr::new(""),
            cwd: dir.path(),
            bundles: &Packs::default(),
        };
        let typed = Typed(RefCell::new(vec![Some("/bin/true")]));
        run(&config, &machine, &Words(screen), &typed, &NoWait).unwrap();
        let now = std::fs::read_to_string(&config).unwrap();
        assert!(!now.contains("name = \"b\""), "{now}");
        assert!(now.contains("[[data]]\npath = \"/bin/true\""), "{now}");
    }
}
