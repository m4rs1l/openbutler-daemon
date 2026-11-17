//! The shell `execute_command` launches, resolved by rules that hold on
//! any machine: PATH, PATHEXT, `$SHELL` and environment-stated install
//! roots only — no location found on this computer is recorded here.
//!
//! Source: legacy `src/tools/shell.ts`, read 2026-11-17. Tests inject
//! platform, environment and the existence probe, so the resolver never
//! consults the host to be checked.

/// Shell family the command text is written for; it decides the flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellDialect {
    Posix,
    Powershell,
    Cmd,
}

impl ShellDialect {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Posix => "posix",
            Self::Powershell => "powershell",
            Self::Cmd => "cmd",
        }
    }
}

/// A launch plan for spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedShell {
    /// Program for spawn: either a PATH name or an absolute path.
    pub executable: String,
    /// Arguments placed before the command string itself.
    pub launcher_args: Vec<String>,
    pub dialect: ShellDialect,
    /// Identity for the tool description: `pwsh (powershell, auto)`.
    pub label: String,
}

/// Every accepted non-path value of `commandShell`.
pub const SHELL_PRESETS: [&str; 9] = [
    "auto",
    "pwsh",
    "powershell",
    "cmd",
    "git-bash",
    "wsl",
    "bash",
    "zsh",
    "sh",
];

pub fn is_shell_preset(value: &str) -> bool {
    SHELL_PRESETS.contains(&value)
}

/// Settings-shape check: a preset name or anything path-shaped.
pub fn is_shell_setting_value(value: &str) -> bool {
    let trimmed = value.trim();
    is_shell_preset(trimmed) || trimmed.contains('/') || trimmed.contains('\\')
}

/// The slice of the process environment the resolver reads. Tests build
/// it directly; production reads the real one.
#[derive(Debug, Clone)]
pub struct ShellEnvironment {
    pub is_windows: bool,
    pub path: String,
    pub pathext: String,
    pub shell: Option<String>,
    pub program_files: Option<String>,
    pub program_files_x86: Option<String>,
    pub local_appdata: Option<String>,
}

impl ShellEnvironment {
    pub fn from_process() -> Self {
        let get = |key: &str| std::env::var(key).ok();
        Self {
            is_windows: cfg!(windows),
            path: get("PATH").or_else(|| get("Path")).unwrap_or_default(),
            pathext: get("PATHEXT").unwrap_or_else(|| ".EXE;.CMD;.BAT;.COM".to_owned()),
            shell: get("SHELL"),
            program_files: get("ProgramFiles"),
            program_files_x86: get("ProgramFiles(x86)"),
            local_appdata: get("LOCALAPPDATA"),
        }
    }

    fn delimiter(&self) -> char {
        if self.is_windows { ';' } else { ':' }
    }
}

// Flags belong to the dialect, never to a machine: PowerShell and CMD
// parse their own command line, POSIX shells read one trailing string,
// and WSL execs bash inside the guest.
const POWERSHELL_FLAGS: [&str; 4] = ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"];
const CMD_FLAGS: [&str; 3] = ["/d", "/s", "/c"];
const POSIX_FLAGS: [&str; 1] = ["-c"];
const WSL_FLAGS: [&str; 3] = ["-e", "bash", "-c"];

/// Joins the way the *injected* platform joins, so fixtures behave the
/// same on every host the tests run on.
fn join_path(is_windows: bool, parts: &[&str]) -> String {
    let separator = if is_windows { '\\' } else { '/' };
    parts.join(&separator.to_string())
}

fn base_name(executable: &str) -> String {
    let last = executable.rsplit(['/', '\\']).next().unwrap_or(executable);
    let lower = last.to_ascii_lowercase();
    lower
        .strip_suffix(".exe")
        .map(str::to_owned)
        .unwrap_or(lower)
}

/// The executable's own filename picks the dialect, so an arbitrary path
/// maps exactly like its preset twin; anything else is POSIX.
fn dialect_of(executable: &str) -> ShellDialect {
    match base_name(executable).as_str() {
        "pwsh" | "powershell" => ShellDialect::Powershell,
        "cmd" => ShellDialect::Cmd,
        _ => ShellDialect::Posix,
    }
}

