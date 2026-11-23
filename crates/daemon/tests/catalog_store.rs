//! The catalog store at its disk surface: the NDJSON snapshot contract,
//! the index validation, the byte-range reads, and the fetch flow with an
//! injected transport (no test reaches the network — `catalog.ts`'s own
//! rule: a test that reaches the internet fails on someone else's box).
//!
//! Sources ported: `provider/catalog.ts` (read 2026-11-24) and the owner's
//! agreed design (full download with a generous runaway guard, the big
//! payload never parsed whole).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::temp_data_dir;
use openbutler_daemon::catalog::{
    CATALOG_URL, CatalogStore, DEFAULT_CATALOG_TTL_MS, FetchFuture, IndexOptions,
    MAX_CATALOG_BYTES, Transport, model_id_candidates, write_catalog_snapshot,
};
use serde_json::{Value, json};

/// A canned transport: counts calls, answers with the scripted result.
struct FakeTransport {
    calls: Arc<AtomicUsize>,
    response: Arc<Mutex<Result<Vec<u8>, String>>>,
}

impl Transport for FakeTransport {
    fn fetch(&self, _url: &'static str) -> FetchFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let response = self.response.lock().unwrap().clone();
        Box::pin(std::future::ready(response))
    }
}

fn fake(response: Result<Vec<u8>, String>) -> (Arc<dyn Transport>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let transport = FakeTransport {
        calls: calls.clone(),
        response: Arc::new(Mutex::new(response)),
    };
    (Arc::new(transport), calls)
}

fn failing() -> (Arc<dyn Transport>, Arc<AtomicUsize>) {
    fake(Err("connection refused".to_owned()))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// One provider line with every shipped field plus an unknown key (the
/// source's "nothing is thrown away" contract).
fn payload() -> Value {
    json!({
        "providers": {
            "mock": {
                "name": "Mock Provider",
                "npm": "@ai-sdk/mock",
                "api": "https://api.mock.dev/v1",
                "doc": "https://mock.dev/docs",
                "env": ["MOCK_API_KEY", "MOCK_TOKEN"],
                "models": {
                    "mock-large": {
                        "name": "Mock Large",
                        "reasoning": true,
                        "reasoning_options": [{ "type": "effort", "values": ["low", "high"] }],
                        "tool_call": true,
                        "structured_output": true,
                        "attachment": true,
                        "modalities": { "input": ["text", "image"], "output": ["text"] },
                        "limit": { "context": 200000, "output": 64000 },
                        "cost": { "input": 2.5, "output": 10.0, "cache_read": 0.25 },
                        "release_date": "2026-01-15",
                        "last_updated": "2026-11-01",
                        "vendor_specific": { "tier": 1 }
                    },
                    "mock-small": { "tool_call": false }
                }
            }
        }
    })
}

fn payload_bytes() -> Vec<u8> {
    serde_json::to_vec(&payload()).unwrap()
}

fn index_json(dir: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(dir.join("catalog-index.json")).unwrap()).unwrap()
}

