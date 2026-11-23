//! Which model a surface shows and a fresh session starts on: the explicit
//! default, else the most-used usable model from recorded usage.
//!
//! Sources (read 2026-11-24): `usage/default-model.ts` L14-61
//! (`USAGE_WINDOW_MS`, `pickMostUsedModel`, `resolveDefaultModel`),
//! `model-roles.ts` L58-68 (`resolveRoleModel`), `provider/registry.ts`
//! L965-1006 (`providerIsConfigured`, `modelIsUsable`, `toolCallMode`) and
//! `settings.ts` L555 (`hasCredential`).
//!
//! One reading, two consumers: `config.report` shows it and a fresh draft
//! starts on it (`create_draft`), so the manager screen and a run can never
//! disagree about which model the machine will use.
//!
//! Nothing here writes settings: only the explicit commands change the
//! default; a model picked inside a session belongs to that session.

use crate::catalog::CatalogStore;
use crate::ledger::UsagePick;
use crate::settings::schema::{ProviderState, SettingsStore};

/// Long enough to see habits, short enough to forget a replaced machine
/// (`USAGE_WINDOW_MS`, default-model.ts L14).
pub const USAGE_WINDOW_MS: i64 = 90 * 24 * 60 * 60 * 1000;

/// Where a default-model answer came from (`DefaultModelChoice.source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultSource {
    /// The explicit `defaultProvider`/`defaultModel` pair.
    Settings,
    /// A role assignment (`roles.<role>`).
    Role,
    /// The most-used usable model in the usage window.
    Usage,
}

impl DefaultSource {
    /// The wire name (`config.report`'s `default.source`).
    pub fn as_str(self) -> &'static str {
        match self {
            DefaultSource::Settings => "settings",
            DefaultSource::Role => "role",
            DefaultSource::Usage => "usage",
        }
    }
}

/// One default-model answer (`DefaultModelChoice`, default-model.ts L16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultModelChoice {
    /// The provider that would serve the run.
    pub provider_id: String,
    /// The model that would serve the run.
    pub model_id: String,
    /// Which rule produced the answer.
    pub source: DefaultSource,
    /// Requests behind an automatic choice; `None` for a fixed pair.
    pub requests: Option<u64>,
}

/// `hasCredential` (settings.ts L555): a saved key or an options block — a
/// provider that signs its own requests (AWS) or reads a service account
/// (Vertex) is configured the moment its options exist.
pub fn has_credential(state: Option<&ProviderState>) -> bool {
    let Some(state) = state else {
        return false;
    };
    !state.keys.is_empty()
        || state
            .options
            .as_ref()
            .and_then(serde_json::Value::as_object)
            .is_some_and(|options| !options.is_empty())
}

/// `providerIsConfigured` (registry.ts L965): a saved credential, or a
/// catalog-declared environment variable that is already set.
pub fn provider_is_configured(
    provider_id: &str,
    state: Option<&ProviderState>,
    catalog: Option<&CatalogStore>,
) -> bool {
    if has_credential(state) {
        return true;
    }
    let Some(catalog) = catalog else {
        return false;
    };
    catalog
        .get_provider(provider_id)
        .and_then(|provider| provider.env)
        .is_some_and(|names| {
            names.iter().any(|name| {
                std::env::var(name)
                    .map(|value| !value.trim().is_empty())
                    .unwrap_or(false)
            })
        })
}

/// `modelIsUsable` (registry.ts L975): a named model whose provider is
/// enabled and configured, and that has a source of truth — the catalog, or
/// the operator's own endpoint/model declaration.
pub fn model_is_usable(
    settings: &SettingsStore,
    catalog: Option<&CatalogStore>,
    provider_id: &str,
    model_id: &str,
) -> bool {
    if provider_id.is_empty() || model_id.is_empty() {
        return false;
    }
    let Some(state) = settings.providers.get(provider_id) else {
        return false;
    };
    if !state.enabled || !provider_is_configured(provider_id, Some(state), catalog) {
        return false;
    }
    let custom = state.npm.is_some() || state.url.is_some();
    let loaded = catalog.is_some_and(CatalogStore::is_loaded);
    loaded || custom
}

/// The most-requested usable model; ties go to the most recently used
/// (`pickMostUsedModel`, default-model.ts L27). Stable on insertion order,
/// matching the source's `Map` + stable `sort`.
pub fn pick_most_used_model(
    settings: &SettingsStore,
    catalog: Option<&CatalogStore>,
    rows: &[UsagePick],
) -> Option<DefaultModelChoice> {
    struct Ranked {
        provider_id: String,
        model_id: String,
        requests: u64,
        last_ms: i64,
    }
    let mut ranked: Vec<Ranked> = Vec::new();
    for row in rows {
        if row.provider_id.is_empty() || row.model_id.is_empty() {
            continue;
        }
        match ranked
            .iter_mut()
            .find(|entry| entry.provider_id == row.provider_id && entry.model_id == row.model_id)
        {
            Some(entry) => {
                entry.requests += 1;
                entry.last_ms = entry.last_ms.max(row.at_ms);
            }
            None => ranked.push(Ranked {
                provider_id: row.provider_id.clone(),
                model_id: row.model_id.clone(),
                requests: 1,
                last_ms: row.at_ms,
            }),
        }
    }
    let mut usable: Vec<Ranked> = ranked
        .into_iter()
        .filter(|entry| model_is_usable(settings, catalog, &entry.provider_id, &entry.model_id))
        .collect();
    usable.sort_by(|left, right| {
        right
            .requests
            .cmp(&left.requests)
            .then(right.last_ms.cmp(&left.last_ms))
    });
    usable.into_iter().next().map(|best| DefaultModelChoice {
        provider_id: best.provider_id,
        model_id: best.model_id,
        source: DefaultSource::Usage,
        requests: Some(best.requests),
    })
}

/// The default as a run should see it: the explicit pair, else the
/// most-used usable model (`resolveDefaultModel`, default-model.ts L43).
pub fn resolve_default_model(
    settings: &SettingsStore,
    catalog: Option<&CatalogStore>,
    rows: &[UsagePick],
) -> Option<DefaultModelChoice> {
    if !settings.default_provider.is_empty() && !settings.default_model.is_empty() {
        return Some(DefaultModelChoice {
            provider_id: settings.default_provider.clone(),
            model_id: settings.default_model.clone(),
            source: DefaultSource::Settings,
            requests: None,
        });
    }
    pick_most_used_model(settings, catalog, rows)
}

/// The model a run on `role` starts with: the assignment when the owner
/// made one and it is still ready, otherwise the shared default chain
/// (`resolveRoleModel`, model-roles.ts L58). A role bound to a broken
/// target degrades to the default instead of failing the run.
///
/// The legacy single-purpose fields (`autoReviewProvider`/`Model`,
/// `custodian.provider`/`model`) need no separate branch here: `parse_store`
/// already folds them into `roles` at read time (schema/mod.rs).
pub fn resolve_role_model(
    settings: &SettingsStore,
    catalog: Option<&CatalogStore>,
    rows: &[UsagePick],
    role: &str,
) -> Option<DefaultModelChoice> {
    if let Some(binding) = settings.roles.as_ref().and_then(|roles| roles.get(role))
        && model_is_usable(settings, catalog, &binding.provider, &binding.model)
    {
        return Some(DefaultModelChoice {
            provider_id: binding.provider.clone(),
            model_id: binding.model.clone(),
            source: DefaultSource::Role,
            requests: None,
        });
    }
    resolve_default_model(settings, catalog, rows)
}
