//! OpenButler background host entry point.
//!
//! `openbutler-daemon run` serves the IPC endpoint until `stop` or Ctrl+C;
//! any other invocation prints the protocol version.

use std::process::ExitCode;
use std::sync::Arc;

use openbutler_daemon::{config, dispatch::Host, server};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("run") => run(),
        _ => {
            println!(
                "openbutler-daemon protocol v{}",
                openbutler_protocol::DAEMON_PROTOCOL_VERSION
            );
            ExitCode::SUCCESS
        }
    }
}

fn run() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("openbutler-daemon: could not start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async {
        let host = Arc::new(Host::new());
        let shutdown = host.subscribe_shutdown();
        let endpoint = server::endpoint_for(&config::data_dir());
        let served = server::serve(endpoint, host.clone(), shutdown);

        tokio::select! {
            result = served => match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("openbutler-daemon: could not serve the IPC endpoint: {error}");
                    ExitCode::FAILURE
                }
            },
            control_c = tokio::signal::ctrl_c() => {
                if control_c.is_ok() {
                    host.request_shutdown();
                }
                ExitCode::SUCCESS
            }
        }
    })
}
