//! `execute_command`: validation, workspace scope, and the shell run.
//!
//! Sources: legacy `src/tools/execute-command.ts` (read 2026-11-17),
//! `src/workspace-scope.ts`, `src/tool-output.ts`, `src/tools/shell.ts`.
//!
//! The pipeline stages around this function (approval, ledger, redaction)
//! are the registry's job and land with the batch loop; this module is
//! the tool itself: what may run, where, and what comes back.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{Value, json};
use std::sync::OnceLock;

use super::shell::{self, ResolvedShell};
use super::tool_output::{TOOL_OUTPUT_COMMAND_CHARS, ToolOutputWriter, tool_output_notice};
use super::workspace_scope::{WorkspaceScope, classify_trusted_path};

const MAX_COMMAND_CHARS: usize = 10_000;
pub const DEFAULT_TOOL_TIMEOUT_SECONDS: u64 = 60;
pub const MAX_TOOL_TIMEOUT_SECONDS: u64 = 500;

/// Everything the tool reads from its caller: the turn's scope and the
/// configured shell, injected so tests never touch the host settings.
#[derive(Debug, Clone)]
pub struct ExecuteEnvironment {
    pub workspace_root: PathBuf,
    pub scratch_root: PathBuf,
    pub command_shell: Option<String>,
    pub session_id: Option<String>,
    pub data_dir: PathBuf,
}

/// `execute` for `execute_command`: never throws — every refusal is a
/// status object the model reads, exactly like the source.
pub fn execute_command(
    input: &Value,
    env: &ExecuteEnvironment,
    target: Option<&Path>,
    timeout_seconds: Option<u64>,
) -> Value {
    let Some(raw) = input.get("command") else {
        return invalid("command must be a string.");
    };
    let Some(raw) = raw.as_str() else {
        return invalid("command must be a string.");
    };
    let command = raw.trim().replace("\r\n", "\n");
    if command.is_empty()
        || command.encode_utf16().count() > MAX_COMMAND_CHARS
        || command.chars().any(is_control_rejected)
    {
        return invalid("command must be 1-10000 characters and contain no control characters.");
    }
    let target = match target {
        Some(target) => target.to_path_buf(),
        None => resolve_command_directory(input.get("cwd"), &env.workspace_root),
    };
    let timeout = timeout_seconds
        .filter(|seconds| (1..=MAX_TOOL_TIMEOUT_SECONDS).contains(seconds))
        .unwrap_or(DEFAULT_TOOL_TIMEOUT_SECONDS);
    run_shell(&command, &target, env, Duration::from_secs(timeout))
}

/// Legacy control-character class: tab, newline and CR survive, the rest
/// of C0 plus DEL never reaches a shell.
fn is_control_rejected(ch: char) -> bool {
    let code = ch as u32;
    code <= 0x08 || (0x0B..=0x1F).contains(&code) || code == 0x7F
}

fn invalid(message: &str) -> Value {
    json!({ "status": "invalid", "error": message })
}

/// `resolveCommandDirectory`: relative paths resolve from the session
/// workspace, then canonicalised through existing parents. The catalog's
/// scope and execute hooks both resolve their target through it.
pub fn resolve_command_directory(value: Option<&Value>, workspace_root: &Path) -> PathBuf {
    let requested = value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.to_path_buf());
    let absolute = if requested.is_absolute() {
        requested
    } else {
        workspace_root.join(requested)
    };
    std::fs::canonicalize(&absolute).unwrap_or(absolute)
}

// --- workspace scope ---------------------------------------------------------

/// The scope hook for this tool: the working directory first, then the
/// command text itself against the trusted roots.
pub fn command_scope(command: &str, cwd: &Path, env: &ExecuteEnvironment) -> WorkspaceScope {
    let roots = vec![env.workspace_root.clone(), env.scratch_root.clone()];
    let cwd_scope = classify_trusted_path(cwd, &roots);
    if cwd_scope != WorkspaceScope::Workspace {
        return cwd_scope;
    }
    if command_changes_directory_outside_workspace(command, cwd, &roots) {
        return WorkspaceScope::Outside;
    }
    if command_may_access_outside_workspace(command, cwd, &roots) {
        return WorkspaceScope::Outside;
    }
    WorkspaceScope::Workspace
}

