//! Publishing the catalog: one file, overwritten by every fetch.
//!
//! The file is staged beside itself and renamed into place, so a reader sees
//! either the previous fetch or this one — never a mixture — and the
//! directory never holds a second copy.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use serde_json::Value;

use super::{CATALOG_FILE, CATALOG_FORMAT_VERSION, CatalogEntry, CatalogIndex};

/// Writes the catalog as one file and returns the index a reader builds from
/// its header line. The returned value is the same one the next read
/// produces, so the caller can install it without re-reading.
pub fn write_catalog_snapshot(
    data_dir: &Path,
    payload: &Value,
    fetched_at_ms: i64,
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

    // Bodies first: their offsets are relative to the first line after the
    // header, so the header can be written once, without a second pass.
    let mut body: Vec<u8> = Vec::new();
    let mut entries: BTreeMap<String, CatalogEntry> = BTreeMap::new();
    for (id, raw) in source {
        let Some(provider) = super::store::normalize_provider(id, raw) else {
            continue;
        };
        let mut line = serde_json::to_string(&provider)
            .map_err(|error| format!("cannot serialize provider {id}: {error}"))?;
        line.push('\n');
        let bytes = line.as_bytes();
        let models = provider.get("models").and_then(Value::as_object);
        let model_ids: Vec<&String> = models.map(|map| map.keys().collect()).unwrap_or_default();
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
        let offset = body.len() as u64;
        body.extend_from_slice(bytes);
        entries.insert(
            id.to_owned(),
            CatalogEntry {
                offset,
                length: (bytes.len() - 1) as u64,
                model_count: model_ids.len() as u64,
                tool_capable_count,
            },
        );
    }

    let fetched_at = iso_from_millis(fetched_at_ms);
    let header = serde_json::json!({
        "version": CATALOG_FORMAT_VERSION,
        "fetchedAt": fetched_at,
        "providers": entries,
    });
    let mut encoded = serde_json::to_string(&header)
        .map_err(|error| format!("cannot serialize the catalog header: {error}"))?
        .into_bytes();
    encoded.push(b'\n');

    let data_path = data_dir.join(CATALOG_FILE);
    let staging = data_dir.join(format!("{CATALOG_FILE}.{}.tmp", uuid::Uuid::new_v4()));
    let written = (|| -> std::io::Result<()> {
        let mut handle = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)?;
        handle.write_all(&encoded)?;
        handle.write_all(&body)?;
        handle.sync_all()?;
        drop(handle);
        fs::rename(&staging, &data_path)
    })();
    let _ = fs::remove_file(&staging);
    written.map_err(|error| format!("cannot publish the catalog: {error}"))?;

    sweep_leftovers(data_dir);
    Ok(CatalogIndex {
        fetched_at,
        body_start: encoded.len() as u64,
        entries,
    })
}

/// Removes what earlier layouts left behind: the separate index document and
/// the generation-named data files. The catalog owns the `catalog.*` names in
/// this directory, and nothing else is touched.
fn sweep_leftovers(data_dir: &Path) {
    let Ok(entries) = fs::read_dir(data_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == CATALOG_FILE {
            continue;
        }
        let generation = name.starts_with("catalog.") && name.ends_with(".ndjson");
        let staging = name.starts_with(&format!("{CATALOG_FILE}.")) && name.ends_with(".tmp");
        if name == "catalog-index.json" || generation || staging {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// `Date.parse`-compatible stamp: `2026-11-24T00:13:32.000Z`.
fn iso_from_millis(millis: i64) -> String {
    chrono::DateTime::from_timestamp_millis(millis)
        .map(|moment| moment.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}
