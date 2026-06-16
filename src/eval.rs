//! Formula engine: a hand-rolled subset of the usual spreadsheet language.
//! No dependencies, no Excel parity — just the ~25 functions that cover real
//! work (SUM/AVERAGE/IF/lookups/text). A `=formula` cell is lexed, parsed
//! (precedence climbing) and evaluated; cell references resolve recursively
//! with a visiting-set so cycles surface as `#CYCLE` instead of looping.
//!
//! Recalc is whole-sheet on every edit. Sheets here are small (hand-built,
//! not million-row imports) so a full pass is cheaper than maintaining a
//! dependency graph, and it stays cold between keystrokes.

use std::collections::{HashMap, HashSet};

use crate::model::{parse_ref, Sheet, Value};

pub fn recalc(sheet: &mut Sheet) {
    let coords: Vec<(u32, u32)> = sheet.cells.keys().copied().collect();
    // Borrow the sheet read-only while computing, then write values back.
    let results: Vec<((u32, u32), Value)> = {
        let mut rc = Recalc { sheet, cache: HashMap::new(), visiting: HashSet::new() };
        coords.iter().map(|&c| (c, rc.eval_cell(c))).collect()
    };
    for (coord, val) in results {
        if let Some(cell) = sheet.cells.get_mut(&coord) {
            cell.value = val;
        }
    }
}

struct Recalc<'a> {
    sheet: &'a Sheet,
    cache: HashMap<(u32, u32), Value>,
    visiting: HashSet<(u32, u32)>,
}

