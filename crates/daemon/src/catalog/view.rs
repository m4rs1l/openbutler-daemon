//! The derived view (owner design, 2026-11-24): the configured providers
//! and models joined with their catalog lines, so hot paths never read
//! the big catalog.
//!
//! - **Resolution is an override:** catalog facts, settings on top; an
//!   id the catalog does not carry (custom) falls back to its settings
//!   information alone.
//! - **The cache follows both revisions:** the view is rebuilt when the
//!   catalog's disk revision or the settings revision moves — two inputs,
//!   one key, so a stale derived copy cannot exist.
//! - **Ambient state stays out:** an env-var credential (`catalog.env`)
//!   is *not* folded in here. Env changes move no revision, and a cached
//!   answer that depends on them would go stale silently — readiness
//!   consumers read env themselves, uncached (documented).

use std::sync::Arc;

use serde::Serialize;

use super::store::{CatalogStore, ensured_provider, refresh_if_changed};
use super::{CatalogCost, CatalogModel, CatalogProvider, model_id_candidates};
use crate::settings::{ProviderModelSpec, ProviderState, SettingsStore};

/// The configured side of the catalog: every provider settings names,
/// in `providerOrder` order, with its registered models resolved.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogView {
    pub default_provider: String,
    pub default_model: String,
    pub providers: Vec<ResolvedProvider>,
}

/// One configured provider: catalog identity plus settings overlay.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedProvider {
    pub id: String,
    /// Catalog display name, else the id (never guessed across providers).
    pub name: String,
    pub enabled: bool,
    /// Settings url override, else the catalog's API endpoint.
    pub url: Option<String>,
    /// Settings package, else the catalog's.
    pub npm: Option<String>,
    /// Key environment variables the catalog declares (unchecked — see
    /// the module note on ambient state).
    pub env: Vec<String>,
    /// Whether settings carries at least one key for this provider.
    pub credentialed: bool,
    pub source: ProviderSource,
    pub models: Vec<ResolvedModel>,
}

/// Where a provider's facts come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderSource {
    /// The catalog carries this provider id.
    Catalog,
    /// Custom: the id exists only in settings.
    Settings,
}

/// One registered model: catalog capabilities plus settings specs.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedModel {
    pub id: String,
    /// Spec name, else the catalog name, else the untouched id.
    pub name: String,
    /// Spec context, else the catalog limit (the one authority rule).
    pub context_window: Option<u64>,
    /// Spec output, else the catalog limit.
    pub output_limit: Option<u64>,
    pub reasoning: Option<bool>,
    pub tool_call: Option<bool>,
    /// USD per one million tokens, from the catalog.
    pub cost: Option<CatalogCost>,
    pub source: ModelSource,
}

/// Where a model's facts come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ModelSource {
    /// The catalog knows the model (specs may still override fields).
    Catalog,
    /// The model exists only in settings (`modelSpecs`).
    Settings,
    /// Neither — e.g. a default model id the catalog does not carry yet.
    Unknown,
}

/// The cached copy: both revisions it was built under.
pub(crate) struct CachedView {
    pub catalog_revision: String,
    pub settings_revision: String,
    pub view: Arc<CatalogView>,
}

impl CatalogStore {
    /// The derived view under `settings`, rebuilt when either revision
    /// moves (`settings_revision` is the caller's `LoadedSettings`
    /// revision, or `""` for defaults that never touched disk).
    pub fn view(&self, settings: &SettingsStore, settings_revision: &str) -> Arc<CatalogView> {
        self.with_state(|inner| {
            refresh_if_changed(inner);
            if let Some(cached) = &inner.view_cache
                && cached.catalog_revision == inner.disk_revision
                && cached.settings_revision == settings_revision
            {
                return cached.view.clone();
            }
            let view = Arc::new(resolve(inner, settings));
            inner.view_cache = Some(CachedView {
                catalog_revision: inner.disk_revision.clone(),
                settings_revision: settings_revision.to_owned(),
                view: view.clone(),
            });
            view
        })
    }
}