fn is_within_directory(root: &Path, target: &Path) -> bool {
    let root = normalize(root);
    let target = normalize(target);
    target.starts_with(&root)
}

fn normalize(path: &Path) -> PathBuf {
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    #[cfg(windows)]
    {
        PathBuf::from(absolute.to_string_lossy().to_lowercase())
    }
    #[cfg(not(windows))]
    {
        absolute
    }
}

fn command_changes_directory_outside_workspace(
    command: &str,
    cwd: &Path,
    roots: &[PathBuf],
) -> bool {
    let segments = segment_split(command);
    let cd = cd_regex();
    let mut current_directory = cwd.to_path_buf();
    for segment in segments {
        let Some(captures) = cd.captures(segment) else {
            continue;
        };
        let operation = captures[1].to_ascii_lowercase();
        if operation == "popd" || operation == "pop-location" {
            return true;
        }
        let target = captures[2].trim().to_owned();
        let target = {
            let stripped = target
                .strip_prefix("--")
                .or_else(|| {
                    target
                        .strip_prefix("-path")
                        .or_else(|| target.strip_prefix("-literalpath"))
                })
                .map(str::trim_start)
                .unwrap_or(&target);
            stripped.to_owned()
        };
        if target.is_empty() || target.contains(['$', '%', '~']) {
            return true;
        }
        let target = if let Some(quoted) = quoted_path(&target) {
            quoted
        } else if target.contains(' ') {
            return true;
        } else {
            target
        };
        let absolute = PathBuf::from(&target);
        let absolute = if absolute.is_absolute() {
            absolute
        } else {
            current_directory.join(&target)
        };
        current_directory = std::fs::canonicalize(&absolute).unwrap_or(absolute);
        if !roots
            .iter()
            .any(|root| is_within_directory(root, &current_directory))
        {
            return true;
        }
    }
    false
}

fn command_may_access_outside_workspace(command: &str, cwd: &Path, roots: &[PathBuf]) -> bool {
    for pattern in install_regexes() {
        if pattern.is_match(command) {
            return true;
        }
    }

    // Remote fetchers and piped interpreters: code from an unreviewed
    // producer, never bounded by the workspace.
    if remote_regex().is_match(command) || piped_shell_regex().is_match(command) {
        return true;
    }

    let text = url_mask_regex().replace_all(command, " ");
    for pattern in path_patterns() {
        for captures in pattern.captures_iter(&text) {
            let Some(candidate) = captures.get(1) else {
                continue;
            };
            let candidate = candidate.as_str();
            let expanded = if let Some(rest) = candidate.strip_prefix('~') {
                home_dir().join(rest)
            } else if Path::new(candidate).is_absolute() {
                PathBuf::from(candidate)
            } else {
                cwd.join(candidate)
            };
            if classify_trusted_path(&expanded, roots) != WorkspaceScope::Workspace {
                return true;
            }
        }
    }
    false
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `command.split(/&&|\|\||[;&|\n]/)` — the segment boundaries the cd
/// walk respects.
fn segment_split(command: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut rest = command;
    while !rest.is_empty() {
        let boundary = rest
            .find("&&")
            .map(|index| (index, 2))
            .or_else(|| rest.find("||").map(|index| (index, 2)))
            .or_else(|| rest.find(['&', ';', '|', '\n']).map(|index| (index, 1)));
        match boundary {
            Some((index, width)) => {
                segments.push(&rest[..index]);
                rest = &rest[index + width..];
            }
            None => {
                segments.push(rest);
                break;
            }
        }
    }
    segments
}

fn quoted_path(target: &str) -> Option<String> {
    let bytes = target.as_bytes();
    if bytes.first() == Some(&b'"') && bytes.last() == Some(&b'"') && target.len() >= 2 {
        return Some(target[1..target.len() - 1].to_owned());
    }
    if bytes.first() == Some(&b'\'') && bytes.last() == Some(&b'\'') && target.len() >= 2 {
        return Some(target[1..target.len() - 1].to_owned());
    }
    None
}

fn regex(pattern: &str) -> Regex {
    // Every pattern here is a static constant verified by
    // `static_patterns_compile`; a failure is a programmer error.
    Regex::new(pattern)
        .unwrap_or_else(|error| panic!("static pattern `{pattern}` is valid: {error}"))
}

fn cd_regex() -> &'static Regex {
    static CD: OnceLock<Regex> = OnceLock::new();
    CD.get_or_init(|| {
        regex(r"(?i)\b(cd|chdir|pushd|push-location|popd|pop-location|set-location|sl)\b(.*)$")
    })
}

