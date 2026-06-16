//! File load/save. CSV is native; xlsx/xlsm/xlsb/xls/ods read via calamine.
//! Writing those formats (rust_xlsxwriter) is the next increment, so saving
//! them is rejected rather than silently rewriting them as CSV. Dispatch is
//! by extension.

use std::io::{Error, ErrorKind};
use std::path::Path;

use crate::model::{fmt_number, Book, Rgb, Sheet};

const SPREADSHEET_EXTS: [&str; 5] = ["xlsx", "xlsm", "xlsb", "xls", "ods"];

pub fn load(path: &Path) -> std::io::Result<Book> {
    let e = ext(path);
    if SPREADSHEET_EXTS.contains(&e.as_str()) {
        load_calamine(path)
    } else {
        load_csv(path)
    }
}

pub fn save(book: &Book) -> std::io::Result<()> {
    match ext(&book.path).as_str() {
        "xlsx" => save_xlsx(book),
        // rust_xlsxwriter only writes .xlsx; the others stay read-only.
        e @ ("xlsm" | "xlsb" | "xls" | "ods") => Err(Error::new(
            ErrorKind::Other,
            format!("writing .{} not supported \u{2014} save as .xlsx or .csv", e),
        )),
        _ => save_csv(book),
    }
}

/// Write every sheet to a real .xlsx. Literals go out as numbers/strings;
/// `=formula` cells are written as formulas with their computed value cached,
/// so the file opens with correct values even before a recalc.
fn save_xlsx(book: &Book) -> std::io::Result<()> {
    use rust_xlsxwriter::{Color, Format, Formula, Workbook};
    let to_io = |e: rust_xlsxwriter::XlsxError| Error::new(ErrorKind::Other, e.to_string());
    let mut wb = Workbook::new();
    for sheet in &book.sheets {
        let ws = wb.add_worksheet();
        let _ = ws.set_name(&sheet.name); // ignore invalid/dup names → keep default
        for (&(r, c), cell) in &sheet.cells {
            let col = c as u16;
            // Build a cell format from its colours (xterm-256 → RGB).
            let fmt = if cell.fg.is_some() || cell.bg.is_some() {
                let rgb = |(r, g, b): Rgb| Color::RGB((r as u32) << 16 | (g as u32) << 8 | b as u32);
                let mut f = Format::new();
                if let Some(fg) = cell.fg {
                    f = f.set_font_color(rgb(fg));
                }
                if let Some(bg) = cell.bg {
                    f = f.set_background_color(rgb(bg));
                }
                Some(f)
            } else {
                None
            };
            let res = if let Some(expr) = cell.raw.strip_prefix('=') {
                let formula = Formula::new(expr).set_result(cell.value.display());
                match &fmt {
                    Some(f) => ws.write_formula_with_format(r, col, formula, f),
                    None => ws.write_formula(r, col, formula),
                }
            } else if let Ok(n) = cell.raw.parse::<f64>() {
                match &fmt {
                    Some(f) => ws.write_number_with_format(r, col, n, f),
                    None => ws.write_number(r, col, n),
                }
            } else {
                match &fmt {
                    Some(f) => ws.write_string_with_format(r, col, &cell.raw, f),
                    None => ws.write_string(r, col, &cell.raw),
                }
            };
            res.map_err(to_io)?;
        }
    }
    wb.save(&book.path).map_err(to_io)?;
    Ok(())
}


/// Read a workbook with calamine. Every sheet is loaded (cycle with Tab);
/// cells carry their *computed* values as literals. Formula text and cell
/// formats (so dates show as serials for now) are not yet preserved.
fn load_calamine(path: &Path) -> std::io::Result<Book> {
    use calamine::{open_workbook_auto, Reader};
    let mut wb = open_workbook_auto(path).map_err(|e| Error::new(ErrorKind::Other, e.to_string()))?;
    let mut sheets = Vec::new();
    for name in wb.sheet_names().to_owned() {
        let range = match wb.worksheet_range(&name) {
            Ok(r) => r,
            Err(_) => continue,
        };
        // Formula text, kept separately by calamine. Prefer it over the cached
        // value so formulas survive an open→edit→save round-trip; our engine
        // recomputes them (unsupported functions surface as #NAME, formula visible).
        let formulas = wb.worksheet_formula(&name).ok();
        let (row_off, col_off) = range.start().unwrap_or((0, 0));
        let mut sheet = Sheet { name, ..Default::default() };
        for (r, row) in range.rows().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                let ar = row_off + r as u32;
                let ac = col_off + c as u32;
                let raw = formulas
                    .as_ref()
                    .and_then(|f| f.get_value((ar, ac)))
                    .filter(|s| !s.is_empty())
                    .map(|s| format!("={}", s))
                    .unwrap_or_else(|| data_to_raw(cell));
                if !raw.is_empty() {
                    sheet.set(ar, ac, raw);
                }
            }
        }
        sheets.push(sheet);
    }
    if sheets.is_empty() {
        sheets.push(Sheet { name: "Sheet1".into(), ..Default::default() });
    }
    Ok(Book { sheets, active: 0, path: path.to_path_buf(), dirty: false })
}

