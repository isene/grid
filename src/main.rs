//! grid — an AI-native TUI spreadsheet for the Fe2O3 suite.
//!
//! This is the CSV-first MVP: load a file, navigate the grid, edit cells,
//! save. Reading xlsx/ods (calamine), the formula engine, and Claude-powered
//! editing land in later increments. Input is a blocking read — the loop
//! sleeps until a key arrives, so idle cost is zero.

mod ai;
mod eval;
mod io;
mod model;

use std::path::PathBuf;

use crust::{style, Crust, Input, Pane};
use model::{cell_ref, col_name, Book, Value};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const COL_W: usize = 10; // fixed column width (display columns)

/// Keybindings shown by the `?` popup.
const KEYS: &[(&str, &str)] = &[
    ("h j k l", "move left / down / up / right (or arrow keys)"),
    ("g  G", "jump to first / last row"),
    ("0  $", "jump to first / last column"),
    ("PgUp PgDn", "page up / down"),
    ("Home End", "first / last column"),
    ("Enter  i", "edit the current cell"),
    ("=", "start a formula in the current cell"),
    ("c", "AI edit \u{2014} change the sheet by instruction (claude)"),
    ("C", "set cell colour (fg,bg)"),
    ("u", "undo the last edit"),
    ("d  Del", "clear the current cell"),
    ("Tab S-Tab", "next / previous sheet"),
    ("s", "save"),
    ("?", "show this help"),
    ("q", "quit (prompts if there are unsaved changes)"),
    ("Q", "quit without saving"),
];

struct App {
    book: Book,
    cur_row: u32,
    cur_col: u32,
    top_row: u32, // first visible data row (scroll)
    top_col: u32, // first visible column
    term_w: u16,
    term_h: u16,
    top: Pane,  // formula / status bar (row 1)
    body: Pane, // the grid (rows 2..h-1)
    foot: Pane, // key hints / prompts (row h)
    status: String,
    undo: Vec<(usize, model::Sheet)>, // (active index, sheet snapshot) before each edit
}

impl App {
    fn new(book: Book) -> Self {
        let (term_w, term_h) = Crust::terminal_size();
        let (top, body, foot) = make_panes(term_w, term_h);
        App {
            book,
            cur_row: 0,
            cur_col: 0,
            top_row: 0,
            top_col: 0,
            term_w,
            term_h,
            top,
            body,
            foot,
            status: String::new(),
            undo: Vec::new(),
        }
    }

    /// Snapshot the active sheet before a mutation (bounded stack).
    fn snapshot(&mut self) {
        const UNDO_CAP: usize = 100;
        if self.undo.len() >= UNDO_CAP {
            self.undo.remove(0);
        }
        self.undo.push((self.book.active, self.book.sheet().clone()));
    }

    /// Restore the most recent snapshot.
    fn undo(&mut self) {
        match self.undo.pop() {
            Some((idx, sheet)) => {
                self.book.active = idx.min(self.book.sheets.len().saturating_sub(1));
                *self.book.sheet_mut() = sheet;
                self.book.dirty = true;
                let s = self.book.sheet();
                self.cur_row = self.cur_row.min(s.nrows);
                self.cur_col = self.cur_col.min(s.ncols);
                self.status = "Undone".into();
            }
            None => self.status = "Nothing to undo".into(),
        }
    }

    /// Rebuild panes after a terminal resize.
    fn resize(&mut self) {
        let (w, h) = Crust::terminal_size();
        if w == self.term_w && h == self.term_h {
            return;
        }
        self.term_w = w;
        self.term_h = h;
        let (top, body, foot) = make_panes(w, h);
        self.top = top;
        self.body = body;
        self.foot = foot;
        Crust::clear_screen();
    }

    /// Rows of data that fit in the body (minus the header line).
    fn data_rows(&self) -> usize {
        (self.term_h.saturating_sub(2)) as usize - 1
    }