impl Recalc<'_> {
    fn eval_cell(&mut self, coord: (u32, u32)) -> Value {
        if let Some(v) = self.cache.get(&coord) {
            return v.clone();
        }
        if self.visiting.contains(&coord) {
            return Value::Error("CYCLE".into());
        }
        let raw = self.sheet.raw(coord.0, coord.1).to_string();
        let v = if raw.is_empty() {
            Value::Empty
        } else if let Some(expr) = raw.strip_prefix('=') {
            self.visiting.insert(coord);
            let val = match parse(expr) {
                Ok(ast) => self.eval(&ast),
                Err(code) => Value::Error(code),
            };
            self.visiting.remove(&coord);
            val
        } else {
            classify(&raw)
        };
        self.cache.insert(coord, v.clone());
        v
    }

    fn eval(&mut self, e: &Expr) -> Value {
        match e {
            Expr::Num(n) => Value::Number(*n),
            Expr::Str(s) => Value::Text(s.clone()),
            Expr::Bool(b) => boolval(*b),
            Expr::ErrName => Value::Error("NAME".into()),
            Expr::Ref(r, c) => self.eval_cell((*r, *c)),
            Expr::Range(..) => Value::Error("VALUE".into()), // range in scalar context
            Expr::Unary(op, inner) => {
                let v = self.eval(inner);
                match op {
                    '-' => match num(&v) {
                        Ok(n) => Value::Number(-n),
                        Err(e) => e,
                    },
                    '%' => match num(&v) {
                        Ok(n) => Value::Number(n / 100.0),
                        Err(e) => e,
                    },
                    _ => v,
                }
            }
            Expr::Binary(op, l, r) => self.eval_binary(*op, l, r),
            Expr::Call(name, args) => self.eval_call(name, args),
        }
    }

    fn eval_binary(&mut self, op: BinOp, l: &Expr, r: &Expr) -> Value {
        if op == BinOp::Concat {
            let a = self.eval(l);
            let b = self.eval(r);
            if let Value::Error(_) = a {
                return a;
            }
            if let Value::Error(_) = b {
                return b;
            }
            return Value::Text(format!("{}{}", a.display(), b.display()));
        }
        let a = self.eval(l);
        let b = self.eval(r);
        // comparisons
        if matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) {
            return compare(op, &a, &b);
        }
        let x = match num(&a) {
            Ok(n) => n,
            Err(e) => return e,
        };
        let y = match num(&b) {
            Ok(n) => n,
            Err(e) => return e,
        };
        match op {
            BinOp::Add => Value::Number(x + y),
            BinOp::Sub => Value::Number(x - y),
            BinOp::Mul => Value::Number(x * y),
            BinOp::Div => {
                if y == 0.0 {
                    Value::Error("DIV/0".into())
                } else {
                    Value::Number(x / y)
                }
            }
            BinOp::Pow => Value::Number(x.powf(y)),
            _ => Value::Error("VALUE".into()),
        }
    }

    /// Flatten args into a value list, expanding ranges. Used by aggregates.
    fn flatten(&mut self, args: &[Expr]) -> Vec<Value> {
        let mut out = Vec::new();
        for a in args {
            if let Expr::Range((r0, c0), (r1, c1)) = a {
                let (r0, r1) = (r0.min(r1), r0.max(r1));
                let (c0, c1) = (c0.min(c1), c0.max(c1));
                for r in *r0..=*r1 {
                    for c in *c0..=*c1 {
                        out.push(self.eval_cell((r, c)));
                    }
                }
            } else {
                out.push(self.eval(a));
            }
        }
        out
    }

    fn eval_call(&mut self, name: &str, args: &[Expr]) -> Value {
        // Short-circuiting forms first.
        match name {
            "IF" => {
                if args.len() < 2 {
                    return Value::Error("N/A".into());
                }
                let cond = self.eval(&args[0]);
                return match truthy(&cond) {
                    Ok(true) => self.eval(&args[1]),
                    Ok(false) => {
                        if args.len() >= 3 {
                            self.eval(&args[2])
                        } else {
                            boolval(false)
                        }
                    }
                    Err(e) => e,
                };
            }
            "AND" | "OR" => {
                let want_all = name == "AND";
                let vals = self.flatten(args);
                let mut acc = want_all;
                for v in &vals {
                    match truthy(v) {
                        Ok(b) => {
                            if want_all {
                                acc = acc && b;
                            } else {
                                acc = acc || b;
                            }
                        }
                        Err(e) => return e,
                    }
                }
                return boolval(acc);
            }
            _ => {}
        }

        // Numeric aggregates over flattened (range-expanded) values.
        let nums = |vals: &[Value]| -> Result<Vec<f64>, Value> {
            let mut v = Vec::new();
            for x in vals {
                match x {
                    Value::Empty => {}
                    Value::Error(_) => return Err(x.clone()),
                    _ => match num(x) {
                        Ok(n) => v.push(n),
                        Err(_) => {} // skip non-numeric in aggregates
                    },
                }
            }
            Ok(v)
        };

        match name {
            "SUM" | "AVERAGE" | "AVG" | "MIN" | "MAX" | "COUNT" | "COUNTA" | "PRODUCT" => {
                let vals = self.flatten(args);
                if name == "COUNTA" {
                    let n = vals.iter().filter(|v| !matches!(v, Value::Empty)).count();
                    return Value::Number(n as f64);
                }
                let ns = match nums(&vals) {
                    Ok(n) => n,
                    Err(e) => return e,
                };
                match name {
                    "COUNT" => Value::Number(ns.len() as f64),
                    "SUM" => Value::Number(ns.iter().sum()),
                    "PRODUCT" => Value::Number(ns.iter().product()),
                    "AVERAGE" | "AVG" => {
                        if ns.is_empty() {
                            Value::Error("DIV/0".into())
                        } else {
                            Value::Number(ns.iter().sum::<f64>() / ns.len() as f64)
                        }
                    }
                    "MIN" => Value::Number(ns.iter().cloned().fold(f64::INFINITY, f64::min)),
                    "MAX" => Value::Number(ns.iter().cloned().fold(f64::NEG_INFINITY, f64::max)),
                    _ => unreachable!(),
                }
            }
            "NOT" => {
                if args.len() != 1 {
                    return Value::Error("N/A".into());
                }
                match truthy(&self.eval(&args[0])) {
                    Ok(b) => boolval(!b),
                    Err(e) => e,
                }
            }
            "ROUND" => {
                let v = self.scalar_num(args.get(0));
                let n = args.get(1).map(|a| self.eval(a)).map(|v| num(&v).unwrap_or(0.0)).unwrap_or(0.0);
                match v {
                    Ok(x) => {
                        let f = 10f64.powi(n as i32);
                        Value::Number((x * f).round() / f)
                    }
                    Err(e) => e,
                }
            }
            "ABS" => self.unary_num(args, f64::abs),
            "INT" => self.unary_num(args, f64::floor),
            "SQRT" => self.unary_num(args, f64::sqrt),
            "MOD" => self.binary_num(args, |a, b| if b == 0.0 { f64::NAN } else { a - b * (a / b).floor() }),
            "POWER" => self.binary_num(args, f64::powf),
            "CONCAT" | "CONCATENATE" => {
                let vals = self.flatten(args);
                let mut s = String::new();
                for v in &vals {
                    if let Value::Error(_) = v {
                        return v.clone();
                    }
                    s.push_str(&v.display());
                }
                Value::Text(s)
            }
            "LEN" => match self.scalar(args.get(0)) {
                Value::Error(e) => Value::Error(e),
                v => Value::Number(v.display().chars().count() as f64),
            },
            "UPPER" => self.unary_text(args, |s| s.to_uppercase()),
            "LOWER" => self.unary_text(args, |s| s.to_lowercase()),
            "TRIM" => self.unary_text(args, |s| s.trim().to_string()),
            "LEFT" => self.text_slice(args, true),
            "RIGHT" => self.text_slice(args, false),
            "MID" => {
                let s = self.scalar(args.get(0)).display();
                let start = self.scalar_num(args.get(1)).unwrap_or(0.0).max(1.0) as usize - 1;
                let len = self.scalar_num(args.get(2)).unwrap_or(0.0).max(0.0) as usize;
                let chars: Vec<char> = s.chars().collect();
                let end = (start + len).min(chars.len());
                let start = start.min(chars.len());
                Value::Text(chars[start..end].iter().collect())
            }
            "VLOOKUP" => self.vlookup(args),
            _ => Value::Error("NAME".into()),
        }
    }

    fn scalar(&mut self, arg: Option<&Expr>) -> Value {
        match arg {
            Some(e) => self.eval(e),
            None => Value::Empty,
        }
    }
    fn scalar_num(&mut self, arg: Option<&Expr>) -> Result<f64, Value> {
        let v = self.scalar(arg);
        num(&v)
    }
    fn unary_num(&mut self, args: &[Expr], f: fn(f64) -> f64) -> Value {
        match self.scalar_num(args.get(0)) {
            Ok(x) => Value::Number(f(x)),
            Err(e) => e,
        }
    }
    fn binary_num(&mut self, args: &[Expr], f: fn(f64, f64) -> f64) -> Value {
        let a = self.scalar_num(args.get(0));
        let b = self.scalar_num(args.get(1));
        match (a, b) {
            (Ok(x), Ok(y)) => {
                let r = f(x, y);
                if r.is_nan() {
                    Value::Error("NUM".into())
                } else {
                    Value::Number(r)
                }
            }
            (Err(e), _) | (_, Err(e)) => e,
        }
    }
    fn unary_text(&mut self, args: &[Expr], f: fn(&str) -> String) -> Value {
        match self.scalar(args.get(0)) {
            Value::Error(e) => Value::Error(e),
            v => Value::Text(f(&v.display())),
        }
    }
    fn text_slice(&mut self, args: &[Expr], from_left: bool) -> Value {
        let s = self.scalar(args.get(0)).display();
        let n = self.scalar_num(args.get(1)).unwrap_or(1.0).max(0.0) as usize;
        let chars: Vec<char> = s.chars().collect();
        let n = n.min(chars.len());
        let slice: String = if from_left {
            chars[..n].iter().collect()
        } else {
            chars[chars.len() - n..].iter().collect()
        };
        Value::Text(slice)
    }

    /// VLOOKUP(key, range, col_index) — exact match only (MVP).
    fn vlookup(&mut self, args: &[Expr]) -> Value {
        if args.len() < 3 {
            return Value::Error("N/A".into());
        }
        let key = self.eval(&args[0]);
        let col = self.scalar_num(args.get(2)).unwrap_or(1.0) as u32;
        if let Expr::Range((r0, c0), (r1, c1)) = &args[1] {
            let (r0, r1) = ((*r0).min(*r1), (*r0).max(*r1));
            let (c0, c1) = ((*c0).min(*c1), (*c0).max(*c1));
            let target = c0 + col.saturating_sub(1);
            if target > c1 {
                return Value::Error("REF".into());
            }
            for r in r0..=r1 {
                let cell = self.eval_cell((r, c0));
                if values_eq(&cell, &key) {
                    return self.eval_cell((r, target));
                }
            }
            Value::Error("N/A".into())
        } else {
            Value::Error("VALUE".into())
        }
    }
}

