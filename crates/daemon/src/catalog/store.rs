//! The store: revision-aware reads over the published catalog, one provider
//! line at a time, with a memo that follows cross-process replacements.
//!
//! The file's first line is the header (fetch time plus every provider's byte
//! range and counts); a run reads that line once per disk revision and then
//! only the provider lines it is asked about.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::Serialize;
use serde_json::Value;

use super::{
    CATALOG_FILE, CATALOG_FORMAT_VERSION, CatalogEntry, CatalogIndex, CatalogProvider,
    MAX_PROVIDER_LINE_BYTES, Transport, model_id_candidates,
};

/// A header is a few hundred bytes per provider; anything far past that is
/// not a header this build wrote.
const MAX_HEADER_BYTES: usize = 8 * 1024 * 1024;

/// One provider plus its index facts — the listing shape the source
/// returns (`{id, ...entry}`).
#[derive(Debug, Clone, Serialize)]
pub struct ProviderSummary {
    pub id: String,
    #[serde(flatten)]
    pub entry: CatalogEntry,
}

pub(crate) struct Inner {
    pub(crate) data_dir: PathBuf,
    pub(crate) index: Option<CatalogIndex>,
    pub(crate) disk_revision: String,
    /// Providers read during this run, so a second lookup costs nothing;
    /// misses are memoised too (an id the catalog does not carry stays
    /// unknown, not retried).
    pub(crate) loaded: HashMap<String, Option<CatalogProvider>>,
    pub(crate) retry_after_ms: i64,
    /// The derived view under the revisions it was built for (owner
    /// design: the hot-path copy follows catalog × settings revisions).
    pub(crate) view_cache: Option<super::view::CachedView>,
}

/// The catalog for one data directory: disk-only reads, one injected
/// transport for refreshes.
pub struct CatalogStore {
    pub(crate) inner: Mutex<Inner>,
    pub(crate) transport: std::sync::Arc<dyn Transport>,
    pub(crate) fetch_gate: tokio::sync::Mutex<()>,
}

impl CatalogStore {
    /// Opens the store over `data_dir`'s published catalog (read lazily on
    /// first access, exactly like the source's first `currentIndex()`).
    pub fn with_transport(data_dir: &Path, transport: std::sync::Arc<dyn Transport>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                data_dir: data_dir.to_owned(),
                index: None,
                disk_revision: String::new(),
                loaded: HashMap::new(),
                retry_after_ms: 0,
                view_cache: None,
            }),
            transport,
            fetch_gate: tokio::sync::Mutex::new(()),
        }
    }

    pub(crate) fn with_state<R>(&self, work: impl FnOnce(&mut Inner) -> R) -> R {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        work(&mut inner)
    }

    /// One provider, read on demand and then remembered for the rest of
    /// the run. Never hits the network: an id the header does not carry is
    /// "not known", not "no".
    pub fn get_provider(&self, provider_id: &str) -> Option<CatalogProvider> {
        self.with_state(|inner| ensured_provider(inner, provider_id))
    }

    /// Ids of every provider in the cached catalog, from the header alone.
    pub fn list_provider_ids(&self) -> Vec<String> {
        self.with_state(|inner| {
            refresh_if_changed(inner);
            inner
                .index
                .as_ref()
                .map(|index| index.entries.keys().cloned().collect())
                .unwrap_or_default()
        })
    }

    /// Per-provider facts held in the header, so listing needs no provider
    /// read (source: `listProviderSummaries`).
    pub fn list_provider_summaries(&self) -> Vec<ProviderSummary> {
        self.with_state(|inner| {
            refresh_if_changed(inner);
            let Some(index) = &inner.index else {
                return Vec::new();
            };
            index
                .entries
                .iter()
                .map(|(id, entry)| ProviderSummary {
                    id: id.clone(),
                    entry: entry.clone(),
                })
                .collect()
        })
    }

    /// One model of one provider, resolved through the candidate
    /// spellings (gateway aliases).
    pub fn get_model(&self, provider_id: &str, model_id: &str) -> Option<super::CatalogModel> {
        self.with_state(|inner| {
            let provider = ensured_provider(inner, provider_id)?;
            model_id_candidates(model_id)
                .into_iter()
                .find_map(|candidate| provider.models.get(&candidate).cloned())
        })
    }

    /// Token price for a model, or `None` when the catalog has no price.
    pub fn get_token_price(&self, provider_id: &str, model_id: &str) -> Option<super::CatalogCost> {
        self.get_model(provider_id, model_id)?.cost
    }

    /// Whether a published catalog is in hand. A count taken with no catalog
    /// is zero, and zero reads as "nothing usable" — a different and wrong
    /// claim from "we do not know yet" (source: `isCatalogLoaded`).
    pub fn is_loaded(&self) -> bool {
        self.with_state(|inner| {
            refresh_if_changed(inner);
            inner.index.is_some()
        })
    }

    /// Tracks disk replacements across processes, including cached misses
    /// (source: `getCatalogRevision`).
    pub fn revision(&self) -> String {
        self.with_state(|inner| {
            refresh_if_changed(inner);
            inner.disk_revision.clone()
        })
    }

    /// Total bytes the catalog occupies on disk.
    pub fn size_on_disk(&self) -> u64 {
        self.with_state(|inner| {
            refresh_if_changed(inner);
            if inner.index.is_none() {
                return 0;
            }
            fs::metadata(inner.data_dir.join(CATALOG_FILE))
                .map(|metadata| metadata.len())
                .unwrap_or(0)
        })
    }

    /// The index as it stands on disk right now (refreshing first); the
    /// fetch flow installs its own result without re-reading.
    pub(crate) fn cached_index(&self) -> Option<CatalogIndex> {
        self.with_state(|inner| {
            refresh_if_changed(inner);
            inner.index.clone()
        })
    }
}