    /// Gutter (row-number column) width, stable across small scrolls.
    fn gutter(&self) -> usize {
        let sheet = self.book.sheet();
        let max_n = sheet.nrows.max(self.cur_row + 1).max(1);
        (max_n.to_string().len() + 1).max(4)
    }

    /// Visible column count given the gutter and fixed column width.
    fn vis_cols(&self) -> usize {
        let avail = (self.term_w as usize).saturating_sub(self.gutter());
        (avail / COL_W).max(1)
    }

    /// Keep the cursor inside the visible window.
    fn clamp_scroll(&mut self) {
        let dr = self.data_rows() as u32;
        let dc = self.vis_cols() as u32;
        if self.cur_row < self.top_row {
            self.top_row = self.cur_row;
        } else if self.cur_row >= self.top_row + dr {
            self.top_row = self.cur_row + 1 - dr;
        }
        if self.cur_col < self.top_col {
            self.top_col = self.cur_col;
        } else if self.cur_col >= self.top_col + dc {
            self.top_col = self.cur_col + 1 - dc;
        }
    }

    fn render(&mut self) {
        self.clamp_scroll();
        let gutter = self.gutter();
        let vcols = self.vis_cols();
        let data_rows = self.data_rows();
        let sheet = self.book.sheet();

        // --- formula / status bar ---
        let cref = cell_ref(self.cur_row, self.cur_col);
        let raw = sheet.raw(self.cur_row, self.cur_col);
        let dirty = if self.book.dirty { " [+]" } else { "" };
        let nsheets = self.book.sheets.len();
        let tag = if nsheets > 1 {
            format!("  [{} {}/{}]", self.book.sheet().name, self.book.active + 1, nsheets)
        } else {
            String::new()
        };
        let topline = format!("{}{}{}  {}", style::coded(&cref, ",,b"), dirty, tag, raw);
        self.top.say(&topline);

        // --- grid ---
        let mut out = String::new();
        // header row: blank gutter, then column letters
        out.push_str(&" ".repeat(gutter));
        for vc in 0..vcols {
            let c = self.top_col + vc as u32;
            let name = center(&col_name(c), COL_W);
            // coded() terminates with a full reset, so the pane restores its
            // own colours after each cell — the current column gets the accent.
            out.push_str(&style::coded(&name, if c == self.cur_col { "11,,b" } else { ",,b" }));
        }
        out.push('\n');

        for dr in 0..data_rows {
            let r = self.top_row + dr as u32;
            // row-number gutter (right aligned, 1-space separator)
            let num = format!("{:>w$} ", r + 1, w = gutter - 1);
            if r == self.cur_row {
                out.push_str(&style::coded(&num, "11,,b"));
            } else {
                out.push_str(&num);
            }
            for vc in 0..vcols {
                let c = self.top_col + vc as u32;
                let val = sheet.value(r, c);
                let cell = fit_cell(&val, COL_W);
                let (cfg, cbg) = sheet.colors(r, c);
                if r == self.cur_row && c == self.cur_col {
                    if matches!(val, Value::Empty) {
                        // A bg colour over pure whitespace gets dropped (no glyph
                        // to anchor the run). Paint a solid white bar instead — it
                        // always renders and reads as a white cell.
                        out.push_str(&style::coded(&"\u{2588}".repeat(COL_W), "15"));
                    } else {
                        // Cursor cell: black on bright white, filling the whole cell.
                        out.push_str(&style::coded(&cell, "0,15"));
                    }
                } else if cfg.is_some() || cbg.is_some() {
                    if matches!(val, Value::Empty) {
                        // Colour over an empty cell: a solid bar in the bg colour
                        // (anchors the run); fg-only on an empty cell shows nothing.
                        match cbg {
                            Some(bg) => out.push_str(&style::coded(&"\u{2588}".repeat(COL_W), &bg.to_string())),
                            None => out.push_str(&cell),
                        }
                    } else {
                        out.push_str(&style::coded(&cell, &color_spec(cfg, cbg)));
                    }
                } else {
                    out.push_str(&cell);
                }
            }
            if dr + 1 < data_rows {
                out.push('\n');
            }
        }
        self.body.say(&out);

        // --- foot ---
        let foot = if self.status.is_empty() {
            format!(
                " hjkl/\u{2191}\u{2193} move  Enter edit  = formula  c AI  C colour  u undo  d clear  Tab sheet  s save  q quit   grid {}",
                VERSION
            )
        } else {
            format!(" {}", self.status)
        };
        self.foot.say(&foot);
    }

