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
    saved_bytes: usize,
    dropped_chars: usize,
    cap: u64,
}

impl ToolOutputWriter {
    /// Opens (or resumes) a spill file under the session's tool-output dir.
    pub fn open_at(root: &Path, session_id: Option<&str>, tool: &str) -> std::io::Result<Self> {
        Self::open_with_cap(root, session_id, tool, TOOL_OUTPUT_MAX_FILE_BYTES)
    }

    /// The same writer under an explicit cap, so the unit tests can prove
    /// the cap's arithmetic without a 64 MiB fixture.
    pub(crate) fn open_with_cap(
        root: &Path,
        session_id: Option<&str>,
        tool: &str,
        cap: u64,
    ) -> std::io::Result<Self> {
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
            saved_bytes: 0,
            dropped_chars: 0,
            cap,
        })
    }

    /// Appends text to the spill file, enforcing the cap in bytes.
    pub fn write(&mut self, text: &str) {
        if text.is_empty() || self.file.is_none() {
            return;
        }
        // The cap is a byte budget, so the room is measured in bytes and
        // the text is cut on a character boundary: a character that does
        // not fit whole is left out rather than split. Counting characters
        // against this budget let a multibyte file reach four times the
        // stated ceiling.
        let room = self.cap.saturating_sub(self.saved_bytes as u64) as usize;
        if room == 0 {
            self.dropped_chars += text.chars().count();
            return;
        }
        let mut used = 0usize;
        let mut taken = 0usize;
        for ch in text.chars() {
            let width = ch.len_utf8();
            if used + width > room {
                break;
            }
            used += width;
            taken += 1;
        }
        self.dropped_chars += text.chars().count() - taken;
        if taken == 0 {
            return;
        }
        let chunk: String = text.chars().take(taken).collect();
        if let Some(file) = self.file.as_mut()
            && file.write_all(chunk.as_bytes()).is_err()
        {
            self.dropped_chars += taken;
        } else {
            self.saved_chars += taken;
            self.saved_bytes += used;
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A scratch directory the tests own and remove.
    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ob-tool-output-{name}-{}", short_token()))
    }

    /// `TOOL_OUTPUT_MAX_FILE_BYTES` says bytes and the module documents a
    /// 64 MiB ceiling, so the cap must hold for text whose characters are
    /// wider than one byte. Counting characters against a byte budget let
    /// the file reach four times the stated ceiling.
    #[test]
    fn the_cap_holds_for_multibyte_text() {
        let dir = scratch("cap-multibyte");
        let mut writer = ToolOutputWriter::open_with_cap(&dir, None, "t", 8).unwrap();
        writer.write("\u{20AC}\u{20AC}\u{20AC}\u{20AC}");
        let (path, saved_chars, dropped_chars) = writer.close();
        let written = std::fs::metadata(&path).unwrap().len();
        assert!(
            written <= 8,
            "the file holds {written} bytes, over the 8-byte cap"
        );
        assert_eq!(saved_chars, 2);
        assert_eq!(dropped_chars, 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ascii_output_fills_the_cap_exactly() {
        let dir = scratch("cap-ascii");
        let mut writer = ToolOutputWriter::open_with_cap(&dir, None, "t", 8).unwrap();
        writer.write("abcdefghij");
        let (path, saved_chars, dropped_chars) = writer.close();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 8);
        assert_eq!(saved_chars, 8);
        assert_eq!(dropped_chars, 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
