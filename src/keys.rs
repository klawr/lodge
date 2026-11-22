//! Which key does what in the interactive screens, in one place.
//!
//! A screen names what it can do as an [`Action`]; a [`Keymap`] says which keys
//! ask for it. The defaults are here, `[keys]` in the config replaces them one
//! action at a time, and every hint line is written from the map, so what the
//! screen says is what the keys do. This module knows nothing of the terminal:
//! the picker turns what the terminal sent into a [`Key`], and asks.
//!
//! There are two contexts. In a [`Context::List`] (a list, or a page of text)
//! every key is a command. In a [`Context::Search`] (a search, or a line being
//! typed) a plain character is text, so a binding to one is not used there, and
//! the commands are keys with Ctrl or Alt, or the named keys.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

/// The key itself, apart from its modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Enter,
    Esc,
    Tab,
    Backspace,
}

/// A key as pressed. Shift is not a modifier here: it is in the character
/// (`G` is not `g`), and the named keys have no shifted form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub code: Code,
    pub ctrl: bool,
    pub alt: bool,
}

impl Key {
    /// With Ctrl or Alt, a letter is the same key in either case.
    pub fn new(code: Code, ctrl: bool, alt: bool) -> Self {
        let code = match code {
            Code::Char(c) if ctrl || alt => Code::Char(c.to_ascii_lowercase()),
            code => code,
        };
        Key { code, ctrl, alt }
    }

    /// Whether, typing, it would be text.
    fn is_text(self) -> bool {
        matches!(self.code, Code::Char(_)) && !self.ctrl && !self.alt
    }
}

const NAMES: &[(&str, Code)] = &[
    ("up", Code::Up),
    ("down", Code::Down),
    ("left", Code::Left),
    ("right", Code::Right),
    ("home", Code::Home),
    ("end", Code::End),
    ("enter", Code::Enter),
    ("esc", Code::Esc),
    ("tab", Code::Tab),
    ("backspace", Code::Backspace),
    ("space", Code::Char(' ')),
];

impl FromStr for Key {
    type Err = String;

    /// `j`, `G`, `esc`, `ctrl-n`, `alt-x`: a character or a name, after any
    /// `ctrl-` and `alt-`.
    fn from_str(text: &str) -> Result<Self, String> {
        let (mut ctrl, mut alt) = (false, false);
        let mut rest = text;
        loop {
            if let Some(r) = rest.strip_prefix("ctrl-") {
                (ctrl, rest) = (true, r);
            } else if let Some(r) = rest.strip_prefix("alt-") {
                (alt, rest) = (true, r);
            } else {
                break;
            }
        }
        let mut chars = rest.chars();
        let code = match (chars.next(), chars.next()) {
            (None, _) => return Err(format!("`{text}` names no key")),
            (Some(c), None) => Code::Char(c),
            _ => NAMES
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(rest))
                .map(|(_, code)| *code)
                .ok_or_else(|| {
                    let names: Vec<&str> = NAMES.iter().map(|(n, _)| *n).collect();
                    format!(
                        "`{text}` is not a key: a character, or one of {}, after any `ctrl-` or `alt-`",
                        names.join(" ")
                    )
                })?,
        };
        Ok(Key::new(code, ctrl, alt))
    }
}

/// As a hint shows it: `↑`, `Enter`, `^N`, `j`.
impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.alt {
            f.write_str("alt-")?;
        }
        match self.code {
            Code::Char(c) if self.ctrl => write!(f, "^{}", c.to_ascii_uppercase()),
            Code::Char(' ') => f.write_str("space"),
            Code::Char(c) => write!(f, "{c}"),
            Code::Up => f.write_str("↑"),
            Code::Down => f.write_str("↓"),
            Code::Left => f.write_str("←"),
            Code::Right => f.write_str("→"),
            Code::Home => f.write_str("Home"),
            Code::End => f.write_str("End"),
            Code::Enter => f.write_str("Enter"),
            Code::Esc => f.write_str("Esc"),
            Code::Tab => f.write_str("Tab"),
            Code::Backspace => f.write_str("Backspace"),
        }
    }
}

/// Something a key can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    // Moving over a list, and answering it.
    Up,
    Down,
    First,
    Last,
    Choose,
    Leave,
    // A search.
    Mark,
    PrevList,
    NextList,
    ClearLine,
    LookWhere,
    Skip,
    Everywhere,
    // The config screen, and the list of bundles to pack into.
    AddBinary,
    AddData,
    AddSecret,
    Remove,
    Pack,
    Publish,
    OpenFile,
    // The places.
    New,
    Config,
    Reinject,
    Recreate,
}

