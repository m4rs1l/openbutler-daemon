//! A host started with `--web` publishes the browser surface it serves: the
//! `status` answer carries `web.bind` and `web.pairUrl` of the live server.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("ob-web-publish-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

#[tokio::test]
async fn a_web_host_reports_its_pair_url_through_status() {
    let root = scratch("e2e");
    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();

    // The default bundle guard walks up from the executable, so the binary
    // runs from a scratch directory that owns a minimal bundle.
    let bin_dir = root.join("bin");
    std::fs::create_dir_all(bin_dir.join("dist").join("client")).unwrap();
    std::fs::write(
        bin_dir.join("dist").join("client").join("index.html"),
        "<!doctype html><title>OpenButler</title>\n",
    )
    .unwrap();
    let binary = bin_dir.join(if cfg!(windows) {
        "openbutlerd.exe"
    } else {
        "openbutlerd"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_openbutlerd"), &binary).unwrap();

    let mut child = std::process::Command::new(&binary)
        .args(["run", "--web", "--web-bind", "127.0.0.1:0"])
        .env("OPENBUTLER_DATA_DIR", &data)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the web host starts");

    let answered = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(status) = openbutler_daemon::start_lock::host_status(&data).await
                && status.get("web").is_some()
            {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;

    child.kill().ok();
    let _ = child.wait();
    let status = answered.expect("status never carried the web surface");
    let pair_url = status["web"]["pairUrl"].as_str().expect("pairUrl");
    assert!(pair_url.starts_with("http://127.0.0.1:"), "{pair_url}");
    assert!(pair_url.contains("/auth/pair?t="), "{pair_url}");
    let bind = status["web"]["bind"].as_str().expect("bind");
    assert!(bind.starts_with("127.0.0.1:"), "{bind}");
    assert!(
        !bind.ends_with(":0"),
        "the live port, not the request: {bind}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
