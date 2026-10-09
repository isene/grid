//! The spreadsheet data model: a Book of Sheets of Cells. Cells are stored
//! sparsely (only non-empty ones). A cell's `raw` is what the user typed —
//! a literal, or a `=formula`. The formula engine (added next) fills `value`.

use std::collections::BTreeMap;

use crate::eval::{rewrite_refs, Ref};

/// A computed cell value, ready to display.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Empty,
    Number(f64),
    Text(String),
    Error(String),
}

impl Value {
    /// Display string for the grid (numbers trimmed, errors as #...).
    pub fn display(&self) -> String {
        match self {
            Value::Empty => String::new(),
            Value::Text(s) => s.clone(),
            Value::Error(e) => format!("#{}", e),
            Value::Number(n) => fmt_number(*n),
        }
    }
    pub fn is_number(&self) -> bool { matches!(self, Value::Number(_)) }
}

/// Trim a float to a tidy string (integers without `.0`, else up to 10 sig).
pub fn fmt_number(n: f64) -> String {
    if n == n.trunc() && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        let s = format!("{:.10}", n);
        let s = s.trim_end_matches('0').trim_end_matches('.');
        s.to_string()
    }
}

/// 24-bit colour.
pub type Rgb = (u8, u8, u8);

#[derive(Clone, Default)]
pub struct Cell {
    /// Exactly what the user typed (literal or `=formula`).
    pub raw: String,
    /// Computed value for display (set by the recalc pass).
    pub value: Value,
    /// Optional truecolor foreground / background (cell formatting).
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
}

impl Cell {
    /// A cell carries nothing worth keeping (no text, no colour).
    fn is_blank(&self) -> bool {
        self.raw.is_empty() && self.fg.is_none() && self.bg.is_none()
    }
}

impl Default for Value {
    fn default() -> Self { Value::Empty }
}

#[derive(Default, Clone)]
pub struct Sheet {
    pub name: String,
    pub cells: BTreeMap<(u32, u32), Cell>, // (row, col), 0-based, sparse
    pub nrows: u32, // extent (one past the last used row)
    pub ncols: u32,
}

impl Sheet {
    pub fn raw(&self, r: u32, c: u32) -> &str {
        self.cells.get(&(r, c)).map(|c| c.raw.as_str()).unwrap_or("")
    }
    pub fn value(&self, r: u32, c: u32) -> Value {
        self.cells.get(&(r, c)).map(|c| c.value.clone()).unwrap_or(Value::Empty)
    }
    /// A cell's (fg, bg) colours, if any.
    pub fn colors(&self, r: u32, c: u32) -> (Option<Rgb>, Option<Rgb>) {
        self.cells.get(&(r, c)).map(|c| (c.fg, c.bg)).unwrap_or((None, None))
    }
    /// Set a cell's raw text. Grows the extent. Does NOT recompute — the caller
    /// runs recalc after a batch of edits. A cell with colour but no text is kept.
    pub fn set(&mut self, r: u32, c: u32, raw: String) {
        let cell = self.cells.entry((r, c)).or_default();
        cell.raw = raw;
        if cell.is_blank() {
            self.cells.remove(&(r, c));
        }
        self.nrows = self.nrows.max(r + 1);
        self.ncols = self.ncols.max(c + 1);
    }
    /// Set a cell's colours (None clears that slot). An otherwise-empty coloured
    /// cell is kept so you can highlight blank cells; clearing both removes it.
    pub fn set_colors(&mut self, r: u32, c: u32, fg: Option<Rgb>, bg: Option<Rgb>) {
        let cell = self.cells.entry((r, c)).or_default();
        cell.fg = fg;
        cell.bg = bg;
        if cell.is_blank() {
            self.cells.remove(&(r, c));
        }
        self.nrows = self.nrows.max(r + 1);
        self.ncols = self.ncols.max(c + 1);
    }
}

/// A block of copied cells, and where its top left corner was.
pub struct Clip {
    pub from: (u32, u32),
    pub rows: Vec<Vec<Cell>>,
}

/// A formula as it reads `down` rows and `right` columns from where it
/// was written: a cell two to the left is still two to the left. A `$`
/// pins a column or a row. A reference that would leave the sheet
/// becomes `#REF!`.
pub fn moved(raw: &str, down: i64, right: i64) -> String {
    let mv = |r: Ref| {
        let row = if r.pin_row { r.row as i64 } else { r.row as i64 + down };
        let col = if r.pin_col { r.col as i64 } else { r.col as i64 + right };
        Some(Ref { row: u32::try_from(row).ok()?, col: u32::try_from(col).ok()?, ..r })
    };
    rewrite_refs(raw, |a, b| {
        Some((mv(a)?, match b {
            Some(b) => Some(mv(b)?),
            None => None,
        }))
    })
}

