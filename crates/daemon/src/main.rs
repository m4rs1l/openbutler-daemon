//! OpenButler background host: state, sessions, IPC, and web serving.

fn main() {
    println!(
        "openbutler-daemon protocol v{}",
        openbutler_protocol::DAEMON_PROTOCOL_VERSION
    );
}
