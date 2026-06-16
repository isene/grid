//! The spreadsheet data model: a Book of Sheets of Cells. Cells are stored
//! sparsely (only non-empty ones). A cell's `raw` is what the user typed —
//! a literal, or a `=formula`. The formula engine (added next) fills `value`.

use std::collections::BTreeMap;

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

#[derive(Clone, Default)]
pub struct Cell {
    /// Exactly what the user typed (literal or `=formula`).
    pub raw: String,
    /// Computed value for display (set by the recalc pass).
    pub value: Value,
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
    /// Set a cell's raw text (empty clears it). Grows the extent. Does NOT
    /// recompute — the caller runs recalc after a batch of edits.
    pub fn set(&mut self, r: u32, c: u32, raw: String) {
        if raw.is_empty() {
            self.cells.remove(&(r, c));
        } else {
            let cell = self.cells.entry((r, c)).or_default();
            cell.raw = raw;
        }
        self.nrows = self.nrows.max(r + 1);
        self.ncols = self.ncols.max(c + 1);
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
