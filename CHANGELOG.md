# Changelog

Notable changes to this workspace are recorded here.
Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- `agent/context.rs`: the runtime context builder — fills
  `SystemPromptContext` from disk exactly as legacy `buildAgentSystem`
  (budgeted user notes with the truncation notice, availability reads,
  skill discovery, role base, team block injected rather than loaded
  secretly), so the assembled system prompt carries every live value
  (`prompt_context`, 4 tests); `settings::config_path` becomes the single
  `config.yaml` path authority.
- Door contract: the assembled system prompt rides `SidecarCall.system`
  (absent from the wire when `None`) from `TurnRequest` through
  `streamText`, pinned by a new conformance vector (present + absent
  states) and an end-to-end test that reads it back from the provider's
  request body as the first `role: system` message — the transport's own
  folding, now part of the contract.
- `agent/providers.rs`: the system-prompt context providers — memory
  path layout (first-24-hex workspace key), bounded index/note reads with
  the line-boundary cut and the verbatim truncation notice, profile
  availability states, the role-exact team block, skill roots/discovery
  (override, per-scope cap, metadata-name match) and the UTF-16 index
  budget (`prompt_providers`, 15 tests; pinned retroactively — the
  implementation landed before its tests, documented in the header).
- `ledger` writer: the tool-action write path (c2a) — idempotent
  `proposed` record as the write-ahead gate before execution, one
  transaction per transition chain, boot recovery to `failed` with the
  source's wording, schema CHECK generated from `TOOL_ACTION_STATES`, and
  the 32 000-char JSON envelope (`ledger_actions`, 7 tests).
- `agent/`: the turn loop (M2c c1) — admission save before the model is
  asked, model-only host-context labels, one-assistant-entry streaming with
  the reasoning splice and block duration, settled tool entries, turn
  outcome stamping (`completed`/`cancelled`/`failed`) and the 1 s mid-stream
  dirty save (`agent_loop`, 7 tests against the real sidecar).
- `state/save.rs`: the session write path — recompute-on-save (display
  fields rebuilt from `entries`, `updatedAt` refreshed), the meta-before-  `entries` header order, skip-without-user (`allow_empty` to override),
  the `Cannot save an invalid session.` gate, and the atomic `0600`
  temp-plus-rename write; `conversation`/`conversationArchive` round-trip
  untouched (`session_save`, 5 tests).
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
