//! `rememberProviderModel` (settings.ts L561): a picked model joins the
//! provider's saved list without becoming the default — the record that
//! keeps the model showing up in the picker.

use super::schema::SettingsStore;

/// Records `model_id` on the provider's saved list. Unknown providers,
/// blank models and already-saved models are no-ops (the source's early
/// returns); the 512-model cap refuses with the source sentence
/// (settings.ts L567).
pub fn remember_provider_model(
    store: &mut SettingsStore,
    provider_id: &str,
    model_id: &str,
) -> Result<(), String> {
    let normalized = model_id.trim();
    let Some(provider) = store.providers.get_mut(provider_id) else {
        return Ok(());
    };
    if normalized.is_empty() {
        return Ok(());
    }
    if provider
        .models
        .as_ref()
        .is_some_and(|models| models.iter().any(|model| model == normalized))
    {
        return Ok(());
    }
    if provider.models.as_ref().map_or(0, Vec::len) >= 512 {
        return Err(format!(
            "Provider {provider_id} already has 512 saved model IDs."
        ));
    }
    provider
        .models
        .get_or_insert_with(Vec::new)
        .push(normalized.to_owned());
    Ok(())
}
