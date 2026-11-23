//! The real HTTP transport: one bounded GET (source: `readBoundedJson`).
//!
//! TLS is the platform's own (`native-tls`: schannel on Windows, openssl
//! elsewhere) — the rustls default in reqwest 0.13 pulls aws-lc-rs, whose
//! C build needs cmake/nasm, and a catalog fetch must not depend on a
//! native toolchain. The 8 s timeout is [`FETCH_TIMEOUT_MS`]; the ceiling
//! is enforced twice, exactly like the source: a declared
//! `content-length` over the limit refuses before any body arrives, and a
//! chunked response (no length) is cut while it streams.

use std::time::Duration;

use super::{FETCH_TIMEOUT_MS, FetchFuture, MAX_CATALOG_BYTES, Transport, exceeded_limit_sentence};

/// The production transport. Construct one per store; the client is built
/// per request because a refresh is a rare event and a stale client is
/// never shared across refreshes.
pub struct HttpTransport;

impl Transport for HttpTransport {
    fn fetch(&self, url: &'static str) -> FetchFuture {
        Box::pin(async move {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_millis(FETCH_TIMEOUT_MS))
                .build()
                .map_err(|error| format!("cannot build the HTTP client: {error}"))?;
            let mut response = client
                .get(url)
                .send()
                .await
                .map_err(|error| error.to_string())?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("models.dev returned HTTP {}", status.as_u16()));
            }
            if let Some(length) = response.content_length()
                && length > MAX_CATALOG_BYTES
            {
                return Err(exceeded_limit_sentence());
            }
            let mut body: Vec<u8> = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
                if body.len() as u64 + chunk.len() as u64 > MAX_CATALOG_BYTES {
                    return Err(exceeded_limit_sentence());
                }
                body.extend_from_slice(&chunk);
            }
            Ok(body)
        })
    }
}
