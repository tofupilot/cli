# Agent protocol — authoritative notes

- **Spec revision**: 1.0 — 2026-04-21
- **Wire `protocol_version`**: 1.0

The two track the same number by convention. The spec revision covers
this document's text; the wire version (emitted on `run_started`)
covers event shape. A text-only clarification bumps the spec revision
(1.0 → 1.1) without touching the wire version.

Wire events are defined in `events.rs`. This file is for cross-cutting
rules that don't belong on a single variant's doc comment.

## Versioning

Every `run_started` carries a `protocol_version` string (see
`events::PROTOCOL_VERSION`). Agents SHOULD read it on handshake and
reject versions they don't understand rather than failing mid-run on
a field-shape surprise.

Bump policy:
- **Patch/minor (no change to field shape)**: adding optional fields,
  adding variants, adding enum cases — no version bump. Agents MUST
  ignore unknown fields per the extension rule below.
- **Major**: removing or renaming a field/variant, or changing a
  field's semantic — bump the leading digit.

## Ordering

- `seq: u64` is assigned **inside the writer task**, not at enqueue, so
  it matches line-write order on stdout exactly. Agents MUST use `seq`
  as the only reliable ordering primitive. It starts at 0 and
  increments by 1 per emitted line.
- `started_at` / `ended_at` are wall-clock ISO 8601. An NTP backward
  jump can reorder them relative to `seq`. They are **advisory** for
  human display; don't use them to order events.

## Terminal invariant

- `run_finished` is the last event. After it is emitted, the emitter
  is **finalized**: any subsequent `enqueue()` is dropped and a
  post-mortem event cannot appear on the wire.
- `run_crashed`, if present, immediately precedes `run_finished`.

## Extension rule

Once shipped, adding a required field to any variant is a breaking
change for agents. All new fields MUST land as:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub new_field: Option<T>,
```

Renames are also breaking — prefer adding a new field + deprecating
the old one. Removing or renaming a variant requires a major version
bump of the protocol.

## Payload caps

- `measurement_recorded.value` is capped at 1 MB of serialized JSON;
  oversize values are replaced with `{"truncated": true,
  "original_size_bytes": N}` and an `internal_warning` is emitted.
- Attachment paths are capped at 4 KB, short labels (measurement name,
  unit, attachment name, mimetype) at 1 KB.
- `internal_warning.detail` is capped at 4 KB for unknown-event
  payloads (connector drift case) and 10 KB for engine-emitted
  warnings (measurement / attachment truncation) so the
  structured context — phase_key, slot_id, per-field truncation
  flags — always fits even when every field hits its individual
  cap. Agents should not rely on receiving full payloads in
  warnings.

Full records land in `phase_finished.measurements` / the upload path;
live events are for streaming previews only.

## Control commands

- `abort_run` before `run_started` → `ui_error { reason:
  "invalid_state", got: "not_started" }`
- `abort_run` after `run_finished` → `ui_error { reason:
  "invalid_state", got: "finished" }`
- Double `abort_run` → `ui_error { reason: "invalid_state", got:
  "already_aborted" }`
- `get_state` reply (`state_snapshot`) carries `run_status`
  (`not_started` / `running` / `finished`) so agents can distinguish
  "empty phases because we haven't booted" from "empty phases because
  the procedure has none". If a UI request is in flight, the snapshot
  includes the full component spec under `active_ui_request`.

## Post-terminal behavior

After `run_finished` is written to stdout the stdin reader is
aborted. Commands sent by the agent after that point are **undefined
behavior** — the CLI may drop them silently, the process may have
already exited. Agents MUST stop sending after observing
`run_finished`.

## Flush timeout

The writer's flush wait is hardcoded to 5 seconds (`FLUSH_TIMEOUT` in
`emitter.rs`). If an agent on a slow consumer can't drain the socket
in that window, late events may be lost under a forced shutdown. This
is deliberately not configurable — a stuck stdout is always a bug;
five seconds is generous for in-process signaling.

## Event reference

Every variant emitted on stdout. Field lists are abbreviated — `events.rs`
is the single source of truth for exact field names, types, and
`Option` / `skip_serializing_if` markers.

| `type`                 | Purpose                                               |
|------------------------|-------------------------------------------------------|
| `run_started`          | First event. Carries `procedure_id`, `protocol_version`. |
| `plan`                 | Phase plan: ordered list of `{key, name}` tuples.      |
| `phase_started`        | Phase entered execution. `(phase_key, attempt, slot_id)`. |
| `phase_finished`       | Phase finished. Outcome + timestamps + `duration_ms` + optional `error`. |
| `phase_skipped`        | Phase never ran (plug init failure upstream, `on_first_failure: stop`). |
| `phase_log`            | Live log line from a running phase.                    |
| `ui_request`           | Operator prompt. Agent must reply with `ui_response`. |
| `ui_auto_continue`     | CLI auto-resolved the prompt (pre-baked or display-only). |
| `ui_timeout`           | Prompt expired without a response (`--ui-timeout`). |
| `ui_error`             | Agent's `ui_response` / command rejected. `reason` enumerates causes. |
| `plug_status`          | Plug lifecycle transition (idle → initializing → active / error). |
| `plug_log`             | Live log line from a plug's Python service. |
| `measurement_recorded` | Live measurement write. `outcome` is always `"unset"` here. |
| `attachment_added`     | Phase attached a blob. YAML: live; OpenHTF: batched post-phase. |
| `state_snapshot`       | Reply to `get_state`. `run_status` + phase history + active UI. |
| `run_crashed`          | Subprocess died before a proper `test_end`. Immediately precedes `run_finished`. |
| `run_upload_queued`    | Run persisted to local queue for deferred upload. |
| `internal_warning`     | Non-fatal CLI-side anomaly (truncation, unknown python event, …). |
| `run_finished`         | Last event. `outcome` + `exit_code`.                   |

Stdin commands (agent → CLI): `ui_response`, `abort_run`, `get_state`.
See `events.rs::CliCommand`.