/// The install/privilege/home/environment access rules, verbatim from
/// the source (one pattern per rule).
fn install_regexes() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r"(?i)\b(?:npm|pnpm|bun)\s+(?:install|i|add)\b[^&;|]*(?:\s-g\b|\s--global\b|\s--location=global\b|\s--prefix(?:=|\s))",
            r"(?i)\b(?:npm|pnpm|bun)\s+link\b|\byarn\s+global\s+(?:add|remove|upgrade)\b",
            r"(?i)\b(?:pip\d*|python(?:\d+(?:\.\d+)?)?)\s+(?:-m\s+pip\s+)?(?:install|uninstall)\b",
            r"(?i)\b(?:brew|apt|apt-get|dnf|yum|pacman|zypper|choco|scoop|winget)\s+(?:install|remove|uninstall|upgrade|update|link|tap)\b",
            r"(?:^|[;&|\s])(?:sudo|doas)\s|\b(?:Invoke-Expression|iex)\b",
            r"(?i)\$\(|`[^`]*`|\$\{[^}]+\}|\$(?:HOME|USERPROFILE|APPDATA|LOCALAPPDATA)\b|\$env:(?:USERPROFILE|APPDATA|LOCALAPPDATA|HOMEDRIVE|HOMEPATH)\b|%(?:USERPROFILE|APPDATA|LOCALAPPDATA)%",
        ]
        .iter()
        .map(|pattern| regex(pattern))
        .collect()
    })
}

fn remote_regex() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        regex(
            r"(?i)\b(?:curl|wget|iwr|Invoke-WebRequest|irm|Invoke-RestMethod|Start-BitsTransfer|bitsadmin)\b",
        )
    })
}

fn piped_shell_regex() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        regex(
            r"(?i)\|\s*(?:[^\s|;&]*[\\/])?(?:sh|bash|zsh|dash|ksh|csh|tcsh|fish|python\d*|node|perl|ruby|php|lua|osascript|iex|Invoke-Expression|cmd|powershell|pwsh)\b",
        )
    })
}

fn url_mask_regex() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| regex(r#"(?i)https?://[^\s"'`<>|;&,\)\]\}]*"#))
}

/// The five path shapes the source scans after URLs are masked out.
fn path_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r#"(?:^|[\s"'=:(])([A-Za-z]:[\\/][^\s"'`<>|;&,\)\]\}]*)"#,
            r#"(?:^|[\s"'=:(])(\\\\[^\\/\s]+[\\/][^\s"'`<>|;&,\)\]\}]*)"#,
            r#"(?:^|[\s"'=:(])((?:\.\.|\.\.?)[\\/][^\s"'`<>|;&,\)\]\}]*)"#,
            r#"(?:^|[\s"'=:(])(~[\\/][^\s"'`<>|;&,\)\]\}]*)"#,
            // The source uses a negative lookahead (`/(?!/)`); the Rust
            // engine has none, so the alternation states it directly: a
            // slash leading a token, or a lone slash on a boundary.
            r#"(?:^|[\s"'=:(])((?:/[^/\s"'`<>|;&,\)\]\}][^\s"'`<>|;&,\)\]\}]*)|(?:/[\s"'`<>|;&,\)\]\}]|$))"#,
        ]
        .iter()
        .map(|pattern| regex(pattern))
        .collect()
    })
}

