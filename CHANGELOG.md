# Changelog

All notable changes to the TofuPilot CLI are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.26.25]

### Fixed

- The operator UI keeps the keyboard alive between phases. Until now only a
  phase whose first component was a text input kept focus, so an acknowledge
  prompt, a switch, a radio group or a bare "press Continue" step dropped
  focus and had to be clicked — once per phase, with the operator's hands on a
  barcode scanner. Enter, including the trailing Enter a scanner sends, now
  acknowledges every prompt shape; Cmd/Ctrl+Enter submits from anywhere,
  including inside a textarea where plain Enter must stay a newline; and a held
  Enter no longer carries over to skip the next prompt. The ⏎ hint on the
  Continue button is shown only when plain Enter really submits from the
  focused element.

## [0.26.24]

### Added

- `tofupilot run ./procedure1_entrypoint.py` runs the file you name. A project
  whose procedures share one codebase (`procedure1_entrypoint.py`,
  `procedure2_entrypoint.py`, …) could already set a per-procedure entry point
  for deployments, but local runs always fell back to `main.py` — a procedure
  could not be validated locally without renaming files or keeping separate
  checkouts. The named file now wins over on-disk detection for openhtf and
  plain-Python procedures; for pytest and robot, where a single file cannot be
  the run target, the CLI warns instead of silently ignoring the argument. This
  is local expressiveness, not remote lookup: the run executes the file you
  name and does not read the procedure's stored entry point.
- A logged-out `tofupilot run` now shows the onboarding block with the local
  run annotated "(no account needed)", instead of a bare "Not logged in" that
  read as "log in first". Only for the no-ID form — `run --deployment <id>`
  still gets the plain message.
- An explicit red `STOP` button in the operator UI. Aborting a running sequence
  used to be a small grey ghost icon at the edge of the progress bar, which
  read as decoration; it is now a labeled solid-red button (the test-bench
  idiom), morphing to `KILL` after a stop and to a spinner while killing, all
  states at a fixed width so the bottom bar never reflows. `KILL` stays
  disabled for 500 ms after a `STOP` press so a touchscreen double-tap cannot
  escalate a graceful stop into a SIGKILL, and if the run has not terminated
  5 s after a kill was sent (command lost on a dropped WebSocket) the control
  re-arms so it can be re-sent. The state also resets between runs, including
  a CLI restart mid-run.
- Python interpreters and deployment artifacts are served from TofuPilot
  domains, so a firewalled station network no longer needs `github.com`,
  `objects.githubusercontent.com` or the R2 endpoint host on its whitelist —
  `*.tofupilot.app` + `tofupilot.sh` + `dl.tofupilot.sh` is enough (plus PyPI
  unless you ship standalone bundles). The CLI points uv at the CPython mirror
  on `dl.tofupilot.sh/python` for station pull venvs and local `tofupilot run`
  bootstraps; an operator-set `UV_PYTHON_INSTALL_MIRROR` is respected,
  `TOFUPILOT_PYTHON_MIRROR=<base>` targets an air-gapped mirror and `=off`
  reverts to upstream GitHub. Older CLIs on firewalled stations can get the
  same effect by setting the uv variable system-wide.

### Changed

- `ls` tables print full IDs. Every table truncated the ID column to 8
  characters while `get`, `update`, `rm` and `link` all need the full UUID —
  so the ID could not be copy-pasted from the CLI's own output.
- The source upload cap is 95 MB (was 100 MB). Uploads now traverse the
  Cloudflare edge, whose body limit is exactly 100 MB, so a cap at the limit
  failed with an opaque HTML error; oversized packs now get a readable message.

### Fixed