    fn edit_cell(&mut self, initial: Option<&str>) {
        let cref = cell_ref(self.cur_row, self.cur_col);
        let cur = match initial {
            Some(s) => s.to_string(),
            None => self.book.sheet().raw(self.cur_row, self.cur_col).to_string(),
        };
        if let Some(v) = self.foot.ask_or_cancel(&format!("{}: ", cref), &cur) {
            if v == cur {
                return; // no change — don't pollute the undo stack
            }
            self.snapshot();
            self.book.sheet_mut().set(self.cur_row, self.cur_col, v);
            self.book.dirty = true;
            eval::recalc(self.book.sheet_mut());
        }
    }

    /// Set the current cell's foreground/background colour. Prompts for a
    /// "fg,bg" spec (palette 0-255 or a colour name; blank clears).
    fn set_color(&mut self) {
        let (fg, bg) = self.book.sheet().colors(self.cur_row, self.cur_col);
        let cur = if fg.is_some() || bg.is_some() { color_spec(fg, bg) } else { String::new() };
        let prompt = "Cell colour fg,bg (0-255 or name; blank clears): ";
        if let Some(input) = self.foot.ask_or_cancel(prompt, &cur) {
            let (fg, bg) = parse_color_spec(&input);
            self.snapshot();
            self.book.sheet_mut().set_colors(self.cur_row, self.cur_col, fg, bg);
            self.book.dirty = true;
        }
    }

    fn clear_cell(&mut self) {
        if !self.book.sheet().raw(self.cur_row, self.cur_col).is_empty() {
            self.snapshot();
            self.book.sheet_mut().set(self.cur_row, self.cur_col, String::new());
            self.book.dirty = true;
            eval::recalc(self.book.sheet_mut());
        }
    }

    /// AI edit: prompt for an instruction, hand the whole sheet to `claude -p`
    /// as CSV, and replace the active sheet with what comes back. Blocks while
    /// Claude runs (shown as "AI working…") — fine, since input is blocking
    /// anyway and this only fires on the `c` key.
    fn ai_edit(&mut self) {
        let instr = match self.foot.ask_or_cancel("AI edit: ", "") {
            Some(s) if !s.trim().is_empty() => s,
            _ => return,
        };
        self.foot.say(" AI working\u{2026} ");
        let csv = io::sheet_to_csv(self.book.sheet());
        let prompt = ai::build_prompt(&csv, &instr);
        match ai::run_claude(&prompt).map(|r| ai::extract_csv(&r)) {
            Ok(Some(body)) => {
                self.snapshot();
                let name = self.book.sheet().name.clone();
                *self.book.sheet_mut() = io::csv_to_sheet(&body, name);
                eval::recalc(self.book.sheet_mut());
                self.book.dirty = true;
                let sheet = self.book.sheet();
                self.cur_row = self.cur_row.min(sheet.nrows);
                self.cur_col = self.cur_col.min(sheet.ncols);
                self.top_row = 0;
                self.top_col = 0;
                self.status = "AI edit applied".into();
            }
            Ok(None) => self.status = "AI: no usable CSV in the reply".into(),
            Err(e) => self.status = format!("AI: {}", e),
        }
        self.top.invalidate();
        self.body.invalidate();
        self.foot.invalidate();
    }

