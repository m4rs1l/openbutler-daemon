//! `web.report`: the browser surface's two states, composed by the host.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::temp_data_dir;
use openbutler_daemon::dispatch::{self, Host};
use openbutler_protocol::WebStatus;
use serde_json::json;

fn report(host: &Host, dir: &std::path::Path) -> serde_json::Value {
    let mut state = dispatch::DispatchState::new();
    let request = openbutler_protocol::DaemonRequest::new("w1".to_owned(), "web.report".to_owned());
    match dispatch::handle(host, dir, &mut state, &request) {
        openbutler_protocol::DaemonResponse::Success { result, .. } => result,
        openbutler_protocol::DaemonResponse::Failure { error, .. } => panic!("refused: {error}"),
    }
}

#[test]
fn without_a_web_server_the_host_says_so() {
    let dir = temp_data_dir("web-report-off");
    let answer = report(&Host::new(), &dir);
    assert_eq!(answer["running"], json!(false));
    assert_eq!(
        answer["lines"][0]["text"],
        json!("The web server is not running on this host.")
    );
}

#[test]
fn a_published_surface_carries_the_pair_url_and_its_bind() {
    let dir = temp_data_dir("web-report-on");
    let host = Host::new();
    host.publish_web(WebStatus {
        bind: "127.0.0.1:45278".to_owned(),
        pair_url: "http://127.0.0.1:45278/?pair=abc".to_owned(),
    });
    let answer = report(&host, &dir);
    assert_eq!(answer["running"], json!(true));
    assert_eq!(answer["bind"], json!("127.0.0.1:45278"));
    assert_eq!(
        answer["lines"][0]["text"],
        json!("Web UI: http://127.0.0.1:45278/?pair=abc")
    );
}
