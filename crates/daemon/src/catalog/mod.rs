//! The provider/model catalog, fetched from models.dev.
//!
//! OpenButler never lists a provider in code. The catalog is the list: per
//! provider it carries the display name, the API endpoint, the environment
//! variables that hold the key, the package that speaks it, and every
//! model's capabilities. A provider this build has never heard of works
//! the moment it is in the catalog and its key is set.
//!
//! ## How it is stored
//!
//! One NDJSON file, one line per provider, plus a small index that maps a
//! provider id to its byte range. Nothing is thrown away: the file holds
//! exactly what models.dev ships, so a field that gains a reader later is
//! already there. What is held in memory is a *decision*, not a copy: a
//! run reads the index once per disk revision and then only the provider
//! lines it is asked about. Measured 2026-11-24: the payload is 6.59 MB
//! (326 providers, 10 185 models) and never lands parsed in memory —
//! `JSON.parse` cannot do this, because a JSON document has no
//! addressable interior; NDJSON does, which is the entire reason for the
//! format.
//!
//! ## Source and divergences (ported from `provider/catalog.ts`, read
//! ## 2026-11-24)
//!
//! - The fetch ceiling is **64 MiB**, not the source's 20 MiB: the guard
//!   is a runaway cap, not a size limit the real payload sits against.
//! - The network call is an injected [`Transport`] (no dependency yet);
//!   the real HTTP transport lands with its own slice. Timeout 8 s lives
//!   with it.
//! - The index is read into typed entries: a malformed entry rejects the
//!   whole index (the source keeps listing and guards each provider read).
//! - The disk revision is portable: `ino` on unix, `len:mtime:ctime` on
//!   Windows (`std` exposes no file index there).
//! - `reasoning_options` stays raw inside [`CatalogModel::rest`] until
//!   its reader (the effort ladder) lands — a union parse must not be
//!   able to reject a whole provider line over a new option type.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

mod fetch;
mod http;
mod store;
mod view;
mod write;

pub use fetch::IndexOptions;
pub use http::HttpTransport;
pub use store::{CatalogStore, ProviderSummary};
pub use view::{CatalogView, ModelSource, ProviderSource, ResolvedModel, ResolvedProvider};
pub use write::write_catalog_snapshot;

/// Where the catalog comes from (source: `CATALOG_URL`).
pub const CATALOG_URL: &str = "https://models.dev/api.json";

/// On-disk names (the index lives at the data-dir root; data files are
/// generation-named beside it).
pub const CATALOG_FILENAMES: (&str, &str) = ("catalog.ndjson", "catalog-index.json");

/// How long a cached catalog is served without asking the network again
/// (source: 12 h — long enough that repeated runs cost nothing, short
/// enough that a model released this morning is usable today).
pub const DEFAULT_CATALOG_TTL_MS: u64 = 12 * 60 * 60 * 1000;

/// Runaway guard on the response body. The real payload measured 6.59 MB;
/// this ceiling exists so a broken endpoint cannot exhaust the disk, and
/// must stay comfortably above a full download.
pub const MAX_CATALOG_BYTES: u64 = 64 * 1024 * 1024;

/// A provider line is a provider, not a payload: anything bigger is a
/// bad line (source: 8 MiB).
pub const MAX_PROVIDER_LINE_BYTES: u64 = 8 * 1024 * 1024;

/// Backoff after a failed fetch when there is nothing cached to serve
/// (source: 60 s, so a broken network does not spin).
pub const RETRY_DELAY_MS: i64 = 60 * 1000;

/// The real transport's request timeout (enforced where the HTTP client
/// lives, with the transport slice).
pub const FETCH_TIMEOUT_MS: u64 = 8_000;

/// One catalog fetch attempt: the raw response body, or the failure
/// reason that ends up inside the wrapped error sentence.
pub type FetchFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + Send>>;

/// The network edge. Production wires the real HTTP client here; tests
/// script the answer and count calls (a test that reaches the internet
/// is a test that fails on someone else's box).
pub trait Transport: Send + Sync + 'static {
    /// Fetches the catalog payload for `url`.
    fn fetch(&self, url: &'static str) -> FetchFuture;
}

/// Per-provider facts held in the index, so a listing needs no provider
/// read (source: `CatalogModel` at read time injects the model id from
/// its key — done by the writer, exactly like `normalizeProvider`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogModel {
    /// Injected from the models map key by the writer.
    #[serde(default)]
    pub id: String,
    pub name: Option<String>,
    pub reasoning: Option<bool>,
    pub tool_call: Option<bool>,
    pub structured_output: Option<bool>,
    pub attachment: Option<bool>,
    pub modalities: Option<CatalogModalities>,
    pub limit: Option<CatalogLimit>,
    /// US dollars per one million tokens.
    pub cost: Option<CatalogCost>,
    pub release_date: Option<String>,
    pub last_updated: Option<String>,
    /// Every field not typed here — "a field that gains a reader later
    /// is already there" (`reasoning_options` lives here today).
    #[serde(flatten, default)]
    pub rest: Map<String, Value>,
}

