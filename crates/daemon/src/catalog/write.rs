//! The snapshot writer: immutable, generation-named NDJSON published
//! before the index pointer moves, so a crash or a concurrent refresh
//! cannot create a torn pair (source: `writeCatalogSnapshot`).
//!
//! Exported because the on-disk format is a contract: a test that
//! hand-builds an index would be testing its own copy of the layout
//! rather than this writer.

use std::fs;
use std::io::Write;
use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::store::normalize_provider;
use super::{CatalogEntry, CatalogIndex, is_generation_name};

/// Publishes `payload` (either `{providers: {...}}` or a bare provider
/// map) as a new generation plus its index. `fetched_at_ms` stamps the
/// snapshot; `now_ms` is the pruning clock (the fetch flow passes its own
/// injected time).
pub fn write_catalog_snapshot(
    data_dir: &Path,
    payload: &Value,
    fetched_at_ms: i64,
    now_ms: i64,
) -> Result<CatalogIndex, String> {
    let Value::Object(payload) = payload else {
        return Err("models.dev returned an invalid provider catalog".to_owned());
    };
    let source = match payload.get("providers") {
        Some(Value::Object(providers)) => providers,
        _ => payload,
    };

    fs::create_dir_all(data_dir)
        .map_err(|error| format!("cannot create the catalog directory: {error}"))?;

    let data_file = format!("catalog.{}.ndjson", Uuid::new_v4());
    let data_path = data_dir.join(&data_file);
    let index_path = super::index_path(data_dir);
    let index_temp = data_dir.join(format!("{}.{}.tmp", CATALOG_INDEX_NAME, Uuid::new_v4()));

    let mut handle = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&data_path)
        .map_err(|error| format!("cannot create the catalog snapshot: {error}"))?;

    let mut offset: u64 = 0;
    let mut entries = std::collections::BTreeMap::new();
    let mut model_providers: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();

    for (id, raw) in source {
        let Some(provider) = normalize_provider(id, raw) else {
            continue;
        };
        let mut line = serde_json::to_string(&provider)
            .map_err(|error| format!("cannot serialize provider {id}: {error}"))?;
        line.push('\n');
        let bytes = line.as_bytes();
        handle
            .write_all(bytes)
            .map_err(|error| format!("cannot write the catalog snapshot: {error}"))?;

        let models = provider.get("models").and_then(Value::as_object);
        let model_ids: Vec<&String> = models.map(|map| map.keys().collect()).unwrap_or_default();
        for model_id in &model_ids {
            model_providers
                .entry(model_id.to_string())
                .or_default()
                .push(id.to_owned());
        }
        let tool_capable_count = model_ids
            .iter()
            .filter(|model_id| {
                models
                    .and_then(|map| map.get(model_id.as_str()))
                    .and_then(|model| model.get("tool_call"))
                    .and_then(Value::as_bool)
                    == Some(true)
            })
            .count() as u64;
        let line_without_newline = &bytes[..bytes.len() - 1];
        entries.insert(
            id.to_owned(),
            CatalogEntry {
                offset,
                length: (bytes.len() - 1) as u64,
                sha256: Some(format!("{:x}", Sha256::digest(line_without_newline))),
                model_count: model_ids.len() as u64,
                tool_capable_count,
            },
        );
        offset += bytes.len() as u64;
    }

    handle
        .sync_all()
        .map_err(|error| format!("cannot flush the catalog snapshot: {error}"))?;
    drop(handle);

    let index = CatalogIndex {
        fetched_at: iso_from_millis(fetched_at_ms),
        data_file: Some(data_file.clone()),
        entries,
        model_providers: Some(model_providers),
    };
    let encoded = serde_json::to_string(&index)
        .map_err(|error| format!("cannot serialize the catalog index: {error}"))?;
    let published = (|| -> std::io::Result<()> {
        let mut temp = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&index_temp)?;
        temp.write_all(encoded.as_bytes())?;
        temp.sync_all()?;
        drop(temp);
        fs::rename(&index_temp, &index_path)
    })();
    let _ = fs::remove_file(&index_temp);
    published.map_err(|error| format!("cannot publish the catalog index: {error}"))?;

    prune_old_generations(data_dir, &data_file, now_ms);
    Ok(index)
}

/// `Date.parse`-compatible stamp: `2026-11-24T00:13:32.000Z`.
fn iso_from_millis(millis: i64) -> String {
    chrono::DateTime::from_timestamp_millis(millis)
        .map(|moment| moment.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

const CATALOG_INDEX_NAME: &str = "catalog-index.json";

/// Keeps a grace period for other running processes still using an older
/// generation; anything older than 30 days relative to `now_ms` goes
/// (source: cleanup never invalidates a committed generation — errors are
/// swallowed and the current file is skipped).
pub fn prune_old_generations(data_dir: &Path, current: &str, now_ms: i64) {
    let Ok(entries) = fs::read_dir(data_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == current || !is_generation_name(&name) {
            continue;
        }
        let age_ms = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| now_ms - since.as_millis() as i64);
        let Some(age_ms) = age_ms else {
            continue;
        };
        if age_ms > 30 * 24 * 60 * 60_000 {
            let _ = fs::remove_file(entry.path());
        }
    }
}
