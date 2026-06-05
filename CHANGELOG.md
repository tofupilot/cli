# Changelog

All notable changes to the TofuPilot CLI are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.22.18]

### Fixed
- Removed monorepo-only and personal filesystem paths from the published repo:
  the agent-protocol test scripts, the kiosk placeholder page, the architecture
  doc, and an internal codegen tool now use repo-relative or caller-supplied
  paths.

## [0.22.17]

Open-source readiness pass. No user-facing behavior changes.

### Added
- OSS project files: `CHANGELOG.md`, `CONTRIBUTING.md`, `SECURITY.md`,
  `ARCHITECTURE.md`, plus `rustfmt.toml`, `clippy.toml`, and `deny.toml`.
- Test CI (`test-cli.yml`): `cargo fmt --check`, `clippy -D warnings`,
  `cargo test`, an agent-protocol end-to-end smoke test, and a `cargo-deny`
  supply-chain gate.
- `sync-cli.yml`: mirrors the CLI to a standalone public repository, vendoring
  the SDK / station-protocol / execution-engine crates and the operator-ui
  build so the published repo compiles on its own.
- Rust unit tests for framework detection, the offline-queue backoff and error
  classification, outcome mapping, timestamp formatting, and credentials.
- Module-level documentation across the crate; `cargo doc` is warning-clean.

### Changed
- Unified error handling onto a single `CliError` type (no more
  `Result<_, String>` / `Box<dyn Error>` sprawl); all error messages and exit
  codes are preserved.
- Standardized command-handler names on the `_cmd` suffix.
- Bumped `validator` to 0.20, clearing a vulnerable transitive `idna` and an
  unmaintained `proc-macro-error`.

### Fixed
- The generated `imports` command is now wired into the CLI dispatch.

## [0.22.16]

### Fixed
- Headless runs no longer hang on unit identification when no UI surface is
  available; they now fail fast with actionable guidance.

### Changed
- `whoami` is cache-first for instant offline identity.
- Update checks are throttled and the station cadence is slowed to reduce load.

### Added
- `link` / `unlink`: bind a local procedure directory to a remote procedure for
  run upload.
- Operator helper text on identify-unit fields.
- Robot Framework connector mirroring the pytest connector.
- Auto-bootstrap of a virtualenv on local-path runs and monorepo workspace
  bootstrap via `uv sync`.

[Unreleased]: https://github.com/tofupilot/cli/compare/v0.22.18...HEAD
[0.22.18]: https://github.com/tofupilot/cli/compare/v0.22.17...v0.22.18
[0.22.17]: https://github.com/tofupilot/cli/compare/v0.22.16...v0.22.17
[0.22.16]: https://github.com/tofupilot/cli/releases/tag/v0.22.16
