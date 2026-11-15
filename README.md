# openbutler-daemon

Backend daemon for OpenButler. This workspace owns the wire protocol
(contract types, conformance vectors) and the background runtime that every
client — web, CLI, desktop — connects to.

Status: private pre-release under active construction. Not installable yet.

## Layout

| Path | What lives there |
|---|---|
| `crates/protocol` | Wire types, frame codec, envelope validation, conformance vectors. The contract every client is tested against. |
| `crates/daemon` | Data directory resolution, session state layer, IPC server, action dispatch. |
| `docs` | Documentation entry point (`docs/index.md`) and topic files. |

Dependency versions and the workspace-wide lint policy are declared once in
the root `Cargo.toml`.

## Development

The toolchain is pinned in `rust-toolchain.toml`; three gates must pass
before any change lands:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs the same gates on Linux and Windows.

## Documentation

Read [docs/index.md](docs/index.md) before touching the code. A stale topic
file is treated as a defect: every change updates the topic it affects.

## License

MIT — see [LICENSE](LICENSE).
