//! A list to choose from with the keyboard: arrows or vim keys, drawn on stderr.
//!
//! Three parts, so the one that needs a terminal is the smallest. [`map_key`]
//! turns what the terminal sent into a [`Key`], [`Cursor`] decides what a key
//! does to the selection, and [`frame`] draws it as text; those are plain
//! functions and tested as such. [`pick`] is the loop around them: raw mode, the
//! events, the redraw. It is the only place that names `crossterm`.
//!
//! The list is drawn on the terminal's alternate screen, from its top left corner
//! each time. Nothing is counted from where the cursor happens to be, so a
//! resized window, which re-wraps whatever is on the main screen, cannot leave
//! half a list behind, and the screen the person came from is back as it was
//! when the pick is over.

use super::table::{Cell, Table, Tone};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, queue};
use std::io::{self, Write};

/// A key of its own that a list answers to, with what it does for the hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub key: char,
    pub label: &'static str,
}

/// What a keypress asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Up,
    Down,
    First,
    Last,
    /// Move to the nth entry, counting from 1.
    Jump(usize),
    Choose,
    Leave,
}

/// The key bindings: the arrows, and vim's (`j` `k` `g` `G`) with the readline
/// ones (`ctrl-n`, `ctrl-p`) that go with them.
fn map_key(event: KeyEvent) -> Option<Key> {
    if event.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
    Some(match event.code {
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Home => Key::First,
        KeyCode::End => Key::Last,
        KeyCode::Enter => Key::Choose,
        KeyCode::Esc => Key::Leave,
        KeyCode::Char('c' | 'd') if ctrl => Key::Leave,
        KeyCode::Char('p') if ctrl => Key::Up,
        KeyCode::Char('n') if ctrl => Key::Down,
        KeyCode::Char('k') => Key::Up,
        KeyCode::Char('j') => Key::Down,
        KeyCode::Char('g') => Key::First,
        KeyCode::Char('G') => Key::Last,
        KeyCode::Char('q') => Key::Leave,
        KeyCode::Char(c @ '1'..='9') => Key::Jump(c as usize - '0' as usize),
        _ => return None,
    })
}

/// What a key did.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    Stay,
    Chose(usize),
    Left,
}

/// Which entry is selected, and which is the first one shown when the list is
/// taller than the terminal.
struct Cursor {
    at: usize,
    top: usize,
    len: usize,
}

impl Cursor {
    fn new(len: usize) -> Self {
        Cursor { at: 0, top: 0, len }
    }

    /// Moves the selection one way or the other, round the ends.
    fn apply(&mut self, key: Key) -> Step {
        match key {
            Key::Up => self.at = (self.at + self.len - 1) % self.len,
            Key::Down => self.at = (self.at + 1) % self.len,
            Key::First => self.at = 0,
            Key::Last => self.at = self.len - 1,
            Key::Jump(n) if n <= self.len => self.at = n - 1,
            Key::Jump(_) => {}
            Key::Choose => return Step::Chose(self.at),
            Key::Leave => return Step::Left,
        }
        Step::Stay
    }

    /// Scrolls just far enough for the selection to be among `rows` rows.
    fn fit(&mut self, rows: usize) {
        if self.at < self.top {
            self.top = self.at;
        } else if self.at >= self.top + rows {
            self.top = self.at + 1 - rows;
        }
    }
}

/// A stretch of a line and how much it matters.
struct Span {
    text: String,
    tone: Tone,
}

impl Span {
    fn new(text: impl Into<String>, tone: Tone) -> Self {
        Span {
            text: text.into(),
            tone,
        }
    }
}

/// One line of the screen, and whether it is the one selected.
struct Line {
    spans: Vec<Span>,
    selected: bool,
}

impl Line {
    fn of(text: impl Into<String>, tone: Tone) -> Self {
        Line {
            spans: vec![Span::new(text, tone)],
            selected: false,
        }
    }

    /// A line of a table: the cells with a gap between and a space at each end.
    fn row(cells: impl Iterator<Item = Span>, selected: bool) -> Self {
        let mut spans = vec![Span::new(" ", Tone::Normal)];
        for (i, cell) in cells.enumerate() {
            if i > 0 {
                spans.push(Span::new("  ", Tone::Normal));
            }
            spans.push(cell);
        }
        spans.push(Span::new(" ", Tone::Normal));
        Line { spans, selected }
    }

    fn width(&self) -> usize {
        self.spans.iter().map(|s| s.text.chars().count()).sum()
    }

    #[cfg(test)]
    fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    /// The line, cut to `width` characters with `…` where it was cut.
    fn clipped(mut self, width: usize) -> Self {
        if self.width() <= width {
            return self;
        }
        let mut room = width.saturating_sub(1);
        let mut kept = Vec::new();
        for span in self.spans.drain(..) {
            let length = span.text.chars().count();
            if length <= room {
                room -= length;
                kept.push(span);
            } else {
                let tone = span.tone;
                kept.push(Span::new(
                    span.text.chars().take(room).collect::<String>(),
                    tone,
                ));
                break;
            }
        }
        kept.push(Span::new("…", Tone::Normal));
        Line {
            spans: kept,
            selected: self.selected,
        }
    }
}

/// What there is room for: the screen has some air round the list when the
/// terminal allows, and none as it gets smaller, so a small window is not given
/// less list for the sake of spacing.
#[derive(Debug, PartialEq, Eq)]
struct Plan {
    /// An empty line after the question and before the hints.
    section_gaps: bool,
    /// How many entries are shown at once; fewer than there are only when even
    /// the plainest screen has no room for all of them.
    rows: usize,
}

impl Plan {
    /// The lines `entries` entries take with this spacing, the question, the
    /// titles and the hints included.
    fn height(section_gaps: bool, entries: usize) -> usize {
        let gaps = if section_gaps { 2 } else { 0 };
        1 + 1 + entries + gaps + 1
    }
}

/// The airy screen if it shows every entry on a screen of `height` lines and
/// leaves one free (a line feed on the last would scroll it), else the plain one,
/// which scrolls when even it has no room for them all.
fn plan(height: usize, entries: usize) -> Plan {
    if Plan::height(true, entries) < height {
        return Plan {
            section_gaps: true,
            rows: entries,
        };
    }
    Plan {
        section_gaps: false,
        rows: height.saturating_sub(4).clamp(1, entries),
    }
}

/// The screen as `plan` says: the question, the titles, the entries from `top` on
/// laid out for `width`, and a line of hints with where the selection is in the
/// list. Nothing is wider than `width`, so a line never wraps.
fn frame(
    question: &str,
    options: &Table,
    cursor: &Cursor,
    plan: &Plan,
    width: usize,
    hotkeys: &[Hotkey],
) -> Vec<Line> {
    // A space at each end of a row.
    let grid = options.grid(width.saturating_sub(2));
    let blank = || Line::of("", Tone::Normal);
    let mut lines = Vec::new();
    lines.push(Line::of(format!(" {question}"), Tone::Strong));
    if plan.section_gaps {
        lines.push(blank());
    }
    lines.push(Line::row(
        grid.header.iter().map(|t| Span::new(t, Tone::Muted)),
        false,
    ));
    let shown = grid
        .rows
        .iter()
        .enumerate()
        .skip(cursor.top)
        .take(plan.rows);
    for (i, cells) in shown {
        let cells = cells.iter().map(|c| Span::new(&c.text, c.tone));
        lines.push(Line::row(cells, i == cursor.at));
    }
    if plan.section_gaps {
        lines.push(blank());
    }
    lines.push(hints(cursor, width, hotkeys));
    lines.into_iter().map(|l| l.clipped(width)).collect()
}

/// What the keys do, and where the selection is, at the far end when there is room.
fn hints(cursor: &Cursor, width: usize, hotkeys: &[Hotkey]) -> Line {
    let mut keys = String::from(" ↑/↓ or j/k move · Enter open");
    for h in hotkeys {
        keys.push_str(&format!(" · {} {}", h.key, h.label));
    }
    keys.push_str(" · q quit");
    let place = format!("{}/{} ", cursor.at + 1, cursor.len);
    let room = width.saturating_sub(keys.chars().count() + place.chars().count());
    let gap = if room > 0 {
        " ".repeat(room)
    } else {
        String::new()
    };
    Line::of(format!("{keys}{gap}{place}"), Tone::Muted)
}

/// The size of the terminal, or 80 by 24 when it reports none (some do, and a
/// width of nothing would cut every line down to a `…`).
fn usable_size((columns, rows): (u16, u16)) -> (u16, u16) {
    (
        if columns == 0 { 80 } else { columns },
        if rows == 0 { 24 } else { rows },
    )
}

/// How a pick ended.
pub enum Picked {
    Chose(usize),
    /// What was typed on the row that is a text field.
    Typed(String),
    /// A hotkey was pressed, with the row that was selected.
    Key(char, usize),
    Left,
    /// The terminal can't be taken over for single keys; ask some other way.
    NoRawMode,
}

/// Puts the terminal back as it was when it goes out of scope, whichever way
/// the pick ends.
struct RawMode;