/// Builds the view: every settings provider in order, joined with its
/// catalog line when the catalog carries it.
fn resolve(inner: &mut super::store::Inner, settings: &SettingsStore) -> CatalogView {
    let mut ids: Vec<String> = settings
        .provider_order
        .iter()
        .filter(|id| settings.providers.contains_key(*id))
        .cloned()
        .collect();
    for id in settings.providers.keys() {
        if !ids.contains(id) {
            // The order never named it: appended, never dropped (the
            // load-time rule appends the same way).
            ids.push(id.clone());
        }
    }

    let providers = ids
        .into_iter()
        .filter_map(|id| {
            let state = settings.providers.get(&id)?;
            let catalog = ensured_provider(inner, &id);
            Some(resolve_provider(&id, state, catalog.as_ref(), settings))
        })
        .collect();

    CatalogView {
        default_provider: settings.default_provider.clone(),
        default_model: settings.default_model.clone(),
        providers,
    }
}

fn resolve_provider(
    id: &str,
    state: &ProviderState,
    catalog: Option<&CatalogProvider>,
    settings: &SettingsStore,
) -> ResolvedProvider {
    let mut registered: Vec<String> = state.models.clone().unwrap_or_default();
    if let Some(specs) = &state.model_specs {
        for model_id in specs.keys() {
            if !registered.contains(model_id) {
                registered.push(model_id.clone());
            }
        }
    }
    if settings.default_provider == id
        && !settings.default_model.is_empty()
        && !registered.contains(&settings.default_model)
    {
        registered.push(settings.default_model.clone());
    }

    let models = registered
        .iter()
        .map(|model_id| {
            let spec = state
                .model_specs
                .as_ref()
                .and_then(|specs| specs.get(model_id));
            resolve_model(model_id, catalog, spec)
        })
        .collect();

    ResolvedProvider {
        id: id.to_owned(),
        name: catalog
            .and_then(|provider| provider.name.clone())
            .unwrap_or_else(|| id.to_owned()),
        enabled: state.enabled,
        url: state
            .url
            .clone()
            .or_else(|| catalog.and_then(|provider| provider.api.clone())),
        npm: state
            .npm
            .clone()
            .or_else(|| catalog.and_then(|provider| provider.npm.clone())),
        env: catalog
            .and_then(|provider| provider.env.clone())
            .unwrap_or_default(),
        credentialed: !state.keys.is_empty(),
        source: if catalog.is_some() {
            ProviderSource::Catalog
        } else {
            ProviderSource::Settings
        },
        models,
    }
}

fn resolve_model(
    model_id: &str,
    catalog: Option<&CatalogProvider>,
    spec: Option<&ProviderModelSpec>,
) -> ResolvedModel {
    let catalog_model: Option<&CatalogModel> = catalog.and_then(|provider| {
        model_id_candidates(model_id)
            .into_iter()
            .find_map(|candidate| provider.models.get(&candidate))
    });

    let source = match (catalog_model.is_some(), spec.is_some()) {
        (true, _) => ModelSource::Catalog,
        (false, true) => ModelSource::Settings,
        (false, false) => ModelSource::Unknown,
    };
    // Window and output share one authority (`resolveContextBudget`):
    // settings beat the catalog per field, and a field the spec omits
    // falls back to the catalog.
    let context_window = spec
        .and_then(|spec| spec.context)
        .or_else(|| catalog_model.and_then(|model| model.limit.as_ref()?.context));
    let output_limit = spec
        .and_then(|spec| spec.output)
        .or_else(|| catalog_model.and_then(|model| model.limit.as_ref()?.output));

    ResolvedModel {
        id: model_id.to_owned(),
        name: spec
            .and_then(|spec| spec.name.clone())
            .or_else(|| catalog_model.and_then(|model| model.name.clone()))
            .unwrap_or_else(|| model_id.to_owned()),
        context_window,
        output_limit,
        reasoning: catalog_model.and_then(|model| model.reasoning),
        tool_call: catalog_model.and_then(|model| model.tool_call),
        cost: catalog_model.and_then(|model| model.cost.clone()),
        source,
    }
}
