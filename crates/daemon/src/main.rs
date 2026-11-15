//! OpenButler background host: state, sessions, IPC, and web serving.

fn main() {
    println!(
        "openbutler-daemon protocol v{}",
        openbutler_protocol::PROTOCOL_VERSION
    );
}