impl RawMode {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut err = io::stderr();
        if let Err(e) = queue!(err, EnterAlternateScreen, cursor::Hide).and_then(|()| err.flush()) {
            // Nobody was told about raw mode, so nothing else will end it.
            terminal::disable_raw_mode()?;
            return Err(e);
        }
        Ok(RawMode)
    }
}

impl Drop for RawMode {
    // Leaving: there is no one to tell if the terminal can't be restored.
    #[allow(clippy::let_underscore_must_use)]
    fn drop(&mut self) {
        let mut err = io::stderr();
        let _ = queue!(err, cursor::Show, LeaveAlternateScreen).and_then(|()| err.flush());
        let _ = terminal::disable_raw_mode();
    }
}

/// A row of the list that is a place to type: the text shows in one of its columns.
#[derive(Debug, Clone)]
pub struct TextRow {
    pub row: usize,
    pub column: usize,
    /// What is in it at first, typed already (what was tried and failed, say).
    pub text: String,
    /// What is taken when nothing is typed: shown dimmed until something is.
    pub placeholder: String,
}

/// Shows `options` and lets the person move over them and choose one.
pub fn pick(question: &str, options: &Table, hotkeys: &[Hotkey]) -> io::Result<Picked> {
    pick_in(question, options, hotkeys, None, None)
}

/// [`pick`], where one row is a place to type: on it the keys write into it
/// (hotkeys included), Enter answers with the text, and Esc steps off it. A
/// `notice` is what went wrong the last time, shown in red.
pub fn pick_typing(
    question: &str,
    options: &Table,
    hotkeys: &[Hotkey],
    field: &TextRow,
    notice: Option<&str>,
) -> io::Result<Picked> {
    pick_in(question, options, hotkeys, Some(field), notice)
}

/// Puts `notice` on the screen `lines`, in red: where the empty line before the
/// hints is, or, on a screen with none, over the question.
fn show_notice(lines: &mut [Line], plan: &Plan, notice: &str, width: usize) {
    let line = Line::of(format!(" ✗ {notice}"), Tone::Error).clipped(width);
    let at = if plan.section_gaps {
        lines.len().saturating_sub(2)
    } else {
        0
    };
    if let Some(slot) = lines.get_mut(at) {
        *slot = line;
    }
}

/// What a key does while the selection is on the text row.
#[derive(Debug, PartialEq, Eq)]
enum Typing {
    /// Nothing for the list: the text changed, or the key means nothing here.
    Stay,
    Move(Key),
    /// Enter, with something to answer.
    Accept,
}

/// What a key does to `text`, or to the selection, when the selection is on the
/// text row. Enter answers when there is text, or a placeholder to take instead.
/// Esc goes up a row: off the text, and from the only row, out.
fn type_key(text: &mut String, placeholder: &str, key: KeyEvent, rows: usize) -> Typing {
    match map_find_key(key) {
        Some(FKey::Up) => Typing::Move(Key::Up),
        Some(FKey::Down) => Typing::Move(Key::Down),
        Some(FKey::Accept) if text.trim().is_empty() && placeholder.is_empty() => Typing::Stay,
        Some(FKey::Accept) => Typing::Accept,
        Some(FKey::Leave) if rows > 1 => Typing::Move(Key::Up),
        Some(FKey::Leave) => Typing::Move(Key::Leave),
        Some(key) => {
            edit_text(text, key);
            Typing::Stay
        }
        None => Typing::Stay,
    }
}

fn pick_in(
    question: &str,
    options: &Table,
    hotkeys: &[Hotkey],
    field: Option<&TextRow>,
    notice: Option<&str>,
) -> io::Result<Picked> {
    if options.is_empty() {
        return Ok(Picked::Left);
    }
    let Ok(_raw) = RawMode::enter() else {
        return Ok(Picked::NoRawMode);
    };
    let mut err = io::stderr();
    let mut cursor = Cursor::new(options.len());
    let mut text = field.map(|f| f.text.clone()).unwrap_or_default();
    // Back where it was typed, when something typed is being tried again.
    if let Some(f) = field.filter(|f| !f.text.is_empty() && f.row < options.len()) {
        cursor.at = f.row;
    }
    let placeholder = field.map_or("", |f| f.placeholder.as_str());
    loop {
        let typing = field.is_some_and(|f| f.row == cursor.at);
        let (columns, height) = usable_size(terminal::size()?);
        let (columns, height) = (usize::from(columns), usize::from(height));
        let plan = plan(height, options.len());
        cursor.fit(plan.rows);
        let mut lines = match field {
            Some(f) => {
                let mut shown = options.clone();
                shown.set(f.row, f.column, field_cell(&text, placeholder, typing));
                frame(question, &shown, &cursor, &plan, columns, hotkeys)
            }
            None => frame(question, options, &cursor, &plan, columns, hotkeys),
        };
        if let Some(notice) = notice {
            show_notice(&mut lines, &plan, notice, columns);
        }
        if typing && let Some(last) = lines.last_mut() {
            *last = Line::of(
                " type · Enter accept · ↑/↓ move · Esc back · ctrl-u clear",
                Tone::Muted,
            )
            .clipped(columns);
        }
        draw(&mut err, &lines, columns)?;
        // Anything but a key (a new size, say) only means the next draw fits it.
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if typing {
            match type_key(&mut text, placeholder, key, options.len()) {
                Typing::Stay => {}
                Typing::Move(Key::Leave) => return Ok(Picked::Left),
                Typing::Move(key) => {
                    cursor.apply(key);
                }
                Typing::Accept => {
                    let typed = text.trim();
                    let answer = if typed.is_empty() { placeholder } else { typed };
                    return Ok(Picked::Typed(answer.to_string()));
                }
            }
            continue;
        }
        if let KeyCode::Char(c) = key.code
            && key.kind != KeyEventKind::Release
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && hotkeys.iter().any(|h| h.key == c)
        {
            return Ok(Picked::Key(c, cursor.at));
        }
        let Some(key) = map_key(key) else { continue };
        match cursor.apply(key) {
            Step::Stay => {}
            Step::Chose(i) => return Ok(Picked::Chose(i)),
            Step::Left => return Ok(Picked::Left),
        }
    }
}

/// The text row's cell: the text, with a caret where it is being typed, or the
/// placeholder dimmed while there is none.
fn field_cell(text: &str, placeholder: &str, typing: bool) -> Cell {
    match (text.is_empty(), typing) {
        (true, true) => Cell::new(format!("▏{placeholder}"), Tone::Muted),
        (true, false) => Cell::new(placeholder, Tone::Muted),
        (false, true) => Cell::new(format!("{text}▏"), Tone::Accent),
        (false, false) => Cell::new(text, Tone::Normal),
    }
}

/// One thing the person can filter for and mark.
pub struct Item {
    /// Whether it is there already: shown marked from the start, and not added again.
    pub added: bool,
    /// Which list it is in, as an index into the names `find` is given.
    pub tab: usize,
    pub name: String,
    pub kind: &'static str,
    pub detail: String,
}

/// A setting of a search that is changed from inside it: pressing Ctrl and
/// `key` opens its text for editing (Enter applies it, Esc leaves it as it was),
/// or, when it is not `editable`, applies it at once. The search then ends with
/// [`Found::Edited`], for the one that asked to look again with the change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Knob {
    pub key: char,
    /// What the setting is, as part of the line that says how it stands:
    /// `looking in`.
    pub name: &'static str,
    /// What pressing the key does: `change where to look`.
    pub action: String,
    /// How it stands now. Empty for one that is not `editable`.
    pub text: String,
    pub editable: bool,
    /// Whether it is other than it began.
    pub changed: bool,
}

/// How a search ended.
pub enum Found {
    /// A knob was edited (its index, and the new text, empty for one that is
    /// not editable): the search is asked again with that change.
    Edited(usize, String),
    /// The items marked, or the one under the selection when none was.
    Chose(Vec<usize>),
    /// Adding as it goes: the item under the selection, to be added now.
    One(usize),
    /// A path typed in full (starting with `/`, `~` or `.`), to be used as it is.
    Path(String),
    Left,
    NoRawMode,
}

/// How well `query` matches `text`: `None` when its letters don't all appear in
/// order, else a score where more is better. Case is ignored; a match at the
/// start, at a word, and letters side by side score higher.
fn score(query: &str, text: &str) -> Option<i64> {
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let mut at = 0;
    let mut total = 0;
    let mut last: Option<usize> = None;
    for q in query.to_lowercase().chars() {
        let found = (at..text.len()).find(|&i| text[i] == q)?;
        total += 1;
        if last.is_some_and(|l| l + 1 == found) {
            total += 3;
        }
        if found == 0 || !text[found - 1].is_alphanumeric() {
            total += 2;
        }
        if found == 0 {
            total += 2;
        }
        total -= (found - last.map_or(0, |l| l + 1)) as i64 / 4;
        last = Some(found);
        at = found + 1;
    }
    // Shorter names beat longer ones that match the same.
    Some(total * 4 - (text.len() as i64).min(3))
}