    /// Centred key-help popup (any key closes it), mirroring the other
    /// Fe2O3 TUIs. coded() keeps the styling reset-clean so the bordered
    /// pane composes correctly; afterwards the main panes are invalidated
    /// so the next render fully repaints over the popup.
    fn show_help(&mut self) {
        let (cols, rows) = (self.term_w, self.term_h);
        let kw = KEYS.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(8);
        let cw = KEYS
            .iter()
            .map(|(_k, d)| kw + 2 + d.chars().count())
            .max()
            .unwrap_or(30);
        let pw = ((cw as u16) + 4).min(cols.saturating_sub(2)).max(24);
        let ph = ((KEYS.len() as u16) + 5).min(rows.saturating_sub(2)).max(6);
        let px = cols.saturating_sub(pw) / 2 + 1;
        let py = rows.saturating_sub(ph) / 2 + 1;
        let mut pane = Pane::new(px, py, pw, ph, 7, 0);
        pane.scroll = false;
        pane.wrap = false;
        pane.border = true;
        pane.border_fg = Some(11);
        let mut s = style::coded(" grid \u{2014} keys", "11,,b");
        s.push_str("\n\n");
        for (k, d) in KEYS {
            s.push_str(&format!(" {}  {}\n", style::coded(&format!("{:>kw$}", k), "14,,b"), d));
        }
        s.push_str("\n Press any key to close.");
        pane.say(&s);
        pane.border_refresh();
        let _ = Input::getchr(None);
        self.top.invalidate();
        self.body.invalidate();
        self.foot.invalidate();
    }

    /// Cycle to another sheet (wraps), resetting the cursor to A1.
    fn switch_sheet(&mut self, delta: i32) {
        let n = self.book.sheets.len() as i32;
        if n <= 1 {
            return;
        }
        let cur = self.book.active as i32;
        self.book.active = (((cur + delta) % n + n) % n) as usize;
        self.cur_row = 0;
        self.cur_col = 0;
        self.top_row = 0;
        self.top_col = 0;
    }

    fn save(&mut self) {
        match io::save(&self.book) {
            Ok(()) => {
                self.book.dirty = false;
                self.status = format!("Saved {}", self.book.path.display());
            }
            Err(e) => self.status = format!("Save failed: {}", e),
        }
    }

    /// Returns false to quit.
    fn handle(&mut self, key: &str) -> bool {
        // A fresh keypress clears any one-shot status message.
        if !self.status.is_empty() {
            self.status.clear();
        }
        let sheet = self.book.sheet();
        let max_row = sheet.nrows; // one empty row past the last lets you append
        let max_col = sheet.ncols;
        match key {
            "q" => return self.confirm_quit(),
            "Q" => return false, // quit without saving, no prompt
            "h" | "LEFT" => self.cur_col = self.cur_col.saturating_sub(1),
            "l" | "RIGHT" => self.cur_col = (self.cur_col + 1).min(max_col),
            "k" | "UP" => self.cur_row = self.cur_row.saturating_sub(1),
            "j" | "DOWN" => self.cur_row = (self.cur_row + 1).min(max_row),
            "g" | "C-HOME" => self.cur_row = 0,
            "G" | "C-END" => self.cur_row = max_row.saturating_sub(1),
            "0" | "HOME" => self.cur_col = 0,
            "$" | "END" => self.cur_col = max_col.saturating_sub(1),
            "PgDOWN" => self.cur_row = (self.cur_row + self.data_rows() as u32).min(max_row),
            "PgUP" => self.cur_row = self.cur_row.saturating_sub(self.data_rows() as u32),
            "ENTER" | "i" => self.edit_cell(None),
            "=" => self.edit_cell(Some("=")),
            "c" => self.ai_edit(),
            "C" => self.set_color(),
            "u" => self.undo(),
            "TAB" => self.switch_sheet(1),
            "S-TAB" => self.switch_sheet(-1),
            "?" => self.show_help(),
            "d" | "DEL" => self.clear_cell(),
            "s" => self.save(),
            "RESIZE" => self.resize(),
            _ => {}
        }
        true
    }

