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

use super::table::{Table, Tone};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, queue};
use std::io::{self, Write};

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
fn frame(question: &str, options: &Table, cursor: &Cursor, plan: &Plan, width: usize) -> Vec<Line> {
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
    lines.push(hints(cursor, width));
    lines.into_iter().map(|l| l.clipped(width)).collect()
}

/// What the keys do, and where the selection is, at the far end when there is room.
fn hints(cursor: &Cursor, width: usize) -> Line {
    let keys = " ↑/↓ or j/k move · Enter open · q quit";
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

/// Shows `options` and lets the person move over them and choose one.
pub fn pick(question: &str, options: &Table) -> io::Result<Picked> {
    if options.is_empty() {
        return Ok(Picked::Left);
    }
    let Ok(_raw) = RawMode::enter() else {
        return Ok(Picked::NoRawMode);
    };
    let mut err = io::stderr();
    let mut cursor = Cursor::new(options.len());
    loop {
        let (columns, height) = usable_size(terminal::size()?);
        let (columns, height) = (usize::from(columns), usize::from(height));
        let plan = plan(height, options.len());
        cursor.fit(plan.rows);
        let lines = frame(question, options, &cursor, &plan, columns);
        draw(&mut err, &lines, columns)?;
        // Anything but a key (a new size, say) only means the next draw fits it.
        let Event::Key(key) = event::read()? else {
            continue;
        };
        let Some(key) = map_key(key) else { continue };
        match cursor.apply(key) {
            Step::Stay => {}
            Step::Chose(i) => return Ok(Picked::Chose(i)),
            Step::Left => return Ok(Picked::Left),
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
        _ => {}
    }
    queue!(err, Print(&span.text))?;
    match (span.tone, selected) {
        (Tone::Strong | Tone::Muted, _) => queue!(err, SetAttribute(Attribute::NormalIntensity))?,
        (Tone::Accent, false) => queue!(err, ResetColor)?,
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
        let lines = frame("Where to?", &options(3), &c, &compact(10), 80);
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
        let wide = hints(&c, 80).text();
        assert_eq!(wide.chars().count(), 80);
        assert!(wide.ends_with("1/12 "), "{wide:?}");
        let tight = hints(&c, 10).text();
        assert!(tight.starts_with(" ↑/↓"), "{tight:?}");
    }

    #[test]
    fn the_frame_shows_only_the_rows_there_is_room_for() {
        let mut c = Cursor::new(20);
        for _ in 0..9 {
            c.apply(Key::Down);
        }
        c.fit(4);
        let lines = frame("Where to?", &options(20), &c, &compact(4), 80);
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
        let wide = texts(&frame("Where to?", &table, &c, &compact(5), 100));
        assert!(wide.iter().all(|l| l.chars().count() <= 100), "{wide:?}");
        assert!(wide[2].contains("/home/me/devel/projects/api"), "{wide:?}");
        // The row reaches the far end: the time is the last thing before the margin.
        assert!(wide[2].ends_with("2 h ago "), "{wide:?}");
        let narrow = texts(&frame("Where to?", &table, &c, &compact(5), 50));
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
        let lines = frame("Where to?", &one, &Cursor::new(1), &compact(5), 30);
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
            let lines = frame("Where to?", &table, &c, &p, 100);
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
        let lines = frame("Where to?", &options(2), &Cursor::new(2), &p, 100);
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
        let lines = frame("Where to?", &options(2), &Cursor::new(2), &compact(2), 40);
        let text = drawn(&lines, 40);
        // Reverse video on, the entry, its padding out to column 40, reverse off.
        let bar = format!("\x1b[7m{:<40}\x1b[0m", " entry 1 ");
        assert!(text.contains(&bar), "{text:?}");
    }
}