/// The items that match `query`, best first and otherwise in the order given.
fn rank(items: &[Item], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..items.len()).collect();
    }
    let mut hits: Vec<(i64, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| {
            let by_name = score(query, &it.name);
            let by_detail = score(query, &it.detail).map(|s| s - 8);
            by_name.max(by_detail).map(|s| (s, i))
        })
        .collect();
    hits.sort_by_key(|&(s, i)| (std::cmp::Reverse(s), i));
    hits.into_iter().map(|(_, i)| i).collect()
}

/// A row of the result: an item, or the path typed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Row {
    Item(usize),
    Path(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FKey {
    Type(char),
    Backspace,
    ClearAll,
    Up,
    Down,
    /// To the previous or next list.
    Prev,
    Next,
    Mark,
    Accept,
    Leave,
    /// Ctrl and a letter: a [`Knob`], when there is one by that key.
    Knob(char),
}

fn map_find_key(event: KeyEvent) -> Option<FKey> {
    if event.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
    Some(match event.code {
        KeyCode::Up => FKey::Up,
        KeyCode::Down => FKey::Down,
        KeyCode::Tab => FKey::Mark,
        KeyCode::Left => FKey::Prev,
        KeyCode::Right => FKey::Next,
        KeyCode::Enter => FKey::Accept,
        KeyCode::Esc => FKey::Leave,
        KeyCode::Backspace => FKey::Backspace,
        KeyCode::Char('c' | 'd') if ctrl => FKey::Leave,
        KeyCode::Char('p') if ctrl => FKey::Up,
        KeyCode::Char('n') if ctrl => FKey::Down,
        KeyCode::Char('u') if ctrl => FKey::ClearAll,
        KeyCode::Char(c) if ctrl => FKey::Knob(c),
        KeyCode::Char(c) => FKey::Type(c),
        _ => return None,
    })
}

/// The state of a search: what was typed, what is marked, which row is selected.
struct Finder {
    query: String,
    marked: std::collections::BTreeSet<usize>,
    /// The list being looked at, of `tabs`.
    tab: usize,
    tabs: usize,
    rows: Vec<Row>,
    cursor: Cursor,
    /// Whether Enter acts on the item at once, and marks it so it is not added
    /// twice, instead of ending the search with what is marked.
    adding: bool,
    knobs: Vec<Knob>,
    /// The knob being edited and the text so far.
    editing: Option<(usize, String)>,
}

impl Finder {
    fn new(items: &[Item], tabs: usize) -> Self {
        let mut f = Finder {
            query: String::new(),
            marked: Default::default(),
            tab: 0,
            tabs: tabs.max(1),
            rows: Vec::new(),
            cursor: Cursor::new(0),
            adding: false,
            knobs: Vec::new(),
            editing: None,
        };
        f.marked = (0..items.len()).filter(|&i| items[i].added).collect();
        f.refresh(items);
        f
    }

    fn refresh(&mut self, items: &[Item]) {
        let mut rows = Vec::new();
        if self.query.starts_with(['/', '~', '.']) {
            rows.push(Row::Path(self.query.clone()));
        }
        rows.extend(
            rank(items, &self.query)
                .into_iter()
                .filter(|&i| items[i].tab == self.tab)
                .map(Row::Item),
        );
        self.cursor = Cursor::new(rows.len());
        self.rows = rows;
    }

    /// Whether the search has settings.
    fn knobs_here(&self) -> bool {
        !self.knobs.is_empty()
    }

    /// How the settings stand, on a line of their own: what is other than it
    /// began says so. `None` when there are none.
    fn knob_state(&self) -> Option<Line> {
        if !self.knobs_here() {
            return None;
        }
        let mut spans = vec![Span::new(" ", Tone::Muted)];
        for (n, k) in self.knobs.iter().filter(|k| k.editable).enumerate() {
            if n > 0 {
                spans.push(Span::new(" · ", Tone::Muted));
            }
            let tone = if k.changed {
                Tone::Accent
            } else {
                Tone::Normal
            };
            let note = if k.changed { " (changed)" } else { "" };
            spans.push(Span::new(format!("{} {}{note}", k.name, k.text), tone));
        }
        Some(Line {
            spans,
            selected: false,
        })
    }

    /// What the keys of the settings do, in the line under the list.
    fn knob_keys(&self) -> Option<Line> {
        if !self.knobs_here() || self.editing.is_some() {
            return None;
        }
        let keys: Vec<String> = self
            .knobs
            .iter()
            .map(|k| format!("^{} {}", k.key.to_ascii_uppercase(), k.action))
            .collect();
        Some(Line::of(format!(" {}", keys.join(" · ")), Tone::Muted))
    }

    /// How many lines the screen has besides the rows.
    fn chrome(&self) -> usize {
        5 + 2 * usize::from(self.knobs_here())
    }

    /// How many items of each list match what was typed.
    fn counts(&self, items: &[Item]) -> Vec<usize> {
        let mut counts = vec![0; self.tabs];
        for i in rank(items, &self.query) {
            counts[items[i].tab] += 1;
        }
        counts
    }

    /// `Some` when the search is over.
    fn apply(&mut self, key: FKey, items: &[Item]) -> Option<Found> {
        if let Some((at, mut text)) = self.editing.take() {
            if key == FKey::Accept {
                return Some(Found::Edited(at, text));
            }
            edit_text(&mut text, key);
            if key != FKey::Leave {
                self.editing = Some((at, text));
            }
            return None;
        }
        match key {
            FKey::Knob(c) => {
                if let Some(at) = self.knobs.iter().position(|k| k.key == c)
                    && self.knobs_here()
                {
                    if self.knobs[at].editable {
                        self.editing = Some((at, self.knobs[at].text.clone()));
                    } else {
                        return Some(Found::Edited(at, String::new()));
                    }
                }
            }
            FKey::Type(_) | FKey::Backspace | FKey::ClearAll => {
                edit_text(&mut self.query, key);
                self.refresh(items);
            }
            FKey::Prev | FKey::Next => {
                self.tab = if key == FKey::Next {
                    (self.tab + 1) % self.tabs
                } else {
                    (self.tab + self.tabs - 1) % self.tabs
                };
                self.refresh(items);
            }
            FKey::Accept | FKey::Mark if self.adding => match self.rows.get(self.cursor.at)? {
                Row::Item(i) if !self.marked.contains(i) => return Some(Found::One(*i)),
                Row::Item(_) => {}
                Row::Path(p) => return Some(Found::Path(p.clone())),
            },
            FKey::Up | FKey::Down if self.rows.is_empty() => {}
            FKey::Up => {
                self.cursor.apply(Key::Up);
            }
            FKey::Down => {
                self.cursor.apply(Key::Down);
            }
            FKey::Mark => {
                if let Some(Row::Item(i)) = self.rows.get(self.cursor.at) {
                    if !self.marked.remove(i) {
                        self.marked.insert(*i);
                    }
                    self.cursor.apply(Key::Down);
                }
            }
            FKey::Accept => {
                if !self.marked.is_empty() {
                    return Some(Found::Chose(self.marked.iter().copied().collect()));
                }
                match self.rows.get(self.cursor.at)? {
                    Row::Item(i) => return Some(Found::Chose(vec![*i])),
                    Row::Path(p) => return Some(Found::Path(p.clone())),
                }
            }
            FKey::Leave => return Some(Found::Left),
        }
        None
    }
}

/// The screen of a search: the question, the query, the rows from the top of
/// the window, and the hints. Nothing is wider than `width`.
fn find_frame(
    question: &str,
    tabs: &[&str],
    items: &[Item],
    finder: &Finder,
    status: &str,
    rows: usize,
    width: usize,
) -> Vec<Line> {
    let counts = finder.counts(items);
    let mut heads = vec![Span::new(" ", Tone::Normal)];
    for (i, name) in tabs.iter().enumerate() {
        let tone = if i == finder.tab {
            Tone::Strong
        } else {
            Tone::Muted
        };
        let text = if i == finder.tab {
            format!("[{name} {}]", counts[i])
        } else {
            format!(" {name} {} ", counts[i])
        };
        heads.push(Span::new(text, tone));
        heads.push(Span::new(" ", Tone::Normal));
    }
    let mut lines = vec![
        Line {
            spans: vec![
                Span::new(format!(" {question}"), Tone::Strong),
                Span::new(if status.is_empty() { "" } else { "  " }, Tone::Normal),
                Span::new(status, Tone::Accent),
            ],
            selected: false,
        },
        Line {
            spans: heads,
            selected: false,
        },
    ];
    lines.extend(finder.knob_state());
    lines.push(match &finder.editing {
        Some((at, text)) => Line {
            spans: vec![
                Span::new(format!(" {}: ", finder.knobs[*at].name), Tone::Accent),
                Span::new(text.clone(), Tone::Normal),
                Span::new("▏", Tone::Accent),
            ],
            selected: false,
        },
        None => Line {
            spans: vec![
                Span::new(" > ", Tone::Muted),
                Span::new(finder.query.clone(), Tone::Normal),
                Span::new("▏", Tone::Accent),
            ],
            selected: false,
        },
    });
    let shown = finder
        .rows
        .iter()
        .enumerate()
        .skip(finder.cursor.top)
        .take(rows);
    for (at, row) in shown {
        let cells = match row {
            Row::Item(i) => {
                let item = &items[*i];
                let mark = if finder.marked.contains(i) {
                    "[x]"
                } else {
                    "[ ]"
                };
                vec![
                    Span::new(mark, Tone::Muted),
                    Span::new(item.name.clone(), Tone::Strong),
                    Span::new(item.kind, Tone::Accent),
                    Span::new(item.detail.clone(), Tone::Muted),
                ]
            }
            Row::Path(p) => vec![
                Span::new("   ", Tone::Muted),
                Span::new(format!("add {p}"), Tone::Strong),
                Span::new("path", Tone::Accent),
            ],
        };
        lines.push(Line::row(cells.into_iter(), at == finder.cursor.at));
    }
    let marked = if finder.marked.is_empty() {
        String::new()
    } else if finder.adding {
        format!(" · {} in the config", finder.marked.len())
    } else {
        format!(" · {} marked", finder.marked.len())
    };
    // The keys of the settings go above the hints, so a narrow window cuts the hints.
    lines.extend(finder.knob_keys());
    let keys = if finder.editing.is_some() {
        " type the new value · Ctrl-U clear · Enter apply · Esc leave it".to_string()
    } else if finder.adding {
        format!(" type to filter · ←/→ list · ↑/↓ move · Enter add · Esc done{marked}")
    } else {
        format!(" type to filter · ←/→ list · ↑/↓ move · Tab mark · Enter add · Esc back{marked}")
    };
    let place = format!(
        "{}/{} ",
        finder.rows.len().min(finder.cursor.at + 1),
        finder.rows.len()
    );
    let gap = width.saturating_sub(keys.chars().count() + place.chars().count());
    lines.push(Line::of(
        format!("{keys}{}{place}", " ".repeat(gap)),
        Tone::Muted,
    ));
    lines.into_iter().map(|l| l.clipped(width)).collect()
}

/// A list that narrows as the person types, where several things can be marked.
pub fn find(question: &str, tabs: &[&str], items: &[Item], knobs: &[Knob]) -> io::Result<Found> {
    let Ok(_raw) = RawMode::enter() else {
        return Ok(Found::NoRawMode);
    };
    let mut err = io::stderr();
    let mut finder = Finder::new(items, tabs.len());
    finder.knobs = knobs.to_vec();
    loop {
        let (columns, height) = usable_size(terminal::size()?);
        let (columns, height) = (usize::from(columns), usize::from(height));
        let rows = height.saturating_sub(finder.chrome()).max(1);
        finder.cursor.fit(rows);
        draw(
            &mut err,
            &find_frame(question, tabs, items, &finder, "", rows, columns),
            columns,
        )?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        let Some(key) = map_find_key(key) else {
            continue;
        };
        if let Some(done) = finder.apply(key, items) {
            return Ok(done);
        }
    }
}

/// How adding ended.
pub enum Added {
    Done,
    NoRawMode,
    /// A knob was edited (see [`Found::Edited`]): look again with the change.
    Edited(usize, String),
}

/// What was chosen while adding.
pub enum Pick {
    Item(usize),
    /// A path typed in full.
    Path(String),
}

/// The same search, but Enter adds what is under the selection at once (through
/// `add`, which says what it did, or why it could not) and marks it, and the search
/// stays open for the next; Esc ends it. Returns `false` when the terminal can't
/// do this.
pub fn find_adding(
    question: &str,
    tabs: &[&str],
    items: &[Item],
    knobs: &[Knob],
    add: &mut dyn FnMut(Pick) -> Result<String, String>,
) -> io::Result<Added> {
    let Ok(_raw) = RawMode::enter() else {
        return Ok(Added::NoRawMode);
    };
    let mut err = io::stderr();
    let mut finder = Finder::new(items, tabs.len());
    finder.knobs = knobs.to_vec();
    finder.adding = true;
    let mut status = String::new();
    loop {
        let (columns, height) = usable_size(terminal::size()?);
        let (columns, height) = (usize::from(columns), usize::from(height));
        let rows = height.saturating_sub(finder.chrome()).max(1);
        finder.cursor.fit(rows);
        let lines = find_frame(question, tabs, items, &finder, &status, rows, columns);
        draw(&mut err, &lines, columns)?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        let Some(key) = map_find_key(key) else {
            continue;
        };
        match finder.apply(key, items) {
            None => {}
            Some(Found::Left) => return Ok(Added::Done),
            Some(Found::Edited(at, text)) => return Ok(Added::Edited(at, text)),
            Some(Found::One(i)) => match add(Pick::Item(i)) {
                Ok(done) => {
                    finder.marked.insert(i);
                    // On to the next, so adding several is Enter, Enter, Enter.
                    if finder.rows.len() > 1 {
                        finder.cursor.apply(Key::Down);
                    }
                    status = format!("✓ {done}");
                }
                Err(why) => status = format!("✗ {why}"),
            },
            Some(Found::Path(typed)) => match add(Pick::Path(typed)) {
                Ok(done) => {
                    finder.query.clear();
                    finder.refresh(items);
                    status = format!("✓ {done}");
                }
                Err(why) => status = format!("✗ {why}"),
            },
            Some(_) => {}
        }
    }
}

/// One line of a [`Doc`] and how much it matters.
pub struct DocLine {
    pub text: String,
    pub tone: Tone,
}

impl DocLine {
    pub fn new(text: impl Into<String>, tone: Tone) -> Self {
        DocLine {
            text: text.into(),
            tone,
        }
    }
}

/// Text to look at, some of whose lines can be selected: the fields of the
/// config's entries.
pub struct Doc {
    pub lines: Vec<DocLine>,
    /// The lines that can be selected, as indexes into `lines`, in order.
    pub stops: Vec<usize>,
}

/// The first line shown, moving `top` no more than it takes for the selected
/// line to be in view; the first stop shows from the top of the text, the last to
/// its end, so what comes before and after them can be read.
fn view_top(doc: &Doc, at: usize, top: usize, rows: usize) -> usize {
    let most = doc.lines.len().saturating_sub(rows);
    let Some(&line) = doc.stops.get(at) else {
        return top.min(most);
    };
    let mut top = top.min(most);
    if line < top {
        top = line;
    } else if line >= top + rows {
        top = line + 1 - rows;
    }
    if at == 0 && line < rows {
        top = 0;
    }
    if at + 1 == doc.stops.len() && line >= most {
        top = most;
    }
    top
}

/// The hints of a view: what the keys do, over as many lines as the width needs
/// (each key stays whole), and where the selection is at the far end of the last.
fn view_hints(doc: &Doc, at: usize, hotkeys: &[Hotkey], width: usize) -> Vec<String> {
    let mut parts = vec!["↑/↓ j/k move".to_string()];
    if !doc.stops.is_empty() {
        parts.push("Enter edit".into());
    }
    parts.extend(hotkeys.iter().map(|h| format!("{} {}", h.key, h.label)));
    parts.push("q back".into());
    let place = if doc.stops.is_empty() {
        String::new()
    } else {
        format!("{}/{} ", at + 1, doc.stops.len())
    };
    let room = width.saturating_sub(place.chars().count() + 1);
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for part in parts {
        let joined = if line.is_empty() {
            format!(" {part}")
        } else {
            format!("{line} · {part}")
        };
        if joined.chars().count() > room && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            line = format!(" {part}");
        } else {
            line = joined;
        }
    }
    let gap = width.saturating_sub(line.chars().count() + place.chars().count());
    lines.push(format!("{line}{}{place}", " ".repeat(gap)));
    lines
}

