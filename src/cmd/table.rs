//! Rows of cells that are laid out when the width is known.
//!
//! What to show, and how much each part matters, is the command's business; how
//! wide the terminal is and how importance looks is the terminal's. A [`Table`] is
//! where they meet: the command gives cells with a [`Tone`], and the one that draws
//! asks for the grid at the width it has, now and after every resize.

/// The room between two columns.
const GAP: usize = 2;
/// How narrow the flexible column may be made before anything else is.
const FLEX_MIN: usize = 12;
/// How narrow any other column may be made, once the flexible one is as narrow
/// as it goes.
const OTHER_MIN: usize = 8;
/// How wide a table is made to look when there is more room than that: past it the
/// eye has too far to go from one end of a row to the other.
const STRETCH_MAX: usize = 120;

/// How much a cell matters, which the one that draws turns into how it looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// What a row is called.
    Strong,
    Normal,
    /// Detail that is there to be found, not read.
    Muted,
    /// Something that sets a row apart (a place that is not this machine).
    Accent,
}

/// One cell: its text and how much it matters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub tone: Tone,
}

impl Cell {
    pub fn new(text: impl Into<String>, tone: Tone) -> Self {
        Cell {
            text: text.into(),
            tone,
        }
    }
}

/// The table at one width: every cell as wide as its column, and the header.
pub struct Grid {
    pub header: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
}

/// Rows of cells, shown as aligned columns under titles.
pub struct Table {
    titles: Vec<&'static str>,
    rows: Vec<Vec<Cell>>,
    /// The column that takes the room that is left over, and gives way first when
    /// there is too little: the one that holds the longest, least important text
    /// (a path).
    flex: usize,
}

impl Table {
    pub fn new(titles: Vec<&'static str>, rows: Vec<Vec<Cell>>, flex: usize) -> Self {
        Table { titles, rows, flex }
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The rows as plain lines with every column as wide as its longest cell, and
    /// no header: what there is when width is no concern, and what is shown
    /// where there is no more than text to show.
    pub fn natural(&self) -> Vec<String> {
        let widths = self.natural_widths();
        self.rows
            .iter()
            .map(|row| join(row.iter().map(|c| c.text.as_str()), &widths))
            .collect()
    }

    /// The table for `width` characters. Too wide: the flexible column is
    /// shortened first (in the middle, so a path keeps both its start and its
    /// end), then the others, widest first; the last column is kept whole, because
    /// it is what tells one row from another at a glance (when). Too narrow, up to
    /// a limit: the flexible column takes the rest. Past what can't be made to fit
    /// a line is cut by whoever draws it.
    pub fn grid(&self, width: usize) -> Grid {
        let mut widths = self.natural_widths();
        let gaps = GAP * widths.len().saturating_sub(1);
        let total = widths.iter().sum::<usize>() + gaps;
        let mut over = total.saturating_sub(width);
        for (column, least) in self.shrink_order(&widths) {
            let give = over.min(widths[column].saturating_sub(least));
            widths[column] -= give;
            over -= give;
        }
        if over == 0 && self.flex < widths.len() {
            widths[self.flex] += width.min(STRETCH_MAX).saturating_sub(total);
        }
        let fit = |text: &str, column: usize| shorten(text, widths[column]);
        Grid {
            header: self
                .titles
                .iter()
                .enumerate()
                .map(|(c, title)| fit(title, c))
                .collect(),
            rows: self
                .rows
                .iter()
                .map(|row| {
                    row.iter()
                        .enumerate()
                        .map(|(c, cell)| Cell::new(fit(&cell.text, c), cell.tone))
                        .collect()
                })
                .collect(),
        }
        .padded(&widths)
    }

    /// How wide each column is when nothing is shortened: its longest cell, or its
    /// title when that is longer.
    fn natural_widths(&self) -> Vec<usize> {
        let columns = self
            .rows
            .iter()
            .map(Vec::len)
            .chain([self.titles.len()])
            .max()
            .unwrap_or(0);
        (0..columns)
            .map(|c| {
                let title = self.titles.get(c).map_or(0, |t| t.chars().count());
                self.rows
                    .iter()
                    .filter_map(|r| r.get(c))
                    .map(|cell| cell.text.chars().count())
                    .chain([title])
                    .max()
                    .unwrap_or(0)
            })
            .collect()
    }

    /// The columns to shorten when the width is short, and the least each may be
    /// shortened to: the flexible one first, then the others by how wide they
    /// are, widest first. The last column is never shortened.
    fn shrink_order(&self, widths: &[usize]) -> Vec<(usize, usize)> {
        let last = widths.len().saturating_sub(1);
        let mut others: Vec<usize> = (0..last).filter(|&c| c != self.flex).collect();
        others.sort_by_key(|&c| std::cmp::Reverse(widths[c]));
        let mut order = Vec::new();
        if self.flex < last {
            order.push((self.flex, FLEX_MIN));
        }
        order.extend(others.into_iter().map(|c| (c, OTHER_MIN)));
        order
    }
}

impl Grid {
    /// Every cell but the last of a row padded to its column, so what follows it
    /// starts in one place in every row.
    fn padded(mut self, widths: &[usize]) -> Self {
        let last = widths.len().saturating_sub(1);
        let pad = |text: &mut String, column: usize| {
            if column < last {
                let missing = widths[column].saturating_sub(text.chars().count());
                text.extend(std::iter::repeat_n(' ', missing));
            }
        };
        for (c, text) in self.header.iter_mut().enumerate() {
            pad(text, c);
        }
        for row in &mut self.rows {
            for (c, cell) in row.iter_mut().enumerate() {
                pad(&mut cell.text, c);
            }
        }
        self
    }
}

/// The cells of a row as one line: padded to `widths`, a gap between, and no
/// trailing space.
fn join<'a>(cells: impl Iterator<Item = &'a str>, widths: &[usize]) -> String {
    let line: Vec<String> = widths
        .iter()
        .zip(cells.chain(std::iter::repeat("")))
        .map(|(w, text)| format!("{text:<w$}"))
        .collect();
    line.join(&" ".repeat(GAP)).trim_end().to_string()
}

