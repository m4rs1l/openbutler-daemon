# openbutler-daemon

The OpenButler host. One background process that owns sessions, the agent
loop, tool execution, scheduling, teams and the local data store. Clients
connect over a newline-delimited JSON frame protocol and render the state it
hands them; every rule, decision and computation lives here, so a client
never re-implements one.

Status: pre-release (0.0.1). The host serves the full action set its CLI
consumes and an integration suite drives the real binary, but there is no
installer yet and the wire may still change — consumers pin a git revision.

## Architecture

| Part | Responsibility |
|---|---|
| `crates/daemon` | The binary and its library: IPC server and dispatch, the turn loop, the tool pipeline, settings and their transaction, the session store, the ledger, scheduling, teams, and the browser surface. |
| `sidecar/` | A Node process that performs model calls with the AI SDK (`ai@7.0.173` plus twelve `@ai-sdk/*` provider packages, all pinned). Stateless: it translates between the door protocol and one provider call. |
| `openbutler-protocol` | Wire types, frame codec and conformance vectors — a separate repository, consumed here as a git dependency pinned by revision. |

The host never speaks to a provider itself. It hands the sidecar one call and
consumes its event stream (`text-delta`, `reasoning-delta`, `tool-call`,
`usage`, `finish`, `error`). The sidecar runs the SDK with its own loop
disabled (`stopWhen: isStepCount(1)`), so a tool call comes back to the host,
which executes it, records it and issues the next call. Retry, key handling,
approvals and every other policy decision stay in the host; the SDK makes
exactly one request per call.

## Requirements

- Rust 1.99.1, pinned by `rust-toolchain.toml` with `rustfmt` and `clippy`
- Node.js on `PATH`, for the sidecar (`OPENBUTLER_NODE` overrides the binary)
- Windows, Linux or macOS

## Build and run

```sh
cargo build --release
./target/release/openbutler-daemon run          # local IPC only
./target/release/openbutler-daemon run --web    # with the browser surface
```

`run` accepts:

| Flag | Effect |
|---|---|
| `--listen <addr:port>` | Also serve remote clients on that address. Only loopback and the tailnet range (`100.64.0.0/10`) are accepted. |
| `--web` | Serve the browser surface. |
| `--web-bind <addr:port>` | Bind the browser surface explicitly (default `127.0.0.1:45278`). |

The data directory holds settings, secrets, sessions and `state.db`; it is
created on first start.

| Variable | Effect |
|---|---|
| `OPENBUTLER_DATA_DIR` | Overrides the data directory (default: the platform's application data directory). |
| `OPENBUTLER_NODE` | Node binary for the sidecar (default `node`). |
| `OPENBUTLER_SIDECAR` | Sidecar script (default: `sidecar/dist/entry.js` beside the binary, falling back to `sidecar/src/entry.mjs` in a checkout). |
| `OPENBUTLER_LOG_LEVEL` | `error`, `warn`, `info` (default) or `debug`. |

## Tests and gates

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs the first two on Linux and the suite on Linux and Windows. The suite
carries the memory budget as an executable constraint: an idle daemon must
stay under 32 MiB (measured baseline 8.5 MiB against an architecture target of
20 MiB), and a change that breaks it fails the build.

## Design rules

- **The host is the authority.** A client sends requests and renders the
  finished state it gets back, including the screen lines for the surfaces the
  CLI prints. Nothing is computed twice.
- **One turn at a time per session.** A message is persisted before the model
  is asked and the settlement save is forced, so a crash cannot orphan a
  transcript.
- **Stopping means stopping.** A stop reaches a running command, kills its
  process tree and leaves the pair in the conversation; the next turn reads
  what was interrupted instead of a finished command.
- **The wire is a contract, not a convenience.** Types live in
  `openbutler-protocol` with conformance vectors; a wire change updates the
  vectors, the pinned revision and both consumers together.
- **Secrets stay in the secret document.** `settings.yaml` never carries a
  key; provider keys live in `config.yaml` and are written through the secrets
  path. Credential-bearing actions — pairing codes, auth tokens, API keys,
  provider setup, MCP server writes — are refused to remote connections.
- **Durability is explicit.** Settings writes commit as a journaled pair,
  session records are staged and renamed, and the ledger is write-ahead in
  `state.db`.
- **No `unwrap`, no `expect`, no `unsafe`** in library code. The workspace
  lint policy enforces it; review is not the gate.

## Repository conventions

- Every behavioural change lands with a test that fails without it.
- Formatting, clippy (`-D warnings`) and the suite pass before a commit.
- Commit messages describe the change technically.
- The internal design notes are working material and are not part of this
  repository.

## License

MIT — see [LICENSE](LICENSE).
