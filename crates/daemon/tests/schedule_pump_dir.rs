//! The schedule pump's data directory: the host hands the pump the
//! directory the daemon was configured with, never a global guess.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::temp_data_dir;
use openbutler_daemon::dispatch::Host;

/// The pump's first act is opening the ledger writer, and that open creates
/// `state.db`. A pump pointed at some other directory therefore leaves this
/// one untouched - which is exactly what a test that triggers a schedule
/// mutation must never do to the owner's real store.
#[tokio::test]
async fn the_pump_uses_the_data_directory_it_is_given() {
    let dir = temp_data_dir("pump-dir");
    let host = Arc::new(Host::new());
    host.set_self_ref(&host);
    host.spawn_schedule_pump(&dir);

    for _ in 0..100 {
        if dir.join("state.db").exists() {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the pump never opened a ledger in the directory it was given");
}
