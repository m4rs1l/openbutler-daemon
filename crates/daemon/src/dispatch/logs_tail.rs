//! `logs.tail`: the machine log's tail, read where the file lives.
//!
//! Source: `cli-main.ts` `runLogs` (read 2026-11-26) read
//! `logs/openbutler.log` from the CLI process and filtered parsed records by
//! level/event. This build's daemon writes no structured file yet, so the
//! honest machine log is the `daemon.log` that `daemon start` captures — raw
//! lines, tailed here. Moving the read into the host follows the
//! backend-owned rule: the client asked for a file it cannot see.
//!
//! The tail is the CLI's own algorithm moved verbatim (blocks read backwards
//! until the wanted newline count is covered, so a large log costs its tail,
//! not its size). An unreadable file answers like an empty one, as the source
//! does.
//!
//! `--file` is resolved by the caller against its own shell directory and must
//! arrive absolute: a relative path would otherwise resolve against the host's
//! working directory, which is not the directory the owner typed the command
//! in. The action is local-only — reading host files over a remote token is
//! new authority the source never granted.

use std::path::{Path, PathBuf};

use openbutler_protocol::DaemonRequest;
use serde_json::{Value, json};

use crate::shape::is_absolute_path;

/// The largest window a caller may ask for, mirroring the CLI parser.
const MAX_LINES: u64 = 100_000;

/// `logs.tail`: `{lines?, file?}`.
pub fn tail(data_dir: &Path, request: &DaemonRequest) -> Result<Value, String> {
    let lines = match request.get("lines") {
        None => 100,
        Some(Value::Number(number)) => match number.as_u64() {
            Some(value) if (1..=MAX_LINES).contains(&value) => value,
            _ => return Err(lines_refusal()),
        },
        Some(_) => return Err(lines_refusal()),
    };
    let path = match request.get("file") {
        None | Some(Value::Null) => data_dir.join("daemon.log"),
        Some(Value::String(text)) if text.is_empty() => data_dir.join("daemon.log"),
        Some(Value::String(text)) if is_absolute_path(text) => PathBuf::from(text),
        Some(_) => return Err("file must be an absolute path.".to_owned()),
    };

    let records = tail_lines(&path, lines as usize).unwrap_or_default();
    if records.is_empty() {
        return Ok(json!({
            "lines": [
                { "tone": "info", "text": format!("No log lines at {}.", path.display()) }
            ]
        }));
    }
    let lines: Vec<Value> = records
        .into_iter()
        .map(|text| json!({ "tone": "plain", "text": text }))
        .collect();
    Ok(json!({ "lines": lines }))
}

fn lines_refusal() -> String {
    format!("lines must be a whole number between 1 and {MAX_LINES}.")
}

/// Tail by seeking: blocks are read backwards until the wanted newline count
/// is covered.
fn tail_lines(path: &Path, lines: usize) -> std::io::Result<Vec<String>> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    const BLOCK: u64 = 64 * 1024;

    let mut position = size;
    let mut newlines = 0usize;
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    while position > 0 && newlines <= lines {
        let start = position.saturating_sub(BLOCK);
        let mut block = vec![0u8; (position - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut block)?;
        newlines += block.iter().filter(|byte| **byte == b'\n').count();
        chunks.insert(0, block);
        position = start;
    }

    let mut data = Vec::new();
    for chunk in chunks {
        data.extend(chunk);
    }
    let text = String::from_utf8_lossy(&data);
    let mut records: Vec<String> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
        .collect();
    if records.last().is_some_and(|line| line.is_empty()) {
        records.pop();
    }
    Ok(records.split_off(records.len().saturating_sub(lines)))
}
