//! The browser bundle: locating it, identifying it, and serving it.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Where the served bundle comes from.
pub enum AssetsSource {
    /// Walk up to six levels from here for `dist/client/index.html` and
    /// refuse to start when the walk fails — a base install without the
    /// `web` overlay never serves an empty shell, and one message covers
    /// both audiences (installed remedy and source remedy).
    Default { start_from: PathBuf },
    /// Use this directory as it is (tests, development servers).
    Explicit(PathBuf),
}

const BUNDLE_MISSING: &str = "The browser client bundle (dist/client) is missing. Re-run the installer with --web, or build it with npm run build:web.";

/// The startup guard behind [`AssetsSource::Default`].
pub fn require_default_assets_dir(start_from: &Path) -> std::io::Result<PathBuf> {
    let mut directory = start_from.to_path_buf();
    for _ in 0..6 {
        let candidate = directory.join("dist").join("client");
        if candidate.join("index.html").is_file() {
            return Ok(candidate);
        }
        match directory.parent() {
            Some(parent) => directory = parent.to_path_buf(),
            None => break,
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        BUNDLE_MISSING,
    ))
}

/// Identifies the bundle being served: the first 12 hex characters of the
/// index digest. Asset names are content-hashed, so the index changes
/// exactly when a rebuild does; read on demand, never cached, because this
/// server outlives rebuilds and open tabs must notice them.
pub fn read_build_id(assets_dir: &Path) -> Option<String> {
    let bytes = std::fs::read(assets_dir.join("index.html")).ok()?;
    let digest = Sha256::digest(bytes);
    Some(
        digest
            .iter()
            .take(6)
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

/// A file resolved inside the bundle, with the headers it must carry.
pub(crate) struct StaticFile {
    pub body: Vec<u8>,
    pub content_type: &'static str,
    pub cache_control: &'static str,
}

/// Maps a request path onto the bundle, or `None` when it does not exist.
///
/// The legacy server never sees a `..` segment (URL parsing normalizes it
/// before the handler runs); this one refuses `..` components outright
/// because the raw request target arrives here. Percent sequences stay
/// literal, exactly like the legacy pathname — a file named `%2e%2e` would
/// have to exist inside the bundle to be served.
pub(crate) fn resolve_static(assets_dir: &Path, url_path: &str) -> Option<StaticFile> {
    let relative = if url_path == "/" {
        "index.html"
    } else {
        url_path.strip_prefix('/')?
    };
    if relative.is_empty()
        || relative
            .split('/')
            .any(|part| part == ".." || part.contains('\0') || part.contains('\\'))
    {
        return None;
    }
    let body = std::fs::read(assets_dir.join(relative)).ok()?;
    let content_type = match Path::new(relative).extension().and_then(|ext| ext.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    };
    let cache_control = if relative == "index.html" {
        "no-store"
    } else {
        "public, max-age=31536000, immutable"
    };
    Some(StaticFile {
        body,
        content_type,
        cache_control,
    })
}