impl Sheet {
    /// The cells of a rectangle, as they are.
    pub fn copy(&self, (r0, c0): (u32, u32), (r1, c1): (u32, u32)) -> Clip {
        let cell = |r, c| self.cells.get(&(r, c)).cloned().unwrap_or_default();
        Clip { from: (r0, c0), rows: (r0..=r1).map(|r| (c0..=c1).map(|c| cell(r, c)).collect()).collect() }
    }

    /// Put copied cells in the rectangle from `r0, c0` to `r1, c1`,
    /// over and over when the rectangle is bigger than the copy, so
    /// one copied cell fills them all. Formulas follow (see `moved`),
    /// and colours come along.
    pub fn paste(&mut self, clip: &Clip, (r0, c0): (u32, u32), (r1, c1): (u32, u32)) {
        for r in r0..=r1 {
            for c in c0..=c1 {
                let i = (r - r0) as usize % clip.rows.len();
                let j = (c - c0) as usize % clip.rows[i].len();
                let cell = &clip.rows[i][j];
                let down = r as i64 - (clip.from.0 as i64 + i as i64);
                let right = c as i64 - (clip.from.1 as i64 + j as i64);
                self.set(r, c, moved(&cell.raw, down, right));
                self.set_colors(r, c, cell.fg, cell.bg);
            }
        }
    }

    /// Insert `n` empty rows (or columns) before `at`, or delete the
    /// `n` from `at` on. Every formula in the sheet follows: a
    /// reference to a cell that moved points at it where it is now, a
    /// range grows or shrinks, and a reference to a deleted cell
    /// becomes `#REF!`.
    pub fn reshape(&mut self, rows: bool, at: u32, n: u32, insert: bool) {
        // Where a row or column ends up. None: it is deleted.
        let place = |k: u32| {
            if k < at {
                Some(k)
            } else if insert {
                Some(k.saturating_add(n))
            } else if k < at.saturating_add(n) {
                None
            } else {
                Some(k - n)
            }
        };
        let of = |r: Ref| if rows { r.row } else { r.col };
        let with = |r: Ref, k: u32| if rows { Ref { row: k, ..r } } else { Ref { col: k, ..r } };
        for ((r, c), mut cell) in std::mem::take(&mut self.cells) {
            let Some(key) = (if rows { place(r).map(|r| (r, c)) } else { place(c).map(|c| (r, c)) }) else {
                continue;
            };
            cell.raw = rewrite_refs(&cell.raw, |a, b| {
                let Some(b) = b else {
                    return Some((with(a, place(of(a))?), None));
                };
                // A range keeps what is left of it.
                let (lo, hi) = (of(a).min(of(b)), of(a).max(of(b)));
                let (new_lo, new_hi) = match (place(lo), place(hi)) {
                    (None, None) => return None,
                    (Some(lo), Some(hi)) => (lo, hi),
                    (None, Some(hi)) => (at, hi),
                    (Some(lo), None) => (lo, at - 1),
                };
                let (ka, kb) = if of(a) <= of(b) { (new_lo, new_hi) } else { (new_hi, new_lo) };
                Some((with(a, ka), Some(with(b, kb))))
            });
            self.cells.insert(key, cell);
        }
        let extent = if rows { &mut self.nrows } else { &mut self.ncols };
        if insert {
            if at < *extent {
                *extent = extent.saturating_add(n);
            }
        } else {
            *extent -= at.saturating_add(n).min(*extent) - at.min(*extent);
        }
    }

