# Changelog

Notable changes to this workspace are recorded here.
Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- `openbutler-protocol`: wire contract types, the newline frame codec with a
  32 MB cap, envelope validation, and 14 conformance vectors (one captured
  from a live host).
- `openbutler-daemon`: data-directory resolution, the
  session state layer (paths, header-only reads, listing), the IPC server
  (endpoint derivation, accept loop, backpressure), and action dispatch.
- Workspace structure: centralised dependency versions, the
  `unsafe_code = "forbid"` lint, CI gates, MIT license.