fn flags_for(dialect: ShellDialect) -> Vec<String> {
    match dialect {
        ShellDialect::Powershell => POWERSHELL_FLAGS
            .iter()
            .map(|flag| (*flag).to_owned())
            .collect(),
        ShellDialect::Cmd => CMD_FLAGS.iter().map(|flag| (*flag).to_owned()).collect(),
        ShellDialect::Posix => POSIX_FLAGS.iter().map(|flag| (*flag).to_owned()).collect(),
    }
}

fn entry(
    executable: &str,
    dialect: ShellDialect,
    source: &str,
    flags: Option<Vec<String>>,
) -> ResolvedShell {
    ResolvedShell {
        executable: executable.to_owned(),
        launcher_args: flags.unwrap_or_else(|| flags_for(dialect)),
        dialect,
        label: format!(
            "{} ({}, {})",
            base_name(executable),
            dialect.as_str(),
            source
        ),
    }
}

fn path_dirs(env: &ShellEnvironment) -> Vec<String> {
    env.path
        .split(env.delimiter())
        .filter(|dir| !dir.is_empty() && is_absolute_path(env.is_windows, dir))
        .map(str::to_owned)
        .collect()
}

fn is_absolute_path(is_windows: bool, value: &str) -> bool {
    if is_windows {
        let bytes = value.as_bytes();
        (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/'))
            || value.starts_with("\\\\")
    } else {
        value.starts_with('/')
    }
}

/// The `where.exe` fallback for Windows name resolution (app-execution
/// aliases stat cannot see). Returns `(status, stdout)` like the source.
fn system_where_probe(command: &str, args: &[&str]) -> (Option<i32>, String) {
    let mut child = match std::process::Command::new(command)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return (None, String::new()),
    };
    // Bounded wait: `where.exe` is fast, but resolution must not hang.
    for _ in 0..200 {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut buffer = Vec::new();
                if let Some(mut stdout) = child.stdout.take() {
                    use std::io::Read;
                    let _ = stdout.read_to_end(&mut buffer);
                }
                return (status.code(), String::from_utf8_lossy(&buffer).into_owned());
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    (None, String::new())
}

fn where_found(candidate: &str) -> bool {
    let (status, stdout) = system_where_probe("where.exe", &[candidate]);
    status == Some(0) && !stdout.trim().is_empty()
}

/// Probe for the `auto`/path walk: stat each PATH candidate first
/// (works on every system), then ask `where.exe` (Windows only) when
/// stat itself is blind.
pub fn path_probe(env: &ShellEnvironment) -> impl Fn(&str) -> bool + '_ {
    let extensions: Vec<String> = if env.is_windows {
        env.pathext
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| extension.to_ascii_lowercase())
            .collect()
    } else {
        Vec::new()
    };
    let dirs = path_dirs(env);
    move |candidate: &str| {
        if candidate.contains('/') || candidate.contains('\\') {
            return std::path::Path::new(candidate).exists()
                || (env.is_windows && where_found(candidate));
        }
        for dir in &dirs {
            let plain = join_path(env.is_windows, &[dir, candidate]);
            if std::path::Path::new(&plain).exists() {
                return true;
            }
            for extension in &extensions {
                let extended =
                    join_path(env.is_windows, &[dir, &format!("{candidate}{extension}")]);
                if std::path::Path::new(&extended).exists() {
                    return true;
                }
            }
        }
        env.is_windows && where_found(candidate)
    }
}

/// Windows hosts two unrelated `bash.exe` launchers: the WSL stubs and
/// real shells such as Git Bash. A stub is not the shell the setting
/// promised, so `git-bash` skips those directories.
fn is_wsl_stub_dir(dir: &str) -> bool {
    let lower = dir.to_ascii_lowercase();
    let trimmed = lower.trim_end_matches(['/', '\\']);
    trimmed.ends_with("\\system32")
        || trimmed.ends_with("/system32")
        || trimmed.ends_with("\\windowsapps")
        || trimmed.ends_with("/windowsapps")
}