/// The screen of a view: the question, the text from `top` with the line of stop
/// `at` selected, and the hints with which stop it is.
fn view_frame(
    question: &str,
    doc: &Doc,
    at: usize,
    top: usize,
    rows: usize,
    width: usize,
    hotkeys: &[Hotkey],
) -> Vec<Line> {
    let mut lines = vec![Line::of(format!(" {question}"), Tone::Strong)];
    let chosen = doc.stops.get(at).copied();
    for (i, line) in doc.lines.iter().enumerate().skip(top).take(rows) {
        let span = Span::new(line.text.clone(), line.tone);
        lines.push(Line::row(std::iter::once(span), Some(i) == chosen));
    }
    for hint in view_hints(doc, at, hotkeys, width) {
        lines.push(Line::of(hint, Tone::Muted));
    }
    lines.into_iter().map(|l| l.clipped(width)).collect()
}

/// Shows `doc` and lets the person move over its selectable lines. Ends with
/// Enter (the stop selected), a hotkey (and the stop that was selected), or by
/// leaving.
pub fn view(question: &str, doc: &Doc, hotkeys: &[Hotkey]) -> io::Result<Picked> {
    let Ok(_raw) = RawMode::enter() else {
        return Ok(Picked::NoRawMode);
    };
    let mut err = io::stderr();
    let mut cursor = Cursor::new(doc.stops.len());
    let mut top = 0;
    loop {
        let (columns, height) = usable_size(terminal::size()?);
        let (columns, height) = (usize::from(columns), usize::from(height));
        // The question, the hints, and a line left free.
        let rows = height
            .saturating_sub(2 + view_hints(doc, cursor.at, hotkeys, columns).len())
            .max(1);
        top = view_top(doc, cursor.at, top, rows);
        let lines = view_frame(question, doc, cursor.at, top, rows, columns, hotkeys);
        draw(&mut err, &lines, columns)?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if let KeyCode::Char(c) = key.code
            && key.kind != KeyEventKind::Release
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && hotkeys.iter().any(|h| h.key == c)
        {
            return Ok(Picked::Key(c, cursor.at));
        }
        match map_key(key) {
            Some(Key::Leave) => return Ok(Picked::Left),
            // Nothing to move over, and nothing for Enter to open.
            _ if doc.stops.is_empty() => {}
            Some(Key::Choose) => return Ok(Picked::Chose(cursor.at)),
            Some(key) => {
                cursor.apply(key);
            }
            None => {}
        }
    }
}