/// Where a key is read, see the module doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    List,
    Search,
}

impl Action {
    pub const ALL: &[Action] = &[
        Action::Up,
        Action::Down,
        Action::First,
        Action::Last,
        Action::Choose,
        Action::Leave,
        Action::Mark,
        Action::PrevList,
        Action::NextList,
        Action::ClearLine,
        Action::LookWhere,
        Action::Skip,
        Action::Everywhere,
        Action::AddBinary,
        Action::AddData,
        Action::AddSecret,
        Action::Remove,
        Action::Pack,
        Action::Publish,
        Action::OpenFile,
        Action::New,
        Action::Config,
        Action::Reinject,
        Action::Recreate,
    ];

    /// How the config spells it.
    pub fn name(self) -> &'static str {
        match self {
            Action::Up => "up",
            Action::Down => "down",
            Action::First => "first",
            Action::Last => "last",
            Action::Choose => "choose",
            Action::Leave => "leave",
            Action::Mark => "mark",
            Action::PrevList => "prev_list",
            Action::NextList => "next_list",
            Action::ClearLine => "clear_line",
            Action::LookWhere => "look_where",
            Action::Skip => "skip",
            Action::Everywhere => "everywhere",
            Action::AddBinary => "add_binary",
            Action::AddData => "add_data",
            Action::AddSecret => "add_secret",
            Action::Remove => "remove",
            Action::Pack => "pack",
            Action::Publish => "publish",
            Action::OpenFile => "open_file",
            Action::New => "new",
            Action::Config => "config",
            Action::Reinject => "reinject",
            Action::Recreate => "recreate",
        }
    }

    /// The keys it has until the config says otherwise. Each key means one thing
    /// everywhere: no two actions share one, whatever the screen.
    fn defaults(self) -> &'static [&'static str] {
        match self {
            Action::Up => &["up", "k", "ctrl-p"],
            Action::Down => &["down", "j", "ctrl-n"],
            Action::First => &["home", "g"],
            Action::Last => &["end", "G"],
            Action::Choose => &["enter"],
            Action::Leave => &["esc", "q", "ctrl-c", "ctrl-d"],
            Action::Mark => &["tab"],
            Action::PrevList => &["left"],
            Action::NextList => &["right"],
            Action::ClearLine => &["ctrl-u"],
            Action::LookWhere => &["ctrl-w"],
            Action::Skip => &["ctrl-b"],
            Action::Everywhere => &["ctrl-a"],
            Action::AddBinary => &["b"],
            Action::AddData => &["d"],
            Action::AddSecret => &["s"],
            Action::Remove => &["x"],
            Action::Pack => &["p"],
            Action::Publish => &["P"],
            Action::OpenFile => &["o"],
            Action::New => &["n"],
            Action::Config => &["c"],
            Action::Reinject => &["i"],
            Action::Recreate => &["r"],
        }
    }

    fn is_in(self, context: Context) -> bool {
        match self {
            Action::Up | Action::Down | Action::Choose | Action::Leave => true,
            Action::Mark
            | Action::PrevList
            | Action::NextList
            | Action::ClearLine
            | Action::LookWhere
            | Action::Skip
            | Action::Everywhere => context == Context::Search,
            _ => context == Context::List,
        }
    }
}

/// What `[keys]` says for one action: a key, or several.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize, schemars::JsonSchema))]
#[serde(untagged)]
pub enum KeyList {
    One(String),
    Many(Vec<String>),
}

impl KeyList {
    fn texts(&self) -> &[String] {
        match self {
            KeyList::One(one) => std::slice::from_ref(one),
            KeyList::Many(many) => many,
        }
    }
}

/// The key of each action.
#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: BTreeMap<Action, Vec<Key>>,
}

impl Default for Keymap {
    fn default() -> Self {
        let bindings = Action::ALL
            .iter()
            .map(|&action| {
                // The defaults are tested to parse.
                let keys = action
                    .defaults()
                    .iter()
                    .filter_map(|text| text.parse().ok())
                    .collect();
                (action, keys)
            })
            .collect();
        Keymap { bindings }
    }
}

