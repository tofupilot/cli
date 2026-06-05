# Architecture

The TofuPilot CLI is a single Rust binary (`tofupilot`) that runs, deploys, and
manages hardware-test procedures from a terminal or as a long-lived station
daemon. This document orients a new contributor; for the command surface run
`tofupilot --help`.

## Workspace layout

```
.
├── src/
│   ├── main.rs            # clap dispatch, signal handling, top-level commands
│   ├── commands/          # command implementations (see below)
│   ├── api/               # CODE-GENERATED CRUD commands (DO NOT EDIT)
│   ├── config/timeouts.rs # global duration constants
│   ├── local_ws/          # embedded operator-ui SPA + WebSocket (kiosk)
│   ├── http.rs            # process-wide reqwest client + bearer ext
│   ├── log.rs display.rs  # output formatting
│   └── browser_open.rs    # cross-platform browser launch
├── build.rs               # ensures operator-ui/dist exists for include_dir!
└── tests/agent_protocol/  # Python end-to-end protocol harness
```

The crate depends on three sibling crates by path:

- **`tofupilot-sdk`** (`crates/tofupilot-sdk`) — the generated HTTP SDK for the V2 API.
- **`station-protocol`** (`crates/station-protocol`) — `StationEvent` /
  `StationCommand` wire types shared with the dashboard.
- **`execution-engine`** (`crates/execution-engine`) — the framework-agnostic
  run loop (phases, measurements, identify-unit, UI requests).

## Command areas

| Area | Path | Responsibility |
|------|------|----------------|
| Auth | `commands/auth/` | device-flow + token login, `whoami` (cache-first), logout, credentials |
| Run | `commands/run/` | execute a procedure; the largest and most involved module |
| Deploy | `commands/pull/` | sync deployments from the dashboard, extract bundles |
| Link | `commands/link/` | bind a local procedure dir to a remote procedure for upload |
| Station | `commands/station/` | long-lived daemon: WS to dashboard, pull + run loop |
| Service | `commands/{service,install,uninstall}.rs` | systemd / launchd lifecycle |
| Update | `commands/update/` | version check, download + checksum, self-replace |
| API (gen) | `api/*.rs` | thin CRUD wrappers over the SDK |

## The run pipeline

`run` is where most of the complexity lives. A run resolves a **source** (a local
path, optionally uploaded; or a pulled deployment, always uploaded), then drives
the `execution-engine` while fanning events out to every active UI surface.

```
                         ┌─────────────────────┐
   local path / pull ───▶│  execution-engine   │  (phases, measurements,
                         │  run loop           │   identify-unit, UI requests)
                         └──────────┬──────────┘
                                    │ StationEvent / UiRequest
                                    ▼
                         ┌─────────────────────┐
                         │   event_router.rs   │  multiplex
                         └──┬──────┬──────┬─────┘
            ┌───────────────┘      │      └───────────────┐
            ▼                      ▼                      ▼
     Centrifugo WS           local_ws (kiosk)        TUI (ratatui)
   (dashboard, via            browser SPA on          in-terminal
    station::client)          loopback :7321          renderer
            │
            └──▶ agent protocol (JSON stdin/stdout) when driven by an agent
```

Key pieces inside `commands/run/`:

- **`connector/`** — detects and spawns the Python test framework (OpenHTF,
  pytest, Robot Framework) and parses its NDJSON event stream into
  `PythonEvent`s.
- **`agent_proto/`** — the JSON stdin/stdout protocol for programmatic drivers:
  an emitter queue, a stdin reader task, and pending-request bookkeeping so a
  late-attaching consumer can reconstruct an in-flight prompt.
- **`identify_host.rs`** — the operator "identify this unit" prompt.
  `can_prompt()` reports whether any UI surface exists; when none does, a
  headless run fails fast (`NoUi`) instead of hanging on a prompt nobody can
  answer.
- **`event_router.rs`** — transforms engine events into `StationEvent`s and
  pushes them to Centrifugo, the kiosk WS, the TUI channel, and the agent
  protocol.
- **`queue.rs`** — when an upload fails (network down), the run and its
  attachments are persisted to the local redb store and retried later
  (`tofupilot queue retry`).
- **`bootstrap.rs`** — provisions a virtualenv via `uv` for local-path runs.

## Station daemon

`commands/station/` is the headless, supervised mode (systemd user service or
launchd agent, written by `install.rs`). It holds a persistent Centrifugo
WebSocket (`station::client::StreamClient`), advertises hardware info, and loops:
receive a `StationCommand` from the dashboard → pull if needed → run via the same
`run::start()` path → publish events back. With no subcommand and valid
credentials, the bare `tofupilot` invocation enters this mode.

## Local state

| Location | Contents |
|----------|----------|
| `~/.tofupilot/credentials.json` | API key, base URL, org, optional installation id (chmod 0600 / ACL-locked) |
| `~/.tofupilot/state.redb` | embedded KV store: whoami cache, update cache, pull sync state, station config, the offline run queue |
| `~/.tofupilot/deployments/` | extracted procedure bundles from `pull` |

The redb store is held under an exclusive per-process lock with a PID-liveness
probe to clear stale locks. `SIGTERM` / `SIGHUP` exit promptly so the OS releases
the lock.

## Backend communication

- **HTTP**: the V2 API via `tofupilot-sdk`, bearer-authenticated. The shared
  `reqwest` client lives in `src/http.rs` (connect timeout + read-inactivity
  timeout tuned for large downloads).
- **Real-time**: Centrifugo over WebSocket, channel `installations/<id>`,
  carrying `StationEvent`s in both directions.
- **Auth**: device flow (browser, interactive) or setup-token redemption
  (headless), both landing in `credentials.json`.

## Code generation

Everything under `src/api/` is generated ("DO NOT EDIT" header) from the SDK
surface. Change the generator, not the output, and ensure the generator emits
`rustfmt`-clean code so `cargo fmt --check` stays green.

## Testing

- **Rust unit tests** live inline (`#[cfg(test)]`) next to the logic they cover —
  version comparison, update-cache windows, queue ids, time formatting,
  credentials, connector request-building, the TUI state machine, and more.
- **End-to-end protocol tests** (`tests/agent_protocol/`) drive the compiled
  binary over the agent protocol against OpenHTF / pytest / Robot fixtures.

See `CONTRIBUTING.md` for how to run them.
