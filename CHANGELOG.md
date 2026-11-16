# Changelog

Notable changes to this workspace are recorded here.
Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- `sidecar/`: the real Node model transport — `ai@7.0.173` exact plus
  eleven provider packages exact on the `provider@4.0.38` line, a lazy
  provider registry keyed by allowlist names, single-step streaming
  (`stopWhen: isStepCount(1)`, `maxRetries: 0`, schema-only tools), the
  door's error taxonomy, and console/stdout ownership. Acceptance suite
  (`sidecar_real`, 7 tests) runs the real SDK against a local mock:
  tool-call stops after exactly one model request, unknown keys refused
  with keys kept off stdout, 429 → rate-limit taxonomy, and per-call
  lazy loading proven from `NODE_DEBUG` traces. A build step
  (`sidecar/build.mjs`, esbuild with per-provider lazy chunks) and the
  bundle acceptance test (`sidecar_bundle`, 1 test) prove the split
  survives bundling: the metafile covers all eleven provider graphs and
  loader traces touch strictly fewer dist files than exist.
- `openbutler-protocol`: wire contract types, the newline frame codec with a
  32 MB cap, envelope validation, and 14 conformance vectors (one captured
  from a live host).
- `openbutler-daemon`: data-directory resolution, the
  session state layer (paths, header-only reads, listing), the IPC server
  (endpoint derivation, accept loop, backpressure), and action dispatch.
- Workspace structure: centralised dependency versions, the
  `unsafe_code = "forbid"` lint, CI gates, MIT license.
