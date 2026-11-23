//! The HTTP transport against a loopback server: real requests, real
//! responses, zero internet (the catalog's own rule — a test that
//! reaches the network fails on someone else's box).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use openbutler_daemon::catalog::{HttpTransport, MAX_CATALOG_BYTES, Transport};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Serves exactly one HTTP response built by `respond` and returns the
/// URL to fetch (leaked to satisfy the transport's static-URL contract,
/// exactly what the production call site passes).
async fn serve_once(respond: impl FnOnce() -> Vec<u8> + Send + 'static) -> &'static str {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        // Read the request head; the body is empty for a GET.
        let mut head = Vec::new();
        let mut buffer = [0u8; 1024];
        loop {
            let read = stream.read(&mut buffer).await.unwrap();
            if read == 0 {
                break;
            }
            head.extend_from_slice(&buffer[..read]);
            if head.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let response = respond();
        stream.write_all(&response).await.unwrap();
        stream.shutdown().await.unwrap();
    });
    Box::leak(format!("http://{address}/api.json").into_boxed_str())
}

#[tokio::test]
async fn a_200_response_returns_the_body() {
    let url = serve_once(|| {
        let body = br#"{"providers":{}}"#;
        let mut response =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        response.extend_from_slice(body);
        response
    })
    .await;
    let body = HttpTransport
        .fetch(url)
        .await
        .expect("a 200 answers the body");
    assert_eq!(body, br#"{"providers":{}}"#);
}

#[tokio::test]
async fn a_non_success_status_names_the_code() {
    let url = serve_once(|| b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()).await;
    let error = HttpTransport.fetch(url).await.unwrap_err();
    assert_eq!(error, "models.dev returned HTTP 404", "the source sentence");
}

#[tokio::test]
async fn a_declared_length_over_the_ceiling_refuses_before_the_body() {
    // The header alone is past the cap; the server never sends a body.
    let url = serve_once(|| {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_CATALOG_BYTES + 1
        )
        .into_bytes()
    })
    .await;
    let error = HttpTransport.fetch(url).await.unwrap_err();
    assert_eq!(
        error, "the provider catalog exceeded the 64 MiB limit",
        "the declared length is refused up front"
    );
}

#[tokio::test]
async fn a_chunked_body_over_the_ceiling_is_cut_while_streaming() {
    // No content-length (chunked), so the ceiling must be enforced while
    // reading — the source's own reason for a streaming check.
    let url = serve_once(|| {
        let mut response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        let chunk = vec![b'x'; 1024 * 1024];
        let mut sent: u64 = 0;
        while sent <= MAX_CATALOG_BYTES {
            response.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            response.extend_from_slice(&chunk);
            response.extend_from_slice(b"\r\n");
            sent += chunk.len() as u64;
        }
        response.extend_from_slice(b"0\r\n\r\n");
        response
    })
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(60), HttpTransport.fetch(url))
        .await
        .expect("the cut must not hang");
    let error = outcome.unwrap_err();
    assert_eq!(error, "the provider catalog exceeded the 64 MiB limit");
}
