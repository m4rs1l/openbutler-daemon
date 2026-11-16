//! The browser surface: bundle serving, pairing, and the API gate.
//!
//! This layer is a backend-owned projection for a browser: it serves the
//! prebuilt client bundle, exchanges a one-shot link for a persistent
//! pairing cookie, and refuses unpaired or cross-origin API calls. Session
//! data and every decision the UI must not make stay with the host — the
//! routes that deliver them arrive with the runtime slice.

pub mod assets;
pub mod auth;
pub mod server;

pub use assets::{AssetsSource, read_build_id, require_default_assets_dir};
pub use server::{DEFAULT_WEB_BIND, WebOptions, WebServer, start_web_server};