// ---- value helpers -------------------------------------------------------

fn boolval(b: bool) -> Value {
    Value::Text(if b { "TRUE" } else { "FALSE" }.into())
}

/// Coerce a value to a number for arithmetic. Empty→0, TRUE/FALSE→1/0.
fn num(v: &Value) -> Result<f64, Value> {
    match v {
        Value::Number(n) => Ok(*n),
        Value::Empty => Ok(0.0),
        Value::Error(_) => Err(v.clone()),
        Value::Text(s) => {
            let t = s.trim();
            if t.eq_ignore_ascii_case("true") {
                Ok(1.0)
            } else if t.eq_ignore_ascii_case("false") {
                Ok(0.0)
            } else {
                t.parse::<f64>().map_err(|_| Value::Error("VALUE".into()))
            }
        }
    }
}

fn truthy(v: &Value) -> Result<bool, Value> {
    match v {
        Value::Error(_) => Err(v.clone()),
        _ => num(v).map(|n| n != 0.0),
    }
}

fn values_eq(a: &Value, b: &Value) -> bool {
    if let (Ok(x), Ok(y)) = (num(a), num(b)) {
        return x == y;
    }
    a.display().eq_ignore_ascii_case(&b.display())
}

fn compare(op: BinOp, a: &Value, b: &Value) -> Value {
    if let Value::Error(_) = a {
        return a.clone();
    }
    if let Value::Error(_) = b {
        return b.clone();
    }
    // Numeric compare when both look numeric, else case-insensitive text.
    let ord = if let (Ok(x), Ok(y)) = (num(a), num(b)) {
        x.partial_cmp(&y)
    } else {
        Some(a.display().to_lowercase().cmp(&b.display().to_lowercase()))
    };
    let Some(ord) = ord else { return Value::Error("NUM".into()) };
    use std::cmp::Ordering::*;
    let res = match op {
        BinOp::Eq => ord == Equal,
        BinOp::Ne => ord != Equal,
        BinOp::Lt => ord == Less,
        BinOp::Le => ord != Greater,
        BinOp::Gt => ord == Greater,
        BinOp::Ge => ord != Less,
        _ => false,
    };
    boolval(res)
}

