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
        Some("run") => run(&mut args),
        _ => {
            println!(
                "openbutler-daemon protocol v{}",
                openbutler_protocol::DAEMON_PROTOCOL_VERSION
            );
            ExitCode::SUCCESS
        }
    }
}

/// `run [--listen <addr:port>]`: serve the local IPC endpoint, and the
/// remote TCP listener (behind the admission gate) when asked for.
fn run(args: &mut impl Iterator<Item = String>) -> ExitCode {
    let mut listen: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => match args.next() {
                Some(addr) => listen = Some(addr),
                None => {
                    eprintln!("openbutler-daemon: --listen needs an address, e.g. 127.0.0.1:45278");
                    return ExitCode::FAILURE;
                }
            },
            other => {
                eprintln!("openbutler-daemon: unknown run option `{other}`");
                return ExitCode::FAILURE;
            }
        }
    }

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

        if let Some(addr) = listen {
            let bound = server::serve_tcp(
                &addr,
                host.clone(),
                config::data_dir(),
                openbutler_daemon::auth::REMOTE_AUTH_TIMEOUT,
            )
            .await;
            match bound {
                Ok(bound) => println!("remote listener: {bound}"),
                Err(error) => {
                    eprintln!("openbutler-daemon: could not listen on {addr}: {error}");
                    return ExitCode::FAILURE;
                }
            }
        }

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
