//! `rememberProviderModel` (settings.ts L561): a picked model joins the
//! provider's saved list without becoming the default, and the change
//! rides the save transaction so it survives a reload.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::temp_data_dir;
use openbutler_daemon::settings::{load_settings, remember_provider_model, save_settings};

/// A store loaded from disk (write authority comes from the read).
fn loaded(dir: &std::path::Path) -> openbutler_daemon::settings::LoadedSettings {
    std::fs::write(
        dir.join("settings.yaml"),
        "providers:\n  mock:\n    enabled: true\n    url: http://127.0.0.1:9/v1\n",
    )
    .unwrap();
    load_settings(dir).expect("settings load")
}

#[test]
fn a_picked_model_joins_the_providers_saved_list() {
    let dir = temp_data_dir("remember-add");
    let mut loaded = loaded(&dir);

    remember_provider_model(&mut loaded.store, "mock", "picked-model").expect("remember");

    assert_eq!(
        loaded.store.providers["mock"].models.as_deref(),
        Some(["picked-model".to_owned()].as_slice()),
        "the model is retained without touching the default"
    );
    assert_eq!(
        loaded.store.default_provider, "",
        "a pick never becomes the default"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_already_saved_model_is_not_duplicated() {
    let dir = temp_data_dir("remember-dup");
    let mut loaded = loaded(&dir);
    remember_provider_model(&mut loaded.store, "mock", "picked").expect("first");
    remember_provider_model(&mut loaded.store, "mock", "picked").expect("second is a no-op");

    let models = loaded.store.providers["mock"].models.clone().unwrap();
    assert_eq!(models, ["picked".to_owned()], "one entry only");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn blank_models_and_unknown_providers_are_no_ops() {
    let dir = temp_data_dir("remember-noop");
    let mut loaded = loaded(&dir);

    remember_provider_model(&mut loaded.store, "mock", "   ").expect("blank is a no-op");
    remember_provider_model(&mut loaded.store, "ghost", "model").expect("unknown is a no-op");

    assert!(
        loaded.store.providers["mock"].models.is_none(),
        "nothing was recorded"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_512_model_cap_refuses_with_the_source_sentence() {
    let dir = temp_data_dir("remember-cap");
    let mut loaded = loaded(&dir);
    let models: Vec<String> = (0..512).map(|index| format!("m{index}")).collect();
    loaded.store.providers.get_mut("mock").unwrap().models = Some(models);

    let error = remember_provider_model(&mut loaded.store, "mock", "one-too-many")
        .expect_err("the cap refuses");
    assert_eq!(
        error, "Provider mock already has 512 saved model IDs.",
        "settings.ts L567"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_remembered_model_survives_the_save_and_reload() {
    let dir = temp_data_dir("remember-save");
    let mut loaded = loaded(&dir);
    remember_provider_model(&mut loaded.store, "mock", "picked-model").expect("remember");
    save_settings(&dir, &mut loaded).expect("save");

    let fresh = load_settings(&dir).expect("reload");
    assert_eq!(
        fresh.store.providers["mock"].models.as_deref(),
        Some(["picked-model".to_owned()].as_slice()),
        "the pick lands in settings.yaml"
    );
    let yaml = std::fs::read_to_string(dir.join("settings.yaml")).unwrap();
    assert!(yaml.contains("picked-model"), "yaml: {yaml}");
    let _ = std::fs::remove_dir_all(&dir);
}