/// `text` in `width` characters: as it is when it fits, else with `…` in the
/// middle, a third of what is kept from the start and the rest from the end.
fn shorten(text: &str, width: usize) -> String {
    let length = text.chars().count();
    if length <= width {
        return text.to_string();
    }
    let kept = width.saturating_sub(1);
    let head = kept / 3;
    let tail = kept - head;
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(length - tail).collect();
    format!("{start}…{end}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cells: [&str; 4]) -> Vec<Cell> {
        cells.map(|c| Cell::new(c, Tone::Normal)).to_vec()
    }

    fn titles() -> Vec<&'static str> {
        vec!["NAME", "WHERE", "VIA", "LAST"]
    }

    fn table() -> Table {
        Table::new(
            titles(),
            vec![
                row([
                    "Demo API",
                    "/home/me/devel/projects/api",
                    "docker",
                    "2 h ago",
                ]),
                row(["db", "container db", "podman on me@box", "1 d ago"]),
            ],
            1,
        )
    }

    /// A row of the grid as one line, the way the cells are shown one after the other.
    fn line(cells: &[Cell]) -> String {
        cells
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("  ")
    }

    fn lines(grid: &Grid) -> Vec<String> {
        grid.rows.iter().map(|r| line(r)).collect()
    }

    #[test]
    fn natural_lines_have_every_column_as_wide_as_its_longest_cell() {
        let lines = table().natural();
        assert_eq!(
            lines[0],
            "Demo API  /home/me/devel/projects/api  docker            2 h ago"
        );
        assert_eq!(
            lines[1],
            "db        container db                 podman on me@box  1 d ago"
        );
    }

    #[test]
    fn a_title_that_is_longer_than_its_cells_widens_the_column() {
        let table = Table::new(
            vec!["NAME", "WHERE", "A LONG TITLE"],
            vec![vec![
                Cell::new("a", Tone::Normal),
                Cell::new("b", Tone::Normal),
                Cell::new("c", Tone::Normal),
            ]],
            1,
        );
        assert_eq!(table.natural(), ["a     b      c"]);
        assert_eq!(table.grid(40).header[2], "A LONG TITLE");
    }

    #[test]
    fn extra_room_goes_to_the_flexible_column_so_the_rest_ends_at_the_edge() {
        let grid = table().grid(100);
        let lines = lines(&grid);
        // Both rows end in the same column, and it is the edge of the table.
        assert!(lines.iter().all(|l| l.chars().count() == 100), "{lines:?}");
        assert!(
            lines[0].contains("/home/me/devel/projects/api"),
            "{lines:?}"
        );
        assert!(lines[0].ends_with("docker            2 h ago"), "{lines:?}");
        assert!(lines[1].ends_with("podman on me@box  1 d ago"), "{lines:?}");
        assert_eq!(grid.header.len(), 4);
        assert!(grid.header[0].starts_with("NAME"));
    }

    #[test]
    fn a_very_wide_terminal_stops_at_a_width_a_row_can_be_read_across() {
        let lines = lines(&table().grid(300));
        assert!(
            lines.iter().all(|l| l.chars().count() == STRETCH_MAX),
            "{lines:?}"
        );
    }

    #[test]
    fn a_short_width_shortens_the_flexible_column_in_the_middle_and_keeps_the_rest() {
        let lines = lines(&table().grid(60));
        assert!(lines.iter().all(|l| l.chars().count() <= 60), "{lines:?}");
        assert!(
            lines[0].starts_with("Demo API  /home/")
                && lines[0].contains('…')
                && lines[0].contains("/api"),
            "{lines:?}"
        );
        assert!(lines[0].ends_with("docker            2 h ago"), "{lines:?}");
        assert!(lines[1].ends_with("podman on me@box  1 d ago"), "{lines:?}");
    }

    #[test]
    fn when_that_is_not_enough_the_other_columns_give_way_widest_first_and_the_last_never() {
        let table = Table::new(
            titles(),
            vec![row([
                "a long project name",
                "/some/long/path/here",
                "docker",
                "2 h ago",
            ])],
            1,
        );
        let lines = lines(&table.grid(40));
        assert!(lines[0].chars().count() <= 40, "{lines:?}");
        assert!(lines[0].ends_with("docker  2 h ago"), "{lines:?}");
        assert!(lines[0].contains('…'), "{lines:?}");
    }

    #[test]
    fn a_wide_route_is_shortened_before_the_time_is_lost() {
        let table = Table::new(
            titles(),
            vec![row([
                "Demo API",
                "/home/me/projects/api",
                "podman on me@buildbox",
                "2 h ago",
            ])],
            1,
        );
        let lines = lines(&table.grid(48));
        assert!(lines[0].chars().count() <= 48, "{lines:?}");
        assert!(lines[0].ends_with("2 h ago"), "{lines:?}");
        // Cut in the middle: it still starts as the engine does and ends with the host.
        assert!(
            lines[0].contains("podm…") && lines[0].contains("buildbox"),
            "{lines:?}"
        );
    }

    #[test]
    fn what_cannot_be_made_to_fit_is_left_for_the_one_who_draws_to_cut() {
        let lines = lines(&table().grid(10));
        assert!(lines[0].chars().count() > 10);
        assert!(lines[0].ends_with("2 h ago"));
    }

    #[test]
    fn the_tone_of_a_cell_survives_the_layout() {
        let table = Table::new(
            vec!["A", "B"],
            vec![vec![
                Cell::new("one", Tone::Strong),
                Cell::new("two", Tone::Accent),
            ]],
            1,
        );
        let tones: Vec<Tone> = table.grid(30).rows[0].iter().map(|c| c.tone).collect();
        assert_eq!(tones, [Tone::Strong, Tone::Accent]);
    }

    #[test]
    fn shortening_keeps_the_start_and_more_of_the_end() {
        assert_eq!(shorten("short", 10), "short");
        assert_eq!(shorten("/home/me/devel/api", 10), "/ho…el/api");
        assert_eq!(shorten("abcdefghij", 1), "…");
        assert_eq!(shorten("abcdefghij", 0), "…");
    }

    #[test]
    fn a_short_row_and_no_rows_are_fine() {
        let table = Table::new(
            vec!["A", "B"],
            vec![
                vec![Cell::new("only", Tone::Normal)],
                vec![Cell::new("a", Tone::Normal), Cell::new("b", Tone::Normal)],
            ],
            1,
        );
        assert_eq!(table.natural(), ["only", "a     b"]);
        let none = Table::new(vec!["A"], Vec::new(), 0);
        assert!(none.is_empty() && none.grid(10).rows.is_empty());
        assert!(none.natural().is_empty());
    }
}
