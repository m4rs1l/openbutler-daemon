//! Tool output that did not fit in the model-facing result: streamed to
//! a capped file under the data directory, with the path handed back in
//! a notice so nothing is silently lost.
//!
//! Source: legacy `src/tool-output.ts`, read 2026-11-17.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Command output is the noisiest source, so it keeps less inline.
pub const TOOL_OUTPUT_COMMAND_CHARS: usize = 8_000;
/// Spill files older than this are pruned (3 days).
pub const TOOL_OUTPUT_RETENTION_MS: i64 = 3 * 24 * 60 * 60_000;
/// The hard spill-file size cap (64 MiB).
pub const TOOL_OUTPUT_MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Host-owned generated data under `data/tool-output`, never the
/// agent's scratch space.
pub fn tool_output_root_in(data_dir: &Path) -> PathBuf {
    data_dir.join("tool-output")
}

fn stamp(now: chrono::DateTime<chrono::Local>) -> String {
    now.format("%Y%m%d-%H%M%S").to_string()
}

fn safe_tool_name(tool: &str) -> String {
    let mut cleaned = String::new();
    for ch in tool.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-') {
            cleaned.push(ch);
        } else {
            cleaned.push('-');
        }
    }
    let trimmed = cleaned.trim_matches('-');
    let sliced: String = trimmed.chars().take(40).collect();
    if sliced.is_empty() {
        "tool".to_owned()
    } else {
        sliced
    }
}

/// Streaming writer for a command's output file: the host keeps only a
/// short inline preview in memory, so this is what makes the tail
/// available at all.
pub struct ToolOutputWriter {
    path: PathBuf,
    root: PathBuf,
    file: Option<File>,
    saved_chars: usize,
    dropped_chars: usize,
    cap: u64,
}

impl ToolOutputWriter {
    /// Opens (or resumes) a spill file under the session's tool-output dir.
    pub fn open_at(root: &Path, session_id: Option<&str>, tool: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(root)?;
        let session = session_id
            .unwrap_or("session")
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-')
            .take(8)
            .collect::<String>();
        let session = if session.is_empty() {
            "session".to_owned()
        } else {
            session
        };
        let name = format!(
            "{}-{session}-{}-{}.log",
            stamp(chrono::Local::now()),
            safe_tool_name(tool),
            short_token(),
        );
        let path = root.join(name);
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        Ok(Self {
            path,
            root: root.to_path_buf(),
            file: Some(file),
            saved_chars: 0,
            dropped_chars: 0,
            cap: TOOL_OUTPUT_MAX_FILE_BYTES,
        })
    }

    /// Appends text to the spill file.
    pub fn write(&mut self, text: &str) {
        if text.is_empty() || self.file.is_none() {
            return;
        }
        let room = self.cap as usize - self.saved_chars;
        if room == 0 {
            self.dropped_chars += text.chars().count();
            return;
        }
        let chunk: String = text.chars().take(room).collect();
        let dropped_in_chunk = text.chars().count() - chunk.chars().count();
        self.dropped_chars += dropped_in_chunk;
        if let Some(file) = self.file.as_mut()
            && file.write_all(chunk.as_bytes()).is_err()
        {
            self.dropped_chars += chunk.chars().count();
        } else {
            self.saved_chars += chunk.chars().count();
        }
    }

    /// Closes the spill: path plus total/inline byte counts.
    pub fn close(&mut self) -> (PathBuf, usize, usize) {
        self.file.take();
        prune_tool_output(&self.root);
        (self.path.clone(), self.saved_chars, self.dropped_chars)
    }
}

/// The sentence the model sees next to a truncated result.
pub fn tool_output_notice(path: &Path, total_chars: usize, dropped_chars: usize) -> String {
    let saved = if dropped_chars > 0 {
        format!(
            " The saved file was itself capped, {} characters past the cap were not written.",
            format_number(dropped_chars)
        )
    } else {
        String::new()
    };
    format!(
        "The full output ({}) characters) is saved at {}. Read it with file_read using offset/limit, or search it with grep; do not re-run the command to see the rest.{}",
        format_number(total_chars),
        path.display(),
        saved
    )
}

/// `toLocaleString('en-US')` — thousands separators for the notice.
fn format_number(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Deletes output files past the retention window (best effort, called
/// when a writer closes).
fn prune_tool_output(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let cutoff = chrono::Local::now().timestamp_millis() - TOOL_OUTPUT_RETENTION_MS;
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| since.as_millis() as i64);
        if modified.is_some_and(|at| at < cutoff) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn short_token() -> String {
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).map(|_| ()).unwrap_or(());
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