#[test]
fn snapshot_round_trip_keeps_every_shipped_field() {
    let dir = temp_data_dir("catalog-roundtrip");
    let index = write_catalog_snapshot(&dir, &payload(), 1_700_000_000_000, 1_700_000_000_000)
        .expect("the writer accepts a providers map");
    let entry = &index.entries["mock"];
    assert_eq!(entry.model_count, 2);
    assert_eq!(entry.tool_capable_count, 1);
    assert_eq!(entry.sha256.as_ref().unwrap().len(), 64, "digest pinned");
    let data_file = index.data_file.as_ref().unwrap();
    assert!(
        data_file.starts_with("catalog.") && data_file.ends_with(".ndjson"),
        "generation-named data file, got {data_file}"
    );

    let (transport, calls) = failing();
    let store = CatalogStore::with_transport(&dir, transport);
    assert!(store.is_loaded(), "the freshly written index is in hand");

    let provider = store.get_provider("mock").expect("byte-range read");
    assert_eq!(provider.name.as_deref(), Some("Mock Provider"));
    assert_eq!(provider.npm.as_deref(), Some("@ai-sdk/mock"));
    assert_eq!(provider.api.as_deref(), Some("https://api.mock.dev/v1"));
    assert_eq!(
        provider.env.as_deref(),
        Some(&["MOCK_API_KEY".to_owned(), "MOCK_TOKEN".to_owned()][..])
    );

    let model = &provider.models["mock-large"];
    assert_eq!(
        model.id, "mock-large",
        "the model id is injected from its key"
    );
    assert_eq!(model.name.as_deref(), Some("Mock Large"));
    assert_eq!(model.reasoning, Some(true));
    assert_eq!(model.tool_call, Some(true));
    assert_eq!(model.structured_output, Some(true));
    assert_eq!(model.attachment, Some(true));
    let modalities = model.modalities.as_ref().unwrap();
    assert_eq!(
        modalities.input.as_deref(),
        Some(&["text".to_owned(), "image".to_owned()][..])
    );
    let limit = model.limit.as_ref().unwrap();
    assert_eq!(limit.context, Some(200_000));
    assert_eq!(limit.output, Some(64_000));
    let cost = model.cost.as_ref().unwrap();
    assert_eq!(cost.input, Some(2.5));
    assert_eq!(cost.output, Some(10.0));
    assert_eq!(cost.cache_read, Some(0.25));
    assert_eq!(model.release_date.as_deref(), Some("2026-01-15"));
    assert_eq!(model.last_updated.as_deref(), Some("2026-11-01"));
    assert!(
        model.rest.get("vendor_specific").is_some(),
        "unknown fields survive a read (a field that gains a reader later is already there)"
    );
    assert_eq!(
        model
            .rest
            .get("reasoning_options")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(1),
        "reasoning_options stays raw until its reader lands"
    );

    let summaries = store.list_provider_summaries();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, "mock");
    assert_eq!(summaries[0].entry.model_count, 2);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "reads never fetch");
}

#[test]
fn a_torn_pair_answers_unknown_not_absent() {
    // (a) The digest no longer matches the committed line.
    let dir = temp_data_dir("catalog-torn-sha");
    write_catalog_snapshot(&dir, &payload(), 1_700_000_000_000, 1_700_000_000_000).unwrap();
    let mut index = index_json(&dir);
    index["entries"]["mock"]["sha256"] = json!("0".repeat(64));
    std::fs::write(
        dir.join("catalog-index.json"),
        serde_json::to_vec(&index).unwrap(),
    )
    .unwrap();
    let (transport, _) = failing();
    let store = CatalogStore::with_transport(&dir, transport);
    assert!(
        store.get_provider("mock").is_none(),
        "a digest mismatch is unknown, not a crash"
    );

    // (b) The line's id does not match its index slot (no digest to check).
    let dir = temp_data_dir("catalog-torn-id");
    std::fs::write(
        dir.join("catalog.11111111-1111-4111-8111-111111111111.ndjson"),
        "{\"id\":\"other\"}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("catalog-index.json"),
        serde_json::to_vec(&json!({
            "fetchedAt": "2026-11-24T00:00:00.000Z",
            "dataFile": "catalog.11111111-1111-4111-8111-111111111111.ndjson",
            "entries": { "ghost": { "offset": 0, "length": 13, "modelCount": 0, "toolCapableCount": 0 } },
        }))
        .unwrap(),
    )
    .unwrap();
    let (transport, _) = failing();
    let store = CatalogStore::with_transport(&dir, transport);
    assert!(
        store.get_provider("ghost").is_none(),
        "an id that does not match its slot is unknown until the next refresh"
    );
}

#[test]
fn an_unreadable_line_does_not_crash() {
    let dir = temp_data_dir("catalog-short-line");
    let index =
        write_catalog_snapshot(&dir, &payload(), 1_700_000_000_000, 1_700_000_000_000).unwrap();
    // The data file shrinks underneath the committed index.
    std::fs::write(dir.join(index.data_file.unwrap()), b"{\"id\"").unwrap();

    let (transport, _) = failing();
    let store = CatalogStore::with_transport(&dir, transport);
    assert!(
        store.get_provider("mock").is_none(),
        "short read answers unknown"
    );
    assert!(store.is_loaded(), "the store survives its own data file");
    assert_eq!(store.list_provider_summaries().len(), 1);
}