/// Input/output modalities, as the API declares them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogModalities {
    pub input: Option<Vec<String>>,
    pub output: Option<Vec<String>>,
}

/// Token limits in absolute tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogLimit {
    pub context: Option<u64>,
    pub output: Option<u64>,
}

/// USD price per one million tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogCost {
    pub input: Option<f64>,
    pub output: Option<f64>,
    #[serde(default)]
    pub reasoning: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

/// One provider line: exactly what models.dev ships, plus the id the
/// writer records. Unknown fields survive in `rest`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogProvider {
    pub id: String,
    pub name: Option<String>,
    /// Package that speaks this provider. Always from data, never code.
    pub npm: Option<String>,
    /// API endpoint.
    pub api: Option<String>,
    pub doc: Option<String>,
    /// Environment variables that may hold the key, in preference order.
    pub env: Option<Vec<String>>,
    #[serde(default)]
    pub models: BTreeMap<String, CatalogModel>,
    #[serde(flatten, default)]
    pub rest: Map<String, Value>,
}

/// The index's per-provider record: the byte range plus what a listing
/// needs without reading the line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    /// Byte offset of this provider's line.
    pub offset: u64,
    /// Byte length of the line, newline excluded.
    pub length: u64,
    #[serde(default)]
    pub sha256: Option<String>,
    pub model_count: u64,
    pub tool_capable_count: u64,
}

/// The committed index: which generation file to read and where each
/// provider lives inside it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogIndex {
    pub fetched_at: String,
    /// An immutable NDJSON generation. Absent only for legacy cache files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_file: Option<String>,
    pub entries: BTreeMap<String, CatalogEntry>,
    /// Provider ids serving each model id, so custom providers can match
    /// cataloged models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_providers: Option<BTreeMap<String, Vec<String>>>,
}

/// Where the index lives for one data directory.
pub(crate) fn index_path(data_dir: &Path) -> PathBuf {
    data_dir.join(CATALOG_FILENAMES.1)
}

/// Where one generation (or the legacy single file) lives.
pub(crate) fn data_path(data_dir: &Path, index: &CatalogIndex) -> PathBuf {
    match &index.data_file {
        Some(file) => data_dir.join(file),
        None => data_dir.join(CATALOG_FILENAMES.0),
    }
}

/// The exact generation filename shape: `catalog.<36 chars of hex and
/// dashes>.ndjson` (the source's `^catalog\.[0-9a-f-]{36}\.ndjson$`).
pub(crate) fn is_generation_name(name: &str) -> bool {
    let Some(middle) = name
        .strip_prefix("catalog.")
        .and_then(|rest| rest.strip_suffix(".ndjson"))
    else {
        return false;
    };
    middle.len() == 36
        && middle
            .chars()
            .all(|char| char.is_ascii_hexdigit() || char == '-')
}

/// The one ceiling sentence: the flow and the transport both refuse with
/// this exact wording (source: `the provider catalog exceeded the {n} MiB limit`).
pub(crate) fn exceeded_limit_sentence() -> String {
    format!(
        "the provider catalog exceeded the {} MiB limit",
        MAX_CATALOG_BYTES / (1024 * 1024)
    )
}

/// The spellings one gateway-style model id may be cataloged under, in
/// preference order (source: exact first, so a real catalog key always
/// beats a normalized one). Model-identity normalization lives here,
/// once, so every catalog lookup agrees on what counts as the same model.
pub fn model_id_candidates(model_id: &str) -> Vec<String> {
    fn strip_vendor(id: &str) -> &str {
        match id.find('/') {
            Some(slash) if slash > 0 && slash < id.len() - 1 => &id[slash + 1..],
            _ => id,
        }
    }
    let at = model_id.rfind('@');
    let without_variant = match at {
        Some(position) if position > 0 => &model_id[..position],
        _ => model_id,
    };
    let mut candidates = vec![
        model_id.to_owned(),
        without_variant.to_owned(),
        strip_vendor(model_id).to_owned(),
        strip_vendor(without_variant).to_owned(),
    ];
    // `new Set(...)` in the source: global dedup, first spelling wins —
    // `Vec::dedup` only collapses adjacent repeats, and the duplicate
    // spellings here are never adjacent.
    let mut seen = std::collections::HashSet::new();
    candidates.retain(|candidate| seen.insert(candidate.clone()));
    candidates
}