// --- the shell run ----------------------------------------------------------

/// Removes terminal escape sequences and control bytes — the one rule
/// for what the model may see. Kept public as a pure function so the
/// cleaning is tested without a shell.
pub fn clean_output(raw: &str) -> String {
    static OSC: OnceLock<Regex> = OnceLock::new();
    static CSI: OnceLock<Regex> = OnceLock::new();
    static FE: OnceLock<Regex> = OnceLock::new();
    let osc = OSC.get_or_init(|| regex(r"\x1b\][^\x07]*(?:\x07|\x1b\\)"));
    let csi = CSI.get_or_init(|| regex(r"\x1b\[[0-?]*[ -/]*[@-~]"));
    let fe = FE.get_or_init(|| regex(r"\x1b[@-_]"));
    let cleaned = osc.replace_all(raw, "");
    let cleaned = csi.replace_all(&cleaned, "");
    let cleaned = fe.replace_all(&cleaned, "");
    cleaned
        .chars()
        .filter(|ch| {
            let code = *ch as u32;
            !(code <= 0x08
                || (0x0B..=0x0C).contains(&code)
                || (0x0E..=0x1F).contains(&code)
                || code == 0x7F)
        })
        .collect()
}

/// Terminates everything the shell started, not just the shell — best
/// effort by design: a timeout must not be delayed by a process that
/// refuses to die.
fn kill_process_tree(pid: u32) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        // The shell was spawned in its own process group, so its pid is
        // the group id.
        let _ = Command::new("kill")
            .args(["-9", &format!("-{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

struct StreamCapture {
    root: PathBuf,
    inline: String,
    truncated: bool,
    spill: Option<ToolOutputWriter>,
    spill_channel: Option<&'static str>,
    prefix_written: bool,
}

impl StreamCapture {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            inline: String::new(),
            truncated: false,
            spill: None,
            spill_channel: None,
            prefix_written: false,
        }
    }

    fn append(&mut self, text: &str, channel: &'static str, session: Option<&str>) {
        self.inline.push_str(text);
        let overflow = self.inline.chars().count() > TOOL_OUTPUT_COMMAND_CHARS;
        if overflow {
            self.truncated = true;
        }
        if overflow || self.spill.is_some() {
            if self.spill.is_none() {
                self.spill = ToolOutputWriter::open_at(&self.root, session, "execute_command").ok();
                if let Some(spill) = self.spill.as_mut() {
                    let buffered = std::mem::take(&mut self.inline);
                    spill.write(&format!(
                        "[{channel}]\n{}{}\n",
                        buffered,
                        if buffered.ends_with('\n') || buffered.is_empty() {
                            ""
                        } else {
                            "\n"
                        }
                    ));
                    self.spill_channel = Some(channel);
                    self.prefix_written = true;
                }
            }
            if let Some(spill) = self.spill.as_mut() {
                if self.spill_channel != Some(channel) {
                    spill.write(&format!("\n[{channel}]\n"));
                    self.spill_channel = Some(channel);
                }
                spill.write(text);
            }
        }
        if self.inline.chars().count() > TOOL_OUTPUT_COMMAND_CHARS {
            let kept: String = self
                .inline
                .chars()
                .take(TOOL_OUTPUT_COMMAND_CHARS)
                .collect();
            self.inline = kept;
        }
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    pipe: Option<R>,
    capture: Arc<Mutex<StreamCapture>>,
    channel: &'static str,
    session: Option<String>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut pipe = match pipe {
            Some(pipe) => pipe,
            None => return,
        };
        let mut buffer = [0u8; 8192];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let text = String::from_utf8_lossy(&buffer[..count]).into_owned();
                    if let Ok(mut capture) = capture.lock() {
                        capture.append(&text, channel, session.as_deref());
                    }
                }
                Err(_) => break,
            }
        }
    })
}