/// How a line typed in [`input`] ended.
pub enum Typed {
    /// What was typed, as Enter left it (maybe nothing).
    Line(String),
    Left,
    /// The terminal can't be taken over for single keys; ask some other way.
    NoRawMode,
}

/// What a key does to a line of text being typed, when it is one that edits:
/// whether it did.
fn edit_text(text: &mut String, key: FKey) -> bool {
    match key {
        FKey::Type(c) => text.push(c),
        FKey::Backspace => {
            text.pop();
        }
        FKey::ClearAll => text.clear(),
        _ => return false,
    }
    true
}

fn input_frame(question: &str, text: &str, width: usize) -> Vec<Line> {
    vec![
        Line::of(format!(" {question}"), Tone::Strong),
        Line::of("", Tone::Normal),
        Line {
            spans: vec![
                Span::new(" > ", Tone::Muted),
                Span::new(text, Tone::Normal),
                Span::new("▏", Tone::Accent),
            ],
            selected: false,
        },
        Line::of("", Tone::Normal),
        Line::of(" Enter accept · Esc cancel · ctrl-u clear", Tone::Muted),
    ]
    .into_iter()
    .map(|l| l.clipped(width))
    .collect()
}

/// Asks for a line of text on the alternate screen, like the lists, so the
/// person is not thrown out to the shell to answer.
pub fn input(question: &str) -> io::Result<Typed> {
    let Ok(_raw) = RawMode::enter() else {
        return Ok(Typed::NoRawMode);
    };
    let mut err = io::stderr();
    let mut text = String::new();
    loop {
        let (columns, _) = usable_size(terminal::size()?);
        draw(
            &mut err,
            &input_frame(question, &text, usize::from(columns)),
            usize::from(columns),
        )?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        match map_find_key(key) {
            Some(FKey::Accept) => return Ok(Typed::Line(text)),
            Some(FKey::Leave) => return Ok(Typed::Left),
            Some(key) => {
                edit_text(&mut text, key);
            }
            None => {}
        }
    }
}

/// Prints one span in the look its tone has. Colour is only the terminal's own
/// cyan, so a theme decides what it is, and crossterm leaves it out when
/// `NO_COLOR` is set; bold and dim stay. A selected line is reversed as a whole,
/// where all that is left of a tone is that a name is bold.
fn paint(err: &mut impl Write, span: &Span, selected: bool) -> io::Result<()> {
    match (span.tone, selected) {
        (Tone::Strong, _) => queue!(err, SetAttribute(Attribute::Bold))?,
        (Tone::Muted, false) => queue!(err, SetAttribute(Attribute::Dim))?,
        (Tone::Accent, false) => queue!(err, SetForegroundColor(Color::DarkCyan))?,
        (Tone::Error, false) => queue!(err, SetForegroundColor(Color::DarkRed))?,
        _ => {}
    }
    queue!(err, Print(&span.text))?;
    match (span.tone, selected) {
        (Tone::Strong | Tone::Muted, _) => queue!(err, SetAttribute(Attribute::NormalIntensity))?,
        (Tone::Accent | Tone::Error, false) => queue!(err, ResetColor)?,
        _ => {}
    }
    Ok(())
}