    /// Prompt to save if dirty. Single keypress — no Enter needed.
    /// Returns false (quit) unless the user cancels with Esc.
    fn confirm_quit(&mut self) -> bool {
        if !self.book.dirty {
            return false;
        }
        self.foot.say(" Unsaved changes \u{2014} save? y / n   (Esc cancels) ");
        loop {
            match Input::getchr(None).as_deref() {
                Some("y") | Some("Y") => {
                    self.save();
                    return false;
                }
                Some("n") | Some("N") => return false,
                Some("ESC") => return true, // cancel — next render restores the foot
                _ => {}
            }
        }
    }
}

fn make_panes(w: u16, h: u16) -> (Pane, Pane, Pane) {
    let mut top = Pane::new(1, 1, w, 1, 7, 0);
    let mut body = Pane::new(1, 2, w, h.saturating_sub(2), 7, 0);
    let mut foot = Pane::new(1, h, w, 1, 0, 6);
    for p in [&mut top, &mut body, &mut foot] {
        p.wrap = false;
        p.word_wrap = false;
        p.scroll = false;
    }
    (top, body, foot)
}

/// Build a crust `coded` spec ("fg,bg") from optional palette colours.
fn color_spec(fg: Option<u8>, bg: Option<u8>) -> String {
    format!(
        "{},{}",
        fg.map(|n| n.to_string()).unwrap_or_default(),
        bg.map(|n| n.to_string()).unwrap_or_default()
    )
}

/// Parse one colour token: a 0-255 palette number, a common name, or blank (None).
fn parse_color(tok: &str) -> Option<u8> {
    let t = tok.trim().to_ascii_lowercase();
    if t.is_empty() {
        return None;
    }
    if let Ok(n) = t.parse::<u8>() {
        return Some(n);
    }
    Some(match t.as_str() {
        "black" => 0,
        "red" => 1,
        "green" => 2,
        "yellow" => 3,
        "blue" => 4,
        "magenta" => 5,
        "cyan" => 6,
        "white" => 7,
        "grey" | "gray" => 8,
        "orange" => 208,
        _ => return None,
    })
}

/// Parse a "fg,bg" colour spec into (fg, bg).
fn parse_color_spec(s: &str) -> (Option<u8>, Option<u8>) {
    let mut it = s.splitn(2, ',');
    let fg = it.next().and_then(parse_color);
    let bg = it.next().and_then(parse_color);
    (fg, bg)
}

/// Fit a cell value into exactly `w` display columns, reserving a 1-col gap.
/// Numbers right-align; everything else left-aligns. Long values truncate.
fn fit_cell(val: &Value, w: usize) -> String {
    let usable = w.saturating_sub(1);
    let s = val.display();
    let t = crust::truncate_ansi(&s, usable);
    let pad = usable.saturating_sub(crust::display_width(&t));
    if val.is_number() {
        format!("{}{} ", " ".repeat(pad), t)
    } else {
        format!("{}{} ", t, " ".repeat(pad))
    }
}

/// Center text in `w` columns (truncating if needed).
fn center(s: &str, w: usize) -> String {
    let sw = crust::display_width(s);
    if sw >= w {
        return crust::truncate_ansi(s, w);
    }
    let left = (w - sw) / 2;
    let right = w - sw - left;
    format!("{}{}{}", " ".repeat(left), s, " ".repeat(right))
}

fn main() {
    let path = std::env::args().nth(1).map(PathBuf::from);
    let mut book = match &path {
        Some(p) if p.exists() => match io::load(p) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("grid: cannot read {}: {}", p.display(), e);
                std::process::exit(1);
            }
        },
        Some(p) => Book::empty(p.clone()),
        None => {
            eprintln!("usage: grid <file.csv>");
            std::process::exit(1);
        }
    };

    for sheet in &mut book.sheets {
        eval::recalc(sheet);
    }

    Crust::init();
    Crust::set_title("grid");
    Crust::clear_screen();
    let mut app = App::new(book);
    loop {
        app.render();
        match Input::getchr(None) {
            Some(k) => {
                if !app.handle(&k) {
                    break;
                }
            }
            None => break,
        }
    }
    Crust::cleanup();
}