#[test]
fn listing_answers_from_the_index_alone() {
    let dir = temp_data_dir("catalog-list");
    let index =
        write_catalog_snapshot(&dir, &payload(), 1_700_000_000_000, 1_700_000_000_000).unwrap();
    // Corrupt every provider line: listing must not notice (it reads the index).
    std::fs::write(dir.join(index.data_file.unwrap()), b"not json at all").unwrap();

    let (transport, _) = failing();
    let store = CatalogStore::with_transport(&dir, transport);
    let summaries = store.list_provider_summaries();
    assert_eq!(summaries.len(), 1, "the index alone answers a listing");
    assert_eq!(summaries[0].entry.model_count, 2);
    assert!(store.get_provider("mock").is_none());
}

#[tokio::test]
async fn an_oversize_payload_is_rejected_and_arms_the_backoff() {
    assert_eq!(
        MAX_CATALOG_BYTES,
        64 * 1024 * 1024,
        "the guard sits above the real payload (6.6 MB measured 2026-11-24)"
    );
    let dir = temp_data_dir("catalog-oversize");
    let (transport, calls) = fake(Ok(vec![b' '; (MAX_CATALOG_BYTES as usize) + 1]));
    let store = CatalogStore::with_transport(&dir, transport);

    let error = store
        .get_index(&IndexOptions::at(1_700_000_000_000))
        .await
        .unwrap_err();
    assert!(
        error.contains("the provider catalog exceeded the 64 MiB limit"),
        "the limit sentence names the ceiling, got: {error}"
    );
    assert!(
        error.contains(CATALOG_URL),
        "the wrapped failure names the URL, got: {error}"
    );
    assert!(!store.is_loaded());
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // A failed attempt backs the next one off: no spin inside the window.
    let _ = store
        .get_index(&IndexOptions::at(1_700_000_000_000))
        .await
        .unwrap_err();
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the retry window holds");
}

#[tokio::test]
async fn a_fresh_cache_serves_without_calling_the_transport() {
    let dir = temp_data_dir("catalog-fresh");
    let at = 1_700_000_000_000;
    write_catalog_snapshot(&dir, &payload(), at, at).unwrap();

    let (transport, calls) = failing();
    let store = CatalogStore::with_transport(&dir, transport);
    let mut options = IndexOptions::at(at + 3_600_000);
    options.ttl_ms = DEFAULT_CATALOG_TTL_MS;
    let index = store.get_index(&options).await.expect("fresh cache serves");
    assert_eq!(index.entries["mock"].model_count, 2);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "fresh means offline");
}

#[tokio::test]
async fn stale_cache_beats_a_failed_fetch() {
    let dir = temp_data_dir("catalog-stale");
    let at = 1_700_000_000_000;
    write_catalog_snapshot(&dir, &payload(), at - 13 * 3_600_000, at).unwrap();

    let (transport, calls) = fake(Err("connection refused".to_owned()));
    let store = CatalogStore::with_transport(&dir, transport);
    let index = store
        .get_index(&IndexOptions::at(at))
        .await
        .expect("a stale cache beats no catalog");
    assert_eq!(index.entries["mock"].model_count, 2);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "stale triggers one attempt"
    );
    assert!(store.is_loaded());
}

#[tokio::test]
async fn nothing_cached_names_the_url_and_the_backoff_holds() {
    let dir = temp_data_dir("catalog-empty");
    let at = 1_700_000_000_000;
    let (transport, calls) = fake(Err("connection refused".to_owned()));
    let store = CatalogStore::with_transport(&dir, transport);

    let error = store.get_index(&IndexOptions::at(at)).await.unwrap_err();
    assert_eq!(
        error,
        format!(
            "the provider catalog is unavailable and nothing is cached yet ({CATALOG_URL}: connection refused)"
        ),
        "the wrapped error names the URL and the reason"
    );
    let _ = store.get_index(&IndexOptions::at(at)).await.unwrap_err();
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the 60 s backoff holds");

    let _ = store
        .get_index(&IndexOptions::at(at + 61_000))
        .await
        .unwrap_err();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "past the window it retries"
    );
}