fn data_to_raw(d: &calamine::Data) -> String {
    use calamine::Data as D;
    match d {
        D::Empty => String::new(),
        D::String(s) => s.clone(),
        D::Float(f) => fmt_number(*f),
        D::Int(i) => i.to_string(),
        D::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        D::DateTime(dt) => excel_serial_to_string(dt.as_f64()),
        D::DateTimeIso(s) => s.clone(),
        D::DurationIso(s) => s.clone(),
        D::Error(_) => String::new(),
    }
}

/// Format an Excel date serial as a readable `YYYY-MM-DD` (with `HH:MM` when
/// there's a time part, or just `HH:MM` for a time-only value < 1). Stored as
/// text — date arithmetic and round-trip to a typed date cell are out of scope.
fn excel_serial_to_string(serial: f64) -> String {
    let whole = serial.floor();
    let frac = serial - whole;
    let secs = (frac * 86_400.0).round() as i64;
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    if whole < 1.0 {
        // time-only value
        return format!("{:02}:{:02}", h, m);
    }
    // Excel serial 25569 == Unix epoch 1970-01-01.
    let (y, mo, d) = civil_from_days(whole as i64 - 25_569);
    if secs > 0 {
        format!("{:04}-{:02}-{:02} {:02}:{:02}", y, mo, d, h, m)
    } else {
        format!("{:04}-{:02}-{:02}", y, mo, d)
    }
}

/// Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn ext(path: &Path) -> String {
    path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

pub fn load_csv(path: &Path) -> std::io::Result<Book> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut sheet = csv_to_sheet(&text, "Sheet1".into());
    apply_color_sidecar(path, &mut sheet);
    Ok(Book { sheets: vec![sheet], active: 0, path: path.to_path_buf(), dirty: false })
}

/// Cell colours can't live in CSV, so they ride in a sidecar `<file>.gcolors`
/// (one `row,col,fg,bg` line per coloured cell) — lets colours round-trip in grid.
fn sidecar_path(path: &Path) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{}.gcolors", path.display()))
}

fn parse_rgb(s: &str) -> Option<Rgb> {
    if s.is_empty() { None } else { crust::style::parse_hex_color(s) }
}

/// `#rrggbb` for a colour, or empty for None.
fn opt_hex(c: Option<Rgb>) -> String {
    c.map(|(r, g, b)| format!("#{:02x}{:02x}{:02x}", r, g, b)).unwrap_or_default()
}

fn apply_color_sidecar(path: &Path, sheet: &mut Sheet) {
    let Ok(text) = std::fs::read_to_string(sidecar_path(path)) else { return };
    for line in text.lines() {
        let p: Vec<&str> = line.split(',').collect();
        if p.len() == 4 {
            if let (Ok(r), Ok(c)) = (p[0].parse::<u32>(), p[1].parse::<u32>()) {
                let fg = parse_rgb(p[2]);
                let bg = parse_rgb(p[3]);
                if fg.is_some() || bg.is_some() {
                    sheet.set_colors(r, c, fg, bg);
                }
            }
        }
    }
}

fn write_color_sidecar(book: &Book) -> std::io::Result<()> {
    let sheet = book.sheet();
    let mut s = String::new();
    for (&(r, c), cell) in &sheet.cells {
        if cell.fg.is_some() || cell.bg.is_some() {
            s.push_str(&format!("{},{},{},{}\n", r, c, opt_hex(cell.fg), opt_hex(cell.bg)));
        }
    }
    let path = sidecar_path(&book.path);
    if s.is_empty() {
        let _ = std::fs::remove_file(&path);
        Ok(())
    } else {
        std::fs::write(&path, s)
    }
}

/// Parse CSV text into a Sheet (raw cell text, formulas preserved).
pub fn csv_to_sheet(text: &str, name: String) -> Sheet {
    let mut sheet = Sheet { name, ..Default::default() };
    for (r, line) in text.lines().enumerate() {
        for (c, field) in parse_csv_line(line).into_iter().enumerate() {
            if !field.is_empty() {
                sheet.set(r as u32, c as u32, field);
            }
        }
    }
    sheet
}

/// Quote-aware CSV line split (RFC 4180-ish: `""` is an escaped quote).
fn parse_csv_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                if in_q && chars.peek() == Some(&'"') { cur.push('"'); chars.next(); }
                else { in_q = !in_q; }
            }
            ',' if !in_q => out.push(std::mem::take(&mut cur)),
            _ => cur.push(ch),
        }
    }
    out.push(cur);
    out
}

pub fn save_csv(book: &Book) -> std::io::Result<()> {
    std::fs::write(&book.path, sheet_to_csv(book.sheet()))?;
    write_color_sidecar(book)
}

/// Serialize a Sheet to CSV text (raw cell text, RFC4180 quoting).
pub fn sheet_to_csv(sheet: &Sheet) -> String {
    let mut s = String::new();
    for r in 0..sheet.nrows {
        for c in 0..sheet.ncols {
            if c > 0 {
                s.push(',');
            }
            let f = sheet.raw(r, c);
            if f.contains([',', '"', '\n']) {
                s.push('"');
                s.push_str(&f.replace('"', "\"\""));
                s.push('"');
            } else {
                s.push_str(f);
            }
        }
        s.push('\n');
    }
    s
}
