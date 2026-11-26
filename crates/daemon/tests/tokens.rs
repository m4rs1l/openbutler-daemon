//! `token.create` / `token.list` / `token.revoke`: the auth token store over
//! IPC, and the local-only gate.
//!
//! Source: `auth-tokens.ts` (the store: salted hashes only, plaintext once)
//! and cli-main.ts L1750-1780 (the command surface), read 2026-11-26. The
//! store lived CLI-side there; here the host owns it (the backend-owned
//! rule), and minting credentials is local-only — the source had no gate,
//! so a remote client holding a token could mint more; this contract closes
//! that (a new sentence, documented in `api-surface.md`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use common::temp_data_dir;
use openbutler_daemon::auth::TokenStore;
use openbutler_daemon::dispatch::{self, DispatchState, Host};
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

fn dispatch_call(
    dir: &Path,
    state: &mut DispatchState,
    action: &str,
    payload: Value,
) -> DaemonResponse {
    let host = Host::new();
    let mut request = DaemonRequest::new("tokens-1", action);
    if let Value::Object(fields) = payload {
        for (key, value) in fields {
            request.payload.insert(key, value);
        }
    }
    dispatch::handle(&host, dir, state, &request)
}

fn success(dir: &Path, state: &mut DispatchState, action: &str, payload: Value) -> Value {
    match dispatch_call(dir, state, action, payload) {
        DaemonResponse::Success { result, .. } => result,
        DaemonResponse::Failure { error, .. } => panic!("expected success, got {error}"),
    }
}

fn refusal(dir: &Path, state: &mut DispatchState, action: &str, payload: Value) -> String {
    match dispatch_call(dir, state, action, payload) {
        DaemonResponse::Failure { error, .. } => error,
        DaemonResponse::Success { result, .. } => panic!("expected a refusal, got {result}"),
    }
}

#[test]
fn create_mints_a_working_token_and_list_never_shows_it() {
    let dir = temp_data_dir("tokens-create");
    let mut state = DispatchState::new();

    let created = success(&dir, &mut state, "token.create", json!({}));
    let id = created["id"].as_str().expect("id").to_owned();
    let token = created["token"].as_str().expect("token").to_owned();
    assert!(token.starts_with("obt_"), "{token}");
    assert_eq!(id.len(), 16, "the id is eight random bytes in hex");

    // The minted plaintext verifies against the on-disk store.
    let store = TokenStore::new(&dir);
    assert!(
        store.verify(&token).unwrap(),
        "the minted token must verify"
    );

    let listed = success(&dir, &mut state, "token.list", json!({}));
    let rows = listed.as_array().expect("token.list answers an array");
    assert_eq!(rows.len(), 1);
    let row = rows[0].as_object().expect("one summary object");
    let mut keys: Vec<&String> = row.keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["createdAt", "id"],
        "a summary carries the id and the mint time, nothing else"
    );
    assert_eq!(row["id"], json!(id));
    assert!(row["createdAt"].as_str().is_some_and(|s| s.ends_with('Z')));
}

#[test]
fn revoke_removes_the_token_and_says_so() {
    let dir = temp_data_dir("tokens-revoke");
    let mut state = DispatchState::new();
    let created = success(&dir, &mut state, "token.create", json!({}));
    let id = created["id"].as_str().unwrap().to_owned();
    let token = created["token"].as_str().unwrap().to_owned();

    let revoked = success(&dir, &mut state, "token.revoke", json!({ "tokenId": id }));
    assert_eq!(revoked, json!({ "revoked": true }));
    assert!(!TokenStore::new(&dir).verify(&token).unwrap());
    let listed = success(&dir, &mut state, "token.list", json!({}));
    assert_eq!(listed, json!([]));

    // A second revoke names the store's own remedy, the source's sentence.
    assert_eq!(
        refusal(&dir, &mut state, "token.revoke", json!({ "tokenId": id })),
        format!("No token with id {id}. List them with `openbutler daemon token list`.")
    );
    // The id is validated like every other string payload (the key is
    // `tokenId`: a payload key named `id` would collide with the envelope's
    // own `id` under the flattened encoding and make the frame unreadable).
    assert_eq!(
        refusal(&dir, &mut state, "token.revoke", json!({})),
        "tokenId must be a non-empty string under 64 bytes."
    );
}

#[test]
fn a_remote_connection_cannot_manage_credentials() {
    let dir = temp_data_dir("tokens-remote");
    let mut state = DispatchState::new();
    state.set_remote(true);

    // The source had no gate here — a remote client holding a token could
    // mint more, and a pairing code into a token. Both are local-only now.
    for (action, payload) in [
        ("token.create", json!({})),
        ("token.list", json!({})),
        ("token.revoke", json!({ "tokenId": "deadbeefdeadbeef" })),
    ] {
        assert_eq!(
            refusal(&dir, &mut state, action, payload),
            "Remote auth tokens are managed on the host machine only."
        );
    }
    assert_eq!(
        refusal(&dir, &mut state, "pair.create", json!({})),
        "Pairing codes are minted on the host machine only."
    );
}
