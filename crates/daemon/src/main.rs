//! OpenButler background host entry point.
//!
//! `openbutler-daemon run` serves the IPC endpoint until `stop` or Ctrl+C;
//! any other invocation prints the protocol version.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use openbutler_daemon::agent::executor::DoorExecutor;
use openbutler_daemon::sidecar::DoorConfig;
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
        // Source order (`runDaemonProcess`): the entry lock first, then
        // the host's own answer, before anything is built. The binding
        // lives to the end of this block, so every path out — failure,
        // refusal, or a clean stop — releases it through `Drop`.
        let _start_lock = match openbutler_daemon::start_lock::acquire(&config::data_dir()).await {
            Ok(lock) => lock,
            Err(message) => {
                eprintln!("openbutler-daemon: {message}");
                return ExitCode::FAILURE;
            }
        };
        if let Some(pid) = openbutler_daemon::start_lock::host_pid(&config::data_dir()).await {
            eprintln!("openbutler-daemon: An OpenButler host is already running (pid {pid}).");
            return ExitCode::FAILURE;
        }
        let host = Arc::new(Host::new());
        // Boot validates and loads the live settings copy; a broken store
        // refuses to start rather than running on defaults.
        if let Err(error) = host.reload_settings(&config::data_dir()) {
            eprintln!("openbutler-daemon: {error}");
            return ExitCode::FAILURE;
        }
        // The models.dev catalog: one store per data dir over the real
        // HTTP transport; the fetch runs lazily on the first price read
        // (usage accounting), so boot never waits on the network.
        host.set_catalog(Arc::new(
            openbutler_daemon::catalog::CatalogStore::with_transport(
                &config::data_dir(),
                Arc::new(openbutler_daemon::catalog::HttpTransport),
            ),
        ));
        // The turn executor is host-wide: one door (spawned on the first
        // send — an idle daemon never pays for the sidecar), one runner
        // per session.
        host.set_executor(Arc::new(DoorExecutor::new(
            config::data_dir(),
            production_door_config(),
            Arc::downgrade(&host),
        )));
        host.set_self_ref(&host);
        // The schedule runner: one host writer, publishes straight into
        // the feed, boot-reloads every stored trigger. Interrupted rows
        // recover first — the one-shot priors read clean state.
        let mut schedule_writer =
            match openbutler_daemon::ledger::LedgerWriter::open(&config::data_dir()) {
                Ok(writer) => writer,
                Err(error) => {
                    eprintln!("openbutler-daemon: {error}");
                    return ExitCode::FAILURE;
                }
            };
        if let Err(error) = schedule_writer.recover_interrupted_actions() {
            eprintln!("openbutler-daemon: {error}");
            return ExitCode::FAILURE;
        }
        if let Err(error) = schedule_writer.recover_interrupted_events() {
            eprintln!("openbutler-daemon: {error}");
            return ExitCode::FAILURE;
        }
        let schedule_writer = Arc::new(std::sync::Mutex::new(schedule_writer));
        let publish_writer = Arc::clone(&schedule_writer);
        let runner = openbutler_daemon::schedule::ScheduleRunner::new(
            Arc::clone(&schedule_writer),
            Arc::new(move |event| {
                publish_writer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert_event(&event)
                    .map(|_inserted| ())
                    .map_err(|error| error.to_string())
            }),
            Arc::new(|message: String| eprintln!("openbutler-daemon: {message}")),
        );
        let stored_schedules = match openbutler_daemon::state::schedules::load(&config::data_dir())
        {
            Ok(schedules) => schedules,
            Err(error) => {
                eprintln!("openbutler-daemon: {error}");
                return ExitCode::FAILURE;
            }
        };
        runner.reload(stored_schedules);
        host.set_scheduler(runner);
        // A pending schedule event from a previous run fires at boot —
        // the source pumps at startup too (recovered interrupted rows
        // are already final, so only `pending` is left to consume).
        // Placed after the host writer: both open the same schema, and
        // racing it deadlocks the first pump pass on Windows
        // (`database is locked`).
        host.spawn_schedule_pump();
        let shutdown = host.subscribe_shutdown();

        if let Some(addr) = listen {
            let bound = server::socket::serve_tcp(
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

        let endpoint = server::socket::endpoint_for(&config::data_dir());
        let served = server::socket::serve(endpoint, host.clone(), shutdown);

        // The browser surface lives in this process: the bundle guard runs
        // before any port opens, and the pair URL is the one line the
        // operator needs to open a browser.
        let _web = if web_enabled {
            let bind = web_bind
                .unwrap_or_else(|| openbutler_daemon::server::http::DEFAULT_WEB_BIND.to_owned());
            let start_from = std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|dir| dir.to_path_buf()))
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            match openbutler_daemon::server::http::start_web_server(
                openbutler_daemon::server::http::WebOptions {
                    bind,
                    data_dir: config::data_dir(),
                    assets: openbutler_daemon::server::http::AssetsSource::Default { start_from },
                },
            ) {
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

/// The sidecar door for a production boot. `OPENBUTLER_NODE` and
/// `OPENBUTLER_SIDECAR` override both paths (development); otherwise the
/// packaged layout sits beside the binary and the workspace copy is the
/// last resort. The heap belt keeps the node process's commit bounded —
/// every measurement already runs this profile.
fn production_door_config() -> DoorConfig {
    let node_program = std::env::var_os("OPENBUTLER_NODE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("node"));
    let script = std::env::var_os("OPENBUTLER_SIDECAR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let beside = std::env::current_exe().ok().and_then(|exe| {
                exe.parent()
                    .map(|dir| dir.join("sidecar").join("dist").join("entry.js"))
            });
            match beside {
                Some(path) if path.exists() => path,
                _ => std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("..")
                    .join("sidecar")
                    .join("src")
                    .join("entry.mjs"),
            }
        });
    DoorConfig {
        node_program,
        node_args: vec!["--max-old-space-size=64".to_owned()],
        script,
        mode: String::new(),
        providers: None,
        log_file: None,
        env: Vec::new(),
        spawn_timeout: Duration::from_secs(15),
        // The M2 memory decision: no sidecar sits idle longer than this.
        idle_kill: Duration::from_secs(600),
        drain_max: Duration::from_millis(900),
        queue_max_wait: Duration::from_millis(2500),
        first_event_timeout: Duration::from_secs(10),
        inter_event_timeout: Duration::from_secs(30),
        concurrency: 4,
        line_cap: 64 * 1024,
    }
}