#[tokio::test]
async fn a_successful_fetch_publishes_a_generation_and_prunes_old_ones() {
    let dir = temp_data_dir("catalog-publish");
    let old_generation = "catalog.aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.ndjson";
    std::fs::write(dir.join(old_generation), b"{}\n").unwrap();

    let (transport, calls) = fake(Ok(payload_bytes()));
    let store = CatalogStore::with_transport(&dir, transport);
    // The injected clock sits past the 30-day grace so the pre-created
    // generation (real mtime) is old enough to prune.
    let at = now_ms() + 31 * 24 * 3_600_000;
    let index = store.get_index(&IndexOptions::at(at)).await.expect("fetch");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(store.is_loaded());
    let current = index.data_file.unwrap();
    assert!(
        dir.join(&current).exists(),
        "the new generation is committed"
    );
    assert!(
        !dir.join(old_generation).exists(),
        "generations older than 30 days are pruned (grace for other processes)"
    );
    assert!(store.get_provider("mock").is_some());
}

#[test]
fn a_cross_process_replacement_invalidates_the_memo() {
    let dir = temp_data_dir("catalog-watch");
    let at = 1_700_000_000_000;
    write_catalog_snapshot(&dir, &payload(), at, at).unwrap();

    let (transport, _) = failing();
    let store = CatalogStore::with_transport(&dir, transport);
    let first = store.get_provider("mock").expect("first read");
    assert!(first.models.contains_key("mock-large"));

    // Another process publishes a bigger pair: the index bytes change, so
    // the disk revision moves and the memo with it.
    let mut bigger = payload();
    bigger["providers"]["mock"]["models"]["mock-small"]["name"] = json!("Mock Small Renamed");
    bigger["providers"]["second"] = json!({ "name": "Second", "models": {} });
    write_catalog_snapshot(&dir, &bigger, at + 1_000, at + 1_000).unwrap();

    let second = store
        .get_provider("mock")
        .expect("re-read after replacement");
    assert_eq!(
        second.models["mock-small"].name.as_deref(),
        Some("Mock Small Renamed"),
        "the memo follows the replacement"
    );
    assert!(store.get_provider("second").is_some());
    assert_eq!(store.list_provider_summaries().len(), 2);
}

#[test]
fn model_id_candidates_spellings_in_preference_order() {
    assert_eq!(
        model_id_candidates("gpt-4o@2024-11-20"),
        vec!["gpt-4o@2024-11-20".to_owned(), "gpt-4o".to_owned()],
        "literal first, then without the variant"
    );
    assert_eq!(
        model_id_candidates("vendor/model"),
        vec!["vendor/model".to_owned(), "model".to_owned()],
        "then without the vendor path"
    );
    assert_eq!(
        model_id_candidates("vendor/model@v2"),
        vec![
            "vendor/model@v2".to_owned(),
            "vendor/model".to_owned(),
            "model@v2".to_owned(),
            "model".to_owned(),
        ],
        "all four spellings, deduped, exact beats normalized"
    );
    assert_eq!(
        model_id_candidates("bare"),
        vec!["bare".to_owned()],
        "a bare id has one spelling"
    );
}

#[test]
fn get_model_and_price_follow_the_candidates() {
    let dir = temp_data_dir("catalog-model-lookup");
    write_catalog_snapshot(&dir, &payload(), 1_700_000_000_000, 1_700_000_000_000).unwrap();

    let (transport, _) = failing();
    let store = CatalogStore::with_transport(&dir, transport);
    let model = store
        .get_model("mock", "mock-large@2026-01-15")
        .expect("the variant spelling resolves to the cataloged model");
    assert_eq!(model.id, "mock-large");

    let price = store
        .get_token_price("mock", "mock-large@2026-01-15")
        .expect("price rides the same lookup");
    assert_eq!(price.input, Some(2.5));

    assert!(store.get_model("mock", "missing-model").is_none());
    assert!(
        store
            .get_token_price("missing-provider", "mock-large")
            .is_none()
    );
}
