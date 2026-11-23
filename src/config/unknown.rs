//! What a config or client file holds that this version of lodge does not know: a
//! key it has no use for, a table it dropped, a key that belongs in the other file. They are found by asking the config's own
//! types (which refuse unknown keys), so there is no second list of the known
//! keys to keep in step, and they can be taken out of the text one by one.
//!
//! This is what makes a config that no longer loads mendable: the config screen
//! shows them, and removing one needs no valid config as its result, since it is
//! what stands in the way of one.

use toml_edit::{ArrayOfTables, DocumentMut, ImDocument, Item, Table, Value};

use super::Document;

/// A step from the top of the file to a key.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Key(String),
    /// The `n`th table of an array of tables.
    At(usize),
}

/// One thing in the file that lodge does not know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unknown {
    steps: Vec<Step>,
    /// How the file spells it: `[[config]]`, or `copy_dep` in `[[binary]]`.
    pub label: String,
}

/// More than this is not a config someone mends one key at a time.
const MOST: usize = 200;

/// Everything in `raw`, a text of the kind `file` is, that its types do not know, in
/// the order they are found. Empty when `raw` is not TOML at all, or knows
/// everything in it.
pub fn find(raw: &str, file: &dyn Document) -> Vec<Unknown> {
    let mut text = raw.to_string();
    let mut found = Vec::new();
    while found.len() < MOST {
        let Some(next) = next(&text, file) else { break };
        let Some(without) = without(&text, &next) else {
            break;
        };
        found.push(next);
        text = without;
    }
    found
}

/// `raw` with `which` taken out, or `None` when it is not there (the file changed
/// since it was found).
pub fn remove(raw: &str, which: &Unknown, file: &dyn Document) -> Option<String> {
    find(raw, file)
        .contains(which)
        .then(|| without(raw, which))
        .flatten()
}

/// The first unknown key the types stop at.
fn next(text: &str, file: &dyn Document) -> Option<Unknown> {
    let error = file.unknown_field(text)?;
    if !error.message().starts_with("unknown field `") {
        return None;
    }
    let span = error.span()?;
    let doc = ImDocument::parse(text).ok()?;
    let steps = locate(doc.as_table(), &span, &mut Vec::new())?;
    let label = label(&doc, &steps);
    Some(Unknown { steps, label })
}

/// The steps to the key spelled at `span`, looking in `table` (which is at `at`).
fn locate(table: &Table, span: &std::ops::Range<usize>, at: &mut Vec<Step>) -> Option<Vec<Step>> {
    for (name, item) in table.iter() {
        at.push(Step::Key(name.to_string()));
        if table.key(name).and_then(|k| k.span()).as_ref() == Some(span) {
            return Some(std::mem::take(at));
        }
        if let Some(found) = locate_in(item, span, at) {
            return Some(found);
        }
        at.pop();
    }
    None
}

fn locate_in(item: &Item, span: &std::ops::Range<usize>, at: &mut Vec<Step>) -> Option<Vec<Step>> {
    match item {
        Item::Table(table) => locate(table, span, at),
        Item::ArrayOfTables(list) => list.iter().enumerate().find_map(|(i, table)| {
            at.push(Step::At(i));
            let found = locate(table, span, at);
            if found.is_none() {
                at.pop();
            }
            found
        }),
        Item::Value(Value::InlineTable(table)) => {
            for (name, value) in table.iter() {
                at.push(Step::Key(name.to_string()));
                if table
                    .get_key_value(name)
                    .and_then(|(k, _)| k.span())
                    .as_ref()
                    == Some(span)
                {
                    return Some(std::mem::take(at));
                }
                if let Some(found) = locate_in(&Item::Value(value.clone()), span, at) {
                    return Some(found);
                }
                at.pop();
            }
            None
        }
        _ => None,
    }
}

/// How the file spells what is at `steps`.
fn label(doc: &ImDocument<&str>, steps: &[Step]) -> String {
    let Some((Step::Key(key), parents)) = steps.split_last() else {
        return String::new();
    };
    let mut within = String::new();
    let mut table: &Table = doc.as_table();
    let mut item: Option<&Item> = None;
    for step in parents {
        match (step, item) {
            (Step::Key(name), _) => {
                item = table.get(name);
                if let Some(Item::Table(t)) = item {
                    table = t;
                    within = format!("[{name}]");
                }
            }
            (Step::At(i), Some(Item::ArrayOfTables(list))) => {
                if let Some(t) = list.get(*i) {
                    table = t;
                }
                if let Some(Step::Key(name)) =
                    parents.iter().rev().find(|s| matches!(s, Step::Key(_)))
                {
                    within = format!("[[{name}]]");
                }
            }
            _ => {}
        }
    }
    let what = match table.get(key) {
        Some(Item::ArrayOfTables(_)) => format!("[[{key}]]"),
        Some(Item::Table(_)) => format!("[{key}]"),
        Some(Item::Value(v)) => format!("{key} = {}", v.to_string().trim()),
        _ => key.clone(),
    };
    if within.is_empty() {
        what
    } else {
        format!("{what}  in {within}")
    }
}