fn run_shell(command: &str, cwd: &Path, env: &ExecuteEnvironment, timeout: Duration) -> Value {
    let shell: ResolvedShell = match shell::resolve_shell(env.command_shell.as_deref()) {
        Ok(shell) => shell,
        Err(error) => {
            return json!({
                "status": "failed",
                "exitCode": Value::Null,
                "stdout": "",
                "stderr": "",
                "error": format!("Shell configuration: {error}"),
                "truncated": false,
            });
        }
    };
    let mut arguments = shell.launcher_args.clone();
    arguments.push(command.to_owned());

    let mut command_builder = Command::new(&shell.executable);
    command_builder
        .args(&arguments)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Never flash a console window for a background tool call.
        command_builder.creation_flags(0x0800_0000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group so a stop can signal the whole group.
        command_builder.process_group(0);
    }

    let mut child = match command_builder.spawn() {
        Ok(child) => child,
        Err(error) => {
            return json!({
                "status": "failed",
                "exitCode": Value::Null,
                "stdout": "",
                "stderr": "",
                "error": error.to_string(),
                "truncated": false,
            });
        }
    };
    let pid = child.id();

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let session = env.session_id.clone();
    let stdout_capture = Arc::new(Mutex::new(StreamCapture::new(env.data_dir.clone())));
    let stderr_capture = Arc::new(Mutex::new(StreamCapture::new(env.data_dir.clone())));
    let readers = [
        spawn_reader(
            stdout_pipe,
            Arc::clone(&stdout_capture),
            "stdout",
            session.clone(),
        ),
        spawn_reader(stderr_pipe, Arc::clone(&stderr_capture), "stderr", session),
    ];

    // One absolute deadline for the whole tree.
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    kill_process_tree(pid);
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.wait();
                break None;
            }
        }
    };
    for reader in readers {
        let _ = reader.join();
    }

    let mut stdout = stdout_capture.lock().unwrap_or_else(|p| p.into_inner());
    let mut stderr = stderr_capture.lock().unwrap_or_else(|p| p.into_inner());
    let mut result = serde_json::Map::new();
    if timed_out {
        result.insert("status".into(), json!("timed_out"));
        result.insert(
            "error".into(),
            json!(format!(
                "Tool exceeded {} seconds. Cancellation was requested; inspect the target before retrying because external effects may be incomplete.",
                timeout.as_secs()
            )),
        );
        result.insert("exitCode".into(), Value::Null);
    } else {
        let code = status.and_then(|status| status.code());
        result.insert(
            "status".into(),
            json!(if code == Some(0) {
                "completed"
            } else {
                "failed"
            }),
        );
        result.insert(
            "exitCode".into(),
            match code {
                Some(code) => json!(code),
                None => Value::Null,
            },
        );
        result.insert("error".into(), Value::Null);
    }
    result.insert("stdout".into(), json!(clean_output(&stdout.inline)));
    result.insert("stderr".into(), json!(clean_output(&stderr.inline)));
    result.insert(
        "truncated".into(),
        json!(stdout.truncated || stderr.truncated),
    );

    let spill = stdout
        .spill
        .as_mut()
        .map(|writer| writer.close())
        .or_else(|| stderr.spill.as_mut().map(|writer| writer.close()));
    if let Some((path, saved_chars, dropped_chars)) = spill {
        let total = stdout.inline.chars().count() + stderr.inline.chars().count() + saved_chars;
        result.insert("spilledTo".into(), json!(path.display().to_string()));
        result.insert(
            "note".into(),
            json!(tool_output_notice(&path, total, dropped_chars)),
        );
    }
    Value::Object(result)
}
