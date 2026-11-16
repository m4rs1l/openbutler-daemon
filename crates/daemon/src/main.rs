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

/// `run [--listen <addr:port>] [--web] [--web-bind <addr:port>]`: serve the
/// local IPC endpoint, the remote TCP listener (behind the admission gate),
/// and/or the browser surface when asked for.
fn run(args: &mut impl Iterator<Item = String>) -> ExitCode {
    let mut listen: Option<String> = None;
    let mut web_enabled = false;
    let mut web_bind: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => match args.next() {
                Some(addr) => listen = Some(addr),
                None => {
                    eprintln!("openbutler-daemon: --listen needs an address, e.g. 127.0.0.1:45278");
                    return ExitCode::FAILURE;
                }
            },
            "--web" => web_enabled = true,
            "--web-bind" => match args.next() {
                Some(addr) => {
                    web_enabled = true;
                    web_bind = Some(addr);
                }
                None => {
                    eprintln!(
                        "openbutler-daemon: --web-bind needs an address, e.g. 127.0.0.1:45278"
                    );
                    return ExitCode::FAILURE;
                }
            },
            other => {
                eprintln!("openbutler-daemon: unknown run option `{other}`");
                return ExitCode::FAILURE;
            }
        }
    }

    // One rule for the flag and for a saved `daemon.listen`: an IPv4 literal
    // in the tailnet or loopback range, port 1-65535 — refused here with the
    // documented message instead of surfacing as a bind failure.
    let listen = match listen {
        Some(spec) => match openbutler_daemon::listen_target::parse_listen_target(&spec) {
            Ok(target) => Some(format!("{}:{}", target.host, target.port)),
            Err(error) => {
                eprintln!("openbutler-daemon: {error}");
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };

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
        // Boot validates and loads the live settings copy; a broken store
        // refuses to start rather than running on defaults.
        if let Err(error) = host.reload_settings(&config::data_dir()) {
            eprintln!("openbutler-daemon: {error}");
            return ExitCode::FAILURE;
        }
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

        // The browser surface lives in this process: the bundle guard runs
        // before any port opens, and the pair URL is the one line the
        // operator needs to open a browser.
        let _web = if web_enabled {
            let bind =
                web_bind.unwrap_or_else(|| openbutler_daemon::web::DEFAULT_WEB_BIND.to_owned());
            let start_from = std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|dir| dir.to_path_buf()))
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            match openbutler_daemon::web::start_web_server(openbutler_daemon::web::WebOptions {
                bind,
                data_dir: config::data_dir(),
                assets: openbutler_daemon::web::AssetsSource::Default { start_from },
            }) {
                Ok(server) => {
                    println!("pair url: {}", server.pair_url());
                    Some(server)
                }
                Err(error) => {
                    eprintln!("openbutler-daemon: could not start the web server: {error}");
                    return ExitCode::FAILURE;
                }
            }
        } else {
            None
        };

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