/// Draws `lines` from the top left corner of the screen. The selected line is a bar
/// across the whole width; the rest are in the look their tones have.
fn draw(err: &mut impl Write, lines: &[Line], width: usize) -> io::Result<()> {
    queue!(err, cursor::MoveTo(0, 0))?;
    for line in lines {
        if line.selected {
            queue!(err, SetAttribute(Attribute::Reverse))?;
        }
        for span in &line.spans {
            paint(err, span, line.selected)?;
        }
        if line.selected {
            let rest = width.saturating_sub(line.width());
            queue!(err, Print(" ".repeat(rest)), SetAttribute(Attribute::Reset))?;
        }
        // What an earlier, longer line left to the right, then the next line; raw
        // mode does not turn a line feed into a new line by itself.
        queue!(err, Clear(ClearType::UntilNewLine), Print("\r\n"))?;
    }
    // And whatever was below the last line, such as a longer list.
    queue!(err, Clear(ClearType::FromCursorDown))?;
    err.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> Option<Key> {
        map_key(KeyEvent::new(code, modifiers))
    }

    fn plain(c: char) -> Option<Key> {
        press(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn typing_edits_a_line_and_other_keys_leave_it_alone() {
        let mut t = String::new();
        for c in "abc".chars() {
            assert!(edit_text(&mut t, FKey::Type(c)));
        }
        assert!(edit_text(&mut t, FKey::Backspace));
        assert_eq!(t, "ab");
        assert!(!edit_text(&mut t, FKey::Accept));
        assert_eq!(t, "ab");
        assert!(edit_text(&mut t, FKey::ClearAll));
        assert_eq!(t, "");
    }

    #[test]
    fn the_arrows_and_vim_keys_move_the_same_way() {
        assert_eq!(press(KeyCode::Up, KeyModifiers::NONE), Some(Key::Up));
        assert_eq!(plain('k'), Some(Key::Up));
        assert_eq!(
            press(KeyCode::Char('p'), KeyModifiers::CONTROL),
            Some(Key::Up)
        );
        assert_eq!(press(KeyCode::Down, KeyModifiers::NONE), Some(Key::Down));
        assert_eq!(plain('j'), Some(Key::Down));
        assert_eq!(
            press(KeyCode::Char('n'), KeyModifiers::CONTROL),
            Some(Key::Down)
        );
        assert_eq!(press(KeyCode::Home, KeyModifiers::NONE), Some(Key::First));
        assert_eq!(plain('g'), Some(Key::First));
        assert_eq!(press(KeyCode::End, KeyModifiers::NONE), Some(Key::Last));
        // Capital G arrives with shift held.
        assert_eq!(
            press(KeyCode::Char('G'), KeyModifiers::SHIFT),
            Some(Key::Last)
        );
    }

    #[test]
    fn enter_chooses_and_escape_q_and_ctrl_c_leave() {
        assert_eq!(press(KeyCode::Enter, KeyModifiers::NONE), Some(Key::Choose));
        assert_eq!(press(KeyCode::Esc, KeyModifiers::NONE), Some(Key::Leave));
        assert_eq!(plain('q'), Some(Key::Leave));
        assert_eq!(
            press(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(Key::Leave)
        );
        assert_eq!(
            press(KeyCode::Char('d'), KeyModifiers::CONTROL),
            Some(Key::Leave)
        );
    }

    #[test]
    fn digits_jump_and_other_keys_do_nothing() {
        assert_eq!(plain('3'), Some(Key::Jump(3)));
        assert_eq!(plain('0'), None);
        assert_eq!(plain('x'), None);
        assert_eq!(press(KeyCode::Tab, KeyModifiers::NONE), None);
        // A plain `c` is not ctrl-c.
        assert_eq!(plain('c'), None);
    }

    #[test]
    fn a_key_being_let_go_is_not_a_second_press() {
        let mut release = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(map_key(release), None);
    }

    #[test]
    fn the_selection_starts_on_the_first_and_wraps_round_the_ends() {
        let mut c = Cursor::new(3);
        assert_eq!(c.at, 0);
        assert_eq!(c.apply(Key::Up), Step::Stay);
        assert_eq!(c.at, 2);
        assert_eq!(c.apply(Key::Down), Step::Stay);
        assert_eq!(c.at, 0);
        c.apply(Key::Last);
        assert_eq!(c.at, 2);
        c.apply(Key::First);
        assert_eq!(c.at, 0);
    }

    #[test]
    fn a_digit_past_the_end_is_ignored_and_one_list_entry_cannot_be_lost() {
        let mut c = Cursor::new(2);
        c.apply(Key::Jump(2));
        assert_eq!(c.at, 1);
        c.apply(Key::Jump(5));
        assert_eq!(c.at, 1);
        let mut one = Cursor::new(1);
        one.apply(Key::Down);
        one.apply(Key::Up);
        assert_eq!(one.apply(Key::Choose), Step::Chose(0));
    }

    #[test]
    fn choosing_and_leaving_end_the_pick() {
        let mut c = Cursor::new(4);
        c.apply(Key::Jump(3));
        assert_eq!(c.apply(Key::Choose), Step::Chose(2));
        assert_eq!(c.apply(Key::Leave), Step::Left);
    }

    #[test]
    fn a_long_list_scrolls_only_as_far_as_the_selection_needs() {
        let mut c = Cursor::new(20);
        c.fit(5);
        assert_eq!(c.top, 0);
        for _ in 0..7 {
            c.apply(Key::Down);
            c.fit(5);
        }
        // At 7 of rows 0..5: the window moved down by exactly what it had to.
        assert_eq!((c.at, c.top), (7, 3));
        c.apply(Key::Up);
        c.fit(5);
        assert_eq!((c.at, c.top), (6, 3), "going back does not scroll yet");
        c.apply(Key::First);
        c.fit(5);
        assert_eq!(c.top, 0);
        c.apply(Key::Last);
        c.fit(5);
        assert_eq!((c.at, c.top), (19, 15));
    }

    use crate::cmd::table::Cell;

    fn options(n: usize) -> Table {
        Table::new(
            vec!["NAME"],
            (1..=n)
                .map(|i| vec![Cell::new(format!("entry {i}"), Tone::Normal)])
                .collect(),
            0,
        )
    }

    /// The plainest screen, with room for `rows` entries.
    fn compact(rows: usize) -> Plan {
        Plan {
            section_gaps: false,
            rows,
        }
    }

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::text).collect()
    }

    #[test]
    fn the_frame_has_the_question_the_titles_the_entries_and_the_hints() {
        let mut c = Cursor::new(3);
        c.apply(Key::Down);
        let lines = frame("Where to?", &options(3), &c, &compact(10), 80, &[]);
        let text = texts(&lines);
        assert_eq!(text[0], " Where to?");
        assert_eq!(text[1].trim_end(), " NAME");
        assert_eq!(
            text[2..5].iter().map(|l| l.trim()).collect::<Vec<_>>(),
            ["entry 1", "entry 2", "entry 3"]
        );
        assert!(
            text[5].contains("j/k") && text[5].ends_with("2/3 "),
            "{text:?}"
        );
        // Only the entry the cursor is on is the bar.
        let selected: Vec<bool> = lines.iter().map(|l| l.selected).collect();
        assert_eq!(selected, [false, false, false, true, false, false]);
    }

    #[test]
    fn the_hints_put_the_position_at_the_far_end_when_there_is_room_and_drop_it_when_not() {
        let c = Cursor::new(12);
        let wide = hints(&c, 80, &[]).text();
        assert_eq!(wide.chars().count(), 80);
        assert!(wide.ends_with("1/12 "), "{wide:?}");
        let tight = hints(&c, 10, &[]).text();
        assert!(tight.starts_with(" ↑/↓"), "{tight:?}");
    }

    #[test]
    fn the_frame_shows_only_the_rows_there_is_room_for() {
        let mut c = Cursor::new(20);
        for _ in 0..9 {
            c.apply(Key::Down);
        }
        c.fit(4);
        let lines = frame("Where to?", &options(20), &c, &compact(4), 80, &[]);
        assert_eq!(lines.len(), 7);
        let shown: Vec<String> = texts(&lines)[2..6]
            .iter()
            .map(|l| l.trim().to_string())
            .collect();
        assert_eq!(shown, ["entry 7", "entry 8", "entry 9", "entry 10"]);
        assert!(lines[5].selected);
    }

    #[test]
    fn the_frame_is_laid_out_again_for_every_width() {
        let table = Table::new(
            vec!["NAME", "WHERE", "VIA", "LAST"],
            vec![
                [
                    "Demo API",
                    "/home/me/devel/projects/api",
                    "docker",
                    "2 h ago",
                ]
                .map(|c| Cell::new(c, Tone::Normal))
                .to_vec(),
            ],
            1,
        );
        let c = Cursor::new(1);
        let wide = texts(&frame("Where to?", &table, &c, &compact(5), 100, &[]));
        assert!(wide.iter().all(|l| l.chars().count() <= 100), "{wide:?}");
        assert!(wide[2].contains("/home/me/devel/projects/api"), "{wide:?}");
        // The row reaches the far end: the time is the last thing before the margin.
        assert!(wide[2].ends_with("2 h ago "), "{wide:?}");
        let narrow = texts(&frame("Where to?", &table, &c, &compact(5), 50, &[]));
        assert!(narrow.iter().all(|l| l.chars().count() <= 50), "{narrow:?}");
        // The path gave way, and the time at the end of the row is still there.
        assert!(
            narrow[2].contains('…') && narrow[2].ends_with("2 h ago "),
            "{narrow:?}"
        );
    }

    #[test]
    fn a_line_is_cut_to_the_width_so_it_never_wraps() {
        let line = Line::of("a very long line", Tone::Normal);
        assert_eq!(line.clipped(8).text(), "a very …");
        assert_eq!(Line::of("short", Tone::Normal).clipped(10).text(), "short");
        assert_eq!(Line::of("anything", Tone::Normal).clipped(0).text(), "…");
        let one = Table::new(
            vec!["A"],
            vec![vec![Cell::new("x".repeat(100), Tone::Normal)]],
            0,
        );
        let lines = frame("Where to?", &one, &Cursor::new(1), &compact(5), 30, &[]);
        assert!(
            texts(&lines).iter().all(|l| l.chars().count() <= 30),
            "{:?}",
            texts(&lines)
        );
    }

    #[test]
    fn a_terminal_that_reports_no_size_gets_a_usual_one() {
        assert_eq!(usable_size((0, 0)), (80, 24));
        assert_eq!(usable_size((120, 0)), (120, 24));
        assert_eq!(usable_size((100, 40)), (100, 40));
    }

    fn drawn(lines: &[Line], width: usize) -> String {
        let mut out = Vec::new();
        draw(&mut out, lines, width).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn drawing_starts_in_the_corner_and_clears_what_an_earlier_drawing_left() {
        let text = drawn(
            &[Line::of("one", Tone::Normal), Line::of("two", Tone::Normal)],
            20,
        );
        // Home first, so it does not matter where the cursor was or how the
        // window was resized since.
        assert!(text.starts_with("\x1b[1;1H"), "{text:?}");
        assert!(
            text.contains("one\x1b[K\r\n"),
            "a line clears what was right of it: {text:?}"
        );
        assert!(text.ends_with("\x1b[J"), "and what was below: {text:?}");
    }

    #[test]
    fn the_selected_line_is_a_bar_across_the_width() {
        let mut bar = Line::of("two", Tone::Normal);
        bar.selected = true;
        let text = drawn(&[bar], 10);
        assert!(text.contains("\x1b[7mtwo       \x1b[0m"), "{text:?}");
    }

    #[test]
    fn what_matters_is_bold_and_detail_is_dim_and_both_are_put_back() {
        let text = drawn(
            &[Line::row(
                [
                    Span::new("name", Tone::Strong),
                    Span::new("when", Tone::Muted),
                ]
                .into_iter(),
                false,
            )],
            30,
        );
        assert!(text.contains("\x1b[1mname\x1b[22m"), "{text:?}");
        assert!(text.contains("\x1b[2mwhen\x1b[22m"), "{text:?}");
    }

    #[test]
    fn a_tall_terminal_gets_room_to_breathe_and_a_short_one_only_the_list() {
        let shape = |height, entries| {
            let p = plan(height, entries);
            (p.section_gaps, p.rows)
        };
        // Two entries: everything fits at 24 lines.
        assert_eq!(shape(24, 2), (true, 2));
        // Eighteen are the most that keep the empty lines round the list.
        assert_eq!(shape(24, 18), (true, 18));
        // Twenty are all the room holds without them.
        assert_eq!(shape(24, 20), (false, 20));
        // More than that scrolls, with the question, titles and hints kept.
        assert_eq!(shape(24, 50), (false, 20));
    }

    #[test]
    fn a_small_window_keeps_the_list_before_the_spacing() {
        let shape = |height, entries| {
            let p = plan(height, entries);
            (p.section_gaps, p.rows)
        };
        // Two entries need 7 lines with the gaps round the list, 5 without.
        assert_eq!(shape(8, 2), (true, 2));
        assert_eq!(shape(7, 2), (false, 2));
        assert_eq!(shape(6, 2), (false, 2));
    }

    #[test]
    fn the_smallest_window_still_shows_one_entry() {
        assert_eq!(plan(4, 5).rows, 1);
        assert_eq!(plan(0, 5).rows, 1);
    }

    #[test]
    fn what_a_plan_counts_is_what_the_frame_draws() {
        let table = options(3);
        let c = Cursor::new(3);
        for section_gaps in [true, false] {
            let p = Plan {
                section_gaps,
                rows: 3,
            };
            let lines = frame("Where to?", &table, &c, &p, 100, &[]);
            assert_eq!(lines.len(), Plan::height(section_gaps, 3), "{section_gaps}");
        }
    }

    #[test]
    fn the_airy_screen_has_a_gap_after_the_question_and_before_the_hints_and_none_between_entries()
    {
        let p = Plan {
            section_gaps: true,
            rows: 2,
        };
        let lines = frame("Where to?", &options(2), &Cursor::new(2), &p, 100, &[]);
        let text = texts(&lines);
        assert_eq!(text[0], " Where to?");
        assert_eq!(text[1], "", "a gap after the question: {text:?}");
        assert_eq!(text[2].trim_end(), " NAME");
        assert_eq!(text[3].trim(), "entry 1");
        assert_eq!(
            text[4].trim(),
            "entry 2",
            "no gap between entries: {text:?}"
        );
        assert_eq!(text[5], "", "a gap before the hints: {text:?}");
        assert!(text[6].contains("q quit"));
        // Only an entry is ever the bar, the gaps never are.
        let bars: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.selected)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(bars, [3]);
    }

    #[test]
    fn the_bar_covers_the_whole_width_not_only_the_text() {
        let lines = frame(
            "Where to?",
            &options(2),
            &Cursor::new(2),
            &compact(2),
            40,
            &[],
        );
        let text = drawn(&lines, 40);
        // Reverse video on, the entry, its padding out to column 40, reverse off.
        let bar = format!("\x1b[7m{:<40}\x1b[0m", " entry 1 ");
        assert!(text.contains(&bar), "{text:?}");
    }

    fn items(names: &[&str]) -> Vec<Item> {
        names
            .iter()
            .map(|n| Item {
                added: false,
                tab: 0,
                name: n.to_string(),
                kind: "program",
                detail: format!("/usr/bin/{n}"),
            })
            .collect()
    }

    #[test]
    fn typing_narrows_the_list_best_match_first_and_ignores_case() {
        let list = items(&["bat", "ripgrep", "rg", "fd", "Fzf"]);
        assert_eq!(rank(&list, ""), vec![0, 1, 2, 3, 4]);
        // `rg` is a subsequence of both, and the shorter, tighter one is first.
        assert_eq!(rank(&list, "rg"), vec![2, 1]);
        assert_eq!(rank(&list, "FZ"), vec![4]);
        assert!(rank(&list, "zzz").is_empty());
    }

    fn type_into(f: &mut Finder, list: &[Item], text: &str) {
        for c in text.chars() {
            assert!(f.apply(FKey::Type(c), list).is_none());
        }
    }

    #[test]
    fn tab_marks_and_enter_takes_the_marked_or_the_one_selected() {
        let list = items(&["a", "b", "c"]);
        let mut f = Finder::new(&list, 1);
        // Nothing marked: Enter takes what is selected.
        f.apply(FKey::Down, &list);
        assert!(matches!(f.apply(FKey::Accept, &list), Some(Found::Chose(v)) if v == [1]));
        // Marks move the selection on, and are kept while the query changes.
        let mut f = Finder::new(&list, 1);
        f.apply(FKey::Mark, &list);
        f.apply(FKey::Mark, &list);
        type_into(&mut f, &list, "c");
        f.apply(FKey::Mark, &list);
        assert!(matches!(f.apply(FKey::Accept, &list), Some(Found::Chose(v)) if v == [0, 1, 2]));
    }

    #[test]
    fn a_path_is_offered_as_it_is_and_nothing_matching_is_not_an_answer() {
        let list = items(&["a"]);
        let mut f = Finder::new(&list, 1);
        type_into(&mut f, &list, "~/.vimrc");
        assert!(matches!(f.apply(FKey::Accept, &list), Some(Found::Path(p)) if p == "~/.vimrc"));
        let mut f = Finder::new(&list, 1);
        type_into(&mut f, &list, "zz");
        assert!(f.apply(FKey::Accept, &list).is_none());
        assert!(f.apply(FKey::Down, &list).is_none(), "no rows to move over");
        f.apply(FKey::ClearAll, &list);
        assert_eq!(f.rows.len(), 1);
        assert!(matches!(f.apply(FKey::Leave, &list), Some(Found::Left)));
    }

    #[test]
    fn the_search_screen_shows_the_query_the_marks_and_fits_the_width() {
        let list = items(&["alpha", "beta"]);
        let mut f = Finder::new(&list, 1);
        f.apply(FKey::Mark, &list);
        let lines = find_frame("Add", &["Programs"], &list, &f, "", 5, 100);
        let text: Vec<String> = lines.iter().map(Line::text).collect();
        assert!(text[2].starts_with(" > "), "{text:?}");
        assert!(
            text[3].contains("[x]") && text[3].contains("alpha"),
            "{text:?}"
        );
        assert!(text[4].contains("[ ]"), "{text:?}");
        assert!(text.last().unwrap().contains("1 marked"), "{text:?}");
        assert!(
            find_frame("Add", &["Programs"], &list, &f, "", 5, 40)
                .iter()
                .all(|l| l.width() <= 40)
        );
    }

    #[test]
    fn hotkeys_are_named_in_the_hints() {
        let c = Cursor::new(3);
        let keys = [Hotkey {
            key: 'c',
            label: "config",
        }];
        assert!(hints(&c, 80, &keys).text().contains("c config"));
    }

    #[test]
    fn the_lists_are_switched_and_each_shows_only_its_own_and_marks_are_kept() {
        let mut list = items(&["rg", "fd"]);
        list.push(Item {
            added: false,
            tab: 1,
            name: "bashrc".into(),
            kind: "config",
            detail: "~/.bashrc".into(),
        });
        let mut f = Finder::new(&list, 2);
        assert_eq!(f.rows, vec![Row::Item(0), Row::Item(1)]);
        f.apply(FKey::Mark, &list);
        f.apply(FKey::Next, &list);
        assert_eq!(f.rows, vec![Row::Item(2)]);
        // The count of each list follows the query, whichever is shown.
        assert_eq!(f.counts(&list), vec![2, 1]);
        type_into(&mut f, &list, "rc");
        assert_eq!(f.counts(&list), vec![0, 1]);
        f.apply(FKey::ClearAll, &list);
        f.apply(FKey::Mark, &list);
        // It goes round, and what was marked in the other list is still marked.
        f.apply(FKey::Next, &list);
        assert_eq!(f.tab, 0);
        assert!(matches!(f.apply(FKey::Accept, &list), Some(Found::Chose(v)) if v == [0, 2]));
        f.apply(FKey::Prev, &list);
        assert_eq!(f.tab, 1);
        let text: Vec<String> = find_frame("Add", &["Programs", "Configs"], &list, &f, "", 5, 100)
            .iter()
            .map(Line::text)
            .collect();
        assert!(
            text[1].contains("Programs 2") && text[1].contains("[Configs 1]"),
            "{text:?}"
        );
    }

    fn form() -> Doc {
        let lines = [
            ("[[inject]]", Tone::Strong),
            ("name = \"a\"", Tone::Normal),
            ("binary = (not set)", Tone::Muted),
            ("", Tone::Normal),
            ("[[inject]]", Tone::Strong),
            ("name = \"b\"", Tone::Normal),
        ]
        .map(|(t, tone)| DocLine::new(t, tone))
        .into();
        Doc {
            lines,
            stops: vec![1, 2, 5],
        }
    }

    #[test]
    fn a_view_selects_the_line_of_the_stop_and_names_which_it_is() {
        let doc = form();
        let keys = [Hotkey {
            key: 'a',
            label: "add",
        }];
        let lines = view_frame("Form", &doc, 1, 0, 20, 70, &keys);
        let marked: Vec<String> = lines
            .iter()
            .filter(|l| l.selected)
            .map(Line::text)
            .collect();
        assert_eq!(marked, vec![" binary = (not set) "]);
        let hint = lines.last().unwrap().text();
        assert!(
            hint.contains("Enter edit") && hint.contains("a add"),
            "{hint}"
        );
        assert!(hint.ends_with("2/3 "), "{hint}");
        // Nothing to select: no marked line, no Enter, no place.
        let plain = Doc {
            lines: form().lines,
            stops: vec![],
        };
        let lines = view_frame("Form", &plain, 0, 0, 20, 70, &[]);
        assert!(lines.iter().all(|l| !l.selected));
        assert!(!lines.last().unwrap().text().contains("Enter"));
    }

    #[test]
    fn the_view_scrolls_to_the_line_and_shows_the_ends_of_the_text_at_the_ends() {
        let doc = form();
        assert_eq!(view_top(&doc, 2, 0, 20), 0);
        // Three lines of room: the last stop shows to the end of the text.
        assert_eq!(view_top(&doc, 2, 0, 3), 3);
        // The first stop shows from the top of the text when it fits there.
        assert_eq!(view_top(&doc, 0, 3, 4), 0);
        // A stop above the window is brought into view.
        assert_eq!(view_top(&doc, 1, 3, 2), 2);
    }

    #[test]
    fn when_adding_enter_acts_at_once_marks_the_item_and_does_not_add_it_twice() {
        let list = items(&["a", "b", "c"]);
        let mut f = Finder::new(&list, 1);
        f.adding = true;
        assert!(matches!(f.apply(FKey::Accept, &list), Some(Found::One(0))));
        // The caller marks it; Enter on a marked item does nothing, so does Tab on it.
        f.marked.insert(0);
        assert!(f.apply(FKey::Accept, &list).is_none());
        assert!(f.apply(FKey::Mark, &list).is_none());
        f.apply(FKey::Down, &list);
        assert!(matches!(f.apply(FKey::Mark, &list), Some(Found::One(1))));
        // A typed path is added as it is; nothing matching is nothing to add.
        type_into(&mut f, &list, "~/.vimrc");
        assert!(matches!(f.apply(FKey::Accept, &list), Some(Found::Path(p)) if p == "~/.vimrc"));
        f.apply(FKey::ClearAll, &list);
        type_into(&mut f, &list, "zz");
        assert!(f.apply(FKey::Accept, &list).is_none());
        let shown: Vec<String> = find_frame("Add", &["Programs"], &list, &f, "✓ added a", 5, 100)
            .iter()
            .map(Line::text)
            .collect();
        assert!(
            shown[0].contains("Add") && shown[0].contains("✓ added a"),
            "{shown:?}"
        );
        assert!(
            shown.last().unwrap().contains("1 in the config")
                && shown.last().unwrap().contains("Esc done"),
            "{shown:?}"
        );
    }

    #[test]
    fn items_that_are_there_already_start_marked_and_cannot_be_added_again() {
        let mut list = items(&["a", "b"]);
        list[0].added = true;
        let mut f = Finder::new(&list, 1);
        f.adding = true;
        assert!(f.marked.contains(&0) && !f.marked.contains(&1));
        assert!(f.apply(FKey::Accept, &list).is_none(), "`a` is in already");
        f.apply(FKey::Down, &list);
        assert!(matches!(f.apply(FKey::Accept, &list), Some(Found::One(1))));
        let shown: Vec<String> = find_frame("Add", &["Programs"], &list, &f, "", 5, 100)
            .iter()
            .map(Line::text)
            .collect();
        assert!(
            shown[3].contains("[x]") && shown[4].contains("[ ]"),
            "{shown:?}"
        );
        assert!(
            shown.last().unwrap().contains("1 in the config"),
            "{shown:?}"
        );
    }

    #[test]
    fn the_hints_go_over_a_second_line_when_they_do_not_fit_and_each_key_stays_whole() {
        let doc = form();
        let keys = [
            Hotkey {
                key: 'b',
                label: "+binary",
            },
            Hotkey {
                key: 'c',
                label: "+config",
            },
            Hotkey {
                key: 'P',
                label: "publish",
            },
        ];
        let wide = view_hints(&doc, 0, &keys, 120);
        assert_eq!(wide.len(), 1);
        assert!(
            wide[0].contains("b +binary") && wide[0].ends_with("1/3 "),
            "{wide:?}"
        );
        let narrow = view_hints(&doc, 0, &keys, 40);
        assert!(narrow.len() > 1, "{narrow:?}");
        for line in &narrow {
            assert!(line.chars().count() <= 40, "{line:?}");
        }
        let joined = narrow.join(" ");
        for key in ["b +binary", "c +config", "P publish", "q back"] {
            assert!(joined.contains(key), "{key} lost in {narrow:?}");
        }
        // The place is at the far end of the last line only.
        assert!(narrow.last().unwrap().ends_with("1/3 "));
        assert!(!narrow[0].ends_with("1/3 "));
    }

    fn knobs() -> Vec<Knob> {
        vec![
            Knob {
                key: 'w',
                name: "looking in",
                action: "change where to look".into(),
                text: "~".into(),
                editable: true,
                changed: false,
            },
            Knob {
                key: 'a',
                name: "everywhere",
                action: "look everywhere".into(),
                text: String::new(),
                editable: false,
                changed: false,
            },
        ]
    }

    fn with_knobs(list: &[Item]) -> Finder {
        let mut f = Finder::new(list, 1);
        f.knobs = knobs();
        f
    }

    #[test]
    fn on_the_text_row_every_letter_types_even_a_hotkey_and_enter_answers_with_the_text() {
        let typed =
            |text: &mut String, code, mods| type_key(text, "", KeyEvent::new(code, mods), 3);
        let mut text = String::new();
        for c in "xqj".chars() {
            let key = KeyCode::Char(c);
            assert_eq!(typed(&mut text, key, KeyModifiers::NONE), Typing::Stay);
        }
        assert_eq!(text, "xqj");
        assert_eq!(
            typed(&mut text, KeyCode::Backspace, KeyModifiers::NONE),
            Typing::Stay
        );
        assert_eq!(text, "xq");
        assert_eq!(
            typed(&mut text, KeyCode::Enter, KeyModifiers::NONE),
            Typing::Accept
        );
        assert_eq!(
            typed(&mut text, KeyCode::Char('u'), KeyModifiers::CONTROL),
            Typing::Stay
        );
        assert_eq!(text, "", "Ctrl-U clears");
        assert_eq!(
            typed(&mut text, KeyCode::Enter, KeyModifiers::NONE),
            Typing::Stay,
            "nothing typed is no answer"
        );
    }

    #[test]
    fn esc_and_ctrl_c_step_off_the_text_row_and_out_of_the_list_from_its_only_row() {
        let text = String::from("a");
        let leave =
            |code, mods, rows| type_key(&mut text.clone(), "", KeyEvent::new(code, mods), rows);
        assert_eq!(
            leave(KeyCode::Esc, KeyModifiers::NONE, 3),
            Typing::Move(Key::Up)
        );
        assert_eq!(
            leave(KeyCode::Char('c'), KeyModifiers::CONTROL, 3),
            Typing::Move(Key::Up)
        );
        assert_eq!(
            leave(KeyCode::Esc, KeyModifiers::NONE, 1),
            Typing::Move(Key::Leave)
        );
        assert_eq!(text, "a", "leaving does not type");
    }

    #[test]
    fn enter_on_an_empty_text_row_takes_the_placeholder_if_there_is_one() {
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            type_key(&mut String::new(), "lodge.tar", enter, 2),
            Typing::Accept
        );
        assert_eq!(type_key(&mut String::new(), "", enter, 2), Typing::Stay);
    }

    #[test]
    fn ctrl_and_a_letter_is_a_knob_and_other_keys_are_still_typing() {
        let ctrl = |c| map_find_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
        assert_eq!(ctrl('w'), Some(FKey::Knob('w')));
        assert_eq!(ctrl('u'), Some(FKey::ClearAll), "Ctrl-U still clears");
        assert_eq!(
            map_find_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)),
            Some(FKey::Type('w'))
        );
    }

    #[test]
    fn an_editable_knob_is_edited_in_place_and_enter_hands_back_the_new_text() {
        let list = items(&["alpha"]);
        let mut f = with_knobs(&list);
        assert!(f.apply(FKey::Knob('w'), &list).is_none());
        assert_eq!(
            f.editing,
            Some((0, "~".to_string())),
            "starts from what it is"
        );
        f.apply(FKey::Type('/'), &list);
        f.apply(FKey::Type('x'), &list);
        f.apply(FKey::Backspace, &list);
        assert_eq!(
            f.query, "",
            "what is typed goes to the knob, not the search"
        );
        assert!(matches!(
            f.apply(FKey::Accept, &list),
            Some(Found::Edited(0, text)) if text == "~/"
        ));
    }

    #[test]
    fn escape_leaves_the_knob_as_it_was_and_the_search_goes_on() {
        let list = items(&["alpha"]);
        let mut f = with_knobs(&list);
        f.apply(FKey::Knob('w'), &list);
        f.apply(FKey::ClearAll, &list);
        assert!(
            f.apply(FKey::Leave, &list).is_none(),
            "Esc ends the edit, not the search"
        );
        assert_eq!(f.editing, None);
        assert!(matches!(f.apply(FKey::Leave, &list), Some(Found::Left)));
    }

    #[test]
    fn a_knob_that_is_not_editable_applies_at_once_and_an_unknown_key_does_nothing() {
        let list = items(&["alpha"]);
        let mut f = with_knobs(&list);
        assert!(matches!(
            f.apply(FKey::Knob('a'), &list),
            Some(Found::Edited(1, text)) if text.is_empty()
        ));
        assert!(f.apply(FKey::Knob('z'), &list).is_none());
        assert_eq!(f.editing, None);
    }

    #[test]
    fn the_screen_says_how_the_settings_stand_and_apart_from_it_what_the_keys_do() {
        let list = items(&["alpha"]);
        let mut f = with_knobs(&list);
        let text = |f: &Finder| texts(&find_frame("Add", &["Files"], &list, f, "", 5, 80));
        let shown = text(&f);
        assert_eq!(shown[2], " looking in ~", "{shown:?}");
        assert_eq!(
            shown[shown.len() - 2],
            " ^W change where to look · ^A look everywhere",
            "{shown:?}"
        );
        assert_eq!(f.chrome(), 7, "the two lines take rows from the list");
        f.apply(FKey::Knob('w'), &list);
        let shown = text(&f);
        assert!(
            shown.iter().any(|l| l.contains("looking in: ~▏")),
            "{shown:?}"
        );
        assert!(shown.last().unwrap().contains("Enter apply"), "{shown:?}");
        assert!(
            shown.iter().all(|l| !l.contains("^W")),
            "no keys while editing: {shown:?}"
        );
        let plain = Finder::new(&list, 1);
        assert_eq!(plain.chrome(), 5, "without knobs the screen is as it was");
    }

    #[test]
    fn what_was_changed_says_so() {
        let list = items(&["alpha"]);
        let mut f = with_knobs(&list);
        f.knobs[0].changed = true;
        let shown = texts(&find_frame("Add", &["Files"], &list, &f, "", 5, 80));
        assert_eq!(shown[2], " looking in ~ (changed)", "{shown:?}");
    }
}