/// The memo path: refresh, hit, read, remember (misses included).
pub(crate) fn ensured_provider(inner: &mut Inner, provider_id: &str) -> Option<CatalogProvider> {
    refresh_if_changed(inner);
    if let Some(hit) = inner.loaded.get(provider_id) {
        return hit.clone();
    }
    let found = provider_from_disk(inner, provider_id);
    inner.loaded.insert(provider_id.to_owned(), found.clone());
    found
}

/// A published line for `provider_id`, verified against its slot: the line
/// must parse as an object and carry the id the header filed it under.
fn provider_from_disk(inner: &Inner, provider_id: &str) -> Option<CatalogProvider> {
    let index = inner.index.as_ref()?;
    let entry = index.entries.get(provider_id)?;
    const JS_MAX_SAFE: u64 = 9_007_199_254_740_991;
    if entry.offset > JS_MAX_SAFE || entry.length == 0 || entry.length > MAX_PROVIDER_LINE_BYTES {
        return None;
    }
    let mut buffer = vec![0u8; entry.length as usize];
    let mut file = fs::File::open(inner.data_dir.join(CATALOG_FILE)).ok()?;
    file.seek(SeekFrom::Start(index.body_start + entry.offset))
        .ok()?;
    file.read_exact(&mut buffer).ok()?;
    let parsed: Value = serde_json::from_slice(&buffer).ok()?;
    if !parsed.is_object() {
        return None;
    }
    let provider: CatalogProvider = serde_json::from_value(parsed).ok()?;
    (provider.id == provider_id).then_some(provider)
}

/// Re-reads the header when another process replaced the file: the disk
/// signature moved, so the header and the memo go with it.
pub(crate) fn refresh_if_changed(inner: &mut Inner) {
    let revision = file_signature(&inner.data_dir.join(CATALOG_FILE));
    if revision == inner.disk_revision {
        return;
    }
    inner.index = read_header(&inner.data_dir);
    inner.loaded.clear();
    inner.disk_revision = revision;
}

/// `{ino}:{size}:{mtimeNs}:{ctimeNs}` on unix (the source's exact
/// signature); Windows has no stable file index in `std`, so the pair
/// there is `{size}:{mtimeNs}:{createdNs}` — an overwrite still moves it.
pub(crate) fn file_signature(path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match fs::metadata(path) {
            Ok(metadata) => format!(
                "{}:{}:{}:{}",
                metadata.ino(),
                metadata.len(),
                metadata.mtime_nsec(),
                metadata.ctime_nsec()
            ),
            Err(_) => String::new(),
        }
    }
    #[cfg(not(unix))]
    {
        let system_ns = |time: std::io::Result<std::time::SystemTime>| {
            time.ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|since| since.as_nanos().to_string())
                .unwrap_or_default()
        };
        match fs::metadata(path) {
            Ok(metadata) => format!(
                "{}:{}:{}",
                metadata.len(),
                system_ns(metadata.modified()),
                system_ns(metadata.created())
            ),
            Err(_) => String::new(),
        }
    }
}

/// Reads the header line and validates it against the file: a range that
/// does not fit means the catalog is not usable, which is "not yet" — never a
/// guess about its contents.
pub(crate) fn read_header(data_dir: &Path) -> Option<CatalogIndex> {
    let path = data_dir.join(CATALOG_FILE);
    let file = fs::File::open(&path).ok()?;
    let total = file.metadata().ok()?.len();
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).ok()?;
    if line.last() != Some(&b'\n') || line.len() > MAX_HEADER_BYTES {
        return None;
    }
    let body_start = line.len() as u64;
    let header: Value = serde_json::from_slice(&line[..line.len() - 1]).ok()?;
    if header.get("version").and_then(Value::as_u64) != Some(CATALOG_FORMAT_VERSION) {
        return None;
    }
    let entries: BTreeMap<String, CatalogEntry> =
        serde_json::from_value(header.get("providers")?.clone()).ok()?;
    for entry in entries.values() {
        let end = body_start
            .checked_add(entry.offset)?
            .checked_add(entry.length)?;
        if entry.length == 0 || end > total {
            return None;
        }
    }
    Some(CatalogIndex {
        fetched_at: header
            .get("fetchedAt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        body_start,
        entries,
    })
}

/// One raw provider entry: the API nests models in an object; the model
/// id is injected from its key (source: `normalizeProvider` — non-object
/// providers and models are skipped, never rejected).
pub(crate) fn normalize_provider(id: &str, raw: &Value) -> Option<Value> {
    let object = raw.as_object()?;
    let mut provider = Value::Object(object.clone());
    let mut models = serde_json::Map::new();
    if let Some(raw_models) = provider.get("models").and_then(Value::as_object) {
        for (model_id, model) in raw_models {
            let Some(model) = model.as_object() else {
                continue;
            };
            let mut model = model.clone();
            model.insert("id".to_owned(), Value::String(model_id.clone()));
            models.insert(model_id.clone(), Value::Object(model));
        }
    }
    provider["models"] = Value::Object(models);
    provider["id"] = Value::String(id.to_owned());
    Some(provider)
}