- An oversized event no longer takes a station offline on the dashboard. Any
  event serializing past the realtime frame limit (64 KB) closed the WebSocket
  with a code the client treats as terminal, and the station daemon's broker
  connect was a one-shot at boot — so a single large event muted the station's
  live link for the rest of the CLI process, while runs kept passing locally
  and uploading over HTTP (the failure read as "the dashboard keeps losing my
  station"). Reproduced with a 362 KB PNG passed as an `image_url` data URI: a
  483 KB prompt event, 7.4× the limit. Oversized events are now degraded to
  fit before publishing — inline images and measurement blobs are replaced by
  a short explanation (the kiosk still shows the full picture), long text is
  truncated on character boundaries with a `[N KB truncated for live
  streaming]` marker, and phase log lists drop from the middle so the setup
  head and failure tail survive. What the event exists to deliver — the prompt
  the operator must answer, the phase outcome — always lands. An event that
  cannot be reduced is dropped rather than published: a missing event beats a
  dead station. Full data reaches the server over HTTP upload either way.

## [0.26.23]

### Added

- Every run now stamps the version of the binary actually executing it:
  a `tofupilot <version> (<os>-<arch>)` line on the terminal at run start
  (all modes), and a `meta` header as the first line of the per-run log.
  A stale daemon or shadowed binary is no longer indistinguishable from
  an up-to-date one in support screenshots and log files.

### Fixed

- The station daemon starts local-first: the kiosk operator UI, the boot
  pull, and locally-triggered runs no longer wait for the dashboard
  realtime link. The broker connect runs in a background supervisor that
  retries with backoff ("Station keeps running local-only meanwhile");
  when it comes up, the dashboard link attaches transparently — remote
  commands, telemetry, and live streaming start flowing, and runs started
  after that point stream live. A station on a network that never allows
  realtime is fully operable at the bench, with results uploaded over
  HTTP as always.
- A deployment run no longer freezes forever when the realtime server is
  unreachable (missing DNS record for the realtime domain, firewalled
  WebSockets). The realtime WebSocket handshake is now bounded inside the
  shared connect primitive, so no caller can await it forever: the station
  daemon's boot retry loop actually cycles instead of hanging before the
  operator UI starts, and a run's dashboard link connects in a background
  task — the run, the local operator UI, and the result upload start
  immediately in every case. When the link cannot be established within
  10 seconds, a warning explains what to check and the run simply stays
  offline; events emitted while the link comes up are buffered and drain
  to the dashboard once connected. Mid-run drops were already self-healing
  (the client auto-reconnects with backoff) and are unchanged.
- Linked local `--upload` runs (user credentials) no longer attempt the
  station-only realtime connection: the dashboard live view is keyed on a
  station identity the server cannot mint for a user key, so the CLI now
  prints `realtime on dashboard: station-only, skipped for user run`
  instead of failing the connection and warning about a phantom auth
  problem.
- Deployment runs resolve the station identity first. A leftover user
  login (`tofupilot login`) on the same machine silently disabled realtime
  streaming for every deployment run: the streaming route is station-only
  and rejected the user key with a 403 that was swallowed. Same shadowing
  class as the `pull` fix in 0.26.15. Every realtime setup failure is now
  reported on stderr instead of being silent.
- The per-run log (`~/.tofupilot/logs/run-<id>.log`) is created at the very
  start of the run setup, before any network step, so a run that wedges
  during setup still leaves a log. The path is now announced in `--json`
  mode too (on stderr), and a log-creation failure warns instead of staying
  silent.

## [0.26.19]

Documentation release: backfilled the changelog for 0.26.5 through 0.26.18.
No functional changes.

## [0.26.18]

### Added

- Debug mode for Python phases: `tofupilot run --debug` starts a debugpy
  listener (default port 5678, override with `--debug-port`) and waits for a
  debugger such as VS Code to attach, so you can set breakpoints, step, and
  inspect variables in phase code. Phase timeouts are suspended while a
  debugger is attached; if debugpy is not installed the run tells you how to
  add it and keeps normal timeouts.
- Per-plug `config` mapping in `procedure.yaml`: each key is passed as a
  keyword argument to the plug class `__init__`, so instrument addresses and
  settings live in the procedure instead of being hard-coded in Python.

## [0.26.17]

_Not published as a standalone release; first shipped with 0.26.18._

### Added

- Run and unit metadata support in the framework. Python phases can set
  custom key-value metadata via `run.metadata[...]` and `unit.metadata[...]`;
  values are validated at assignment and uploaded with the run.
- The operator identify form can collect unit metadata: declare
  `unit.metadata.<key>` fields in `procedure.yaml` and they render as text
  inputs next to the serial number on the TUI, kiosk, web operator UI, and
  agent `--ui-values`.

## [0.26.16]

_Not published as a standalone release; first shipped with 0.26.18._

### Fixed

- The bundled kiosk operator UI now ships with product analytics and session
  replay enabled; release builds previously omitted the analytics key, so
  kiosk usage was never recorded.

## [0.26.15]

### Fixed

- `tofupilot pull` resolves the station identity first instead of the user
  key, fixing `403 Station authentication required` on stations that had
  also performed a user login (regression from the 0.26.9 credential split).

## [0.26.14]

### Added

- macOS release binaries are signed and notarized with an Apple Developer
  ID certificate, so Gatekeeper no longer blocks `tofupilot` on download.

## [0.26.13]

Version-only bump to ship the first Windows release with Authenticode-signed
binaries. No code changes.

## [0.26.12]

### Fixed

- The Linux station launcher now resolves the localized desktop folder
  (e.g. `~/Bureau` on French systems) instead of hardcoding `~/Desktop`, so
  the station shortcut lands where the operator can see it. Windows resolves
  the Known Folder (OneDrive-aware) as well.

## [0.26.11]

### Fixed

- Operator answers with a `bind:` directive now record their measurement on
  the in-terminal TUI and the agent `--json` protocol, matching the kiosk
  and Studio. Previously bound `radio`/`select` inputs recorded no
  measurement, and later phases reading it crashed.

## [0.26.10]

### Added

- Stations can opt into system Python packages for deployments: the new
  `system_packages` station config toggle builds deployment venvs with
  `--system-site-packages`, so natively installed instrument drivers and
  vendor SDKs are importable from procedures.

## [0.26.9]

### Fixed

- A user login (`tofupilot login` or `deploy`) no longer overwrites the
  station credential: user and station identities are stored separately, so
  the station service keeps working after a user logs in on the same
  machine.

## [0.26.8]

### Fixed

- `tofupilot run --kiosk` works again when launched as root in the
  foreground (e.g. headless jigs viewed over an SSH port-forward). The kiosk
  server stays disabled for the root station daemon, where it would be a
  privilege-escalation risk.

## [0.26.7]

### Fixed

- `tofupilot update` retries transient connection resets during the binary
  download instead of aborting mid-transfer.

## [0.26.6]

### Fixed

- `tofupilot update` retries transient network failures during the version
  check instead of failing on a single blip.

## [0.26.5]

### Fixed

- `attach.data` attachments on the native YAML engine are now written to
  disk and uploaded; previously they were silently dropped.
- Uploaded attachments are finalized after upload, so the dashboard shows
  their size, content type, and preview instead of `Unknown`.

## [0.26.4]

### Fixed

- OpenHTF operator prompts now honor `--ui-values` in agent/JSON mode. Pre-baked
  responses auto-resolve `prompts.prompt(...)` instead of timing out, matching the
  native YAML procedure path.

## [0.22.19]

### Fixed
- Corrected the install command in the README: the one-liner now points at the
  real installer (`https://tofupilot.sh/install`) instead of a URL that served
  the website, and adds the Windows PowerShell installer.

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

[Unreleased]: https://github.com/tofupilot/cli/compare/v0.26.19...HEAD
[0.26.19]: https://github.com/tofupilot/cli/compare/v0.26.18...v0.26.19
[0.26.18]: https://github.com/tofupilot/cli/compare/v0.26.15...v0.26.18
[0.26.15]: https://github.com/tofupilot/cli/compare/v0.26.14...v0.26.15
[0.26.14]: https://github.com/tofupilot/cli/compare/v0.26.13...v0.26.14
[0.26.13]: https://github.com/tofupilot/cli/compare/v0.26.12...v0.26.13
[0.26.12]: https://github.com/tofupilot/cli/compare/v0.26.11...v0.26.12
[0.26.11]: https://github.com/tofupilot/cli/compare/v0.26.10...v0.26.11
[0.26.10]: https://github.com/tofupilot/cli/compare/v0.26.9...v0.26.10
[0.26.9]: https://github.com/tofupilot/cli/compare/v0.26.8...v0.26.9
[0.26.8]: https://github.com/tofupilot/cli/compare/v0.26.7...v0.26.8
[0.26.7]: https://github.com/tofupilot/cli/compare/v0.26.6...v0.26.7
[0.26.6]: https://github.com/tofupilot/cli/compare/v0.26.5...v0.26.6
[0.26.5]: https://github.com/tofupilot/cli/compare/v0.26.4...v0.26.5
[0.26.4]: https://github.com/tofupilot/cli/compare/v0.26.3...v0.26.4
[0.22.19]: https://github.com/tofupilot/cli/compare/v0.22.18...v0.22.19
[0.22.18]: https://github.com/tofupilot/cli/compare/v0.22.17...v0.22.18
[0.22.17]: https://github.com/tofupilot/cli/compare/v0.22.16...v0.22.17
[0.22.16]: https://github.com/tofupilot/cli/releases/tag/v0.22.16
