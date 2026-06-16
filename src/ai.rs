//! AI editing: hand the sheet to `claude -p` with a natural-language
//! instruction and get the whole sheet back as CSV. The prompt goes on stdin
//! (mirrors the rest of the Fe2O3 suite); the result is fenced between markers
//! so commentary can't leak into the data. User-triggered only (the `c` key),
//! never in a loop — no idle cost.

use std::io::Write;
use std::process::{Command, Stdio};

const BEGIN: &str = "<<<CSV";
const END: &str = "CSV>>>";

/// Build the prompt: the current sheet as CSV plus the user's instruction.
pub fn build_prompt(csv: &str, instruction: &str) -> String {
    format!(
        "You are editing a spreadsheet. Below is the current sheet as CSV \
(RFC 4180; the first row is row 1, the first field is column A). Apply the \
instruction and return the COMPLETE resulting sheet.\n\
Rules:\n\
- A cell starting with `=` is a formula (A1-style refs; functions such as \
SUM, AVERAGE, MIN, MAX, COUNT, IF, AND, OR, ROUND, CONCAT, VLOOKUP). Prefer \
formulas over hard-coded values where it fits; never replace a formula with \
its computed value unless asked.\n\
- Return ONLY the CSV, wrapped exactly between a line `{BEGIN}` and a line \
`{END}`. No text before or after.\n\n\
INSTRUCTION: {instruction}\n\n\
{BEGIN}\n{csv}{END}\n"
    )
}

/// Run `claude -p` with `prompt` on stdin; return stdout or an error string.
pub fn run_claude(prompt: &str) -> Result<String, String> {
    let mut child = Command::new("claude")
        .arg("-p")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn claude: {} \u{2014} is the `claude` CLI on PATH?", e))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .map_err(|e| format!("write prompt: {}", e))?;
    }
    let out = child.wait_with_output().map_err(|e| format!("wait: {}", e))?;
    if !out.status.success() {
        // The CLI prints API/policy errors to stdout, not stderr.
        let err = String::from_utf8_lossy(&out.stderr);
        let so = String::from_utf8_lossy(&out.stdout);
        let detail = if !err.trim().is_empty() {
            err.trim().to_string()
        } else {
            so.trim().chars().take(200).collect()
        };
        return Err(format!("claude failed: {}", detail));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Extract the CSV body between the markers (last occurrence wins). Falls back
/// to a ```-fenced block, then to the raw response, so a stray reply still
/// yields something to parse.
pub fn extract_csv(resp: &str) -> Option<String> {
    if let Some(start) = resp.rfind(BEGIN) {
        let rest = &resp[start + BEGIN.len()..];
        if let Some(end) = rest.find(END) {
            return Some(rest[..end].trim_matches('\n').to_string());
        }
    }
    if let Some(start) = resp.find("```") {
        let rest = &resp[start + 3..];
        // skip an optional language tag on the fence line
        let rest = rest.splitn(2, '\n').nth(1).unwrap_or(rest);
        if let Some(end) = rest.find("```") {
            return Some(rest[..end].trim_matches('\n').to_string());
        }
    }
    let t = resp.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}