/// Bash outside PATH can only be found through install roots the
/// environment itself states; none of them is hard-coded.
fn git_bash_candidates(env: &ShellEnvironment) -> Vec<String> {
    let mut roots: Vec<String> = Vec::new();
    if let Some(root) = env.program_files.as_deref().filter(|root| !root.is_empty()) {
        roots.push(root.to_owned());
    }
    if let Some(root) = env
        .program_files_x86
        .as_deref()
        .filter(|root| !root.is_empty())
    {
        roots.push(root.to_owned());
    }
    if let Some(root) = env.local_appdata.as_deref().filter(|root| !root.is_empty()) {
        roots.push(join_path(env.is_windows, &[root, "Programs"]));
    }
    roots
        .iter()
        .map(|root| join_path(env.is_windows, &[root, "Git", "bin", "bash.exe"]))
        .collect()
}

fn find_git_bash(env: &ShellEnvironment, has: &dyn Fn(&str) -> bool) -> Option<String> {
    for dir in path_dirs(env) {
        if is_wsl_stub_dir(&dir) {
            continue;
        }
        for name in ["bash.exe", "bash"] {
            let candidate = join_path(env.is_windows, &[&dir, name]);
            if has(&candidate) {
                return Some(candidate);
            }
        }
    }
    git_bash_candidates(env)
        .into_iter()
        .find(|candidate| has(candidate))
}

fn resolve_auto(env: &ShellEnvironment, has: &dyn Fn(&str) -> bool) -> ResolvedShell {
    if env.is_windows {
        // Newest first; a missing program only ends the walk later.
        if has("pwsh") {
            return entry("pwsh", ShellDialect::Powershell, "auto", None);
        }
        if has("powershell") {
            return entry("powershell", ShellDialect::Powershell, "auto", None);
        }
        return entry("cmd", ShellDialect::Cmd, "auto", None);
    }
    if let Some(shell) = env.shell.as_deref().filter(|shell| !shell.is_empty())
        && has(shell)
    {
        return entry(shell, dialect_of(shell), "auto", None);
    }
    entry("/bin/sh", ShellDialect::Posix, "auto", None)
}

/// Production entry point: resolve against the real process environment.
pub fn resolve_shell(configured: Option<&str>) -> Result<ResolvedShell, String> {
    let env = ShellEnvironment::from_process();
    let probe = path_probe(&env);
    resolve_shell_with(configured, &env, &probe)
}

/// Resolution with an injected existence probe — the test seam.
pub fn resolve_shell_with(
    configured: Option<&str>,
    env: &ShellEnvironment,
    has: &dyn Fn(&str) -> bool,
) -> Result<ResolvedShell, String> {
    let value = configured
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("auto");

    if value.contains('/') || value.contains('\\') {
        if !has(value) {
            return Err(format!(
                "commandShell points at a path that does not exist: {value}"
            ));
        }
        return Ok(entry(value, dialect_of(value), "path", None));
    }
    if !is_shell_preset(value) {
        return Err(format!(
            "commandShell must be one of {} or an executable path; got \"{value}\"",
            SHELL_PRESETS.join(", ")
        ));
    }
    match value {
        "auto" => Ok(resolve_auto(env, has)),
        "git-bash" => find_git_bash(env, has).map(|found| entry(&found, ShellDialect::Posix, "preset", None)).ok_or_else(|| {
            "commandShell git-bash was not found on PATH or in the install roots of this system; \
             add bash to PATH or set commandShell to its full path"
                .to_owned()
        }),
        // wsl on Windows execs bash inside the guest; elsewhere it is a
        // plain PATH name whose filename decides the dialect.
        "wsl" if env.is_windows => Ok(entry(
            "wsl.exe",
            ShellDialect::Posix,
            "preset",
            Some(WSL_FLAGS.iter().map(|flag| (*flag).to_owned()).collect()),
        )),
        // pwsh | powershell | cmd | bash | zsh | sh: a plain PATH name
        // whose own filename already decides dialect and flags.
        other => Ok(entry(other, dialect_of(other), "preset", None)),
    }
}