    /// Sort the rows `r0..=r1` by what column `col` shows in them:
    /// numbers first, the smallest first, then text by the alphabet,
    /// then errors. `descending` turns that around. Empty cells go
    /// last both ways. Whole rows move, and a formula that points into
    /// its own row points into the row it lands in.
    pub fn sort_rows(&mut self, r0: u32, r1: u32, col: u32, descending: bool) {
        use std::cmp::Ordering;
        let rank = |v: &Value| match v {
            Value::Number(_) => 0,
            Value::Text(_) => 1,
            Value::Error(_) => 2,
            Value::Empty => 3,
        };
        let mut order: Vec<u32> = (r0..=r1).collect();
        order.sort_by(|&a, &b| {
            let (x, y) = (self.value(a, col), self.value(b, col));
            if rank(&x) == 3 || rank(&y) == 3 {
                return rank(&x).cmp(&rank(&y));
            }
            let by = match (&x, &y) {
                (Value::Number(m), Value::Number(n)) => m.partial_cmp(n).unwrap_or(Ordering::Equal),
                (Value::Text(s), Value::Text(t)) => s.to_lowercase().cmp(&t.to_lowercase()),
                _ => rank(&x).cmp(&rank(&y)),
            };
            if descending { by.reverse() } else { by }
        });
        // Where each old row goes.
        let mut to = vec![0u32; order.len()];
        for (i, &old) in order.iter().enumerate() {
            to[(old - r0) as usize] = r0 + i as u32;
        }
        let mut rows = self.cells.split_off(&(r0, 0));
        let mut below = rows.split_off(&(r1.saturating_add(1), 0));
        self.cells.append(&mut below);
        for ((old, c), mut cell) in rows {
            let new = to[(old - r0) as usize];
            if new != old {
                cell.raw = rewrite_refs(&cell.raw, |a, b| {
                    let mv = |r: Ref| if r.row == old && !r.pin_row { Ref { row: new, ..r } } else { r };
                    Some((mv(a), b.map(mv)))
                });
            }
            self.cells.insert((new, c), cell);
        }
    }
}

pub struct Book {
    pub sheets: Vec<Sheet>,
    pub active: usize,
    pub path: std::path::PathBuf,
    /// True once the user edits, so quit can warn / save knows it's dirty.
    pub dirty: bool,
}

impl Book {
    pub fn sheet(&self) -> &Sheet { &self.sheets[self.active] }
    pub fn sheet_mut(&mut self) -> &mut Sheet { &mut self.sheets[self.active] }

    /// A fresh, empty book bound to `path` (for a not-yet-existing file).
    pub fn empty(path: std::path::PathBuf) -> Self {
        let sheet = Sheet { name: "Sheet1".into(), ..Default::default() };
        Book { sheets: vec![sheet], active: 0, path, dirty: false }
    }
}

/// 0-based column index → spreadsheet column letters (A, B, … Z, AA, AB, …).
pub fn col_name(mut c: u32) -> String {
    let mut s = String::new();
    loop {
        s.insert(0, (b'A' + (c % 26) as u8) as char);
        if c < 26 { break; }
        c = c / 26 - 1;
    }
    s
}

/// Parse column letters → 0-based index. None if not all A–Z.
pub fn col_index(s: &str) -> Option<u32> {
    if s.is_empty() { return None; }
    let mut c: u32 = 0;
    for ch in s.chars() {
        if !ch.is_ascii_alphabetic() { return None; }
        c = c.checked_mul(26)?.checked_add((ch.to_ascii_uppercase() as u32) - 'A' as u32 + 1)?;
    }
    Some(c - 1)
}

/// "A1"-style reference for a 0-based (row, col).
pub fn cell_ref(r: u32, c: u32) -> String { format!("{}{}", col_name(c), r + 1) }

