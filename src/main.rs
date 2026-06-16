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
use std::process::Command;

use crust::{style, Crust, Input, Pane};
use model::{cell_ref, col_name, Book, Value};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const COL_W: usize = 10; // fixed column width (display columns)
const HEADER_BG: &str = "240"; // grey band behind the column headers + row gutter

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
    ("v", "start / stop a rectangular selection"),
    ("C", "set cell / selection colour (prism picker)"),
    ("D", "clear cell / selection colour"),
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
    sel_anchor: Option<(u32, u32)>,   // Some => a rectangular selection from here to the cursor
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
            sel_anchor: None,
        }
    }

    /// The selected rectangle as ((r0,c0),(r1,c1)) — just the cursor cell when
    /// no selection is active.
    fn selection_rect(&self) -> ((u32, u32), (u32, u32)) {
        match self.sel_anchor {
            Some((ar, ac)) => (
                (ar.min(self.cur_row), ac.min(self.cur_col)),
                (ar.max(self.cur_row), ac.max(self.cur_col)),
            ),
            None => ((self.cur_row, self.cur_col), (self.cur_row, self.cur_col)),
        }
    }

    fn in_selection(&self, r: u32, c: u32) -> bool {
        self.sel_anchor.is_some() && {
            let ((r0, c0), (r1, c1)) = self.selection_rect();
            r >= r0 && r <= r1 && c >= c0 && c <= c1
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
        self.sel_anchor = None;
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

        // --- grid (HEADER_BG = grey background for the header row + gutter) ---
        let mut out = String::new();
        // header row: grey corner, then column letters on a grey band.
        // (a bg over pure whitespace gets dropped, so the corner is a █ block)
        out.push_str(&style::coded(&"\u{2588}".repeat(gutter), HEADER_BG));
        for vc in 0..vcols {
            let c = self.top_col + vc as u32;
            let name = center(&col_name(c), COL_W);
            // coded() terminates with a full reset, so the pane restores its
            // own colours after each cell — the current column gets the accent.
            out.push_str(&style::coded(&name, if c == self.cur_col { "11,240,b" } else { "252,240,b" }));
        }
        out.push('\n');

        for dr in 0..data_rows {
            let r = self.top_row + dr as u32;
            // row-number gutter (right aligned, 1-space separator) on a grey band
            let num = format!("{:>w$} ", r + 1, w = gutter - 1);
            if r == self.cur_row {
                out.push_str(&style::coded(&num, "11,240,b"));
            } else {
                out.push_str(&style::coded(&num, "252,240"));
            }
            for vc in 0..vcols {
                let c = self.top_col + vc as u32;
                let val = sheet.value(r, c);
                let cell = fit_cell(&val, COL_W);
                let (cfg, cbg) = sheet.colors(r, c);
                if r == self.cur_row && c == self.cur_col {
                    // Cursor cell: black on bright white, full-width.
                    out.push_str(&hl_cell_pal(&cell, 0, 15));
                } else if self.in_selection(r, c) {
                    // Selection band (grey), overrides the cell's own colour.
                    out.push_str(&hl_cell_pal(&cell, 0, 245));
                } else if let Some(bg) = cbg {
                    // Coloured cell with a background, full-width.
                    out.push_str(&hl_cell_rgb(&cell, cfg, bg));
                } else if cfg.is_some() {
                    // Foreground-only: no background to fill, so no anchor needed.
                    out.push_str(&style::coded_rgb(&cell, cfg, None));
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
                " hjkl move  Enter edit  = formula  v select  C colour  D clr  c AI  u undo  d clear  s save  ? keys  q quit   grid {}",
                VERSION
            )
        } else {
            format!(" {}", self.status)
        };
        self.foot.say(&foot);
    }

    fn edit_cell(&mut self, initial: Option<&str>) {
        self.sel_anchor = None;
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

    /// Set colour on the current cell or selection — the prism picker if
    /// available, else a "fg,bg" text prompt. Applies the chosen pair to the
    /// whole selection rectangle.
    fn set_color(&mut self) {
        let (cf, cb) = self.book.sheet().colors(self.cur_row, self.cur_col);
        if let Some((fg, bg)) = self.pick_color_prism(cf, cb).or_else(|| self.prompt_color(cf, cb)) {
            self.apply_colors(fg, bg);
        }
    }

    /// Apply (fg, bg) to every cell in the selection rect, then clear it.
    fn apply_colors(&mut self, fg: Option<model::Rgb>, bg: Option<model::Rgb>) {
        self.snapshot();
        let ((r0, c0), (r1, c1)) = self.selection_rect();
        for r in r0..=r1 {
            for c in c0..=c1 {
                self.book.sheet_mut().set_colors(r, c, fg, bg);
            }
        }
        self.book.dirty = true;
        self.sel_anchor = None;
    }

    /// Clear colour on the current cell or selection.
    fn clear_color(&mut self) {
        self.apply_colors(None, None);
        self.status = "Colour cleared".into();
    }

    /// Launch prism (preloaded with fg/bg) as a picker; returns the chosen pair,
    /// or None when prism can't be launched (caller falls back to the prompt).
    /// prism writes its result to a file (`--out`) so its UI and grid don't clash.
    fn pick_color_prism(
        &mut self,
        fg: Option<model::Rgb>,
        bg: Option<model::Rgb>,
    ) -> Option<(Option<model::Rgb>, Option<model::Rgb>)> {
        let outfile = format!("/tmp/grid_pick_{}.txt", std::process::id());
        let _ = std::fs::remove_file(&outfile);
        let fg_hex = fg.map(rgb_hex).unwrap_or_else(|| "#ffffff".into());
        let bg_hex = bg.map(rgb_hex).unwrap_or_else(|| "#000000".into());
        Crust::cleanup();
        let status = Command::new("prism")
            .arg("--pair")
            .arg(format!("--out={}", outfile))
            .arg(&fg_hex)
            .arg(&bg_hex)
            .status();
        Crust::init();
        Crust::clear_screen();
        self.top.invalidate();
        self.body.invalidate();
        self.foot.invalidate();
        if status.is_err() {
            let _ = std::fs::remove_file(&outfile);
            return None; // prism not on PATH → caller uses the text prompt
        }
        let mut nfg = fg;
        let mut nbg = bg;
        if let Ok(text) = std::fs::read_to_string(&outfile) {
            for line in text.lines() {
                if let Some(h) = line.strip_prefix("fg=") {
                    nfg = style::parse_hex_color(h.trim());
                } else if let Some(h) = line.strip_prefix("bg=") {
                    nbg = style::parse_hex_color(h.trim());
                }
            }
        }
        let _ = std::fs::remove_file(&outfile);
        Some((nfg, nbg))
    }

    /// Text fallback: "fg,bg" (hex `#rrggbb` or a colour name; blank clears).
    fn prompt_color(
        &mut self,
        fg: Option<model::Rgb>,
        bg: Option<model::Rgb>,
    ) -> Option<(Option<model::Rgb>, Option<model::Rgb>)> {
        let cur = if fg.is_some() || bg.is_some() { color_spec(fg, bg) } else { String::new() };
        let prompt = "Cell colour fg,bg (#rrggbb or name; blank clears): ";
        self.foot.ask_or_cancel(prompt, &cur).map(|s| parse_color_spec(&s))
    }

    fn clear_cell(&mut self) {
        let ((r0, c0), (r1, c1)) = self.selection_rect();
        let any = (r0..=r1).any(|r| (c0..=c1).any(|c| !self.book.sheet().raw(r, c).is_empty()));
        if any {
            self.snapshot();
            for r in r0..=r1 {
                for c in c0..=c1 {
                    self.book.sheet_mut().set(r, c, String::new());
                }
            }
            self.book.dirty = true;
            eval::recalc(self.book.sheet_mut());
        }
        self.sel_anchor = None;
    }

    /// AI edit: prompt for an instruction, hand the whole sheet to `claude -p`
    /// as CSV, and replace the active sheet with what comes back. Blocks while
    /// Claude runs (shown as "AI working…") — fine, since input is blocking
    /// anyway and this only fires on the `c` key.
    fn ai_edit(&mut self) {
        self.sel_anchor = None;
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
        self.sel_anchor = None;
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
            "D" => self.clear_color(),
            "v" => {
                self.sel_anchor = if self.sel_anchor.is_some() {
                    None
                } else {
                    Some((self.cur_row, self.cur_col))
                };
            }
            "ESC" => self.sel_anchor = None,
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

/// Render a fitted cell with a background, filling its trailing padding with a
/// solid block in the bg colour. A terminal drops a bg over trailing whitespace
/// (nothing anchors the run after the last glyph), so left-aligned text would
/// only highlight its text; the block fixes that. (Palette colours.)
fn hl_cell_pal(cell: &str, fg: u8, bg: u8) -> String {
    let head = cell.trim_end_matches(' ');
    let tail = cell.chars().count() - head.chars().count();
    let mut s = String::new();
    if !head.is_empty() {
        s.push_str(&style::coded(head, &format!("{},{}", fg, bg)));
    }
    if tail > 0 {
        s.push_str(&style::coded(&"\u{2588}".repeat(tail), &bg.to_string()));
    }
    s
}

/// Same as `hl_cell_pal` for truecolor cells (the trailing block is the bg RGB).
fn hl_cell_rgb(cell: &str, fg: Option<model::Rgb>, bg: model::Rgb) -> String {
    let head = cell.trim_end_matches(' ');
    let tail = cell.chars().count() - head.chars().count();
    let mut s = String::new();
    if !head.is_empty() {
        s.push_str(&style::coded_rgb(head, fg, Some(bg)));
    }
    if tail > 0 {
        s.push_str(&style::coded_rgb(&"\u{2588}".repeat(tail), Some(bg), None));
    }
    s
}

/// `#rrggbb` for a colour, empty for None (for the text-prompt prefill).
fn rgb_hex(c: model::Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}
fn opt_hex(c: Option<model::Rgb>) -> String {
    c.map(rgb_hex).unwrap_or_default()
}

/// Build the "fg,bg" hex spec shown when re-editing a coloured cell.
fn color_spec(fg: Option<model::Rgb>, bg: Option<model::Rgb>) -> String {
    format!("{},{}", opt_hex(fg), opt_hex(bg))
}

/// Parse one colour token: `#rrggbb`/`rgb`, a common name, or blank (None).
fn parse_color(tok: &str) -> Option<model::Rgb> {
    let t = tok.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(rgb) = style::parse_hex_color(t) {
        return Some(rgb);
    }
    Some(match t.to_ascii_lowercase().as_str() {
        "black" => (0, 0, 0),
        "red" => (0xcc, 0, 0),
        "green" => (0, 0xaa, 0),
        "yellow" => (0xcc, 0xaa, 0),
        "blue" => (0, 0, 0xcc),
        "magenta" => (0xaa, 0, 0xaa),
        "cyan" => (0, 0xaa, 0xaa),
        "white" => (0xff, 0xff, 0xff),
        "grey" | "gray" => (0x88, 0x88, 0x88),
        "orange" => (0xf7, 0x4c, 0x00),
        _ => return None,
    })
}

/// Parse a "fg,bg" colour spec into (fg, bg).
fn parse_color_spec(s: &str) -> (Option<model::Rgb>, Option<model::Rgb>) {
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
