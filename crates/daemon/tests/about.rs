//! `about.report` pins: the version, the resolved shell (the same
//! authority the tool uses), the runtime triple and the two configuration
//! paths — never a file's contents.
//!
//! Source: `cli-main.ts` `runAbout` (L997-1006) and `configuredShellLabel`
//! (L985-995), read 2026-11-26.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use common::temp_data_dir;
use openbutler_daemon::dispatch::{DispatchState, Host, handle};
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::Value;

fn call(dir: &Path) -> Value {
    let host = Host::new();
    host.reload_settings(dir).expect("settings load");
    let request = DaemonRequest::new("r1", "about.report");
    let DaemonResponse::Success { result, .. } =
        handle(&host, dir, &mut DispatchState::new(), &request)
    else {
        panic!("expected a success envelope");
    };
    result
}

#[test]
fn about_names_the_program_the_shell_and_the_paths() {
    let dir = temp_data_dir("about");
    let result = call(&dir);
    assert_eq!(result["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(result["pid"], std::process::id());
    // The runtime row is this host's os/arch triple, not a Node version.
    assert!(result["runtime"].as_str().unwrap().contains('/'));
    let settings = result["settingsPath"].as_str().unwrap();
    let secrets = result["secretsPath"].as_str().unwrap();
    assert!(settings.ends_with("settings.yaml"), "{settings}");
    assert!(secrets.ends_with("config.yaml"), "{secrets}");
    assert!(!result["packages"].as_str().unwrap().is_empty());
    // A resolved shell label reads "<name> (<dialect>, <source>)".
    let shell = result["shell"].as_str().unwrap();
    assert!(shell.contains('('), "{shell}");
}

#[cfg(windows)]
#[test]
fn about_reports_the_configured_shell() {
    let dir = temp_data_dir("about-shell");
    std::fs::write(dir.join("settings.yaml"), "commandShell: cmd\n").unwrap();
    let result = call(&dir);
    let shell = result["shell"].as_str().unwrap();
    assert!(shell.contains("cmd"), "{shell}");
}

#[test]
fn about_never_reads_the_secrets_file() {
    // The answer names paths only: a config.yaml full of keys must not
    // leak a single character of their values.
    let dir = temp_data_dir("about-secrets");
    std::fs::write(
        dir.join("config.yaml"),
        "providers:\n  mock:\n    keys:\n      - sk-super-secret-value\n",
    )
    .unwrap();
    let result = call(&dir);
    let encoded = result.to_string();
    assert!(!encoded.contains("sk-super-secret-value"), "{encoded}");
}
