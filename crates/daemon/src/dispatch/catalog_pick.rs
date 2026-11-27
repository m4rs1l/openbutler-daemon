//! The setup pickers: the host's catalog, in the shape a picker needs.
//!
//! Read-only and host-wide. The catalog is one per machine and nothing here
//! is a secret, so both actions answer before an attachment and on a view-only
//! connection; the pickers belong to the setup wizard, which runs before any
//! workspace is bound.

use openbutler_protocol::DaemonRequest;
use openbutler_protocol::payload::require_string;
use serde_json::{Value, json};

use super::Host;

/// Every provider the catalog carries: `id`, display `name` and the model
/// count the picker shows. `loaded`/`loading` tell the wizard whether an
/// empty list means "none" or "not yet" — a download in flight is started
/// on the first ask.
pub fn list_providers(host: &Host) -> Result<Value, String> {
    let Some(catalog) = host.catalog() else {
        return Ok(unavailable(false));
    };
    if !catalog.is_loaded() {
        return Ok(unavailable(host.warm_catalog()));
    }
    let providers: Vec<Value> = catalog
        .list_provider_rows()
        .into_iter()
        .map(|row| {
            json!({
                "id": row.id,
                "name": row.name,
                "models": row.models,
                "npm": row.npm,
            })
        })
        .collect();
    Ok(json!({ "loaded": true, "loading": false, "providers": providers }))
}

fn unavailable(loading: bool) -> Value {
    json!({ "loaded": false, "loading": loading, "providers": [] })
}

/// One provider's models: `id`, display `name`, and the declared context and
/// output limits where the catalog states them.
pub fn list_models(host: &Host, request: &DaemonRequest) -> Result<Value, String> {
    let provider_id = require_string(request.payload.get("provider"), "provider", 128)
        .map_err(|error| error.to_string())?;
    let Some(catalog) = host.catalog().filter(|catalog| catalog.is_loaded()) else {
        return Err("the catalog is not available on this host yet.".to_owned());
    };
    let Some(provider) = catalog.get_provider(&provider_id) else {
        return Err(format!(
            "the catalog does not carry a provider called {provider_id}."
        ));
    };
    let models: Vec<Value> = provider
        .models
        .values()
        .map(|model| {
            json!({
                "id": model.id,
                "name": model.name.clone().unwrap_or_else(|| model.id.clone()),
                "context": model.limit.as_ref().and_then(|limit| limit.context),
                "output": model.limit.as_ref().and_then(|limit| limit.output),
            })
        })
        .collect();
    Ok(json!({ "provider": provider_id, "models": models }))
}