/// Classify a non-formula literal as number or text.
fn classify(raw: &str) -> Value {
    if raw.is_empty() {
        Value::Empty
    } else if let Ok(n) = raw.parse::<f64>() {
        Value::Number(n)
    } else {
        Value::Text(raw.to_string())
    }
}

// ---- AST -----------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Pow,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

enum Expr {
    Num(f64),
    Str(String),
    Bool(bool),
    ErrName,
    Ref(u32, u32),
    Range((u32, u32), (u32, u32)),
    Unary(char, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
}

// ---- lexer ---------------------------------------------------------------

#[derive(Clone, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Name(String),
    Plus,
    Minus,
    Star,
    Slash,
    Caret,
    Amp,
    Percent,
    LParen,
    RParen,
    Comma,
    Colon,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

fn lex(s: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\r' | '\n' => i += 1,
            '+' => { out.push(Tok::Plus); i += 1; }
            '-' => { out.push(Tok::Minus); i += 1; }
            '*' => { out.push(Tok::Star); i += 1; }
            '/' => { out.push(Tok::Slash); i += 1; }
            '^' => { out.push(Tok::Caret); i += 1; }
            '&' => { out.push(Tok::Amp); i += 1; }
            '%' => { out.push(Tok::Percent); i += 1; }
            '(' => { out.push(Tok::LParen); i += 1; }
            ')' => { out.push(Tok::RParen); i += 1; }
            ',' => { out.push(Tok::Comma); i += 1; }
            ':' => { out.push(Tok::Colon); i += 1; }
            '=' => { out.push(Tok::Eq); i += 1; }
            '<' => {
                if chars.get(i + 1) == Some(&'=') { out.push(Tok::Le); i += 2; }
                else if chars.get(i + 1) == Some(&'>') { out.push(Tok::Ne); i += 2; }
                else { out.push(Tok::Lt); i += 1; }
            }
            '>' => {
                if chars.get(i + 1) == Some(&'=') { out.push(Tok::Ge); i += 2; }
                else { out.push(Tok::Gt); i += 1; }
            }
            '"' => {
                i += 1;
                let mut str = String::new();
                while i < chars.len() {
                    if chars[i] == '"' {
                        if chars.get(i + 1) == Some(&'"') { str.push('"'); i += 2; }
                        else { i += 1; break; }
                    } else {
                        str.push(chars[i]);
                        i += 1;
                    }
                }
                out.push(Tok::Str(str));
            }
            d if d.is_ascii_digit() || d == '.' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                let n: f64 = chars[start..i].iter().collect::<String>().parse().map_err(|_| "NUM".to_string())?;
                out.push(Tok::Num(n));
            }
            a if a.is_ascii_alphabetic() || a == '_' || a == '$' => {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '$' || chars[i] == '.')
                {
                    i += 1;
                }
                let name: String = chars[start..i].iter().filter(|&&c| c != '$').collect();
                out.push(Tok::Name(name));
            }
            _ => return Err("SYNTAX".into()),
        }
    }
    Ok(out)
}

