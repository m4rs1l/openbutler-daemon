//! The refresh flow: fetch-on-stale with a shared attempt, a stale cache
//! beating no catalog, and a 60 s backoff after a failure (source:
//! `getCatalogIndex`). The transport is injected; the HTTP client lands
//! with its own slice.

use super::store::CatalogStore;
use super::write::write_catalog_snapshot;
use super::{CATALOG_URL, DEFAULT_CATALOG_TTL_MS, MAX_CATALOG_BYTES, RETRY_DELAY_MS};

/// One `get_index` decision: `refresh` forces the network, `ttl_ms` is
/// how old a cache may be, `now_ms` is the clock (injectable so tests
/// exercise fresh and unreachable paths without depending on the machine
/// they run on).
#[derive(Debug, Clone)]
pub struct IndexOptions {
    pub refresh: bool,
    pub ttl_ms: u64,
    pub now_ms: i64,
}

impl IndexOptions {
    /// The production shape at one instant: no refresh, the default TTL.
    pub fn at(now_ms: i64) -> Self {
        Self {
            refresh: false,
            ttl_ms: DEFAULT_CATALOG_TTL_MS,
            now_ms,
        }
    }
}

/// The wrapped failure with nothing to fall back on: a bare "fetch
/// failed" does not say which URL, and a first run on a locked-down
/// network must be able to tell that apart from a bad install (source:
/// the wrapping comment itself).
const UNAVAILABLE: &str = "the provider catalog is unavailable and nothing is cached yet";

impl CatalogStore {
    /// Ensures the catalog index is in hand, fetching only when the cache
    /// is stale. A stale cache beats no catalog: when the network is
    /// unreachable this returns what it has rather than failing.
    pub async fn get_index(&self, options: &IndexOptions) -> Result<super::CatalogIndex, String> {
        if let Some(answer) = self.fast_path(options) {
            return answer;
        }
        // One attempt at a time: waiters re-check after the gate instead
        // of stacking a second request on a broken network.
        let _gate = self.fetch_gate.lock().await;
        if let Some(answer) = self.fast_path(options) {
            return answer;
        }
        self.fetch_and_install(options).await
    }

    /// Fresh cache, or the backoff window already answering.
    fn fast_path(&self, options: &IndexOptions) -> Option<Result<super::CatalogIndex, String>> {
        if let Some(cached) = self.cached_index()
            && !options.refresh
            && is_fresh(&cached, options)
        {
            return Some(Ok(cached));
        }
        self.backoff_answer(options)
    }

    /// Inside the 60 s window: serve the cache if there is one, refuse
    /// if there is not — and never touch the transport.
    fn backoff_answer(
        &self,
        options: &IndexOptions,
    ) -> Option<Result<super::CatalogIndex, String>> {
        self.with_state(|inner| {
            if options.now_ms < inner.retry_after_ms {
                Some(match inner.index.clone() {
                    Some(cached) => Ok(cached),
                    None => Err(UNAVAILABLE.to_owned()),
                })
            } else {
                None
            }
        })
    }

    async fn fetch_and_install(
        &self,
        options: &IndexOptions,
    ) -> Result<super::CatalogIndex, String> {
        let outcome = match self.transport.fetch(CATALOG_URL).await {
            Ok(body) => self.ingest(&body, options),
            Err(reason) => Err(reason),
        };
        match outcome {
            Ok(index) => {
                self.with_state(|inner| {
                    inner.index = Some(index.clone());
                    inner.loaded.clear();
                    inner.retry_after_ms = 0;
                    inner.disk_revision =
                        super::store::file_signature(&super::index_path(&inner.data_dir));
                });
                Ok(index)
            }
            Err(reason) => {
                self.with_state(|inner| {
                    inner.retry_after_ms = options.now_ms + RETRY_DELAY_MS;
                });
                match self.cached_index() {
                    Some(cached) => Ok(cached),
                    None => Err(format!("{UNAVAILABLE} ({CATALOG_URL}: {reason})")),
                }
            }
        }
    }

    /// The bounded read: ceiling first (the source enforces it while
    /// streaming because a chunked response carries no length), then the
    /// payload gate, then the writer.
    fn ingest(&self, body: &[u8], options: &IndexOptions) -> Result<super::CatalogIndex, String> {
        if body.len() as u64 > MAX_CATALOG_BYTES {
            let limit = MAX_CATALOG_BYTES / (1024 * 1024);
            return Err(format!(
                "the provider catalog exceeded the {limit} MiB limit"
            ));
        }
        let payload: serde_json::Value = serde_json::from_slice(body)
            .map_err(|_| "models.dev returned an invalid JSON payload".to_owned())?;
        let data_dir = self.with_state(|inner| inner.data_dir.clone());
        write_catalog_snapshot(&data_dir, &payload, options.now_ms, options.now_ms)
    }
}

/// A cached index serves while it is younger than the TTL; an
/// unparseable `fetchedAt` is never fresh (source: `isFresh`).
fn is_fresh(index: &super::CatalogIndex, options: &IndexOptions) -> bool {
    chrono::DateTime::parse_from_rfc3339(&index.fetched_at)
        .ok()
        .map(|fetched| options.now_ms - fetched.timestamp_millis() < options.ttl_ms as i64)
        .unwrap_or(false)
}