/// Parse an "A1" reference → 0-based (row, col).
pub fn parse_ref(s: &str) -> Option<(u32, u32)> {
    let split = s.find(|ch: char| ch.is_ascii_digit())?;
    let (letters, digits) = s.split_at(split);
    let col = col_index(letters)?;
    let row: u32 = digits.parse().ok()?;
    if row == 0 { return None; }
    Some((row - 1, col))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::recalc;

    /// A sheet from rows of cells, worked out.
    fn sheet(rows: &[&[&str]]) -> Sheet {
        let mut s = Sheet::default();
        for (r, row) in rows.iter().enumerate() {
            for (c, raw) in row.iter().enumerate() {
                if !raw.is_empty() {
                    s.set(r as u32, c as u32, raw.to_string());
                }
            }
        }
        recalc(&mut s);
        s
    }

    /// What a sheet holds, row by row.
    fn raws(s: &Sheet) -> Vec<Vec<String>> {
        (0..s.nrows).map(|r| (0..s.ncols).map(|c| s.raw(r, c).to_string()).collect()).collect()
    }

    #[test]
    fn a_pasted_formula_points_the_same_way_from_its_new_cell() {
        assert_eq!(moved("=A1+B$1+$A2+$C$3", 2, 1), "=B3+C$1+$A4+$C$3");
        assert_eq!(moved("=SUM(A1:A3)", 0, 2), "=SUM(C1:C3)");
        // Up and out of the sheet: the reference is gone.
        assert_eq!(moved("=A1*2", -1, 0), "=#REF!*2");
        assert_eq!(moved("plain text A1", 5, 5), "plain text A1");
    }

    #[test]
    fn one_copied_cell_fills_a_selection() {
        let mut s = sheet(&[&["1", "10", "=A1*B1"], &["2", "20"], &["3", "30"]]);
        s.cells.get_mut(&(0, 2)).unwrap().bg = Some((1, 2, 3));
        let clip = s.copy((0, 2), (0, 2));
        s.paste(&clip, (1, 2), (2, 2));
        recalc(&mut s);
        assert_eq!(s.raw(1, 2), "=A2*B2");
        assert_eq!(s.raw(2, 2), "=A3*B3");
        assert_eq!(s.value(2, 2), Value::Number(90.0));
        assert_eq!(s.colors(2, 2), (None, Some((1, 2, 3))), "the colour comes along");
    }

    #[test]
    fn a_copied_block_lands_whole_and_covers_what_was_there() {
        let mut s = sheet(&[&["a", "b"], &["", "d"], &["x", "y"], &["z", "w"]]);
        let clip = s.copy((0, 0), (1, 1));
        s.paste(&clip, (2, 0), (3, 1));
        assert_eq!(raws(&s)[2..], [["a", "b"], ["", "d"]], "an empty copied cell empties its target");
    }

    #[test]
    fn rows_put_in_and_taken_out_keep_the_formulas_right() {
        let mut s = sheet(&[&["1"], &["2"], &["3"], &["=SUM(A1:A3)", "=A2*2", "=A1"]]);
        s.reshape(true, 1, 2, true);
        assert_eq!(s.nrows, 6);
        assert_eq!(s.raw(3, 0), "2", "the old row 2 is row 4 now");
        assert_eq!(s.raw(5, 0), "=SUM(A1:A5)", "a range over the new rows grows");
        assert_eq!(s.raw(5, 1), "=A4*2");
        assert_eq!(s.raw(5, 2), "=A1", "a cell above the new rows stays");
        s.reshape(true, 1, 2, false);
        assert_eq!(raws(&s), [vec!["1", "", ""], vec!["2", "", ""], vec!["3", "", ""], vec!["=SUM(A1:A3)", "=A2*2", "=A1"]]);
        // Row 2 goes: the range shrinks, the cell that pointed at it has nothing left.
        s.reshape(true, 1, 1, false);
        assert_eq!(raws(&s), [vec!["1", "", ""], vec!["3", "", ""], vec!["=SUM(A1:A2)", "=#REF!*2", "=A1"]]);
        recalc(&mut s);
        assert_eq!(s.value(2, 0), Value::Number(4.0));
        assert_eq!(s.value(2, 1), Value::Error("REF".into()));
    }

    #[test]
    fn columns_put_in_and_taken_out_keep_the_formulas_right() {
        let mut s = sheet(&[&["1", "2", "3", "=SUM(A1:C1)", "=$B$1"]]);
        s.reshape(false, 0, 1, true);
        assert_eq!(raws(&s), [["", "1", "2", "3", "=SUM(B1:D1)", "=$C$1"]]);
        // A range that is deleted whole is gone; one that loses its first column keeps the rest.
        s.reshape(false, 1, 1, false);
        assert_eq!(raws(&s), [["", "2", "3", "=SUM(B1:C1)", "=$B$1"]]);
        s.reshape(false, 1, 2, false);
        assert_eq!(raws(&s), [["", "=SUM(#REF!)", "=#REF!"]]);
        assert_eq!(s.ncols, 3);
    }

    #[test]
    fn rows_sort_by_a_column_and_take_their_formulas_along() {
        let rows: &[&[&str]] = &[
            &["pear", "3", "=B1*$B$6"],
            &["Apple", "10", "=B2*$B$6"],
            &["", "", ""],
            &["fig", "2", "=B4*$B$6"],
            &["12", "1", "=B5*$B$6"],
            &["rate", "2"],
        ];
        let mut s = sheet(rows);
        s.sort_rows(0, 4, 0, false);
        recalc(&mut s);
        let first = |s: &Sheet| (0..5).map(|r| s.raw(r, 0).to_string()).collect::<Vec<_>>();
        assert_eq!(first(&s), ["12", "Apple", "fig", "pear", ""], "numbers, then text without regard to capitals, empty last");
        assert_eq!(s.raw(3, 2), "=B4*$B$6", "pear went from row 1 to row 4 and its formula with it");
        assert_eq!(s.value(3, 2), Value::Number(6.0));
        assert_eq!(s.raw(5, 0), "rate", "the row below the sorted ones stays");
        s.sort_rows(0, 4, 1, true);
        recalc(&mut s);
        assert_eq!(first(&s), ["Apple", "pear", "fig", "12", ""], "largest first, and empty still last");
        assert_eq!(s.value(0, 2), Value::Number(20.0));
    }
}
