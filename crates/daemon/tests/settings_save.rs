//! Write-authority pins: a settings copy earns the right to write by
//! loading first, and loses it when the disk moves under it.
//!
//! Source: `settings.ts` `saveSettings` / `saveSettingsUnlocked` /
//! `diskSettingsRevision` (read 2026-11-16). The guard is the defect
//! class that wiped the owner's settings: a defaults object saved over
//! a live configuration because nothing tied the copy to what it read.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use openbutler_daemon::settings::{LoadedSettings, load_settings, save_settings};

const STALE: &str = "Settings changed after this copy was loaded. Reload settings before saving to avoid overwriting another writer.";
const NEVER_LOADED: &str =
    "Refusing to overwrite existing configuration: this store was never loaded from";

fn scratch(tag: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("ob-settings-save-{}-{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("scratch dir");
    root
}

fn settings_path(root: &Path) -> PathBuf {
    root.join("settings.yaml")
}

fn config_path(root: &Path) -> PathBuf {
    root.join("config.yaml")
}

fn put(root: &Path, name: &str, body: &str) {
    fs::write(root.join(name), body).expect("fixture write");
}

fn read(root: &Path, name: &str) -> String {
    fs::read_to_string(root.join(name)).expect("fixture read")
}

#[test]
fn a_loaded_copy_saves_and_the_next_save_sees_its_own_write() {
    let root = scratch("roundtrip");
    let mut loaded = load_settings(&root).expect("load on an empty machine");
    loaded.store.default_provider = "vercel".to_owned();
    save_settings(&root, &mut loaded).expect("create");
    assert!(settings_path(&root).exists(), "settings.yaml is created");
    assert!(config_path(&root).exists(), "config.yaml is created");

    // The saved revision matches the disk, so the same copy can save again.
    save_settings(&root, &mut loaded).expect("save after its own write");
    let reloaded = load_settings(&root).expect("reload");
    assert_eq!(reloaded.store.default_provider, "vercel");
}

#[test]
fn an_outside_write_makes_the_copy_stale() {
    let root = scratch("stale");
    let mut loaded = load_settings(&root).expect("load");
    save_settings(&root, &mut loaded).expect("create");
    put(&root, "settings.yaml", "defaultProvider: other\n");

    let before = read(&root, "settings.yaml");
    let error = save_settings(&root, &mut loaded).expect_err("stale save");
    assert_eq!(error.0, STALE);
    assert_eq!(read(&root, "settings.yaml"), before, "nothing is written");
}

#[test]
fn a_never_loaded_store_refuses_to_replace_existing_configuration() {
    let root = scratch("never-loaded");
    put(&root, "settings.yaml", "defaultProvider: kept\n");
    put(&root, "config.yaml", "providers:\n  a:\n    keys: [k]\n");
    let settings_before = read(&root, "settings.yaml");
    let config_before = read(&root, "config.yaml");

    let mut from_defaults =
        LoadedSettings::never_loaded(openbutler_daemon::settings::default_settings());
    let error = save_settings(&root, &mut from_defaults).expect_err("refuse");
    assert!(
        error.0.starts_with(NEVER_LOADED),
        "message names the guard: {}",
        error.0
    );
    assert!(
        error
            .0
            .contains(&settings_path(&root).display().to_string()),
        "path is interpolated: {}",
        error.0
    );
    assert_eq!(
        read(&root, "settings.yaml"),
        settings_before,
        "settings untouched"
    );
    assert_eq!(
        read(&root, "config.yaml"),
        config_before,
        "secrets untouched"
    );
}

#[test]
fn never_loaded_defaults_may_create_an_empty_machine() {
    let root = scratch("create-empty");
    let mut from_defaults =
        LoadedSettings::never_loaded(openbutler_daemon::settings::default_settings());
    save_settings(&root, &mut from_defaults).expect("creating an empty machine is allowed");
    assert!(settings_path(&root).exists());
    assert!(config_path(&root).exists());
}

#[test]
fn providers_round_trip_ordered_with_keys_split_into_secrets() {
    let root = scratch("split");
    put(
        &root,
        "settings.yaml",
        "providers:\n  b:\n    enabled: true\n  a:\n    enabled: true\nproviderOrder: [b, a]\ndefaultProvider: b\n",
    );
    let mut loaded = load_settings(&root).expect("load");
    // The store merged secret keys in; saving splits them back out.
    loaded.store.default_provider = "a".to_owned();
    save_settings(&root, &mut loaded).expect("save");

    let settings = read(&root, "settings.yaml");
    assert!(
        !settings.contains("keys:"),
        "keys never land in settings.yaml"
    );
    assert!(
        settings.contains("providerOrder"),
        "the declared order is written"
    );
    assert!(
        settings.contains("- b"),
        "the first ordered id is written first"
    );

    let reloaded = load_settings(&root).expect("reload");
    assert_eq!(reloaded.store.default_provider, "a");
    assert_eq!(
        reloaded.store.provider_order,
        vec!["b".to_owned(), "a".to_owned()]
    );
}

#[test]
fn an_unreadable_secrets_store_refuses_the_save() {
    let root = scratch("bad-secrets");
    let mut loaded = load_settings(&root).expect("load");
    save_settings(&root, &mut loaded).expect("create");
    put(&root, "config.yaml", "providers: [this is not a mapping]\n");
    let settings_before = read(&root, "settings.yaml");

    let error = save_settings(&root, &mut loaded).expect_err("unreadable secrets");
    assert!(
        !error.0.is_empty(),
        "the parse refusal surfaces: {}",
        error.0
    );
    assert_eq!(
        read(&root, "settings.yaml"),
        settings_before,
        "nothing committed"
    );
}

#[test]
fn a_team_ticket_rotation_does_not_invalidate_the_copy() {
    let root = scratch("ticket-rotation");
    let mut loaded = load_settings(&root).expect("load");
    save_settings(&root, &mut loaded).expect("create");

    // Another owner rotates the team ticket; provider keys are untouched.
    put(&root, "config.yaml", "teamTicket: rotated\n");
    save_settings(&root, &mut loaded).expect("save still allowed");
    let reloaded = load_settings(&root).expect("reload");
    assert!(
        read(&root, "config.yaml").contains("rotated"),
        "the rotated ticket survives the save"
    );
    let _ = reloaded;
}

#[test]
fn a_provider_key_rotation_invalidates_the_copy() {
    let root = scratch("key-rotation");
    let mut loaded = load_settings(&root).expect("load");
    save_settings(&root, &mut loaded).expect("create");

    // A separate owner rotates a provider key; the copy did not see it.
    put(
        &root,
        "config.yaml",
        "providers:\n  x:\n    keys: [rotated]\n",
    );
    let error = save_settings(&root, &mut loaded).expect_err("stale after key rotation");
    assert_eq!(error.0, STALE);
}

#[test]
fn save_is_deterministic_for_the_same_store() {
    let root = scratch("deterministic");
    let mut loaded = load_settings(&root).expect("load");
    loaded.store.default_provider = "vercel".to_owned();
    save_settings(&root, &mut loaded).expect("first");
    let first = read(&root, "settings.yaml");

    save_settings(&root, &mut loaded).expect("second");
    let second = read(&root, "settings.yaml");
    assert_eq!(
        first, second,
        "same store, same bytes — the revision hash reads what we wrote"
    );
}