/// `text` without what is at `which`.
fn without(text: &str, which: &Unknown) -> Option<String> {
    let mut doc: DocumentMut = text.parse().ok()?;
    let (Step::Key(key), parents) = which.steps.split_last()? else {
        return None;
    };
    let mut table: &mut Table = doc.as_table_mut();
    let mut steps = parents.iter().peekable();
    while let Some(step) = steps.next() {
        let Step::Key(name) = step else { return None };
        let item = table.get_mut(name)?;
        table = match (item, steps.peek()) {
            (Item::Table(t), _) => t,
            (Item::ArrayOfTables(list), Some(Step::At(i))) => {
                let i = *i;
                steps.next();
                list_table(list, i)?
            }
            _ => return None,
        };
    }
    table.remove(key)?;
    Some(doc.to_string())
}

fn list_table(list: &mut ArrayOfTables, i: usize) -> Option<&mut Table> {
    list.get_mut(i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ClientFile, ConfigFile};

    fn config() -> ConfigFile {
        ConfigFile::new("/x/config.toml", crate::files::disk())
    }

    fn client() -> ClientFile {
        ClientFile::new("/x/client.toml", crate::files::disk())
    }

    fn labels(raw: &str) -> Vec<String> {
        find(raw, &config()).into_iter().map(|u| u.label).collect()
    }

    fn client_labels(raw: &str) -> Vec<String> {
        find(raw, &client()).into_iter().map(|u| u.label).collect()
    }

    #[test]
    fn a_config_that_knows_everything_in_it_has_nothing_unknown() {
        assert!(labels("[[binary]]\nname = \"a\"\npath = \"/a\"\n").is_empty());
        assert!(
            client_labels("default_target = \"docker\"\n[[secret]]\nname = \"A\"\nfrom = { env = \"A\" }\nas = \"env\"\n")
                .is_empty()
        );
        assert!(labels("= not toml").is_empty());
        assert!(labels("").is_empty());
    }

    #[test]
    fn a_dropped_table_and_a_misspelled_key_are_each_found_where_they_are() {
        let raw = "bogus = 1\n\n\
                   [[config]]\nname = \"nvim\"\npath = \"~/.config/nvim\"\n\n\
                   [[config]]\nname = \"tmux\"\npath = \"~/.config/tmux\"\n\n\
                   [[binary]]\nname = \"a\"\npath = \"/a\"\ncopy_dep = true\n\n\
                   [session]\nshel = \"/bin/sh\"\n";
        assert_eq!(
            labels(raw),
            [
                "bogus = 1",
                "[[config]]",
                "copy_dep = true  in [[binary]]",
                "shel = \"/bin/sh\"  in [session]",
            ]
        );
    }

    #[test]
    fn a_setting_in_the_file_it_does_not_belong_in_is_unknown_there() {
        // The client's settings written into the config, which does not take them.
        let raw = "default_target = \"docker\"\n[[secret]]\nname = \"A\"\nfrom = { env = \"A\" }\nas = \"env\"\n\
                   [[binary]]\nname = \"a\"\npath = \"/a\"\n";
        assert_eq!(labels(raw), ["default_target = \"docker\"", "[[secret]]"]);
        assert_eq!(
            client_labels("[[binary]]\nname = \"a\"\npath = \"/a\"\n"),
            ["[[binary]]"]
        );
    }

    #[test]
    fn what_is_removed_is_all_that_goes_and_the_rest_of_the_text_stays() {
        let raw = "# mine\n[[config]]\nname = \"x\"\n\n[[binary]]\nname = \"a\"\npath = \"/a\"\n\
                   copy_dep = true # typo\n\n[[binary]]\nname = \"b\"\npath = \"/b\"\nbogus = 2\n";
        let all = find(raw, &config());
        assert_eq!(all.len(), 3);
        // Each goes by itself, the others and the comments left as they were.
        let text = remove(raw, &all[1], &config()).unwrap();
        assert!(
            text.contains("# mine") && text.contains("[[config]]"),
            "{text}"
        );
        assert!(
            !text.contains("copy_dep") && text.contains("bogus = 2"),
            "{text}"
        );
        // All three taken out, the file is a config again.
        let mut text = raw.to_string();
        while let Some(next) = find(&text, &config()).first() {
            text = remove(&text, next, &config()).unwrap();
        }
        assert!(find(&text, &config()).is_empty());
        crate::config::Config::parse(&text, std::path::Path::new("/x/config.toml")).unwrap();
        // What is no longer there cannot be removed.
        assert_eq!(remove(&text, &all[0], &config()), None);
    }

    #[test]
    fn a_table_that_repeats_is_one_unknown_thing_however_many_entries_it_has() {
        // `[[config]]` is not a table of this config: it is one thing that is not
        // known, however many entries it has.
        let raw = "[[config]]\nname = \"nvim\"\npath = \"~/.config/nvim\"\ntarget = \"~/.config/nvim\"\n\n\
                   [[config]]\nname = \"tmux\"\npath = \"~/.config/tmux\"\ntarget = \"~/.config/tmux\"\n\n\
                   [[binary]]\nname = \"nvim\"\npath = \"/usr/local/bin/nvim\"\n";
        let all = find(raw, &config());
        assert_eq!(all.len(), 1);
        let text = remove(raw, &all[0], &config()).unwrap();
        assert_eq!(
            text.trim(),
            "[[binary]]\nname = \"nvim\"\npath = \"/usr/local/bin/nvim\""
        );
        crate::config::Config::parse(&text, std::path::Path::new("/x/config.toml")).unwrap();
    }
}