// ---- parser (precedence climbing) ----------------------------------------

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

fn parse(s: &str) -> Result<Expr, String> {
    let toks = lex(s)?;
    let mut p = Parser { toks, pos: 0 };
    let e = p.expr(0)?;
    if p.pos != p.toks.len() {
        return Err("SYNTAX".into());
    }
    Ok(e)
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }
    fn expect(&mut self, t: Tok) -> Result<(), String> {
        if self.peek() == Some(&t) {
            self.pos += 1;
            Ok(())
        } else {
            Err("SYNTAX".into())
        }
    }

    fn expr(&mut self, min_bp: u8) -> Result<Expr, String> {
        let mut lhs = self.prefix()?;
        while let Some(op) = self.peek().and_then(binop) {
            let bp = lbp(op);
            if bp < min_bp {
                break;
            }
            self.pos += 1;
            let next_min = if op == BinOp::Pow { bp } else { bp + 1 };
            let rhs = self.expr(next_min)?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn prefix(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Some(Tok::Minus) => {
                self.pos += 1;
                Ok(Expr::Unary('-', Box::new(self.expr(8)?)))
            }
            Some(Tok::Plus) => {
                self.pos += 1;
                self.expr(8)
            }
            _ => self.primary(),
        }
    }

    fn primary(&mut self) -> Result<Expr, String> {
        let tok = self.next().ok_or("SYNTAX".to_string())?;
        let mut e = match tok {
            Tok::Num(n) => Expr::Num(n),
            Tok::Str(s) => Expr::Str(s),
            Tok::LParen => {
                let inner = self.expr(0)?;
                self.expect(Tok::RParen)?;
                inner
            }
            Tok::Name(name) => self.name(name)?,
            _ => return Err("SYNTAX".into()),
        };
        // postfix percent
        while self.peek() == Some(&Tok::Percent) {
            self.pos += 1;
            e = Expr::Unary('%', Box::new(e));
        }
        Ok(e)
    }

    fn name(&mut self, name: String) -> Result<Expr, String> {
        // function call?
        if self.peek() == Some(&Tok::LParen) {
            self.pos += 1;
            let mut args = Vec::new();
            if self.peek() != Some(&Tok::RParen) {
                loop {
                    args.push(self.expr(0)?);
                    match self.peek() {
                        Some(Tok::Comma) => self.pos += 1,
                        _ => break,
                    }
                }
            }
            self.expect(Tok::RParen)?;
            return Ok(Expr::Call(name.to_uppercase(), args));
        }
        // cell reference or range?
        if let Some((r, c)) = parse_ref(&name) {
            if self.peek() == Some(&Tok::Colon) {
                self.pos += 1;
                if let Some(Tok::Name(n2)) = self.next() {
                    if let Some((r2, c2)) = parse_ref(&n2) {
                        return Ok(Expr::Range((r, c), (r2, c2)));
                    }
                }
                return Err("REF".into());
            }
            return Ok(Expr::Ref(r, c));
        }
        match name.to_uppercase().as_str() {
            "TRUE" => Ok(Expr::Bool(true)),
            "FALSE" => Ok(Expr::Bool(false)),
            _ => Ok(Expr::ErrName),
        }
    }
}

fn binop(t: &Tok) -> Option<BinOp> {
    Some(match t {
        Tok::Plus => BinOp::Add,
        Tok::Minus => BinOp::Sub,
        Tok::Star => BinOp::Mul,
        Tok::Slash => BinOp::Div,
        Tok::Caret => BinOp::Pow,
        Tok::Amp => BinOp::Concat,
        Tok::Eq => BinOp::Eq,
        Tok::Ne => BinOp::Ne,
        Tok::Lt => BinOp::Lt,
        Tok::Le => BinOp::Le,
        Tok::Gt => BinOp::Gt,
        Tok::Ge => BinOp::Ge,
        _ => return None,
    })
}

/// Left binding power. Higher binds tighter. `^` is right-associative
/// (handled by the caller passing `bp` rather than `bp+1`).
fn lbp(op: BinOp) -> u8 {
    match op {
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 1,
        BinOp::Concat => 2,
        BinOp::Add | BinOp::Sub => 3,
        BinOp::Mul | BinOp::Div => 4,
        BinOp::Pow => 5,
    }
}
