//! Spawning the child process, and the writer that owns its stdin.
//!
//! Orphan discipline, in layers: closing stdin makes the sidecar exit on
//! its own (the portable rule on every platform), `kill_on_drop` covers a
//! dropped writer, and the hard per-platform guards (Windows Job Object,
//! Linux PDEATHSIG) are pending the workspace `unsafe_code = "forbid"`
//! decision — see `docs/sidecar-door.md`.

use std::process::Stdio;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

use super::{DoorConfig, DoorError};

pub(crate) struct ChildWriter {
    stdin: Option<BufWriter<ChildStdin>>,
    child: Child,
}

impl ChildWriter {
    pub(crate) async fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "sidecar stdin is closed",
            ));
        };
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await
    }

    pub(crate) fn close_stdin(&mut self) {
        self.stdin = None;
    }

    pub(crate) async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    pub(crate) async fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill().await
    }
}

/// Start `node <script>` with a minimal environment: only what the
/// contract allows (PATH/HOME, SystemRoot on Windows) plus the door's
/// own extras — the sidecar never inherits the daemon's full environment.
pub(crate) async fn spawn(config: &DoorConfig) -> Result<(ChildWriter, ChildStdout), DoorError> {
    let mut command = Command::new(&config.node_program);
    for flag in &config.node_args {
        command.arg(flag);
    }
    command.arg(&config.script);
    command.env_clear();
    command.env(
        "PATH",
        std::env::var("PATH").unwrap_or_else(|_| String::from("/usr/bin:/bin")),
    );
    if let Ok(home) = std::env::var("HOME") {
        command.env("HOME", home);
    }
    #[cfg(windows)]
    {
        if let Ok(system_root) = std::env::var("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
    }
    for (key, value) in config.child_env() {
        command.env(key, value);
    }
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);
    #[cfg(windows)]
    {
        // tokio's Command has `creation_flags` natively on Windows.
        command.creation_flags(0x08000000);
    }

    let mut child = command.spawn().map_err(|error| {
        DoorError::SpawnFailed(format!("{}: {error}", config.node_program.display()))
    })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| DoorError::SpawnFailed("stdin pipe missing".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| DoorError::SpawnFailed("stdout pipe missing".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| DoorError::SpawnFailed("stderr pipe missing".into()))?;
    tokio::spawn(drain_stderr(stderr));

    Ok((
        ChildWriter {
            stdin: Some(BufWriter::new(stdin)),
            child,
        },
        stdout,
    ))
}

/// Keep the child's stderr flowing: a full pipe would freeze the
/// sidecar. Diagnostics are drained (the contract keeps stdout pure);
/// a bounded tail for debugging is a later hook, not a door concern.
async fn drain_stderr(mut stderr: ChildStderr) {
    let mut chunk = [0u8; 4096];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}