impl Keymap {
    /// The defaults, with each action in `settings` given the keys it says (a
    /// list replaces the defaults; an empty one unbinds). Every problem is
    /// reported, not the first.
    pub fn build(settings: &BTreeMap<String, KeyList>) -> Result<Self, Vec<String>> {
        let mut map = Keymap::default();
        let mut problems = Vec::new();
        for (name, list) in settings {
            let Some(&action) = Action::ALL.iter().find(|a| a.name() == name) else {
                let names: Vec<&str> = Action::ALL.iter().map(|a| a.name()).collect();
                problems.push(format!(
                    "[keys] `{name}` is not something a key does; the actions are {}",
                    names.join(", ")
                ));
                continue;
            };
            let mut keys = Vec::new();
            for text in list.texts() {
                match text.parse() {
                    Ok(key) => keys.push(key),
                    Err(why) => problems.push(format!("[keys] `{name}`: {why}")),
                }
            }
            map.bindings.insert(action, keys);
        }
        problems.extend(map.problems());
        if problems.is_empty() {
            Ok(map)
        } else {
            Err(problems)
        }
    }

    /// What is wrong with the map as it stands: an answer that can't be given, a
    /// key that means two things in one place, a digit that is spoken for.
    fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for action in [Action::Choose, Action::Leave] {
            if self.keys(action).is_empty() {
                problems.push(format!(
                    "[keys] `{}` needs a key, or there is no way to answer a list",
                    action.name()
                ));
            }
        }
        for context in [Context::List, Context::Search] {
            let mut seen: BTreeMap<String, Vec<Action>> = BTreeMap::new();
            for (&action, keys) in &self.bindings {
                if !action.is_in(context) {
                    continue;
                }
                for key in keys {
                    if context == Context::Search && key.is_text() {
                        continue;
                    }
                    if context == Context::List
                        && !key.ctrl
                        && !key.alt
                        && matches!(key.code, Code::Char('1'..='9'))
                    {
                        problems.push(format!(
                            "[keys] `{}`: {key} jumps to a row and can't be bound",
                            action.name()
                        ));
                    }
                    let acts = seen.entry(key.to_string()).or_default();
                    if !acts.contains(&action) {
                        acts.push(action);
                    }
                }
            }
            for (key, acts) in seen.into_iter().filter(|(_, a)| a.len() > 1) {
                let names: Vec<&str> = acts.iter().map(|a| a.name()).collect();
                problems.push(format!(
                    "[keys] {key} is bound to {}, which are read in the same place",
                    names.join(" and ")
                ));
            }
        }
        problems
    }

    pub fn keys(&self, action: Action) -> &[Key] {
        self.bindings.get(&action).map_or(&[], Vec::as_slice)
    }

    /// What `key` asks for in `context`, if anything.
    pub fn action(&self, context: Context, key: Key) -> Option<Action> {
        if context == Context::Search && key.is_text() {
            return None;
        }
        self.bindings
            .iter()
            .find(|(action, keys)| action.is_in(context) && keys.contains(&key))
            .map(|(action, _)| *action)
    }

    /// How a hint names the action's key: the first it has, or `-` for none.
    pub fn label(&self, action: Action) -> String {
        self.keys(action)
            .first()
            .map_or_else(|| "-".to_string(), ToString::to_string)
    }

    /// Two actions that go together, as one hint: `↑/↓ k/j`, up to two keys each.
    pub fn pair(&self, first: Action, second: Action) -> String {
        let (a, b) = (self.keys(first), self.keys(second));
        let both: Vec<String> = a
            .iter()
            .zip(b)
            .take(2)
            .map(|(a, b)| format!("{a}/{b}"))
            .collect();
        if both.is_empty() {
            format!("{}/{}", self.label(first), self.label(second))
        } else {
            both.join(" ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(text: &str) -> Key {
        text.parse().unwrap()
    }

    fn settings(pairs: &[(&str, &[&str])]) -> BTreeMap<String, KeyList> {
        pairs
            .iter()
            .map(|(name, keys)| {
                (
                    name.to_string(),
                    KeyList::Many(keys.iter().map(ToString::to_string).collect()),
                )
            })
            .collect()
    }

    #[test]
    fn keys_are_a_character_or_a_name_after_modifiers() {
        assert_eq!(key("j"), Key::new(Code::Char('j'), false, false));
        assert_eq!(key("G"), Key::new(Code::Char('G'), false, false));
        assert_eq!(key("Esc"), Key::new(Code::Esc, false, false));
        assert_eq!(key("ctrl-N"), key("ctrl-n"));
        assert_eq!(key("ctrl-alt-x"), Key::new(Code::Char('x'), true, true));
        assert_eq!(key("ctrl--"), Key::new(Code::Char('-'), true, false));
        assert_eq!(key("space"), Key::new(Code::Char(' '), false, false));
        assert!("".parse::<Key>().is_err());
        assert!("ctrl-".parse::<Key>().is_err());
        assert!("escape".parse::<Key>().unwrap_err().contains("not a key"));
    }

    #[test]
    fn hints_name_keys_the_way_people_read_them() {
        let shown = |t: &str| key(t).to_string();
        assert_eq!(shown("up"), "↑");
        assert_eq!(shown("ctrl-n"), "^N");
        assert_eq!(shown("enter"), "Enter");
        assert_eq!(shown("G"), "G");
        assert_eq!(shown("alt-x"), "alt-x");
    }

    #[test]
    fn the_defaults_parse_and_no_two_actions_share_a_key() {
        for action in Action::ALL {
            for text in action.defaults() {
                assert!(text.parse::<Key>().is_ok(), "{}: {text}", action.name());
            }
        }
        // Built through `build`, so the same check that guards the config runs.
        assert_eq!(Keymap::build(&BTreeMap::new()).map(|_| ()), Ok(()));
        // And the strict form: no key is used by two actions anywhere.
        let mut all: Vec<String> = Keymap::default()
            .bindings
            .into_values()
            .flatten()
            .map(|k| k.to_string())
            .collect();
        let n = all.len();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), n);
    }

    #[test]
    fn a_plain_character_is_text_in_a_search_and_a_command_in_a_list() {
        let map = Keymap::default();
        assert_eq!(map.action(Context::List, key("q")), Some(Action::Leave));
        assert_eq!(map.action(Context::Search, key("q")), None);
        assert_eq!(map.action(Context::Search, key("esc")), Some(Action::Leave));
        assert_eq!(
            map.action(Context::Search, key("ctrl-w")),
            Some(Action::LookWhere)
        );
        assert_eq!(map.action(Context::List, key("ctrl-w")), None);
        assert_eq!(map.action(Context::List, key("b")), Some(Action::AddBinary));
    }

    #[test]
    fn a_setting_replaces_the_keys_of_its_action_and_none_unbinds() {
        let map = Keymap::build(&settings(&[("leave", &["ctrl-x"]), ("first", &[])])).unwrap();
        assert_eq!(map.action(Context::List, key("q")), None);
        assert_eq!(
            map.action(Context::List, key("ctrl-x")),
            Some(Action::Leave)
        );
        assert_eq!(map.action(Context::List, key("g")), None);
        assert_eq!(map.label(Action::First), "-");
        assert_eq!(map.label(Action::Leave), "^X");
    }

    #[test]
    fn a_single_key_may_be_written_without_a_list() {
        let one: BTreeMap<String, KeyList> =
            toml::from_str("pack = \"ctrl-k\"\nleave = [\"q\"]").unwrap();
        let map = Keymap::build(&one).unwrap();
        assert_eq!(map.action(Context::List, key("ctrl-k")), Some(Action::Pack));
    }

    #[test]
    fn every_problem_is_reported() {
        let problems = Keymap::build(&settings(&[
            ("lieve", &["q"]),
            ("up", &["bogus"]),
            ("pack", &["x"]),
            ("choose", &[]),
            ("add_data", &["3"]),
        ]))
        .unwrap_err();
        let all = problems.join("\n");
        assert!(all.contains("`lieve` is not something a key does"), "{all}");
        assert!(all.contains("`up`: `bogus` is not a key"), "{all}");
        assert!(all.contains("x is bound to remove and pack"), "{all}");
        assert!(all.contains("`choose` needs a key"), "{all}");
        assert!(all.contains("3 jumps to a row"), "{all}");
    }

    #[test]
    fn a_key_two_screens_use_for_different_things_is_still_one_key() {
        // Moving a screen's key onto another's is a clash even if they are never
        // on the screen together: one key, one meaning.
        let problems = Keymap::build(&settings(&[("reinject", &["b"])])).unwrap_err();
        assert!(
            problems[0].contains("add_binary and reinject"),
            "{problems:?}"
        );
    }

    #[test]
    fn plain_characters_bound_for_a_search_do_not_clash_there() {
        // `mark` on `x` is text while searching, so it is not a clash with `clear`,
        // which is a list's; the two are never read in the same place.
        let map = Keymap::build(&settings(&[("mark", &["x"])])).unwrap();
        assert_eq!(map.action(Context::Search, key("x")), None);
    }

    #[test]
    fn pairs_read_as_one_hint() {
        let map = Keymap::default();
        assert_eq!(map.pair(Action::Up, Action::Down), "↑/↓ k/j");
        let map = Keymap::build(&settings(&[("up", &[]), ("down", &["j"])])).unwrap();
        assert_eq!(map.pair(Action::Up, Action::Down), "-/j");
    }
}
