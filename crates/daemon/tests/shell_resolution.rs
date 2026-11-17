//! Shell resolution for `execute_command`: one setting, three shapes,
//! platform probes injected so the resolver never consults the host to
//! be checked.
//!
//! Source: legacy `src/tools/shell.ts`, read 2026-11-17.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use openbutler_daemon::tools::shell::{
    ResolvedShell, ShellDialect, ShellEnvironment, is_shell_preset, is_shell_setting_value,
    resolve_shell_with,
};

/// A synthetic machine: PATH contents and an existence set, no host I/O.
fn environment(is_windows: bool, path: &str, extra: &[(&str, &str)]) -> ShellEnvironment {
    let mut env = ShellEnvironment {
        is_windows,
        path: path.to_owned(),
        pathext: if is_windows {
            ".EXE;.CMD;.BAT;.COM".to_owned()
        } else {
            String::new()
        },
        shell: None,
        program_files: None,
        program_files_x86: None,
        local_appdata: None,
    };
    for (key, value) in extra {
        match *key {
            "SHELL" => env.shell = Some((*value).to_owned()),
            "PATHEXT" => env.pathext = (*value).to_owned(),
            "ProgramFiles" => env.program_files = Some((*value).to_owned()),
            "ProgramFiles(x86)" => env.program_files_x86 = Some((*value).to_owned()),
            "LOCALAPPDATA" => env.local_appdata = Some((*value).to_owned()),
            other => panic!("unexpected fixture key {other}"),
        }
    }
    env
}

fn existing<'a>(names: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
    move |candidate: &str| {
        let base = candidate
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(candidate)
            .to_ascii_lowercase();
        names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(candidate) || name.eq_ignore_ascii_case(&base))
    }
}

fn resolve(
    configured: Option<&str>,
    env: &ShellEnvironment,
    has: impl Fn(&str) -> bool,
) -> Result<ResolvedShell, String> {
    resolve_shell_with(configured, env, &has)
}

#[test]
fn auto_on_windows_walks_the_preference_order() {
    let env = environment(true, "", &[]);
    let only_powershell = |candidate: &str| candidate.eq_ignore_ascii_case("powershell");
    let shell = resolve(None, &env, only_powershell).expect("resolves");
    assert_eq!(shell.executable, "powershell");
    assert_eq!(shell.dialect, ShellDialect::Powershell);
    assert_eq!(shell.label, "powershell (powershell, auto)");
    assert_eq!(
        shell.launcher_args,
        vec!["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"]
    );

    let nothing = |_candidate: &str| false;
    let shell = resolve(None, &env, nothing).expect("cmd fallback");
    assert_eq!(shell.executable, "cmd");
    assert_eq!(shell.dialect, ShellDialect::Cmd);
    assert_eq!(shell.launcher_args, vec!["/d", "/s", "/c"]);
}

#[test]
fn auto_on_posix_prefers_the_shell_environment() {
    let env = environment(false, "/bin:/usr/bin", &[("SHELL", "/usr/bin/zsh")]);
    let has = existing(&["/usr/bin/zsh"]);
    let shell = resolve(None, &env, has).expect("resolves");
    assert_eq!(shell.executable, "/usr/bin/zsh");
    assert_eq!(shell.dialect, ShellDialect::Posix);
    assert_eq!(shell.label, "zsh (posix, auto)");
    assert_eq!(shell.launcher_args, vec!["-c"]);

    let env = environment(false, "/bin", &[]);
    let shell = resolve(None, &env, |_| false).expect("fallback");
    assert_eq!(shell.executable, "/bin/sh");
    assert_eq!(shell.label, "sh (posix, auto)");
}

#[test]
fn the_executable_filename_alone_picks_the_dialect() {
    // An arbitrary path maps exactly like its preset twin.
    let env = environment(true, "", &[]);
    let shell = resolve(Some(r"C:\tools\pwsh.exe"), &env, |_| true).expect("resolves");
    assert_eq!(shell.dialect, ShellDialect::Powershell);
    assert_eq!(shell.label, "pwsh (powershell, path)");
    assert_eq!(
        shell.launcher_args,
        vec!["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"]
    );

    let shell = resolve(Some("/opt/fish"), &env, |_| true).expect("resolves");
    assert_eq!(shell.dialect, ShellDialect::Posix);
    assert_eq!(shell.launcher_args, vec!["-c"]);
}

#[test]
fn presets_resolve_by_name_and_wsl_carries_its_flags() {
    let env = environment(true, "", &[]);
    let shell = resolve(Some("bash"), &env, |_| false).expect("resolves");
    assert_eq!(shell.executable, "bash");
    assert_eq!(shell.dialect, ShellDialect::Posix);
    assert_eq!(shell.label, "bash (posix, preset)");

    let shell = resolve(Some("wsl"), &env, |_| false).expect("resolves");
    assert_eq!(shell.executable, "wsl.exe");
    assert_eq!(shell.label, "wsl (posix, preset)");
    assert_eq!(shell.launcher_args, vec!["-e", "bash", "-c"]);
}

#[test]
fn a_missing_path_and_an_unknown_word_explain_themselves() {
    let env = environment(true, "", &[]);
    let error = resolve(Some(r"C:\gone\pwsh.exe"), &env, |_| false).unwrap_err();
    assert_eq!(
        error,
        "commandShell points at a path that does not exist: C:\\gone\\pwsh.exe"
    );

    let error = resolve(Some("nushell"), &env, |_| false).unwrap_err();
    assert_eq!(
        error,
        "commandShell must be one of auto, pwsh, powershell, cmd, git-bash, wsl, bash, zsh, sh \
         or an executable path; got \"nushell\""
    );
}

#[test]
fn git_bash_skips_the_wsl_stubs_and_falls_back_to_install_roots() {
    // PATH lists the System32 stub and a real Git Bash install.
    let env = environment(
        true,
        r"C:\Windows\System32;C:\Program Files\Git\bin",
        &[
            ("ProgramFiles", r"C:\Program Files"),
            ("ProgramFiles(x86)", r"C:\Program Files (x86)"),
            ("LOCALAPPDATA", r"C:\Users\u\AppData\Local"),
        ],
    );
    let has = existing(&[r"C:\Program Files\Git\bin\bash.exe"]);
    let shell = resolve(Some("git-bash"), &env, has).expect("resolves");
    assert_eq!(
        shell.executable, r"C:\Program Files\Git\bin\bash.exe",
        "the System32 stub is skipped, the real bash wins"
    );
    assert_eq!(shell.label, "bash (posix, preset)");

    // Nothing anywhere: the error names both discovery routes.
    let error = resolve(Some("git-bash"), &env, |_| false).unwrap_err();
    assert_eq!(
        error,
        "commandShell git-bash was not found on PATH or in the install roots of this system; \
         add bash to PATH or set commandShell to its full path"
    );
}

#[test]
fn the_settings_shape_check_accepts_presets_and_paths_only() {
    assert!(is_shell_preset("auto"));
    assert!(!is_shell_preset("AUTO"), "presets are exact, not fuzzy");
    assert!(is_shell_setting_value(" pwsh "));
    assert!(is_shell_setting_value(r"C:\bin\sh.exe"));
    assert!(is_shell_setting_value("/bin/sh"));
    assert!(!is_shell_setting_value("nushell"));
}

#[test]
fn labels_keep_the_source_and_lowercase_basename() {
    let env = environment(false, "", &[]);
    let shell = resolve(Some(r"D:\Tools\PowerShell.EXE"), &env, |_| true).expect("path shape");
    assert_eq!(shell.label, "powershell (powershell, path)");
}
